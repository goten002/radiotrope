// Copyright 2022-2022 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use crossbeam_channel::{unbounded, Receiver, Sender};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};

use crate::MenuId;

/// Describes a menu event emitted when a menu item is activated
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct MenuEvent {
    /// Id of the menu item which triggered this event
    pub id: MenuId,
}

/// A receiver that could be used to listen to menu events.
pub type MenuEventReceiver = Receiver<MenuEvent>;
type MenuEventHandler = Box<dyn Fn(MenuEvent) + Send + Sync + 'static>;

static MENU_CHANNEL: LazyLock<(Sender<MenuEvent>, MenuEventReceiver)> = LazyLock::new(unbounded);
static MENU_EVENT_HANDLER: OnceLock<Option<MenuEventHandler>> = OnceLock::new();
// Radiotrope: the handlers set after the first one. Upstream drops them
// (the `OnceLock` keeps the first), so of two users of muda in one program
// only one ever hears of a menu event: here the tray's menu (tray-icon) and
// the menu bar (Slint). Each of them tells its own items by their ids.
static LATER_EVENT_HANDLERS: Mutex<Vec<Arc<dyn Fn(MenuEvent) + Send + Sync + 'static>>> =
    Mutex::new(Vec::new());

impl MenuEvent {
    /// Returns the id of the menu item which triggered this event
    pub fn id(&self) -> &MenuId {
        &self.id
    }

    /// Gets a reference to the event channel's [`MenuEventReceiver`]
    /// which can be used to listen for menu events.
    ///
    /// ## Note
    ///
    /// This will not receive any events if [`MenuEvent::set_event_handler`] has been called with a `Some` value.
    pub fn receiver<'a>() -> &'a MenuEventReceiver {
        &MENU_CHANNEL.1
    }

    /// Set a handler to be called for new events. Useful for implementing custom event sender.
    ///
    /// ## Note
    ///
    /// Calling this function with a `Some` value,
    /// will not send new events to the channel associated with [`MenuEvent::receiver`]
    pub fn set_event_handler<F: Fn(MenuEvent) + Send + Sync + 'static>(f: Option<F>) {
        if let Some(f) = f {
            // Radiotrope: a handler after the first is kept too (see
            // LATER_EVENT_HANDLERS); upstream ignores it
            let f = Arc::new(f);
            let first = f.clone();
            if MENU_EVENT_HANDLER
                .set(Some(Box::new(move |event| first(event))))
                .is_err()
            {
                if let Ok(mut later) = LATER_EVENT_HANDLERS.lock() {
                    later.push(f);
                }
            }
        } else {
            let _ = MENU_EVENT_HANDLER.set(None);
        }
    }

    pub(crate) fn send(event: MenuEvent) {
        if let Some(handler) = MENU_EVENT_HANDLER.get_or_init(|| None) {
            // Radiotrope: the later handlers hear of it as well, called
            // without the lock held (a handler may set another)
            let later = LATER_EVENT_HANDLERS
                .lock()
                .map(|later| later.clone())
                .unwrap_or_default();
            handler(event.clone());
            for handler in later {
                handler(event.clone());
            }
        } else {
            let _ = MENU_CHANNEL.0.send(event);
        }
    }
}
