//! macOS: what AppKit asks the application, answered by the player
//!
//! Four things reach a Mac app only through its application delegate or
//! the process's own Cocoa objects, and winit (whose delegate it is) passes
//! none of them on. They are added here with the Objective-C runtime:
//!
//! - **Quit** (below);
//! - **reopen** ([`on_reopen`]): a click on the Dock icon, or a second
//!   launch from Finder, Spotlight or `open`, which Launch Services turns
//!   into a message to the running app instead of a second process;
//! - **open** ([`on_open`]): files and addresses opened with the app in
//!   Finder (Open With, a double click, a drop on the Dock icon) or with
//!   `open -a Radiotrope file.m3u`. They come as an Apple event, never as
//!   arguments, also when the app is started by them;
//! - **staying awake** ([`Activity`]): the Mac doesn't go to sleep by
//!   itself while a recording runs.
//!
//! # Every way to quit goes through the player's own exit
//!
//! Quit in the app's menu (and its Cmd+Q), Quit in the Dock's menu, and
//! logging out, restarting or shutting down all end in AppKit's
//! `-[NSApplication terminate:]`, which exits the process from inside the
//! event loop: `main` never gets to save the settings, credit the listening
//! session or let a recording finish its file. AppKit asks the application's
//! delegate first, though (`applicationShouldTerminate:`), and the delegate
//! is winit's, which doesn't answer that question. So the answer is added
//! to winit's delegate class here, with the Objective-C runtime:
//!
//! - the user asked (menu, Cmd+Q, Dock): the termination is cancelled and
//!   the player's own Quit runs instead, which may ask first (a recording
//!   would stop) and then ends the event loop, so `main` exits as it does
//!   everywhere else;
//! - the system asked (logout, restart, shutdown): cancelling would stop
//!   the logout, so the exit's work is done at once, inside the question,
//!   and AppKit is told to go on.
//!
//! The menu bar is the player's own (ui/platform/macos/window.slint): its
//! Quit runs the player's Quit directly, and so does Cmd+Q, which is that
//! item's key. The answer here is still what the Dock's Quit and a logout
//! get.

use std::cell::{Cell, RefCell};
use std::ffi::{c_char, CStr, CString};
use std::path::PathBuf;
use std::rc::Rc;

use objc2::ffi;
use objc2::rc::autoreleasepool;
use objc2::runtime::{AnyClass, AnyObject, Bool, Imp, Sel};
use objc2::{class, msg_send, sel};

use crate::open_file::Opened;

/// Who asked the application to terminate
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AskedBy {
    /// Quit in the app's menu or the Dock's, Cmd+Q
    User,
    /// Logout, restart or shutdown
    System,
}

/// What to tell AppKit
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    /// Don't terminate: the player quits by itself (or asks first)
    Cancel,
    /// Everything is saved: terminate
    TerminateNow,
}

/// `NSApplicationTerminateReply`
const NS_TERMINATE_CANCEL: usize = 0;
const NS_TERMINATE_NOW: usize = 1;

/// `keyAEQuitReason` ('why?'): set on the quit event of a logout, restart
/// or shutdown, absent on a plain Quit
const KEY_AE_QUIT_REASON: u32 = u32::from_be_bytes(*b"why?");

type Handler = Rc<dyn Fn(AskedBy) -> Answer>;
type ReopenHandler = Rc<dyn Fn()>;
type OpenHandler = Rc<dyn Fn(Vec<Opened>)>;

thread_local! {
    /// The player's answer; on the main thread, where AppKit asks
    static HANDLER: RefCell<Option<Handler>> = const { RefCell::new(None) };
    /// What a click on the Dock icon does
    static REOPEN: RefCell<Option<ReopenHandler>> = const { RefCell::new(None) };
    /// What is done with opened files and addresses
    static OPEN: RefCell<Option<OpenHandler>> = const { RefCell::new(None) };
    /// Opened before anyone listened: handed over by [`on_open`]
    static OPENED_EARLY: RefCell<Vec<Opened>> = const { RefCell::new(Vec::new()) };
    /// The delegate's class has our methods
    static INSTALLED: Cell<bool> = const { Cell::new(false) };
}

/// Answer AppKit's "should the application terminate?" with `handler` from
/// now on. Call on the main thread, once the window (and with it winit's
/// event loop and application delegate) has been made.
pub fn on_terminate(handler: impl Fn(AskedBy) -> Answer + 'static) -> Result<(), &'static str> {
    HANDLER.with(|slot| *slot.borrow_mut() = Some(Rc::new(handler)));
    install()
}

/// Call `handler` when the user asks for the running app again: a click on
/// its Dock icon, or a second launch from Finder, Spotlight or `open`
/// (AppKit's `applicationShouldHandleReopen:hasVisibleWindows:`). The
/// handler runs on the main thread, inside AppKit's call: it should only
/// note what to do. Call as [`on_terminate`].
pub fn on_reopen(handler: impl Fn() + 'static) -> Result<(), &'static str> {
    REOPEN.with(|slot| *slot.borrow_mut() = Some(Rc::new(handler)));
    install()
}

/// Call `handler` with the files and addresses opened with the app
/// (AppKit's `application:openURLs:`), at once with any that came before
/// this call. It runs on the main thread, inside AppKit's call: it should
/// only note what to do. Call as [`on_terminate`], before the event loop
/// runs: what the app was started to open comes as the loop starts.
pub fn on_open(handler: impl Fn(Vec<Opened>) + 'static) -> Result<(), &'static str> {
    listen_for_opened(Rc::new(handler));
    install()
}

/// From now on `handler` gets what is opened, and at once what was opened
/// before
fn listen_for_opened(handler: OpenHandler) {
    OPEN.with(|slot| *slot.borrow_mut() = Some(handler.clone()));
    let early = OPENED_EARLY.with(|early| std::mem::take(&mut *early.borrow_mut()));
    if !early.is_empty() {
        handler(early);
    }
}

/// Give the application delegate's class our methods, once
fn install() -> Result<(), &'static str> {
    if INSTALLED.get() {
        return Ok(());
    }
    // SAFETY: plain messages to the shared application, whose `delegate`
    // is an object or nil. The methods added to the delegate's class have
    // the signatures AppKit calls them with (see `add_methods`), and a
    // class may get methods after its objects were made.
    unsafe {
        let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
        let delegate: *mut AnyObject = msg_send![app, delegate];
        if delegate.is_null() {
            return Err("the application has no delegate");
        }
        let class = ffi::object_getClass(delegate).cast_mut();
        if class.is_null() {
            return Err("the application's delegate has no class");
        }
        add_methods(class);
        // AppKit may note what a delegate answers when it is set
        let _: () = msg_send![app, setDelegate: delegate];
    }
    INSTALLED.set(true);
    Ok(())
}

/// Window > Bring All to Front: the app's windows in front of the other
/// apps' (`-[NSApplication arrangeInFront:]`, what the item does in every
/// Mac app). Call on the main thread.
pub fn bring_all_to_front() {
    // SAFETY: a plain message to the shared application, with a nil
    // sender, as from a menu item without one
    unsafe {
        let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
        let nobody: *mut AnyObject = std::ptr::null_mut();
        let _: () = msg_send![app, arrangeInFront: nobody];
    }
}

/// `BOOL (id self, SEL _cmd, id sender, BOOL flag)`: `BOOL` is a C `bool`
/// on Apple Silicon and a `signed char` on Intel
#[cfg(target_arch = "aarch64")]
const REOPEN_TYPES: &CStr = c"B@:@B";
#[cfg(not(target_arch = "aarch64"))]
const REOPEN_TYPES: &CStr = c"c@:@c";

/// Give `class` our answers to AppKit (in place of any it has)
///
/// # Safety
/// `class` is a registered Objective-C class.
unsafe fn add_methods(class: *mut AnyClass) {
    // SAFETY: the caller's
    unsafe { add_should_terminate(class) };

    let method: extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject, Bool) -> Bool =
        should_handle_reopen;
    // SAFETY: an `IMP` is called with the signature its method was added
    // with, which is `should_handle_reopen`'s (`REOPEN_TYPES`)
    let imp: Imp = unsafe { std::mem::transmute(method) };
    // SAFETY: the caller's; the type string is NUL-terminated and copied
    unsafe {
        ffi::class_replaceMethod(
            class,
            sel!(applicationShouldHandleReopen:hasVisibleWindows:),
            imp,
            REOPEN_TYPES.as_ptr(),
        );
    }

    let method: extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject, *mut AnyObject) =
        open_urls;
    // SAFETY: as above: `void (id self, SEL _cmd, id application, id urls)`
    let imp: Imp = unsafe { std::mem::transmute(method) };
    // SAFETY: as above
    unsafe {
        ffi::class_replaceMethod(class, sel!(application:openURLs:), imp, c"v@:@@".as_ptr());
    }
}

/// `-[delegate applicationShouldHandleReopen:hasVisibleWindows:]`: the
/// player shows its window itself, so AppKit has nothing left to do (it
/// would bring back a window minimized to the Dock, which the player's
/// own showing does too). With nobody to ask, AppKit does as before.
extern "C-unwind" fn should_handle_reopen(
    _this: *mut AnyObject,
    _cmd: Sel,
    _sender: *mut AnyObject,
    _has_visible_windows: Bool,
) -> Bool {
    let handler = REOPEN.with(|slot| slot.borrow().clone());
    match handler {
        Some(handler) => {
            handler();
            Bool::NO
        }
        None => Bool::YES,
    }
}

/// `-[delegate application:openURLs:]`
extern "C-unwind" fn open_urls(
    _this: *mut AnyObject,
    _cmd: Sel,
    _application: *mut AnyObject,
    urls: *mut AnyObject,
) {
    // SAFETY: AppKit passes an `NSArray` of `NSURL`
    let opened = unsafe { opened_from(urls) };
    if opened.is_empty() {
        return;
    }
    let handler = OPEN.with(|slot| slot.borrow().clone());
    match handler {
        Some(handler) => handler(opened),
        None => OPENED_EARLY.with(|early| early.borrow_mut().extend(opened)),
    }
}

/// The files and addresses of an `NSArray` of `NSURL`
///
/// # Safety
/// `urls` is nil or an `NSArray` of `NSURL`.
unsafe fn opened_from(urls: *mut AnyObject) -> Vec<Opened> {
    if urls.is_null() {
        return Vec::new();
    }
    let mut opened = Vec::new();
    // SAFETY: plain messages to an array and its URLs; the strings are
    // copied before the objects can go
    unsafe {
        let count: usize = msg_send![urls, count];
        for index in 0..count {
            let url: *mut AnyObject = msg_send![urls, objectAtIndex: index];
            if url.is_null() {
                continue;
            }
            let is_file: Bool = msg_send![url, isFileURL];
            if is_file.as_bool() {
                let path: *mut AnyObject = msg_send![url, path];
                if let Some(path) = string_of(path) {
                    opened.push(Opened::File(PathBuf::from(path)));
                }
            } else {
                let address: *mut AnyObject = msg_send![url, absoluteString];
                if let Some(address) = string_of(address) {
                    opened.push(Opened::Url(address));
                }
            }
        }
    }
    opened
}

/// An `NSString`'s text
///
/// # Safety
/// `string` is nil or an `NSString`.
unsafe fn string_of(string: *mut AnyObject) -> Option<String> {
    if string.is_null() {
        return None;
    }
    // SAFETY: `UTF8String` is NUL-terminated and lives as long as the
    // string (or the autorelease pool); copied here
    unsafe {
        let utf8: *const c_char = msg_send![string, UTF8String];
        if utf8.is_null() {
            return None;
        }
        Some(CStr::from_ptr(utf8).to_string_lossy().into_owned())
    }
}

/// An `NSString` (autoreleased) of `text`; nil for a text with a NUL
///
/// # Safety
/// Call inside an autorelease pool.
unsafe fn ns_string(text: &str) -> *mut AnyObject {
    let Ok(text) = CString::new(text) else {
        return std::ptr::null_mut();
    };
    // SAFETY: a NUL-terminated UTF-8 string, copied by the call
    unsafe { msg_send![class!(NSString), stringWithUTF8String: text.as_ptr()] }
}

/// `NSActivityUserInitiated`: work the user asked for, which also keeps
/// the Mac from going to sleep when left alone (it includes
/// `NSActivityIdleSystemSleepDisabled`, 1 << 20) and the app out of App
/// Nap. The display may still sleep, and closing the lid or choosing
/// Sleep still puts the Mac to sleep.
const NS_ACTIVITY_USER_INITIATED: u64 = 0x00FF_FFFF;

/// While this lives, the Mac doesn't go to sleep by itself (a recording
/// runs). `NSProcessInfo`'s `beginActivityWithOptions:reason:`, ended when
/// it is dropped.
pub struct Activity {
    /// The activity's token, retained
    token: *mut AnyObject,
}

impl Activity {
    /// Start an activity; `reason` is what `pmset -g assertions` shows
    pub fn begin(reason: &str) -> Option<Self> {
        autoreleasepool(|_| {
            // SAFETY: plain messages to the process's `NSProcessInfo`,
            // with the options as `NSActivityOptions` (a `uint64_t`) and
            // an `NSString`; the token is an object, kept by retaining it
            unsafe {
                let reason = ns_string(reason);
                if reason.is_null() {
                    return None;
                }
                let info: *mut AnyObject = msg_send![class!(NSProcessInfo), processInfo];
                let token: *mut AnyObject = msg_send![
                    info,
                    beginActivityWithOptions: NS_ACTIVITY_USER_INITIATED,
                    reason: reason
                ];
                if token.is_null() {
                    return None;
                }
                Some(Self {
                    token: ffi::objc_retain(token),
                })
            }
        })
    }
}

impl Drop for Activity {
    fn drop(&mut self) {
        // SAFETY: the token of `begin`, retained there and not used again
        unsafe {
            let info: *mut AnyObject = msg_send![class!(NSProcessInfo), processInfo];
            let _: () = msg_send![info, endActivity: self.token];
            ffi::objc_release(self.token);
        }
    }
}

/// Give `class` our `applicationShouldTerminate:` (in place of one it has)
///
/// # Safety
/// `class` is a registered Objective-C class.
unsafe fn add_should_terminate(class: *mut AnyClass) {
    let method: extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject) -> usize =
        should_terminate;
    // SAFETY: an `IMP` is called with the signature its method was added
    // with, which is `should_terminate`'s (see `on_terminate`)
    let imp: Imp = unsafe { std::mem::transmute(method) };
    // SAFETY: the caller's; the type string is NUL-terminated and copied
    unsafe {
        ffi::class_replaceMethod(
            class,
            sel!(applicationShouldTerminate:),
            imp,
            c"Q@:@".as_ptr(),
        );
    }
}

/// `-[delegate applicationShouldTerminate:]`
extern "C-unwind" fn should_terminate(
    _this: *mut AnyObject,
    _cmd: Sel,
    _sender: *mut AnyObject,
) -> usize {
    let asked_by = if quit_reason().is_some() {
        AskedBy::System
    } else {
        AskedBy::User
    };
    // Taken out of the cell for the call: the handler may do anything
    let handler = HANDLER.with(|slot| slot.borrow().clone());
    match handler.map(|handler| handler(asked_by)) {
        Some(Answer::Cancel) => NS_TERMINATE_CANCEL,
        // With nobody to ask, as before: terminate
        Some(Answer::TerminateNow) | None => NS_TERMINATE_NOW,
    }
}

/// Why the quit event being handled was sent (`kAELogOut`, `kAERestart`,
/// `kAEShutDown`...): `None` for a plain Quit, and when the termination
/// didn't come from an Apple event at all (the menu's Quit, Cmd+Q)
fn quit_reason() -> Option<u32> {
    // SAFETY: plain messages; `currentAppleEvent` and the descriptor are
    // objects or nil, and `enumCodeValue` returns an `OSType` (u32)
    unsafe {
        let manager: *mut AnyObject =
            msg_send![class!(NSAppleEventManager), sharedAppleEventManager];
        if manager.is_null() {
            return None;
        }
        let event: *mut AnyObject = msg_send![manager, currentAppleEvent];
        if event.is_null() {
            return None;
        }
        let reason: *mut AnyObject =
            msg_send![event, attributeDescriptorForKeyword: KEY_AE_QUIT_REASON];
        if reason.is_null() {
            return None;
        }
        let code: u32 = msg_send![reason, enumCodeValue];
        (code != 0).then_some(code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2::runtime::{ClassBuilder, NSObject};
    use objc2::ClassType;

    /// What the test's handler was asked, and what it answers
    fn answer_with(answer: Answer) -> Rc<RefCell<Vec<AskedBy>>> {
        let asked = Rc::new(RefCell::new(Vec::new()));
        let seen = asked.clone();
        HANDLER.with(|slot| {
            *slot.borrow_mut() = Some(Rc::new(move |by| {
                seen.borrow_mut().push(by);
                answer
            }))
        });
        asked
    }

    // The method as AppKit would call it, on a class of the test's own
    // that has no `applicationShouldTerminate:` (as winit's delegate
    // doesn't): after `add_should_terminate` its objects answer the
    // question with the handler's answer.
    #[test]
    fn a_delegate_class_gets_the_question() {
        let builder = ClassBuilder::new(c"RadiotropeTestDelegate", NSObject::class())
            .expect("no class of this name yet");
        let class = builder.register();
        let responds = |class: &AnyClass| class.responds_to(sel!(applicationShouldTerminate:));
        assert!(!responds(class));
        // SAFETY: a registered class
        unsafe { add_should_terminate(std::ptr::from_ref(class).cast_mut()) };
        assert!(responds(class));

        // SAFETY: `new` on an NSObject subclass with no instance variables;
        // the messages have the signature the method was added with
        unsafe {
            let delegate: *mut AnyObject = msg_send![class, new];
            let nobody: *mut AnyObject = std::ptr::null_mut();

            let asked = answer_with(Answer::Cancel);
            let reply: usize = msg_send![delegate, applicationShouldTerminate: nobody];
            assert_eq!(reply, NS_TERMINATE_CANCEL);
            // No Apple event is being handled in a test: the user's Quit
            assert_eq!(*asked.borrow(), [AskedBy::User]);

            let asked = answer_with(Answer::TerminateNow);
            let reply: usize = msg_send![delegate, applicationShouldTerminate: nobody];
            assert_eq!(reply, NS_TERMINATE_NOW);
            assert_eq!(asked.borrow().len(), 1);

            // Nobody to ask: AppKit goes on, as it did before
            HANDLER.with(|slot| *slot.borrow_mut() = None);
            let reply: usize = msg_send![delegate, applicationShouldTerminate: nobody];
            assert_eq!(reply, NS_TERMINATE_NOW);

            let _: () = msg_send![delegate, release];
        }
    }

    // A click on the Dock icon, as AppKit would send it: the player's
    // handler runs and AppKit is told not to do anything itself
    #[test]
    fn a_delegate_class_is_told_of_a_reopen() {
        let builder = ClassBuilder::new(c"RadiotropeTestReopenDelegate", NSObject::class())
            .expect("no class of this name yet");
        let class = builder.register();
        let selector = sel!(applicationShouldHandleReopen:hasVisibleWindows:);
        assert!(!class.responds_to(selector));
        // SAFETY: a registered class
        unsafe { add_methods(std::ptr::from_ref(class).cast_mut()) };
        assert!(class.responds_to(selector));
        assert!(class.responds_to(sel!(applicationShouldTerminate:)));

        // SAFETY: as in `a_delegate_class_gets_the_question`
        unsafe {
            let delegate: *mut AnyObject = msg_send![class, new];
            let nobody: *mut AnyObject = std::ptr::null_mut();

            // Nobody to tell: AppKit does what it did before
            REOPEN.with(|slot| *slot.borrow_mut() = None);
            let reply: Bool = msg_send![
                delegate,
                applicationShouldHandleReopen: nobody,
                hasVisibleWindows: Bool::NO
            ];
            assert!(reply.as_bool());

            let asked = Rc::new(Cell::new(0));
            let seen = asked.clone();
            REOPEN.with(|slot| {
                *slot.borrow_mut() = Some(Rc::new(move || seen.set(seen.get() + 1)));
            });
            for visible in [Bool::NO, Bool::YES] {
                let reply: Bool = msg_send![
                    delegate,
                    applicationShouldHandleReopen: nobody,
                    hasVisibleWindows: visible
                ];
                assert!(!reply.as_bool());
            }
            assert_eq!(asked.get(), 2);

            REOPEN.with(|slot| *slot.borrow_mut() = None);
            let _: () = msg_send![delegate, release];
        }
    }

    // A playlist and an address opened with the app, as AppKit would send
    // them: files come as paths, the rest as written; what comes before
    // anyone listens is kept for the first listener
    #[test]
    fn a_delegate_class_is_given_what_was_opened() {
        let builder = ClassBuilder::new(c"RadiotropeTestOpenDelegate", NSObject::class())
            .expect("no class of this name yet");
        let class = builder.register();
        assert!(!class.responds_to(sel!(application:openURLs:)));
        // SAFETY: a registered class
        unsafe { add_methods(std::ptr::from_ref(class).cast_mut()) };
        assert!(class.responds_to(sel!(application:openURLs:)));

        autoreleasepool(|_| {
            // SAFETY: as in `a_delegate_class_gets_the_question`; the
            // array and its URLs are autoreleased objects of this pool
            unsafe {
                let delegate: *mut AnyObject = msg_send![class, new];
                let nobody: *mut AnyObject = std::ptr::null_mut();
                let file: *mut AnyObject = msg_send![
                    class!(NSURL),
                    fileURLWithPath: ns_string("/Users/anna/My Radio/jazz & blues.m3u")
                ];
                let address: *mut AnyObject = msg_send![
                    class!(NSURL),
                    URLWithString: ns_string("http://example.com/live.mp3?id=7")
                ];
                let urls: *mut AnyObject = msg_send![class!(NSMutableArray), array];
                let _: () = msg_send![urls, addObject: file];
                let _: () = msg_send![urls, addObject: address];
                let expected = vec![
                    Opened::File(PathBuf::from("/Users/anna/My Radio/jazz & blues.m3u")),
                    Opened::Url("http://example.com/live.mp3?id=7".into()),
                ];
                assert_eq!(opened_from(urls), expected);
                assert!(opened_from(nobody).is_empty());

                // Before the player listens
                OPEN.with(|slot| *slot.borrow_mut() = None);
                OPENED_EARLY.with(|early| early.borrow_mut().clear());
                let _: () = msg_send![delegate, application: nobody, openURLs: urls];
                assert_eq!(OPENED_EARLY.with(|early| early.borrow().clone()), expected);

                // The listener gets those at once
                let got = Rc::new(RefCell::new(Vec::new()));
                let seen = got.clone();
                listen_for_opened(Rc::new(move |opened| seen.borrow_mut().push(opened)));
                assert_eq!(got.borrow().as_slice(), std::slice::from_ref(&expected));
                assert!(OPENED_EARLY.with(|early| early.borrow().is_empty()));

                // And from then on what is opened
                let _: () = msg_send![delegate, application: nobody, openURLs: urls];
                assert_eq!(*got.borrow(), [expected.clone(), expected]);

                OPEN.with(|slot| *slot.borrow_mut() = None);
                let _: () = msg_send![delegate, release];
            }
        });
    }

    #[test]
    fn an_activity_begins_and_ends() {
        let activity = Activity::begin("Radiotrope test").expect("an activity");
        assert!(!activity.token.is_null());
        drop(activity);
        // A reason AppKit can't take as a C string
        assert!(Activity::begin("with a \0 NUL").is_none());
    }

    #[test]
    fn the_activity_keeps_the_mac_awake() {
        // NSProcessInfo.h: NSActivityUserInitiated =
        // (0x00FFFFFFULL | NSActivityIdleSystemSleepDisabled), the latter 1 << 20
        assert_eq!(NS_ACTIVITY_USER_INITIATED, 0x00FF_FFFF | (1 << 20));
    }

    #[test]
    fn the_quit_reason_keyword_is_apples() {
        // AE/AERegistry.h: keyAEQuitReason = 'why?'
        assert_eq!(KEY_AE_QUIT_REASON, 0x7768_793F);
        // Outside an Apple event there is no reason
        assert_eq!(quit_reason(), None);
    }
}
