//! A Slint platform with no screen: windows draw into memory and time moves
//! only when told to. The snapshot tool renders with it, and the tests open
//! the real `App` with it.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Platform, PlatformError, WindowAdapter};

#[derive(Default)]
pub struct HeadlessPlatform {
    /// The window Slint made last, taken by whoever shows it
    pub window: Rc<Cell<Option<Rc<MinimalSoftwareWindow>>>>,
    /// The clock Slint's timers and animations run on
    pub time: Rc<Cell<Duration>>,
}

impl Platform for HeadlessPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        self.window.set(Some(window.clone()));
        Ok(window)
    }

    fn duration_since_start(&self) -> Duration {
        self.time.get()
    }
}
