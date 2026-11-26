//! macOS: every way to quit goes through the player's own exit
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

use std::cell::RefCell;
use std::rc::Rc;

use objc2::ffi;
use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
use objc2::{class, msg_send, sel};

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

thread_local! {
    /// The player's answer; on the main thread, where AppKit asks
    static HANDLER: RefCell<Option<Handler>> = const { RefCell::new(None) };
}

/// Answer AppKit's "should the application terminate?" with `handler` from
/// now on. Call on the main thread, once the window (and with it winit's
/// event loop and application delegate) has been made.
pub fn on_terminate(handler: impl Fn(AskedBy) -> Answer + 'static) -> Result<(), &'static str> {
    HANDLER.with(|slot| *slot.borrow_mut() = Some(Rc::new(handler)));
    // SAFETY: plain messages to the shared application, whose `delegate`
    // is an object or nil. The method added to the delegate's class has
    // the signature AppKit calls `applicationShouldTerminate:` with
    // (`NSUInteger (id self, SEL _cmd, id sender)`, "Q@:@"), and a class
    // may get methods after its objects were made.
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
        add_should_terminate(class);
        // AppKit may note what a delegate answers when it is set
        let _: () = msg_send![app, setDelegate: delegate];
    }
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

    #[test]
    fn the_quit_reason_keyword_is_apples() {
        // AE/AERegistry.h: keyAEQuitReason = 'why?'
        assert_eq!(KEY_AE_QUIT_REASON, 0x7768_793F);
        // Outside an Apple event there is no reason
        assert_eq!(quit_reason(), None);
    }
}
