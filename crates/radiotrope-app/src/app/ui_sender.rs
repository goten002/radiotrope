//! Commands from the UI thread to the controller, without ever blocking it
//!
//! The command channel is bounded. A controller stuck on its audio device
//! stops taking commands, and a blocking send from the UI thread would
//! freeze the window. Commands that don't fit wait here instead, in order,
//! and go out once there is room (`flush`, called by the UI's poll timer).
//! A slider dragged meanwhile keeps only its latest value.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Sender, TrySendError};

use super::state::AppCommand;

/// The UI thread's handle on the command channel (cheap to clone)
#[derive(Clone)]
pub struct UiSender {
    tx: Sender<AppCommand>,
    waiting: Rc<RefCell<VecDeque<AppCommand>>>,
}

/// A value a slider sets: a newer one replaces any still waiting
#[derive(PartialEq)]
enum Slot {
    Volume,
    EqBand(usize),
    EqPreamp,
}

fn slot(cmd: &AppCommand) -> Option<Slot> {
    match cmd {
        AppCommand::SetVolume(_) => Some(Slot::Volume),
        AppCommand::SetEqBand { band, .. } => Some(Slot::EqBand(*band)),
        AppCommand::SetEqPreamp(_) => Some(Slot::EqPreamp),
        _ => None,
    }
}

impl UiSender {
    pub fn new(tx: Sender<AppCommand>) -> Self {
        Self {
            tx,
            waiting: Default::default(),
        }
    }

    /// Send `cmd` now if there is room, or after the commands waiting
    pub fn send(&self, cmd: AppCommand) {
        let mut waiting = self.waiting.borrow_mut();
        if waiting.is_empty() {
            match self.tx.try_send(cmd) {
                Ok(()) | Err(TrySendError::Disconnected(_)) => return,
                Err(TrySendError::Full(cmd)) => waiting.push_back(cmd),
            }
            return;
        }
        // The newer value goes to the back, after anything sent since the
        // old one, so the controller ends up where the UI is
        if let Some(kind) = slot(&cmd) {
            waiting.retain(|c| slot(c).as_ref() != Some(&kind));
        }
        waiting.push_back(cmd);
        drop(waiting);
        self.flush();
    }

    /// Send what waits, as far as there is room
    pub fn flush(&self) {
        let mut waiting = self.waiting.borrow_mut();
        while let Some(cmd) = waiting.pop_front() {
            match self.tx.try_send(cmd) {
                Ok(()) => {}
                Err(TrySendError::Full(cmd)) => {
                    waiting.push_front(cmd);
                    return;
                }
                // The controller is gone: nothing will take them
                Err(TrySendError::Disconnected(_)) => {
                    waiting.clear();
                    return;
                }
            }
        }
    }

    /// Send what waits, then `Shutdown`, waiting at most `timeout` for
    /// room. False if the controller didn't take it in time.
    pub fn shutdown(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut waiting = std::mem::take(&mut *self.waiting.borrow_mut());
        waiting.push_back(AppCommand::Shutdown);
        waiting
            .into_iter()
            .all(|cmd| self.tx.send_deadline(cmd, deadline).is_ok())
    }

    #[cfg(test)]
    fn waiting(&self) -> usize {
        self.waiting.borrow().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::{bounded, Receiver};

    fn volume(rx: &Receiver<AppCommand>) -> Option<f32> {
        match rx.try_recv().ok()? {
            AppCommand::SetVolume(v) => Some(v),
            _ => None,
        }
    }

    #[test]
    fn commands_go_straight_out_while_there_is_room() {
        let (tx, rx) = bounded(4);
        let ui = UiSender::new(tx);
        ui.send(AppCommand::SetVolume(0.5));
        ui.send(AppCommand::Stop);
        assert_eq!(ui.waiting(), 0);
        assert_eq!(volume(&rx), Some(0.5));
        assert!(matches!(rx.try_recv(), Ok(AppCommand::Stop)));
    }

    #[test]
    fn a_full_queue_never_blocks_and_keeps_the_latest_slider_value() {
        let (tx, rx) = bounded(1);
        let ui = UiSender::new(tx);
        ui.send(AppCommand::Stop);
        // A volume drag while the controller takes nothing
        for i in 1..=100 {
            ui.send(AppCommand::SetVolume(i as f32 / 100.0));
        }
        ui.send(AppCommand::Mute);
        ui.send(AppCommand::SetVolume(0.25));
        // The drag's last value, after the Mute sent in between
        assert_eq!(ui.waiting(), 2);

        // Room again: one at a time, in order
        assert!(matches!(rx.try_recv(), Ok(AppCommand::Stop)));
        ui.flush();
        assert!(matches!(rx.try_recv(), Ok(AppCommand::Mute)));
        ui.flush();
        assert_eq!(volume(&rx), Some(0.25));
        assert_eq!(ui.waiting(), 0);
    }

    #[test]
    fn each_eq_band_keeps_its_own_latest_value() {
        let (tx, rx) = bounded(1);
        let ui = UiSender::new(tx);
        ui.send(AppCommand::Stop);
        for gain in [1.0, 2.0, 3.0] {
            ui.send(AppCommand::SetEqBand {
                band: 0,
                gain_db: gain,
            });
            ui.send(AppCommand::SetEqBand {
                band: 1,
                gain_db: -gain,
            });
        }
        assert_eq!(ui.waiting(), 2);
        let _ = rx.try_recv();
        let mut bands = Vec::new();
        for _ in 0..2 {
            ui.flush();
            if let Ok(AppCommand::SetEqBand { band, gain_db }) = rx.try_recv() {
                bands.push((band, gain_db));
            }
        }
        assert_eq!(bands, [(0, 3.0), (1, -3.0)]);
    }

    #[test]
    fn a_new_command_waits_behind_older_ones() {
        let (tx, rx) = bounded(1);
        let ui = UiSender::new(tx);
        ui.send(AppCommand::Stop);
        ui.send(AppCommand::Mute);
        let _ = rx.try_recv();
        // Room for one: the Mute that waited goes first
        ui.send(AppCommand::Unmute);
        assert!(matches!(rx.try_recv(), Ok(AppCommand::Mute)));
        ui.flush();
        assert!(matches!(rx.try_recv(), Ok(AppCommand::Unmute)));
    }

    #[test]
    fn shutdown_gives_up_on_a_stuck_controller() {
        let (tx, _rx) = bounded(1);
        let ui = UiSender::new(tx);
        ui.send(AppCommand::Stop);
        let started = Instant::now();
        assert!(!ui.shutdown(Duration::from_millis(50)));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn shutdown_follows_what_waits() {
        let (tx, rx) = bounded(8);
        let ui = UiSender::new(tx.clone());
        // Fill the queue from elsewhere, so the UI's volume has to wait
        for _ in 0..8 {
            tx.send(AppCommand::Stop).unwrap();
        }
        ui.send(AppCommand::SetVolume(0.75));
        let drain = std::thread::spawn(move || {
            let mut got = Vec::new();
            while let Ok(cmd) = rx.recv_timeout(Duration::from_secs(2)) {
                let last = matches!(cmd, AppCommand::Shutdown);
                got.push(cmd);
                if last {
                    break;
                }
            }
            got
        });
        assert!(ui.shutdown(Duration::from_secs(2)));
        let got = drain.join().unwrap();
        let n = got.len();
        assert!(matches!(got[n - 2], AppCommand::SetVolume(v) if v == 0.75));
        assert!(matches!(got[n - 1], AppCommand::Shutdown));
    }

    #[test]
    fn a_gone_controller_drops_commands() {
        let (tx, rx) = bounded(1);
        drop(rx);
        let ui = UiSender::new(tx);
        ui.send(AppCommand::Stop);
        ui.send(AppCommand::SetVolume(0.5));
        assert_eq!(ui.waiting(), 0);
        assert!(!ui.shutdown(Duration::from_millis(10)));
    }
}
