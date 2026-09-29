//! Logos of the station browser's rows
//!
//! Only rows on screen (and a few either side) get a decoded logo. Rows that
//! scroll far away give theirs back, and closing the browser lets go of
//! them all, so memory stays flat however many stations are loaded. The
//! logos themselves come from [`BrowseLogos`]: downloaded once, shrunk and
//! kept on disk.

use std::cell::RefCell;
use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use slint::{Model, SharedPixelBuffer};

use radiotrope_app::network::browse_logos::BrowseLogos;
use radiotrope_app::network::LogoService;

use crate::App;

/// Threads downloading and decoding logos
const WORKERS: usize = 6;

/// Rows beyond those on screen whose logos are loaded ahead of scrolling
const PRELOAD_ROWS: usize = 10;

/// Rows beyond those on screen that keep their logo; farther ones let go
const KEEP_ROWS: usize = 40;

/// Logos waiting to load, for the list of one generation
#[derive(Default)]
struct Queue {
    gen: u64,
    /// (row, logo URL), nearest to the screen first
    pending: VecDeque<(usize, String)>,
    /// Rows being loaded right now, with their generation
    busy: HashSet<(u64, usize)>,
}

#[derive(Clone)]
pub struct RowLogos {
    logos: Arc<BrowseLogos>,
    service: Arc<LogoService>,
    /// Bumped whenever the browser list is replaced, so late logos of an
    /// older list are dropped
    gen: Arc<AtomicU64>,
    queue: Arc<(Mutex<Queue>, Condvar)>,
}

thread_local! {
    /// Rows whose logo is set in the model, for the generation shown
    static SHOWN: RefCell<(u64, HashSet<usize>)> = RefCell::new((0, HashSet::new()));
}

impl RowLogos {
    /// Start the worker threads, which wait until there is work
    pub fn start(
        ui: slint::Weak<App>,
        logos: Arc<BrowseLogos>,
        service: Arc<LogoService>,
        gen: Arc<AtomicU64>,
    ) -> Self {
        let this = Self {
            logos,
            service,
            gen,
            queue: Default::default(),
        };
        for w in 0..WORKERS {
            let this = this.clone();
            let ui = ui.clone();
            std::thread::Builder::new()
                .name(format!("row-logo-{w}"))
                .spawn(move || this.work(ui))
                .ok();
        }
        this
    }

    pub fn logos(&self) -> &BrowseLogos {
        &self.logos
    }

    fn work(&self, ui: slint::Weak<App>) {
        let (lock, ready) = &*self.queue;
        loop {
            let (gen, row, url) = {
                let mut q = lock.lock().unwrap_or_else(|e| e.into_inner());
                loop {
                    if let Some((row, url)) = q.pending.pop_front() {
                        let gen = q.gen;
                        q.busy.insert((gen, row));
                        break (gen, row, url);
                    }
                    q = ready.wait(q).unwrap_or_else(|e| e.into_inner());
                }
            };
            let logo = (self.gen.load(Ordering::Relaxed) == gen)
                .then(|| self.logos.row_logo(&self.service, &url))
                .flatten();
            lock.lock()
                .unwrap_or_else(|e| e.into_inner())
                .busy
                .remove(&(gen, row));
            let Some((rgba, w, h)) = logo else { continue };
            let ui = ui.clone();
            let this = self.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = ui.upgrade() else { return };
                this.set_logo(&ui, gen, row, &rgba, w, h);
            });
        }
    }

    /// Put a loaded logo on its row, if the row is still near the screen
    fn set_logo(&self, ui: &App, gen: u64, row: usize, rgba: &[u8], w: u32, h: u32) {
        if self.gen.load(Ordering::Relaxed) != gen || !keep_range(ui).contains(&row) {
            return;
        }
        let model = ui.get_browse_logos();
        if row >= model.row_count() {
            return;
        }
        let pixels = SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(rgba, w, h);
        model.set_row_data(row, slint::Image::from_rgba8(pixels));
        SHOWN.with(|shown| {
            let mut shown = shown.borrow_mut();
            if shown.0 != gen {
                *shown = (gen, HashSet::new());
            }
            shown.1.insert(row);
        });
    }

    /// Load the logos of the rows on screen and nearby, nearest first, and
    /// let go of those far away. Called whenever the rows shown or the list
    /// change.
    pub fn refresh(&self, ui: &App) {
        let gen = self.gen.load(Ordering::Relaxed);
        let model = ui.get_browse_logos();
        let keep = keep_range(ui);
        let shown = SHOWN.with(|shown| {
            let mut shown = shown.borrow_mut();
            if shown.0 != gen {
                *shown = (gen, HashSet::new());
            }
            let far: Vec<usize> = shown
                .1
                .iter()
                .copied()
                .filter(|r| !keep.contains(r))
                .collect();
            for row in far {
                shown.1.remove(&row);
                if row < model.row_count() {
                    model.set_row_data(row, slint::Image::default());
                }
            }
            shown.1.clone()
        });

        let results = ui.get_search_results();
        let (first, count) = visible(ui);
        let end = (first + count + PRELOAD_ROWS).min(results.row_count());
        let start = first.saturating_sub(PRELOAD_ROWS);
        // On-screen rows first, then below, then above
        let order = (first..end).chain((start..first).rev());

        let (lock, ready) = &*self.queue;
        let mut q = lock.lock().unwrap_or_else(|e| e.into_inner());
        q.gen = gen;
        q.pending = order
            .filter(|row| !shown.contains(row) && !q.busy.contains(&(gen, *row)))
            .filter_map(|row| {
                let url = results.row_data(row)?.logo_url;
                (!url.is_empty()).then(|| (row, url.to_string()))
            })
            .collect();
        ready.notify_all();
    }

    /// Let go of every logo in the list (the browser closed, or the list is
    /// being put aside), and stop loading
    pub fn release(&self, ui: &App) {
        let model = ui.get_browse_logos();
        SHOWN.with(|shown| {
            for row in shown.borrow_mut().1.drain() {
                if row < model.row_count() {
                    model.set_row_data(row, slint::Image::default());
                }
            }
        });
        let (lock, _) = &*self.queue;
        lock.lock()
            .unwrap_or_else(|e| e.into_inner())
            .pending
            .clear();
    }
}

/// First row on screen and how many
fn visible(ui: &App) -> (usize, usize) {
    (
        ui.get_browse_visible_first().max(0) as usize,
        ui.get_browse_visible_count().max(0) as usize,
    )
}

/// Rows that keep their logo
fn keep_range(ui: &App) -> std::ops::Range<usize> {
    let (first, count) = visible(ui);
    first.saturating_sub(KEEP_ROWS)..first + count + KEEP_ROWS
}

/// Give memory freed by the browser back to the system. Windows' heap does
/// this by itself; glibc keeps freed memory for reuse unless asked.
pub fn return_freed_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: malloc_trim only releases free heap pages; it has no
    // preconditions.
    unsafe {
        libc::malloc_trim(0);
    }
}
