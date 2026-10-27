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
//! This module runs a small host thread that watches two counters the
//! emulation loop already maintains:
//!
//! * guest CPU progress ([crate::environment::GUEST_PC_RING_IDX], bumped on
//!   every `run_or_step`); and
//! * presented frames ([note_frame_presented]).
//!
//! If the CPU stops advancing, the emulator is blocked. If the CPU keeps
//! running but no frame comes out, the app is alive and the problem is
//! presentation. Those are different bugs with different fixes, so they get
//! different reports. Either way the report names the last guest PC/LR and the
//! last 32 executed guest PCs, and (on Android) offers to share the log,
//! because a hang leaves its evidence in a file the user cannot otherwise
//! reach without ADB.
//!
//! The two counters alone are not enough to name a culprit, because they are
//! process-wide: a single guest thread spinning (or blocked inside a host
//! function that never returns) keeps the CPU counter moving while every other
//! thread, including the one that presents frames and pumps the run loop, is
//! stopped. That case used to be reported as a *presentation* problem, which
//! sends the reader looking at the GPU while the app is stuck in a startup
//! call. So each guest thread also publishes the host function it last entered
//! and whether it is still inside that call ([note_host_call_enter]); the
//! reports then say *which* thread stopped, in *which* host function, and
//! which threads are still running (see [threads_inside_host_call]).
//!
//! The watchdog never touches the [`crate::Environment`]: the guest thread may
//! be blocked inside it, so everything it reports has to come from atomics.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Frames handed to the window since start-up.
static FRAMES_PRESENTED: AtomicU64 = AtomicU64::new(0);
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
///
/// This is also the unit of [`TICK`], the clock the per-thread host-call
/// bookkeeping is stamped with: guest threads cannot afford an
/// `Instant::now()` on every dispatch of `objc_msgSend`, so the watchdog hands
/// them its own coarse clock instead (see [note_host_call_enter]).
const POLL_INTERVAL: Duration = Duration::from_millis(500);
/// [`POLL_INTERVAL`] in milliseconds, for converting tick counts back into
/// durations.
const POLL_INTERVAL_MS: u64 = 500;
/// Guest threads whose host-call state is tracked. Thread ids are indices into
/// the environment's thread table and grow by one per thread creation; 64 is
/// well beyond what an app of this era creates, and a larger id is simply not
/// tracked (the reports then fall back to the process-wide PCs).
const MAX_TRACKED_THREADS: usize = 64;
/// No guest CPU progress for this long means the emulator is blocked. Generous
/// on purpose: loading a big asset can block the guest thread for seconds on
/// slow storage, and a false report would be worse than a late one.
const NO_PROGRESS_REPORT_AFTER: Duration = Duration::from_secs(20);
/// The guest keeps running but nothing reaches the screen for this long.
const NO_FRAMES_REPORT_AFTER: Duration = Duration::from_secs(30);
/// How long the watchdog waits for the emulation loop to service a request to
/// dump every guest thread's stack before it writes its report without one.
const DUMP_WAIT_TIMEOUT: Duration = Duration::from_secs(2);
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

/// Record that one frame reached the window.
///
/// Called from [crate::window::Window::swap_window], which is the single choke
/// point every presenter (GPU copy, RAM readback, IOSurface) goes through.
pub fn note_frame_presented() {
    FRAMES_PRESENTED.fetch_add(1, Ordering::Relaxed);
}

/// The watchdog's own coarse clock, in units of [`POLL_INTERVAL`].
///
/// It is bumped by the watcher thread only, and it stops while the app is
/// suspended by the host, so every duration derived from it automatically
/// excludes time spent in the background. Guest threads only ever read it.
static TICK: AtomicU64 = AtomicU64::new(1);

/// Set by the watchdog when it wants every guest thread's stack dumped into
/// the log. Cleared by the emulation loop, which is the only place that can do
/// it: reading guest memory and thread contexts needs the
/// [`crate::Environment`], and the watchdog must never touch that.
static DUMP_REQUESTED: AtomicBool = AtomicBool::new(false);
/// Set by the emulation loop once the dump asked for above has been written.
static DUMP_DONE: AtomicBool = AtomicBool::new(false);

/// Host-call state of one guest thread.
///
/// Written by the owning guest thread on every call into a host function and
/// read by the watcher thread; every field is therefore a plain atomic and the
/// two halves of the symbol are not read atomically together (same caveat as
/// the crash-report snapshot of the last host call in [`crate::environment`]).
struct ThreadHostCall {
    /// Address of the stub through which the thread entered the host function.
    stub_addr: AtomicU32,
    /// The host function's name, stored as the (pointer, length) of the
    /// `&'static str` that dyld publishes for the current dispatch.
    symbol_ptr: AtomicUsize,
    symbol_len: AtomicUsize,
    /// Nesting depth of host calls on this thread. A host function can call
    /// back into guest code, which can in turn block in another host call, and
    /// only the outermost return means the thread runs guest code again.
    depth: AtomicU32,
    /// [`TICK`] at which the outermost host call was entered, or 0 while the
    /// thread is not inside a host call.
    entered_tick: AtomicU64,
    /// [`TICK`] at which the thread last returned to guest code, or 0 if it has
    /// not returned yet.
    last_return_tick: AtomicU64,
}

impl ThreadHostCall {
    const fn new() -> Self {
        Self {
            stub_addr: AtomicU32::new(0),
            symbol_ptr: AtomicUsize::new(0),
            symbol_len: AtomicUsize::new(0),
            depth: AtomicU32::new(0),
            entered_tick: AtomicU64::new(0),
            last_return_tick: AtomicU64::new(0),
        }
    }
}

#[allow(clippy::declare_interior_mutable_const)]
const NEW_THREAD_HOST_CALL: ThreadHostCall = ThreadHostCall::new();

/// Per-thread host-call state, indexed by guest thread id.
static HOST_CALLS: [ThreadHostCall; MAX_TRACKED_THREADS] =
    [NEW_THREAD_HOST_CALL; MAX_TRACKED_THREADS];

/// Note that `thread` is entering (or re-entering, if nested) the host
/// function reached through the stub at `stub_addr`.
///
/// `stub_addr` and the function's name are overwritten by every nested call, so
/// the name that survives is the innermost one: that is the function the thread
/// is actually blocked in. `entered_tick` deliberately keeps the time of the
/// outermost call, so the reported duration is how long the thread has been
/// unable to run guest code at all.
pub fn note_host_call_enter(thread: usize, stub_addr: u32) {
    let Some(state) = HOST_CALLS.get(thread) else {
        return;
    };
    state.stub_addr.store(stub_addr, Ordering::Relaxed);
    state.symbol_ptr.store(
        crate::environment::LAST_HOST_CALL_SYMBOL_PTR.load(Ordering::Relaxed),
        Ordering::Relaxed,
    );
    state.symbol_len.store(
        crate::environment::LAST_HOST_CALL_SYMBOL_LEN.load(Ordering::Relaxed),
        Ordering::Relaxed,
    );
    if state.depth.fetch_add(1, Ordering::Relaxed) == 0 {
        state
            .entered_tick
            .store(TICK.load(Ordering::Relaxed), Ordering::Relaxed);
    }
}

/// Note that `thread`'s outermost host call returned, i.e. the thread is
/// running guest code again.
pub fn note_host_call_exit(thread: usize) {
    let Some(state) = HOST_CALLS.get(thread) else {
        return;
    };
    // A host function that unwinds instead of returning can leave the depth at
    // zero here; the saturating check keeps the counters sane in that case.
    if state
        .depth
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |depth| {
            depth.checked_sub(1)
        })
        .is_err()
    {
        return;
    }
    if state.depth.load(Ordering::Relaxed) == 0 {
        state.entered_tick.store(0, Ordering::Relaxed);
        // TICK 0 means "never returned", so the first return is booked as 1.
        let tick = TICK.load(Ordering::Relaxed).max(1);
        state.last_return_tick.store(tick, Ordering::Relaxed);
    }
}

/// Marks one guest-to-host dispatch as in-flight for as long as it is alive, so
/// a host function that never returns is visible to the watchdog.
///
/// A guard rather than an explicit pair of calls because the dispatcher has
/// several early returns (guest termination, redirected control flow) and a
/// panic must not leave the thread looking permanently blocked either.
#[must_use = "the guard must stay alive for as long as the host call runs"]
pub struct HostCallGuard {
    thread: usize,
}

impl HostCallGuard {
    pub fn new(thread: usize, stub_addr: u32) -> Self {
        note_host_call_enter(thread, stub_addr);
        Self { thread }
    }
}

impl Drop for HostCallGuard {
    fn drop(&mut self) {
        note_host_call_exit(self.thread);
    }
}

/// One thread that is currently inside a host call.
struct ThreadInHostCall {
    thread: usize,
    /// Innermost host function name, or `<unknown>` if dyld's symbol snapshot
    /// was torn.
    symbol: String,
    stub_addr: u32,
    /// How long the thread has been unable to run guest code.
    inside: Duration,
}

/// The name recorded for one thread's innermost host call.
///
/// The length is clamped and the bytes validated because the pointer and length
/// are two separate relaxed stores, so a dispatch racing with this read can
/// leave them mismatched (see
/// [`crate::environment::LAST_HOST_CALL_SYMBOL_PTR`]).
fn thread_symbol(state: &ThreadHostCall) -> String {
    const MAX_SYMBOL_LEN: usize = 512;
    let ptr = state.symbol_ptr.load(Ordering::Relaxed);
    let len = state.symbol_len.load(Ordering::Relaxed);
    if ptr == 0 || len == 0 || len > MAX_SYMBOL_LEN {
        return "<unknown>".to_string();
    }
    // SAFETY: dyld stores the (pointer, length) of a `&'static str` that lives
    // for the whole process, so the bytes stay mapped; the length was clamped
    // above and the result is validated as UTF-8.
    let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, len) };
    match std::str::from_utf8(bytes) {
        Ok(symbol) => symbol.to_string(),
        Err(_) => "<unparsable>".to_string(),
    }
}

/// How many [`TICK`]s cover `duration`.
fn ticks_for(duration: Duration) -> u64 {
    (duration.as_millis() as u64).div_ceil(POLL_INTERVAL_MS)
}

/// A sentence naming the thread that has been stuck inside one host call the
/// longest, or an empty string if no thread is stuck.
fn stuck_host_call_sentence(tick: u64) -> String {
    match threads_inside_host_call(tick, ticks_for(NO_PROGRESS_REPORT_AFTER)).first() {
        Some(worst) => format!(
            " Guest thread {} has been inside the host function `{}` (entered \
             through the stub at {:#010x}) for {:.1} s.",
            worst.thread,
            worst.symbol,
            worst.stub_addr,
            worst.inside.as_secs_f64(),
        ),
        None => String::new(),
    }
}

/// Write the per-thread activity line, if anything has been tracked yet.
fn log_thread_activity(tick: u64) {
    let activity = thread_activity(tick);
    if !activity.is_empty() {
        log!("  {}", activity);
    }
}

/// Every thread that has been inside its current host call for at least
/// `min_ticks` poll intervals, the longest-blocked thread first.
///
/// This is the answer to "the CPU is running, so why is nothing on screen?": if
/// the thread that presents frames is in this list, the app is blocked in a
/// host function and the missing frames are a symptom, not the bug.
fn threads_inside_host_call(now: u64, min_ticks: u64) -> Vec<ThreadInHostCall> {
    let mut stuck: Vec<ThreadInHostCall> = HOST_CALLS
        .iter()
        .enumerate()
        .filter_map(|(thread, state)| {
            let entered = state.entered_tick.load(Ordering::Relaxed);
            if entered == 0 {
                return None;
            }
            let ticks = now.saturating_sub(entered);
            if ticks < min_ticks {
                return None;
            }
            let stub_addr = state.stub_addr.load(Ordering::Relaxed);
            let mut symbol = thread_symbol(state);
            if symbol == "<unknown>" {
                // dyld publishes a name for most dispatches, but a stub that
                // is still lazy can be entered before it has one; the stub map
                // built during linking covers those.
                if let Some(name) = imported_function_at(stub_addr) {
                    symbol = name;
                }
            }
            Some(ThreadInHostCall {
                thread,
                symbol,
                stub_addr,
                inside: Duration::from_millis(ticks.saturating_mul(POLL_INTERVAL_MS)),
            })
        })
        .collect();
    stuck.sort_by_key(|t| std::cmp::Reverse(t.inside));
    stuck
}

/// One line saying what every tracked guest thread is doing, e.g.
/// `No host call from thread 0 (31.0 s); still running: thread 8 (0.0 s).`.
///
/// Worth printing even when nothing looks stuck: it shows that the process-wide
/// CPU counter is being kept alive by *other* threads, which is exactly the
/// case that used to be misreported as a presentation problem.
fn thread_activity(now: u64) -> String {
    let mut stopped: Vec<String> = Vec::new();
    let mut running: Vec<String> = Vec::new();
    for (thread, state) in HOST_CALLS.iter().enumerate() {
        let entered = state.entered_tick.load(Ordering::Relaxed);
        let returned = state.last_return_tick.load(Ordering::Relaxed);
        if entered != 0 {
            // Being inside a call is reported in detail by the caller; here we
            // only need to say that the thread is not running guest code.
            continue;
        }
        if returned == 0 {
            continue;
        }
        let since = Duration::from_millis(
            now.saturating_sub(returned)
                .saturating_mul(POLL_INTERVAL_MS),
        );
        if since >= NO_FRAMES_REPORT_AFTER {
            stopped.push(format!("thread {thread} ({:.1} s)", since.as_secs_f64()));
        } else {
            running.push(format!("thread {thread} ({:.1} s)", since.as_secs_f64()));
        }
    }
    let mut text = String::new();
    if !stopped.is_empty() {
        text.push_str(&format!(
            "No host call from {}; ",
            stopped.join(", ")
        ));
    }
    if !running.is_empty() {
        text.push_str(&format!(
            "still running: {}.",
            running.join(", ")
        ));
    }
    if text.ends_with("; ") {
        text.truncate(text.len() - 2);
    }
    text
}

/// Tell the watchdog that the emulator is (or is no longer) paused by the host.
///
/// On Android, switching away from the app blocks the emulator thread until
/// the user comes back. That is a *deliberate* absence of progress and must not
/// be reported as a hang.
pub fn set_suspended(suspended: bool) {
    SUSPENDED.store(suspended, Ordering::Relaxed);
}

fn frames_presented() -> u64 {
    FRAMES_PRESENTED.load(Ordering::Relaxed)
}

/// True once, in the emulation loop, when the watchdog has asked for a dump of
/// every guest thread's stack. The caller must dump and then call
/// [note_dump_done].
pub fn take_dump_request() -> bool {
    DUMP_REQUESTED.swap(false, Ordering::Relaxed)
}

/// Tell the watchdog that the dump it asked for has been written to the log.
pub fn note_dump_done() {
    DUMP_DONE.store(true, Ordering::Relaxed);
}

/// Ask the guest side for a dump of every thread's stack and wait briefly for
/// it, returning whether it happened.
///
/// The watchdog itself cannot read guest memory or thread contexts, so the
/// guest side does it, from the scheduler's return-to-host path, where a live
/// [`crate::Environment`] is available. The answer is diagnostic too: if no
/// guest thread services the request, every guest thread is blocked inside a
/// host call, which is worth saying in the report.
fn dump_thread_stacks() -> bool {
    DUMP_DONE.store(false, Ordering::Relaxed);
    DUMP_REQUESTED.store(true, Ordering::Relaxed);
    let started = Instant::now();
    while started.elapsed() < DUMP_WAIT_TIMEOUT {
        if DUMP_DONE.load(Ordering::Relaxed) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    DUMP_DONE.load(Ordering::Relaxed)
}

/// The line that says whether the guest stacks made it into the log.
fn log_dump_status(dumped: bool) {
    log!(
        "  {}",
        if dumped {
            "guest thread stacks were dumped above (STALL-DIAG)"
        } else {
            "no guest thread could service a stack dump: every guest thread is \
             blocked inside a host call"
        }
    );
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

fn watch() {
    let mut last_steps = guest_cpu_steps();
    let mut last_frames = frames_presented();
    let mut steps_changed_at = Instant::now();
    let mut frames_changed_at = Instant::now();
    // Log reports are rate-limited: a false positive (a genuinely long load)
    // must not fill the log, but a real hang should still be re-reported a few
    // times in case the first report scrolled out of the user's paste.
    let mut stall_reports_left = MAX_STALL_REPORTS;
    let mut next_stall_report_at: Option<Instant> = None;
    let mut frames_reported = false;

    loop {
        std::thread::sleep(POLL_INTERVAL);

        let steps = guest_cpu_steps();
        let frames = frames_presented();
        let now = Instant::now();

        if SUSPENDED.load(Ordering::Relaxed) {
            // Paused by the host: restart the clocks so that time spent in the
            // background is not counted as a stall.
            last_steps = steps;
            last_frames = frames;
            steps_changed_at = now;
            frames_changed_at = now;
            next_stall_report_at = None;
            continue;
        }

        // Advance the clock the per-thread host-call bookkeeping is stamped
        // with. It is deliberately not advanced while suspended above, so
        // every duration derived from it excludes background time.
        TICK.fetch_add(1, Ordering::Relaxed);
        let tick = TICK.load(Ordering::Relaxed);

        if steps != last_steps {
            last_steps = steps;
            steps_changed_at = now;
            next_stall_report_at = None;
        }
        if frames != last_frames {
            last_frames = frames;
            frames_changed_at = now;
        }

        // Nothing has run yet (still booting, or sitting in the app picker):
        // there is nothing to conclude.
        if steps == 0 {
            continue;
        }

        let idle = steps_changed_at.elapsed();
        if idle >= NO_PROGRESS_REPORT_AFTER {
            // The guest thread is blocked. Report at 20 s, then every
            // STALL_REPORT_INTERVAL after that, up to MAX_STALL_REPORTS.
            let due = next_stall_report_at.is_none_or(|at| now >= at);
            if stall_reports_left > 0 && due {
                stall_reports_left -= 1;
                next_stall_report_at = Some(now + STALL_REPORT_INTERVAL);
                report_no_progress(idle, frames_changed_at.elapsed(), tick);
            }
        } else if !frames_reported
            // Only a *running* emulator can have a presentation problem; if the
            // CPU is blocked the stall report above already covers it.
            && idle < POLL_INTERVAL * 4
            && frames_changed_at.elapsed() >= NO_FRAMES_REPORT_AFTER
        {
            frames_reported = true;
            report_no_frames(frames_changed_at.elapsed(), tick);
        }
    }
}

/// The guest thread is blocked: no instruction has retired for `idle`.
fn report_no_progress(idle: Duration, since_frame: Duration, tick: u64) {
    // Get the guest stacks into the log first: the report's own lines are only
    // useful next to them.
    let dumped = dump_thread_stacks();
    let pc = crate::environment::LAST_GUEST_PC.load(Ordering::Relaxed);
    let lr = crate::environment::LAST_GUEST_LR.load(Ordering::Relaxed);
    let text = format!(
        "The emulated CPU has not advanced for {:.1} s (last frame {:.1} s ago), \
         so the app is blocked, not slow. This is what a \"black screen\" hang \
         looks like from the inside. Last guest PC: {pc:#010x}, LR: {lr:#010x}. \
         The app is most likely waiting inside a host function (a socket, a \
         lock, a wait) that will never return.{}",
        idle.as_secs_f64(),
        since_frame.as_secs_f64(),
        stuck_host_call_sentence(tick),
    );
    log!("{}", text);
    if let Some(name) = last_imported_function() {
        log!("  the last imported function the app called was {name}");
    }
    log_thread_activity(tick);
    log!("  recent guest PCs: {}", recent_guest_pcs(REPORT_PC_COUNT));
    log_dump_status(dumped);
    log!(
        "  this run's log: {} (the previous run is kept as {})",
        crate::log::log_file_path().display(),
        crate::log::previous_log_file_path().display(),
    );
    offer_log_sharing(&text);
}

/// The guest is running but nothing reaches the screen.
///
/// Usually a rendering problem, but not always: if one guest thread is stuck
/// inside a host function while the others keep running (a loader thread
/// finishing its work, an SDK's worker thread polling), the process-wide CPU
/// counter keeps moving while the thread that owns the frame never comes back.
/// That is a startup hang, not a broken present path, and the report has to say
/// so — otherwise it sends the reader to the wrong subsystem.
fn report_no_frames(since_frame: Duration, tick: u64) {
    let dumped = dump_thread_stacks();
    let stuck = threads_inside_host_call(tick, ticks_for(NO_PROGRESS_REPORT_AFTER));
    let text = if let Some(worst) = stuck.first() {
        format!(
            "The emulated CPU is running, but no frame has been presented for \
             {:.1} s, and guest thread {} has been inside the host function \
             `{}` (entered through the stub at {:#010x}) for {:.1} s without \
             returning, so that thread has not run any guest code since then. If \
             `{}` is a run-loop wait for events, the app is idle rather than \
             stuck; otherwise that call is the hang, and the missing frames are \
             its symptom, not a presentation problem.",
            since_frame.as_secs_f64(),
            worst.thread,
            worst.symbol,
            worst.stub_addr,
            worst.inside.as_secs_f64(),
            worst.symbol,
        )
    } else {
        format!(
            "The emulated CPU is running, but no frame has been presented for \
             {:.1} s. No guest thread is blocked inside a host function, so the \
             app is alive and this is a presentation problem (the drawable being \
             rendered is not the one on screen, or the GPU present path produces \
             nothing on this driver), not a stuck app.",
            since_frame.as_secs_f64(),
        )
    };
    log!("{}", text);
    if let Some(name) = last_imported_function() {
        log!("  the last imported function the app called was {name}");
    }
    log_thread_activity(tick);
    log!("  recent guest PCs: {}", recent_guest_pcs(REPORT_PC_COUNT));
    log_dump_status(dumped);
    // Still offer to share: from the user's point of view "black screen but the
    // app is running" is the same problem, and the dialog is the only way the
    // log leaves the device. Only one dialog is ever shown (see
    // offer_log_sharing), so this cannot fight with the stall dialog.
    offer_log_sharing(&text);
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

    // The per-thread tables are process-wide like the counters above, so the
    // tests below each claim a thread id no other test uses, and never assume
    // their own entry is the only one in the table.

    /// A slot that only this test uses.
    fn test_thread_slot(offset: usize) -> usize {
        MAX_TRACKED_THREADS - 1 - offset
    }

    #[test]
    fn a_host_call_is_visible_while_it_runs_and_cleared_afterwards() {
        {
            let _guard = HostCallGuard::new(test_thread_slot(0), 0x1234);
            let state = &HOST_CALLS[test_thread_slot(0)];
            assert_ne!(
                state.entered_tick.load(Ordering::Relaxed),
                0,
                "a call that has not returned must read as in flight"
            );
            assert_eq!(state.stub_addr.load(Ordering::Relaxed), 0x1234);
        }
        let state = &HOST_CALLS[test_thread_slot(0)];
        assert_eq!(
            state.entered_tick.load(Ordering::Relaxed),
            0,
            "returning from the call must clear it"
        );
        assert_ne!(
            state.last_return_tick.load(Ordering::Relaxed),
            0,
            "the return must be timestamped"
        );
    }

    #[test]
    fn a_nested_call_keeps_the_entry_time_of_the_outermost_one() {
        let thread = test_thread_slot(1);
        let outer = HostCallGuard::new(thread, 0x1000);
        let entered = HOST_CALLS[thread].entered_tick.load(Ordering::Relaxed);
        let inner = HostCallGuard::new(thread, 0x2000);
        // The innermost stub is the one the thread is actually blocked in...
        assert_eq!(HOST_CALLS[thread].stub_addr.load(Ordering::Relaxed), 0x2000);
        // ...but the clock still measures how long the thread has been unable
        // to run guest code, which is the outermost call.
        assert_eq!(
            HOST_CALLS[thread].entered_tick.load(Ordering::Relaxed),
            entered
        );
        drop(inner);
        assert_ne!(
            HOST_CALLS[thread].entered_tick.load(Ordering::Relaxed),
            0,
            "returning from an inner call must not declare the thread unblocked"
        );
        drop(outer);
        assert_eq!(HOST_CALLS[thread].entered_tick.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_long_host_call_is_reported_with_its_function_name() {
        let thread = test_thread_slot(2);
        let _guard = HostCallGuard::new(thread, 0x0bad_cafe);
        let entered = HOST_CALLS[thread].entered_tick.load(Ordering::Relaxed);
        assert_ne!(entered, 0);

        // Not stuck yet: 9 ticks is below the 10-tick threshold.
        assert!(!threads_inside_host_call(entered + 9, 10)
            .iter()
            .any(|call| call.thread == thread));

        let stuck = threads_inside_host_call(entered + 10, 10);
        let mine = stuck
            .iter()
            .find(|call| call.thread == thread)
            .expect("a thread inside a host call must be reported once it is old enough");
        assert_eq!(mine.stub_addr, 0x0bad_cafe);
        assert_eq!(mine.inside, Duration::from_millis(500 * 10));
        // Tests never publish a symbol, so this is the documented fallback.
        assert_eq!(mine.symbol, "<unknown>");
    }

    #[test]
    fn a_dump_request_is_handed_over_exactly_once() {
        DUMP_DONE.store(false, Ordering::Relaxed);
        DUMP_REQUESTED.store(true, Ordering::Relaxed);
        assert!(take_dump_request());
        assert!(
            !take_dump_request(),
            "a request must not be serviced twice"
        );
        assert!(!DUMP_DONE.load(Ordering::Relaxed));
        note_dump_done();
        assert!(DUMP_DONE.load(Ordering::Relaxed));
        // Leave the statics as they were found, for the other tests.
        DUMP_DONE.store(false, Ordering::Relaxed);
    }

    #[test]
    fn durations_convert_to_the_poll_clock() {
        assert_eq!(ticks_for(Duration::from_millis(0)), 0);
        assert_eq!(ticks_for(Duration::from_millis(1)), 1);
        assert_eq!(ticks_for(Duration::from_millis(500)), 1);
        assert_eq!(ticks_for(Duration::from_secs(30)), 60);
    }

    #[test]
    fn a_thread_that_has_not_called_the_host_for_a_while_is_reported_as_stopped() {
        // A thread that returned from a call long (in ticks) ago must show up
        // in the "stopped" half of the activity line, and a recently active one
        // in the "running" half.
        let quiet = test_thread_slot(3);
        let busy = test_thread_slot(4);
        for thread in [quiet, busy] {
            assert_eq!(HOST_CALLS[thread].depth.load(Ordering::Relaxed), 0);
        }
        // Establish a return for each thread, then pretend time has passed for
        // one of them only.
        drop(HostCallGuard::new(quiet, 1));
        drop(HostCallGuard::new(busy, 2));
        let now = TICK.load(Ordering::Relaxed) + 1;
        HOST_CALLS[quiet]
            .last_return_tick
            .store(now.saturating_sub(ticks_for(NO_FRAMES_REPORT_AFTER) + 1), Ordering::Relaxed);
        HOST_CALLS[busy]
            .last_return_tick
            .store(now, Ordering::Relaxed);

        // Other tests' threads can be listed too, so look for this test's
        // threads in the half they belong to rather than at a fixed position.
        let activity = thread_activity(now);
        let (stopped_part, running_part) = match activity.split_once("still running: ") {
            Some((stopped, running)) => (stopped, running),
            None => (activity.as_str(), ""),
        };
        assert!(
            stopped_part.contains(&format!("thread {quiet}")),
            "quiet thread missing from: {activity}"
        );
        assert!(
            running_part.contains(&format!("thread {busy}")),
            "running thread missing from: {activity}"
        );
    }
}
