/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! Detect an emulator that has stopped making progress.
//!
//! The failure this exists for is a *hang*, not a crash: the game shows its
//! splash screen (or a publisher logo), the picture goes black (or freezes)
//! and nothing else happens, forever. There is no panic, no crash report and
//! no log line that says what went wrong, because the guest thread is sitting
//! inside some host function (a socket read, a lock, a wait) or spinning in
//! guest code that never reaches an observable API again. From the outside
//! this is indistinguishable from "the emulator is being slow", so users
//! reasonably report "black screen after the logo" and nothing more.
//!
//! This module runs a small host thread that watches counters the emulation
//! loop already maintains, and reports **what was measured**, not a guess at
//! the cause:
//!
//! * guest CPU progress ([crate::environment::GUEST_PC_RING_IDX], bumped on
//!   every `run_or_step`);
//! * guest present calls ([note_guest_present], one per
//!   `-[EAGLContext presentRenderbuffer:]` that reached the presenter);
//! * host swaps ([note_frame_presented], one per
//!   [crate::window::Window::swap_window]); and
//! * visible content ([note_visible_pixels]), only when a presenter has
//!   pixels in hand anyway.
//!
//! A host swap is **not** visible output: a swap of a black or stale
//! framebuffer is still a swap, and a guest that is running without swapping
//! may simply be loading or spinning. CPU activity without swaps therefore
//! proves neither a presentation fault nor a guest fault. What it does
//! distinguish is:
//!
//! * CPU not advancing: the guest thread is blocked in a host function;
//! * guest calls present but the host never swaps: the present path is not
//!   reaching the window;
//! * guest running, no present calls, no swaps: the guest is not presenting
//!   (loading, waiting or spinning), which can be legitimate;
//! * swaps continue but sampled content stays black.
//!
//! Every report names the last guest PC/LR and the last executed guest PCs,
//! and (on Android) the blocked/black cases offer to share the log, because a
//! hang leaves its evidence in a file the user cannot otherwise reach without
//! ADB. The "not presenting" case is logged only, never a dialog.
//!
//! [`Tracker`] holds all the decision logic and takes its inputs as plain
//! values, so it is unit-tested with synthetic clocks.
//!
//! The watchdog never touches the [`crate::Environment`]: the guest thread may
//! be blocked inside it, so everything it reports has to come from atomics.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Host window swaps since start-up. A swap is not proof of visible pixels.
static HOST_SWAPS: AtomicU64 = AtomicU64::new(0);
/// Guest `presentRenderbuffer:` calls that reached the presenter.
static GUEST_PRESENTS: AtomicU64 = AtomicU64::new(0);
/// Bumped whenever a new app session begins (see [begin_session]).
static SESSION: AtomicU64 = AtomicU64::new(0);
/// Most recent [Visibility] sample, stored as its discriminant.
static VISIBILITY: AtomicU8 = AtomicU8::new(Visibility::NotMeasured as u8);
/// Number of visibility samples taken so far.
static VISIBILITY_SAMPLES: AtomicU64 = AtomicU64::new(0);
/// Set once the watcher thread exists.
static STARTED: AtomicBool = AtomicBool::new(false);
/// Whether the user wants dialogs (`--no-error-popup` disables them).
static POPUPS_ALLOWED: AtomicBool = AtomicBool::new(false);
/// Set once a stall dialog has been offered, so dialogs never stack up.
#[cfg(target_os = "android")]
static DIALOG_SHOWN: AtomicBool = AtomicBool::new(false);
/// Set while the host has paused the emulator (the app went to the
/// background). A paused emulator has no guest progress by design.
static SUSPENDED: AtomicBool = AtomicBool::new(false);

/// How often the watcher looks at the counters. Frequent enough to notice a
/// hang promptly, rare enough to be invisible in a profile.
const POLL_INTERVAL: Duration = Duration::from_millis(500);
/// No guest CPU progress for this long means the emulator is blocked. Generous
/// on purpose: loading a big asset can block the guest thread for seconds on
/// slow storage, and a false report would be worse than a late one.
const NO_PROGRESS_REPORT_AFTER: Duration = Duration::from_secs(20);
/// No host swap for this long, while the guest still runs, is worth a log
/// line. It is a measurement, not a diagnosis: loading screens and spinning
/// guests look the same from here (see [FindingKind::NotPresenting]).
const NO_FRAMES_REPORT_AFTER: Duration = Duration::from_secs(30);
/// Swaps keep arriving but every sampled frame has been black for this long.
const BLACK_REPORT_AFTER: Duration = Duration::from_secs(30);
/// At most this many stall reports per process (the first one is the useful
/// one; the rest are there in case it is lost).
const MAX_STALL_REPORTS: u32 = 3;
/// Minimum gap between repeated stall reports.
const STALL_REPORT_INTERVAL: Duration = Duration::from_secs(120);
/// How many of the recent guest PCs to include (the ring holds 32).
const REPORT_PC_COUNT: usize = 16;
/// Largest `__symbol_stub` entry (32-bit ARM: 4, 12 or 16 bytes). Used to
/// decide whether an address lies inside a stub.
const MAX_STUB_ENTRY_SIZE: u32 = 16;

/// Guest import stubs and the linked function each one stands for.
///
/// Filled once per binary by `dyld::setup_lazy_linking`, which already knows
/// the name of every indirect symbol. It exists so that a stall report can say
/// "the app is blocked calling `_gethostbyname`" instead of printing an
/// address nobody can act on: the report is written by a host thread that must
/// not touch the guest's [`crate::Environment`], so the mapping has to be
/// available independently of it.
static IMPORT_STUBS: Mutex<Vec<(u32, String)>> = Mutex::new(Vec::new());

/// Record that `addr` is the import stub for the linked function `name`.
///
/// Called from the dynamic linker; harmless if called more than once.
pub fn register_import_stub(addr: u32, name: &str) {
    if let Ok(mut stubs) = IMPORT_STUBS.lock() {
        stubs.push((addr, name.to_string()));
    }
}

/// The linked function whose import stub contains `addr`, if any.
fn imported_function_at(addr: u32) -> Option<String> {
    let stubs = IMPORT_STUBS.lock().ok()?;
    // Stubs are laid out consecutively in name order, so the match is the
    // closest stub at or below the address, as long as the address is within
    // one entry of it.
    stubs
        .iter()
        .filter(|(start, _)| *start <= addr && addr - *start < MAX_STUB_ENTRY_SIZE)
        .max_by_key(|(start, _)| *start)
        .map(|(_, name)| name.clone())
}

/// What a presenter saw in the pixels it handled, when it looked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Visibility {
    /// No presenter has sampled pixels (the GPU-copy presenter never reads
    /// them back, so this is the usual state there).
    NotMeasured = 0,
    /// The sampled frame was entirely black.
    Black = 1,
    /// The sampled frame had at least one non-black byte.
    Content = 2,
}

impl Visibility {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Visibility::Black,
            2 => Visibility::Content,
            _ => Visibility::NotMeasured,
        }
    }
}

/// Record one host window swap.
///
/// Called from [crate::window::Window::swap_window], which is the single choke
/// point every presenter (GPU copy, RAM readback, IOSurface) goes through.
/// This counts swaps only; it says nothing about what was drawn.
pub fn note_frame_presented() {
    HOST_SWAPS.fetch_add(1, Ordering::Relaxed);
}

/// Record one guest `presentRenderbuffer:` call that reached the presenter.
///
/// Together with [note_frame_presented] this separates "the guest is not
/// presenting" from "the guest presents but nothing is swapped".
pub fn note_guest_present() {
    GUEST_PRESENTS.fetch_add(1, Ordering::Relaxed);
}

/// Record what a presenter saw in a frame it already had in RAM.
///
/// `max_channel` is the largest byte in the sampled pixels. Only call this
/// where the pixels exist anyway; never add a readback just for this.
pub fn note_visible_pixels(max_channel: u8) {
    let visibility = if max_channel == 0 {
        Visibility::Black
    } else {
        Visibility::Content
    };
    VISIBILITY.store(visibility as u8, Ordering::Relaxed);
    VISIBILITY_SAMPLES.fetch_add(1, Ordering::Relaxed);
}

/// Tell the watchdog that a new app session has begun (a different app was
/// launched, or the app picker was shown again). Baselines and per-session
/// report limits restart, so a long gap that belonged to the previous session
/// is never blamed on the new one.
pub fn begin_session() {
    VISIBILITY.store(Visibility::NotMeasured as u8, Ordering::Relaxed);
    SESSION.fetch_add(1, Ordering::Relaxed);
}

/// Tell the watchdog that the emulator is (or is no longer) paused by the host.
///
/// On Android, switching away from the app blocks the emulator thread until
/// the user comes back. That is a *deliberate* absence of progress and must not
/// be reported as a hang.
pub fn set_suspended(suspended: bool) {
    SUSPENDED.store(suspended, Ordering::Relaxed);
}

fn host_swaps() -> u64 {
    HOST_SWAPS.load(Ordering::Relaxed)
}

/// Guest CPU progress, reused from the crash-report machinery rather than
/// adding another counter to the hottest loop in the emulator.
fn guest_cpu_steps() -> u64 {
    crate::environment::GUEST_PC_RING_IDX.load(Ordering::Relaxed) as u64
}

/// The most recently executed guest PCs, oldest first.
fn recent_guest_pc_values(count: usize) -> Vec<u32> {
    let count = count.min(32);
    let newest = crate::environment::GUEST_PC_RING_IDX.load(Ordering::Relaxed);
    let start = newest.saturating_sub(count);
    (0..count)
        .map(|offset| crate::environment::GUEST_PC_RING[(start + offset) % 32].load(Ordering::Relaxed))
        .collect()
}

/// The most recently executed guest PCs, oldest first, formatted for the log.
fn recent_guest_pcs(count: usize) -> String {
    recent_guest_pc_values(count)
        .iter()
        .map(|pc| format!("{pc:#010x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Name the imported function the emulator was last seen entering, if the
/// recent PCs end in one. This is the single most useful line of a stall
/// report: it turns "blocked somewhere" into "blocked in `_recv`".
fn last_imported_function() -> Option<String> {
    // Newest first: the most recent stub the guest entered is the call it is
    // stuck in.
    recent_guest_pc_values(REPORT_PC_COUNT)
        .into_iter()
        .rev()
        .find_map(imported_function_at)
}

/// Start the watcher thread, once per process.
///
/// `popups_allowed` mirrors `--no-error-popup`: when error pop-ups are
/// disabled the watchdog still writes its report to the log, it just does not
/// interrupt the user with a dialog.
pub fn start(popups_allowed: bool) {
    POPUPS_ALLOWED.store(popups_allowed, Ordering::Relaxed);
    if STARTED.swap(true, Ordering::Relaxed) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("stall-watchdog".to_string())
        .spawn(watch);
    if let Err(err) = spawned {
        // Not fatal: the emulator works fine without the watchdog.
        log!("Warning: could not start the stall watchdog: {}", err);
    }
}

/// Which measured condition a [Finding] reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FindingKind {
    /// No guest instruction has retired for a long time.
    CpuBlocked,
    /// The guest keeps calling present but the host has not swapped.
    PresentCallsWithoutSwaps,
    /// The guest runs but has not called present; the host has not swapped.
    NotPresenting,
    /// Swaps continue but every sampled frame was black.
    BlackDespiteSwaps,
}

impl FindingKind {
    fn index(self) -> usize {
        self as usize
    }
    /// Whether this is worth interrupting the user with a dialog.
    fn offers_dialog(self) -> bool {
        // A guest that is not presenting may be loading; log it, don't nag.
        self != FindingKind::NotPresenting
    }
}

/// The measured numbers behind a report.
#[derive(Clone, Debug)]
pub struct Finding {
    pub kind: FindingKind,
    pub cpu_idle: Duration,
    pub since_swap: Duration,
    pub since_guest_present: Option<Duration>,
    pub guest_presents: u64,
    pub host_swaps: u64,
    pub presents_since_swap: u64,
    pub visibility: Visibility,
    pub black_for: Option<Duration>,
}

impl Finding {
    /// Facts first; any interpretation is worded as what is *not* shown.
    fn describe(&self) -> String {
        let presents = match self.since_guest_present {
            Some(ago) => format!(
                "{} guest present call(s), the last {:.1} s ago",
                self.guest_presents,
                ago.as_secs_f64()
            ),
            None => "no guest present calls this session".to_string(),
        };
        let counters = format!(
            "{presents}; {} host swap(s), the last {:.1} s ago \
             ({} guest present call(s) since it); visible content: {}",
            self.host_swaps,
            self.since_swap.as_secs_f64(),
            self.presents_since_swap,
            match self.visibility {
                Visibility::NotMeasured => "not measured (the GPU-copy \
                    presenter does not read pixels back)",
                Visibility::Black => "sampled frames were all black",
                Visibility::Content => "sampled frames had non-black pixels",
            },
        );
        match self.kind {
            FindingKind::CpuBlocked => format!(
                "The emulated CPU has not advanced for {:.1} s, so the guest \
                 thread is blocked, not slow. Measured: {counters}. The guest \
                 is probably inside a host function that has not returned; \
                 the import name and PCs below say which.",
                self.cpu_idle.as_secs_f64(),
            ),
            FindingKind::PresentCallsWithoutSwaps => format!(
                "The emulated CPU is running and the guest is calling present, \
                 but the host has not swapped a frame for {:.1} s. Measured: \
                 {counters}. Present calls are reaching the emulator but not \
                 the window; the presenter's path needs inspecting.",
                self.since_swap.as_secs_f64(),
            ),
            FindingKind::NotPresenting => format!(
                "The emulated CPU is running but the guest has not called \
                 present and the host has not swapped for {:.1} s. Measured: \
                 {counters}. This does not show a presentation fault: the \
                 guest may be loading, waiting or spinning, and some apps \
                 legitimately show no frames for this long.",
                self.since_swap.as_secs_f64(),
            ),
            FindingKind::BlackDespiteSwaps => format!(
                "The host is swapping frames, but every sampled frame has \
                 been black for {:.1} s. Measured: {counters}. Swaps do not \
                 prove visible pixels; the guest may be drawing black, or \
                 the sampled source may not be what reaches the screen.",
                self.black_for.unwrap_or_default().as_secs_f64(),
            ),
        }
    }
}

/// One reading of every counter the watcher looks at.
#[derive(Clone, Copy, Debug)]
pub struct Reading {
    pub steps: u64,
    pub guest_presents: u64,
    pub host_swaps: u64,
    pub visibility: Visibility,
    pub visibility_samples: u64,
    pub session: u64,
    pub suspended: bool,
}

fn read_counters() -> Reading {
    Reading {
        steps: guest_cpu_steps(),
        guest_presents: GUEST_PRESENTS.load(Ordering::Relaxed),
        host_swaps: host_swaps(),
        visibility: Visibility::from_u8(VISIBILITY.load(Ordering::Relaxed)),
        visibility_samples: VISIBILITY_SAMPLES.load(Ordering::Relaxed),
        session: SESSION.load(Ordering::Relaxed),
        suspended: SUSPENDED.load(Ordering::Relaxed),
    }
}

/// All of the watchdog's decisions, with no globals and no clock of its own,
/// so every scenario can be tested with synthetic time.
pub struct Tracker {
    session: u64,
    last_steps: u64,
    steps_changed_at: Instant,
    last_presents: u64,
    present_changed_at: Option<Instant>,
    last_swaps: u64,
    swap_changed_at: Instant,
    presents_at_last_swap: u64,
    last_samples: u64,
    sampled_at: Option<Instant>,
    black_since: Option<Instant>,
    reported: [bool; 4],
    reports_left: u32,
    next_blocked_report_at: Option<Instant>,
}

impl Tracker {
    pub fn new(now: Instant, reading: &Reading) -> Self {
        Tracker {
            session: reading.session,
            last_steps: reading.steps,
            steps_changed_at: now,
            last_presents: reading.guest_presents,
            present_changed_at: None,
            last_swaps: reading.host_swaps,
            // "Never swapped" is measured from the start of the session.
            swap_changed_at: now,
            presents_at_last_swap: reading.guest_presents,
            last_samples: reading.visibility_samples,
            sampled_at: None,
            black_since: None,
            reported: [false; 4],
            reports_left: MAX_STALL_REPORTS,
            next_blocked_report_at: None,
        }
    }

    /// Feed one poll; returns a finding when one is due.
    pub fn update(&mut self, now: Instant, r: &Reading) -> Option<Finding> {
        if r.session != self.session {
            // A new session: nothing measured before it counts.
            *self = Tracker::new(now, r);
            return None;
        }
        if r.suspended {
            // Paused by the host: time in the background is not a stall.
            let session = self.session;
            *self = Tracker::new(now, r);
            self.session = session;
            return None;
        }

        if r.steps != self.last_steps {
            self.last_steps = r.steps;
            self.steps_changed_at = now;
            self.next_blocked_report_at = None;
        }
        if r.guest_presents != self.last_presents {
            self.last_presents = r.guest_presents;
            self.present_changed_at = Some(now);
        }
        if r.host_swaps != self.last_swaps {
            self.last_swaps = r.host_swaps;
            self.swap_changed_at = now;
            self.presents_at_last_swap = r.guest_presents;
            // Swaps resumed: a later gap is a new event.
            self.reported[FindingKind::PresentCallsWithoutSwaps.index()] = false;
            self.reported[FindingKind::NotPresenting.index()] = false;
        }
        if r.visibility_samples != self.last_samples {
            self.last_samples = r.visibility_samples;
            self.sampled_at = Some(now);
            match r.visibility {
                Visibility::Black => {
                    self.black_since.get_or_insert(now);
                }
                _ => {
                    self.black_since = None;
                    self.reported[FindingKind::BlackDespiteSwaps.index()] = false;
                }
            }
        }

        // Nothing has run yet (still booting, or in the app picker).
        if r.steps == 0 {
            return None;
        }

        let cpu_idle = now.saturating_duration_since(self.steps_changed_at);
        let since_swap = now.saturating_duration_since(self.swap_changed_at);
        let finding = |kind: FindingKind| Finding {
            kind,
            cpu_idle,
            since_swap,
            since_guest_present: self
                .present_changed_at
                .map(|at| now.saturating_duration_since(at)),
            guest_presents: r.guest_presents,
            host_swaps: r.host_swaps,
            presents_since_swap: r
                .guest_presents
                .saturating_sub(self.presents_at_last_swap),
            visibility: r.visibility,
            black_for: self
                .black_since
                .map(|at| now.saturating_duration_since(at)),
        };

        if cpu_idle >= NO_PROGRESS_REPORT_AFTER {
            // Blocked: report at 20 s, then every STALL_REPORT_INTERVAL.
            let due = self.next_blocked_report_at.is_none_or(|at| now >= at);
            if self.reports_left > 0 && due {
                self.reports_left -= 1;
                self.next_blocked_report_at = Some(now + STALL_REPORT_INTERVAL);
                return Some(finding(FindingKind::CpuBlocked));
            }
            return None;
        }
        if cpu_idle >= POLL_INTERVAL * 4 {
            // Not running; the blocked report covers it once it is due.
            return None;
        }
        if self.reports_left == 0 {
            return None;
        }

        // Swaps are arriving but the sampled content is black.
        let swapping = since_swap < NO_FRAMES_REPORT_AFTER;
        let sampling = self
            .sampled_at
            .is_some_and(|at| now.saturating_duration_since(at) < NO_FRAMES_REPORT_AFTER);
        if swapping
            && sampling
            && self
                .black_since
                .is_some_and(|at| now.saturating_duration_since(at) >= BLACK_REPORT_AFTER)
            && !self.reported[FindingKind::BlackDespiteSwaps.index()]
        {
            self.reported[FindingKind::BlackDespiteSwaps.index()] = true;
            self.reports_left -= 1;
            return Some(finding(FindingKind::BlackDespiteSwaps));
        }

        // No swaps for a long time, while the guest runs.
        if since_swap >= NO_FRAMES_REPORT_AFTER {
            let kind = if r.guest_presents > self.presents_at_last_swap {
                FindingKind::PresentCallsWithoutSwaps
            } else {
                FindingKind::NotPresenting
            };
            if !self.reported[kind.index()] {
                self.reported[kind.index()] = true;
                self.reports_left -= 1;
                return Some(finding(kind));
            }
        }
        None
    }
}

fn watch() {
    let mut tracker = Tracker::new(Instant::now(), &read_counters());
    loop {
        std::thread::sleep(POLL_INTERVAL);
        let reading = read_counters();
        if let Some(finding) = tracker.update(Instant::now(), &reading) {
            report(&finding);
        }
    }
}

/// Log a finding with the last PCs, and offer log sharing when it warrants it.
fn report(finding: &Finding) {
    let pc = crate::environment::LAST_GUEST_PC.load(Ordering::Relaxed);
    let lr = crate::environment::LAST_GUEST_LR.load(Ordering::Relaxed);
    let text = format!(
        "{} Last guest PC: {pc:#010x}, LR: {lr:#010x}.",
        finding.describe()
    );
    log!("{}", text);
    if let Some(name) = last_imported_function() {
        log!("  the last imported function the app called was {name}");
    }
    log!("  recent guest PCs: {}", recent_guest_pcs(REPORT_PC_COUNT));
    if finding.kind == FindingKind::CpuBlocked {
        log!(
            "  this run's log: {} (the previous run is kept as {})",
            crate::log::log_file_path().display(),
            crate::log::previous_log_file_path().display(),
        );
    }
    if finding.kind.offers_dialog() {
        offer_log_sharing(&text);
    }
}

/// Show a dialog with a "share the log" button (Android only).
///
/// A Java dialog rather than an SDL message box, because the SDL event loop
/// lives on the guest thread, which is exactly what is stuck.
#[cfg(target_os = "android")]
fn offer_log_sharing(short_message: &str) {
    if !POPUPS_ALLOWED.load(Ordering::Relaxed) {
        return;
    }
    // One dialog per process: a stalled-and-recovered app must not stack
    // dialogs on the user.
    if DIALOG_SHOWN.swap(true, Ordering::Relaxed) {
        return;
    }
    let Some(uri) = crate::log_share::log_document_uri() else {
        return;
    };
    // Make the shared copy contain the report we just wrote.
    crate::log::flush_log();
    let message = format!(
        "The emulator has not made any progress for a while — the app is \
         stuck rather than slow.\n\n{short_message}\n\nSharing the log makes \
         the hang diagnosable."
    );
    log_no_panic!(
        "Stall watchdog: offering to share the log (dialogs {})",
        if crate::android_web_view::show_stall_dialog(
            &message,
            crate::log_share::SHARE_SUBJECT,
            &uri
        ) {
            "shown"
        } else {
            "unavailable"
        }
    );
}

#[cfg(not(target_os = "android"))]
fn offer_log_sharing(_short_message: &str) {
    // Desktop users can reach the log file directly; there is no share sheet
    // to pop up, and popping up a dialog on top of a stuck window would not
    // help anyone.
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The counters are process-wide, so the tests below avoid the global
    /// frame counter and only exercise the pure formatting/decision helpers.

    #[test]
    fn recent_pcs_are_clamped_to_the_ring_size() {
        // Asking for more PCs than the ring holds must not read out of bounds
        // or repeat entries.
        let text = recent_guest_pcs(1000);
        assert_eq!(text.split(' ').count(), 32);
        assert!(text.split(' ').all(|pc| pc.starts_with("0x")));
    }

    #[test]
    fn import_stubs_resolve_to_the_function_they_jump_to() {
        // The stall report's most useful line is the imported function the app
        // was last seen calling, so a PC anywhere inside a stub must resolve.
        register_import_stub(0x0001_0000, "_recv");
        register_import_stub(0x0001_0010, "_gethostbyname");
        assert_eq!(
            imported_function_at(0x0001_0000).as_deref(),
            Some("_recv")
        );
        // Mid-stub (the PC stops after the first instruction, typically).
        assert_eq!(
            imported_function_at(0x0001_0004).as_deref(),
            Some("_recv")
        );
        // The next stub starts exactly one (largest) entry later.
        assert_eq!(
            imported_function_at(0x0001_0010).as_deref(),
            Some("_gethostbyname")
        );
        // Addresses that are not stubs must not borrow a neighbouring name.
        assert_eq!(imported_function_at(0x0000_ffff), None);
        assert_eq!(imported_function_at(0x0002_0000), None);
    }

    #[test]
    fn recent_pcs_are_formatted_as_guest_addresses() {
        let text = recent_guest_pcs(2);
        assert_eq!(text.split(' ').count(), 2);
        for pc in text.split(' ') {
            assert_eq!(pc.len(), "0x00000000".len(), "unexpected formatting: {pc}");
        }
    }

    // --- Tracker scenarios: synthetic clock, no globals. ---

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn reading() -> Reading {
        Reading {
            steps: 1,
            guest_presents: 0,
            host_swaps: 0,
            visibility: Visibility::NotMeasured,
            visibility_samples: 0,
            session: 0,
            suspended: false,
        }
    }

    /// Poll every 500 ms for `total`, advancing the guest CPU each poll (a
    /// running guest) and applying `step` to the reading each time. Returns
    /// every finding with the time it was raised.
    fn run(
        tracker: &mut Tracker,
        start: Instant,
        from: Duration,
        total: Duration,
        r: &mut Reading,
        mut step: impl FnMut(&mut Reading, Duration),
    ) -> Vec<(Duration, Finding)> {
        let mut out = Vec::new();
        let mut t = from;
        while t < from + total {
            t += POLL_INTERVAL;
            r.steps += 1000;
            step(r, t);
            if let Some(f) = tracker.update(start + t, r) {
                out.push((t, f));
            }
        }
        out
    }

    #[test]
    fn spinning_guest_is_reported_as_not_presenting_not_as_a_render_fault() {
        // The observed case: CPU keeps running, nothing is presented and
        // nothing is swapped. That must not be called a presentation problem.
        let start = Instant::now();
        let mut r = reading();
        let mut tracker = Tracker::new(start, &r);
        let found = run(&mut tracker, start, secs(0), secs(40), &mut r, |_, _| {});
        assert_eq!(found.len(), 1);
        let (at, f) = &found[0];
        assert_eq!(f.kind, FindingKind::NotPresenting);
        assert!(*at >= NO_FRAMES_REPORT_AFTER);
        let text = f.describe();
        assert!(text.contains("does not show a presentation fault"), "{text}");
        assert!(!text.contains("presentation problem"), "{text}");
        // A pure "not presenting" report must never pop a dialog.
        assert!(!f.kind.offers_dialog());
    }

    #[test]
    fn a_legitimate_gap_without_frames_is_not_reported() {
        // Frames, then a 25 s load (under the threshold), then frames again.
        let start = Instant::now();
        let mut r = reading();
        let mut tracker = Tracker::new(start, &r);
        let found = run(&mut tracker, start, secs(0), secs(60), &mut r, |r, t| {
            let loading = t >= secs(10) && t < secs(35);
            if !loading {
                r.guest_presents += 1;
                r.host_swaps += 1;
            }
        });
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn present_calls_without_swaps_name_the_present_path() {
        let start = Instant::now();
        let mut r = reading();
        let mut tracker = Tracker::new(start, &r);
        let found = run(&mut tracker, start, secs(0), secs(40), &mut r, |r, _| {
            r.guest_presents += 1; // the guest presents, the host never swaps
        });
        assert_eq!(found.len(), 1);
        let f = &found[0].1;
        assert_eq!(f.kind, FindingKind::PresentCallsWithoutSwaps);
        assert!(f.presents_since_swap > 0);
        assert_eq!(f.host_swaps, 0);
        assert!(f.kind.offers_dialog());
    }

    #[test]
    fn swaps_alone_do_not_prove_visible_pixels() {
        // Swaps every poll, every sampled frame black: reported as such, and
        // the text separates "swapped" from "visible".
        let start = Instant::now();
        let mut r = reading();
        let mut tracker = Tracker::new(start, &r);
        let found = run(&mut tracker, start, secs(0), secs(45), &mut r, |r, _| {
            r.guest_presents += 1;
            r.host_swaps += 1;
            r.visibility = Visibility::Black;
            r.visibility_samples += 1;
        });
        assert_eq!(found.len(), 1);
        let f = &found[0].1;
        assert_eq!(f.kind, FindingKind::BlackDespiteSwaps);
        assert!(f.describe().contains("Swaps do not prove visible pixels"));
    }

    #[test]
    fn swaps_with_content_or_unmeasured_pixels_are_never_called_black() {
        let start = Instant::now();
        for visibility in [Visibility::Content, Visibility::NotMeasured] {
            let mut r = reading();
            let mut tracker = Tracker::new(start, &r);
            let found = run(&mut tracker, start, secs(0), secs(60), &mut r, |r, _| {
                r.guest_presents += 1;
                r.host_swaps += 1;
                r.visibility = visibility;
                if visibility == Visibility::Content {
                    r.visibility_samples += 1;
                }
            });
            assert!(found.is_empty(), "{visibility:?}: {found:?}");
        }
    }

    #[test]
    fn a_blocked_cpu_is_reported_once_per_interval_and_offers_a_dialog() {
        let start = Instant::now();
        let mut r = reading();
        let mut tracker = Tracker::new(start, &r);
        // Run for 5 s, then the guest thread stops advancing.
        let mut found = run(&mut tracker, start, secs(0), secs(5), &mut r, |_, _| {});
        assert!(found.is_empty());
        let mut t = secs(5);
        while t < secs(60) {
            t += POLL_INTERVAL;
            if let Some(f) = tracker.update(start + t, &r) {
                found.push((t, f));
            }
        }
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].1.kind, FindingKind::CpuBlocked);
        assert!(found[0].1.kind.offers_dialog());
        assert!(found[0].0 >= secs(5) + NO_PROGRESS_REPORT_AFTER);
    }

    #[test]
    fn suspension_is_not_a_stall() {
        let start = Instant::now();
        let mut r = reading();
        let mut tracker = Tracker::new(start, &r);
        r.suspended = true;
        let mut t = secs(0);
        while t < secs(120) {
            t += POLL_INTERVAL;
            assert!(tracker.update(start + t, &r).is_none());
        }
        // Resuming restarts the clocks instead of reporting the absence.
        r.suspended = false;
        assert!(tracker.update(start + t + POLL_INTERVAL, &r).is_none());
    }

    #[test]
    fn a_new_session_does_not_inherit_the_previous_sessions_gap() {
        let start = Instant::now();
        let mut r = reading();
        let mut tracker = Tracker::new(start, &r);
        // 29 s with no swaps: just under the threshold.
        let found = run(&mut tracker, start, secs(0), secs(29), &mut r, |_, _| {});
        assert!(found.is_empty());
        // A new session begins. Another 29 s must still be quiet, even though
        // 58 s have passed since the first session's last swap.
        r.session += 1;
        let found = run(&mut tracker, start, secs(29), secs(29), &mut r, |_, _| {});
        assert!(found.is_empty(), "{found:?}");
        // And the new session's own long gap is still detected.
        let found = run(&mut tracker, start, secs(58), secs(10), &mut r, |_, _| {});
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1.kind, FindingKind::NotPresenting);
    }

    #[test]
    fn a_new_session_restarts_the_report_budget() {
        let start = Instant::now();
        let mut r = reading();
        let mut tracker = Tracker::new(start, &r);
        let first = run(&mut tracker, start, secs(0), secs(40), &mut r, |_, _| {});
        assert_eq!(first.len(), 1);
        r.session += 1;
        let second = run(&mut tracker, start, secs(40), secs(40), &mut r, |_, _| {});
        assert_eq!(second.len(), 1, "{second:?}");
    }
}
