//! Pairing a phone: with a QR code, or with a 4-digit code
//!
//! QR: the user asks the player for a QR code; it holds a one-time secret
//! (128 bits, so it can't be guessed) and the certificate's fingerprint, and
//! the phone that scans it sends the secret back. It works once, for
//! [`CODE_LIFETIME`].
//!
//! Code: a phone asks to pair; the player shows a code, and the phone sends
//! back what the user typed. A code works for [`CODE_LIFETIME`] and takes
//! [`CODE_TRIES`] wrong guesses, then the player shows a new one. Every
//! wrong guess counts, whichever code or pairing it was for: after
//! [`MAX_WRONG_GUESSES`] of them within [`FAILED_PAIRING_WINDOW`] (as many
//! as [`MAX_FAILED_PAIRINGS`] codes take), or [`MAX_STARTS`] pairings
//! started without one working, pairing by code stops until the user opens
//! the Remote Control dialog. It also stops after [`MAX_WRONG_GUESSES_EVER`]
//! wrong guesses however far apart, counted since the dialog was last
//! opened or a code last worked. So asking for a new code, waiting for one
//! to run out, or guessing slowly gives no more guesses. A QR code still
//! pairs then. One
//! pairing runs at a time. Nothing here touches the clock or the disk:
//! callers pass the time in, so tests can move it.

use std::time::{Duration, Instant};

use radiotrope_app::config::remote::{
    CODE_LIFETIME, CODE_TRIES, FAILED_PAIRING_WINDOW, MAX_FAILED_PAIRINGS,
};
use radiotrope_app::data::agent_token;
use radiotrope_app::data::remote::{device_name, random_hex};

/// Wrong guesses allowed within [`FAILED_PAIRING_WINDOW`], over every code
/// and pairing, before pairing by code stops
pub const MAX_WRONG_GUESSES: usize = CODE_TRIES as usize * MAX_FAILED_PAIRINGS;

/// Wrong guesses allowed however far apart, since the user last opened the
/// Remote Control dialog or a code last worked: guessing slowly, under
/// [`MAX_WRONG_GUESSES`] a window, stops here (30 guesses find a 4-digit
/// code 3 times in 1000)
pub const MAX_WRONG_GUESSES_EVER: usize = 30;

/// Pairings a phone may start within [`FAILED_PAIRING_WINDOW`] without one
/// working, before pairing by code stops: each shows a code window on the
/// player
pub const MAX_STARTS: usize = 10;

/// The pairing in progress, if any, and what failed lately
#[derive(Default)]
pub struct Pairing {
    pending: Option<Pending>,
    /// When wrong codes were sent lately
    failures: Vec<Instant>,
    /// Wrong codes sent since the dialog was opened or a code worked
    wrong_ever: usize,
    /// When pairings were started lately, since the last that worked
    starts: Vec<Instant>,
    /// Too many failures: no pairing until [`Pairing::unlock`]
    locked: bool,
    /// The QR code shown, if any
    qr: Option<QrOffer>,
}

struct QrOffer {
    secret: String,
    expires: Instant,
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
        self.starts
            .retain(|at| now.saturating_duration_since(*at) < FAILED_PAIRING_WINDOW);
        if self.starts.len() >= MAX_STARTS {
            self.locked = true;
            self.pending = None;
            return Err(StartError::Locked);
        }
        let id = random_hex(8).map_err(|_| StartError::Busy)?;
        self.starts.push(now);
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
        if agent_token::matches(&p.code, code.trim()) {
            let p = self.pending.take().expect("checked above");
            self.starts.clear();
            self.wrong_ever = 0;
            return Ok((p.device_id, p.device_name));
        }
        // Every wrong guess counts, so a new pairing or a new code never
        // gives more of them
        self.failures
            .retain(|at| now.saturating_duration_since(*at) < FAILED_PAIRING_WINDOW);
        self.failures.push(now);
        self.wrong_ever += 1;
        if self.failures.len() >= MAX_WRONG_GUESSES || self.wrong_ever >= MAX_WRONG_GUESSES_EVER {
            self.locked = true;
            self.pending = None;
            return Err(CheckError::Locked);
        }
        let p = self.pending.as_mut().expect("checked above");
        p.tries_left -= 1;
        if p.tries_left > 0 {
            return Err(CheckError::WrongCode {
                tries_left: p.tries_left,
            });
        }
        // Used up: a new code
        p.code = new_code();
        p.tries_left = CODE_TRIES;
        p.expires = now + CODE_LIFETIME;
        Err(CheckError::NewCode)
    }

    /// The user asked for a QR code: the secret it holds. Asking again
    /// makes a new one, and the old one stops working.
    pub fn start_qr(&mut self, now: Instant) -> Option<String> {
        let secret = random_hex(16).ok()?;
        self.qr = Some(QrOffer {
            secret: secret.clone(),
            expires: now + CODE_LIFETIME,
        });
        Some(secret)
    }

    /// A phone sends the secret it scanned: true once, for the QR code
    /// shown, while it lasts
    pub fn take_qr(&mut self, secret: &str, now: Instant) -> bool {
        let matches = self.qr.as_ref().is_some_and(|q| {
            now < q.expires && radiotrope_app::data::agent_token::matches(&q.secret, secret)
        });
        if matches {
            self.qr = None;
        }
        matches
    }

    /// The QR code's secret and how long it has left, while one is shown
    pub fn qr_shown(&self, now: Instant) -> Option<(String, Duration)> {
        self.qr
            .as_ref()
            .filter(|q| now < q.expires)
            .map(|q| (q.secret.clone(), q.expires - now))
    }

    /// The user closed the QR window
    pub fn cancel_qr(&mut self) {
        self.qr = None;
    }

    /// The user closed the code window
    pub fn cancel(&mut self) {
        self.pending = None;
    }

    /// The user opened the Remote Control dialog: pairing works again
    pub fn unlock(&mut self) {
        self.locked = false;
        self.failures.clear();
        self.wrong_ever = 0;
        self.starts.clear();
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
    fn asking_for_a_new_code_gives_no_more_guesses() {
        let now = Instant::now();
        let mut p = Pairing::default();
        // Two guesses, then a new pairing before the code is used up
        let mut guesses = 0;
        let stopped = 'guessing: loop {
            let id = match p.start("phone", "Pixel", now) {
                Ok(id) => id,
                Err(e) => break 'guessing e,
            };
            for _ in 0..CODE_TRIES - 1 {
                let c = code(&p, now);
                guesses += 1;
                if p.check(&id, &wrong(&c), now) == Err(CheckError::Locked) {
                    break 'guessing StartError::Locked;
                }
            }
            assert!(guesses < 100, "guessing never stops");
        };
        assert_eq!(stopped, StartError::Locked);
        assert_eq!(guesses, MAX_WRONG_GUESSES);
        assert!(p.is_locked());
        assert!(p.shown(now).is_none());
    }

    #[test]
    fn guessing_slowly_stops_too() {
        let mut now = Instant::now();
        let mut p = Pairing::default();
        let mut guesses = 0;
        // One guess a code, each left to run out: never many in a window
        while !p.is_locked() {
            let id = p.start("phone", "Pixel", now).unwrap();
            let c = code(&p, now);
            let _ = p.check(&id, &wrong(&c), now);
            guesses += 1;
            now += CODE_LIFETIME;
            assert!(guesses <= MAX_WRONG_GUESSES_EVER, "guessing never stops");
        }
        assert_eq!(guesses, MAX_WRONG_GUESSES_EVER);
        assert_eq!(p.start("phone", "Pixel", now), Err(StartError::Locked));

        // A code that works in between: the user is there, count again
        let mut p = Pairing::default();
        for _ in 0..3 {
            for _ in 0..MAX_WRONG_GUESSES_EVER - 1 {
                let id = p.start("phone", "Pixel", now).unwrap();
                let c = code(&p, now);
                let _ = p.check(&id, &wrong(&c), now);
                now += CODE_LIFETIME;
            }
            let id = p.start("phone", "Pixel", now).unwrap();
            let c = code(&p, now);
            assert!(p.check(&id, &c, now).is_ok());
        }
        assert!(!p.is_locked());
    }

    #[test]
    fn pairings_started_over_and_over_stop_pairing() {
        let now = Instant::now();
        let mut p = Pairing::default();
        for _ in 0..MAX_STARTS {
            assert!(p.start("phone", "Pixel", now).is_ok());
        }
        assert_eq!(p.start("phone", "Pixel", now), Err(StartError::Locked));
        assert!(p.shown(now).is_none(), "no code window is left open");
        // A QR code the user shows still pairs
        let secret = p.start_qr(now).unwrap();
        assert!(p.take_qr(&secret, now));
        p.unlock();
        assert!(p.start("phone", "Pixel", now).is_ok());

        // Pairings that work don't add up
        let mut p = Pairing::default();
        for _ in 0..MAX_STARTS * 2 {
            let id = p.start("phone", "Pixel", now).unwrap();
            let c = code(&p, now);
            assert!(p.check(&id, &c, now).is_ok());
        }
        // Nor do ones from long ago
        let mut p = Pairing::default();
        let mut later = now;
        for _ in 0..MAX_STARTS * 2 {
            assert!(p.start("phone", "Pixel", later).is_ok());
            later += FAILED_PAIRING_WINDOW / (MAX_STARTS as u32 - 1);
        }
    }

    #[test]
    fn a_qr_code_works_once_and_not_after_its_time() {
        let now = Instant::now();
        let mut p = Pairing::default();
        let secret = p.start_qr(now).unwrap();
        assert_eq!(secret.len(), 32);
        assert!(p.qr_shown(now).is_some());
        assert!(!p.take_qr("0123", now));
        assert!(p.take_qr(&secret, now));
        assert!(!p.take_qr(&secret, now));
        assert!(p.qr_shown(now).is_none());

        let secret = p.start_qr(now).unwrap();
        assert!(!p.take_qr(&secret, now + CODE_LIFETIME));

        // A new code replaces the old one
        let old = p.start_qr(now).unwrap();
        let new = p.start_qr(now).unwrap();
        assert!(!p.take_qr(&old, now));
        assert!(p.take_qr(&new, now));
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
