//! Pairing a phone with a 4-digit code
//!
//! A phone asks to pair; the player shows a code, and the phone sends back
//! what the user typed. A code works for [`CODE_LIFETIME`] and takes
//! [`CODE_TRIES`] wrong guesses, then the player shows a new one; after
//! [`MAX_FAILED_PAIRINGS`] codes guessed wrong within
//! [`FAILED_PAIRING_WINDOW`], pairing stops until the user opens the Remote
//! Control dialog. One pairing runs at a time. Nothing here touches the
//! clock or the disk: callers pass the time in, so tests can move it.

use std::time::{Duration, Instant};

use radiotrope_app::config::remote::{
    CODE_LIFETIME, CODE_TRIES, FAILED_PAIRING_WINDOW, MAX_FAILED_PAIRINGS,
};
use radiotrope_app::data::remote::{device_name, random_hex};

/// The pairing in progress, if any, and what failed lately
#[derive(Default)]
pub struct Pairing {
    pending: Option<Pending>,
    /// When codes were last used up by wrong guesses
    failures: Vec<Instant>,
    /// Too many failures: no pairing until [`Pairing::unlock`]
    locked: bool,
}

struct Pending {
    id: String,
    code: String,
    device_id: String,
    device_name: String,
    expires: Instant,
    tries_left: u32,
}

/// What the window shows while a phone pairs
#[derive(Debug, Clone, PartialEq)]
pub struct Shown {
    pub device_name: String,
    pub code: String,
}

/// Why a phone can't start pairing
#[derive(Debug, PartialEq)]
pub enum StartError {
    /// Too many wrong codes lately
    Locked,
    /// Another phone is pairing
    Busy,
}

/// How a code check went wrong
#[derive(Debug, PartialEq)]
pub enum CheckError {
    /// No pairing with that id, or it ran out of time or was cancelled
    NoPairing,
    WrongCode {
        tries_left: u32,
    },
    /// The code was used up; the player shows a new one
    NewCode,
    /// Too many wrong codes lately
    Locked,
}

impl Pairing {
    /// A phone asks to pair: a new code to show. The same phone asking
    /// again gets a new code; another phone waits for this one.
    pub fn start(
        &mut self,
        device_id: &str,
        name: &str,
        now: Instant,
    ) -> Result<String, StartError> {
        if self.locked {
            return Err(StartError::Locked);
        }
        if let Some(p) = self.live(now) {
            if p.device_id != device_id {
                return Err(StartError::Busy);
            }
        }
        let id = random_hex(8).map_err(|_| StartError::Busy)?;
        self.pending = Some(Pending {
            id: id.clone(),
            code: new_code(),
            device_id: device_id.to_string(),
            device_name: device_name(name),
            expires: now + CODE_LIFETIME,
            tries_left: CODE_TRIES,
        });
        Ok(id)
    }

    /// The phone sends the code the user typed. Right: the phone's id and
    /// name, and the pairing is over.
    pub fn check(
        &mut self,
        id: &str,
        code: &str,
        now: Instant,
    ) -> Result<(String, String), CheckError> {
        if self.locked {
            return Err(CheckError::Locked);
        }
        let Some(p) = self.live(now).filter(|p| p.id == id) else {
            return Err(CheckError::NoPairing);
        };
        if p.code == code.trim() {
            let p = self.pending.take().expect("checked above");
            return Ok((p.device_id, p.device_name));
        }
        let p = self.pending.as_mut().expect("checked above");
        p.tries_left -= 1;
        if p.tries_left > 0 {
            return Err(CheckError::WrongCode {
                tries_left: p.tries_left,
            });
        }
        // Used up: count it, then a new code or a stop
        self.failures
            .retain(|at| now.saturating_duration_since(*at) < FAILED_PAIRING_WINDOW);
        self.failures.push(now);
        if self.failures.len() >= MAX_FAILED_PAIRINGS {
            self.locked = true;
            self.pending = None;
            return Err(CheckError::Locked);
        }
        let p = self.pending.as_mut().expect("checked above");
        p.code = new_code();
        p.tries_left = CODE_TRIES;
        p.expires = now + CODE_LIFETIME;
        Err(CheckError::NewCode)
    }

    /// The user closed the code window
    pub fn cancel(&mut self) {
        self.pending = None;
    }

    /// The user opened the Remote Control dialog: pairing works again
    pub fn unlock(&mut self) {
        self.locked = false;
        self.failures.clear();
    }

    #[cfg(test)]
    pub fn is_locked(&self) -> bool {
        self.locked
    }

    /// What the window shows now, if a phone is pairing
    pub fn shown(&self, now: Instant) -> Option<Shown> {
        self.live(now).map(|p| Shown {
            device_name: p.device_name.clone(),
            code: p.code.clone(),
        })
    }

    /// How long the code shown has left
    pub fn time_left(&self, now: Instant) -> Option<Duration> {
        self.live(now).map(|p| p.expires - now)
    }

    fn live(&self, now: Instant) -> Option<&Pending> {
        self.pending.as_ref().filter(|p| now < p.expires)
    }
}

/// Four random digits
fn new_code() -> String {
    let mut bytes = [0u8; 4];
    // Without randomness a fixed code is still guarded by the tries
    let _ = getrandom::fill(&mut bytes);
    format!("{:04}", u32::from_le_bytes(bytes) % 10_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(p: &Pairing, now: Instant) -> String {
        p.shown(now).unwrap().code
    }

    fn wrong(code: &str) -> String {
        format!("{:04}", (code.parse::<u32>().unwrap() + 1) % 10_000)
    }

    #[test]
    fn the_right_code_pairs_once() {
        let now = Instant::now();
        let mut p = Pairing::default();
        let id = p.start("phone", "Pixel", now).unwrap();
        let shown = p.shown(now).unwrap();
        assert_eq!(shown.device_name, "Pixel");
        assert_eq!(shown.code.len(), 4);
        assert_eq!(
            p.check(&id, &shown.code, now),
            Ok(("phone".into(), "Pixel".into()))
        );
        assert_eq!(p.check(&id, &shown.code, now), Err(CheckError::NoPairing));
        assert!(p.shown(now).is_none());
    }

    #[test]
    fn a_code_runs_out_of_time() {
        let now = Instant::now();
        let mut p = Pairing::default();
        let id = p.start("phone", "Pixel", now).unwrap();
        let c = code(&p, now);
        let later = now + CODE_LIFETIME;
        assert!(p.shown(later).is_none());
        assert_eq!(p.check(&id, &c, later), Err(CheckError::NoPairing));
    }

    #[test]
    fn wrong_codes_bring_a_new_code_then_a_stop() {
        let now = Instant::now();
        let mut p = Pairing::default();
        let id = p.start("phone", "Pixel", now).unwrap();
        for round in 0..MAX_FAILED_PAIRINGS {
            for left in (1..CODE_TRIES).rev() {
                let c = code(&p, now);
                assert_eq!(
                    p.check(&id, &wrong(&c), now),
                    Err(CheckError::WrongCode { tries_left: left })
                );
            }
            let c = code(&p, now);
            let last = p.check(&id, &wrong(&c), now);
            if round + 1 < MAX_FAILED_PAIRINGS {
                assert_eq!(last, Err(CheckError::NewCode));
            } else {
                assert_eq!(last, Err(CheckError::Locked));
            }
        }
        assert!(p.is_locked());
        assert_eq!(p.start("phone", "Pixel", now), Err(StartError::Locked));
        p.unlock();
        assert!(p.start("phone", "Pixel", now).is_ok());
    }

    #[test]
    fn old_failures_are_forgotten() {
        let mut now = Instant::now();
        let mut p = Pairing::default();
        for _ in 0..MAX_FAILED_PAIRINGS {
            let id = p.start("phone", "Pixel", now).unwrap();
            for _ in 0..CODE_TRIES {
                let c = code(&p, now);
                let _ = p.check(&id, &wrong(&c), now);
            }
            p.cancel();
            now += FAILED_PAIRING_WINDOW;
        }
        assert!(!p.is_locked());
    }

    #[test]
    fn one_phone_pairs_at_a_time() {
        let now = Instant::now();
        let mut p = Pairing::default();
        p.start("a", "A", now).unwrap();
        assert_eq!(p.start("b", "B", now), Err(StartError::Busy));
        // The same phone asking again gets a new pairing
        assert!(p.start("a", "A", now).is_ok());
        p.cancel();
        assert!(p.start("b", "B", now).is_ok());
    }
}
