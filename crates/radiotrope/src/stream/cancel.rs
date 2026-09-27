//! Cancelling a stream
//!
//! One [`StreamCancel`] follows a station from its resolve to its last
//! buffered byte. Stopping or switching station cancels it, and everything
//! working for that station stops at once: the resolve, the ICY or HLS
//! thread, their waits and backoff sleeps, and the stream buffer.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossbeam_channel::{bounded, select, Receiver, Sender};

type Callback = Box<dyn FnOnce() + Send>;

/// Stops everything working for one stream.
///
/// Clones share one state. Cancelling wakes whoever waits in
/// [`StreamCancel::sleep`] (and the crate's channel waits) and runs the
/// callbacks registered with [`StreamCancel::on_cancel`]. A blocking network
/// call can't be interrupted: it ends at its timeout, and its thread then
/// sees the cancel instead of carrying on.
#[derive(Clone)]
pub struct StreamCancel(Arc<Inner>);

struct Inner {
    cancelled: AtomicBool,
    /// Dropped on cancel, which disconnects `closed` and wakes its waiters
    open: Mutex<Option<Sender<()>>>,
    closed: Receiver<()>,
    callbacks: Mutex<Vec<Callback>>,
}

/// How a wait on a channel ended
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Waited<T> {
    Got(T),
    /// The sending side is gone
    Closed,
    TimedOut,
    Cancelled,
}

impl Default for StreamCancel {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for StreamCancel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("StreamCancel")
            .field(&self.is_cancelled())
            .finish()
    }
}

impl StreamCancel {
    pub fn new() -> Self {
        let (open, closed) = bounded(0);
        Self(Arc::new(Inner {
            cancelled: AtomicBool::new(false),
            open: Mutex::new(Some(open)),
            closed,
            callbacks: Mutex::new(Vec::new()),
        }))
    }

    /// Stop the stream. Only the first call does anything.
    pub fn cancel(&self) {
        if self.0.cancelled.swap(true, Ordering::SeqCst) {
            return;
        }
        drop(lock(&self.0.open).take());
        let callbacks = std::mem::take(&mut *lock(&self.0.callbacks));
        for callback in callbacks {
            callback();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::SeqCst)
    }

    /// Run `callback` once the stream is cancelled, or now if it already is
    pub fn on_cancel(&self, callback: impl FnOnce() + Send + 'static) {
        let mut callbacks = lock(&self.0.callbacks);
        // `cancel` sets the flag before taking the callbacks, so a callback
        // pushed while the flag is clear is still taken
        if self.is_cancelled() {
            drop(callbacks);
            callback();
        } else {
            callbacks.push(Box::new(callback));
        }
    }

    /// Sleep for `duration` unless cancelled first. Returns true if the
    /// whole time passed.
    pub fn sleep(&self, duration: Duration) -> bool {
        match self.0.closed.recv_timeout(duration) {
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => !self.is_cancelled(),
            _ => false,
        }
    }

    /// Wait for a message on `rx`, for at most `timeout` if given
    pub(crate) fn recv<T>(&self, rx: &Receiver<T>, timeout: Option<Duration>) -> Waited<T> {
        if self.is_cancelled() {
            return Waited::Cancelled;
        }
        let got = |msg: Result<T, _>| match msg {
            Ok(value) => Waited::Got(value),
            Err(_) => Waited::Closed,
        };
        match timeout {
            Some(timeout) => select! {
                recv(rx) -> msg => got(msg),
                recv(self.0.closed) -> _ => Waited::Cancelled,
                default(timeout) => Waited::TimedOut,
            },
            None => select! {
                recv(rx) -> msg => got(msg),
                recv(self.0.closed) -> _ => Waited::Cancelled,
            },
        }
    }

    /// Send `msg` on `tx`, waiting for room. Returns false if the receiver
    /// is gone or the stream was cancelled.
    pub(crate) fn send<T>(&self, tx: &Sender<T>, msg: T) -> bool {
        if self.is_cancelled() {
            return false;
        }
        select! {
            send(tx, msg) -> sent => sent.is_ok(),
            recv(self.0.closed) -> _ => false,
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::thread;
    use std::time::Instant;

    /// Cancel `cancel` from another thread after `after`
    fn cancel_after(cancel: &StreamCancel, after: Duration) {
        let cancel = cancel.clone();
        thread::spawn(move || {
            thread::sleep(after);
            cancel.cancel();
        });
    }

    #[test]
    fn clones_share_the_cancel() {
        let cancel = StreamCancel::new();
        let clone = cancel.clone();
        assert!(!clone.is_cancelled());
        cancel.cancel();
        assert!(clone.is_cancelled());
        cancel.cancel();
        assert!(clone.is_cancelled(), "cancelling twice is harmless");
    }

    #[test]
    fn a_sleep_ends_at_once_when_cancelled() {
        let cancel = StreamCancel::new();
        assert!(cancel.sleep(Duration::from_millis(20)), "a full sleep");

        cancel_after(&cancel, Duration::from_millis(50));
        let start = Instant::now();
        assert!(!cancel.sleep(Duration::from_secs(10)));
        assert!(start.elapsed() < Duration::from_secs(1));
        // Already cancelled: no sleep at all
        let start = Instant::now();
        assert!(!cancel.sleep(Duration::from_secs(10)));
        assert!(start.elapsed() < Duration::from_millis(50));
    }

    #[test]
    fn waits_on_a_channel_end_at_once_when_cancelled() {
        let cancel = StreamCancel::new();
        let (tx, rx) = bounded::<u32>(1);
        tx.send(7).unwrap();
        assert_eq!(cancel.recv(&rx, None), Waited::Got(7));
        assert_eq!(
            cancel.recv(&rx, Some(Duration::from_millis(20))),
            Waited::TimedOut
        );

        // Blocked on an empty channel with no timeout
        cancel_after(&cancel, Duration::from_millis(50));
        let start = Instant::now();
        assert_eq!(cancel.recv(&rx, None), Waited::Cancelled);
        assert!(start.elapsed() < Duration::from_secs(1));

        // Blocked sending into a full channel
        let cancel = StreamCancel::new();
        tx.send(1).unwrap();
        cancel_after(&cancel, Duration::from_millis(50));
        let start = Instant::now();
        assert!(!cancel.send(&tx, 2));
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn a_closed_channel_is_not_a_cancel() {
        let cancel = StreamCancel::new();
        let (tx, rx) = bounded::<u32>(1);
        drop(tx);
        assert_eq!(cancel.recv(&rx, None), Waited::Closed);

        let (tx, rx) = bounded::<u32>(1);
        drop(rx);
        assert!(!cancel.send(&tx, 1));
        assert!(!cancel.is_cancelled());
    }

    #[test]
    fn callbacks_run_once_whenever_they_are_added() {
        let cancel = StreamCancel::new();
        let runs = Arc::new(AtomicUsize::new(0));
        let counter = runs.clone();
        cancel.on_cancel(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(runs.load(Ordering::SeqCst), 0);
        cancel.cancel();
        cancel.cancel();
        assert_eq!(runs.load(Ordering::SeqCst), 1);

        // Added after the cancel: runs right away
        let counter = runs.clone();
        cancel.on_cancel(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(runs.load(Ordering::SeqCst), 2);
    }
}
