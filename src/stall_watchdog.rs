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
//! The watchdog never touches the [`crate::Environment`]: the guest thread may
//! be blocked inside it, so everything it reports has to come from atomics.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
const POLL_INTERVAL: Duration = Duration::from_millis(500);
/// No guest CPU progress for this long means the emulator is blocked. Generous
/// on purpose: loading a big asset can block the guest thread for seconds on
/// slow storage, and a false report would be worse than a late one.
const NO_PROGRESS_REPORT_AFTER: Duration = Duration::from_secs(20);
/// The guest keeps running but nothing reaches the screen for this long.
const NO_FRAMES_REPORT_AFTER: Duration = Duration::from_secs(30);
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
                report_no_progress(idle, frames_changed_at.elapsed());
            }
        } else if !frames_reported
            // Only a *running* emulator can have a presentation problem; if the
            // CPU is blocked the stall report above already covers it.
            && idle < POLL_INTERVAL * 4
            && frames_changed_at.elapsed() >= NO_FRAMES_REPORT_AFTER
        {
            frames_reported = true;
            report_no_frames(frames_changed_at.elapsed());
        }
    }
}

/// The guest thread is blocked: no instruction has retired for `idle`.
fn report_no_progress(idle: Duration, since_frame: Duration) {
    let pc = crate::environment::LAST_GUEST_PC.load(Ordering::Relaxed);
    let lr = crate::environment::LAST_GUEST_LR.load(Ordering::Relaxed);
    let text = format!(
        "The emulated CPU has not advanced for {:.1} s (last frame {:.1} s ago), \
         so the app is blocked, not slow. This is what a \"black screen\" hang \
         looks like from the inside. Last guest PC: {pc:#010x}, LR: {lr:#010x}. \
         The app is most likely waiting inside a host function (a socket, a \
         lock, a wait) that will never return.",
        idle.as_secs_f64(),
        since_frame.as_secs_f64(),
    );
    log!("{}", text);
    if let Some(name) = last_imported_function() {
        log!("  the last imported function the app called was {name}");
    }
    log!("  recent guest PCs: {}", recent_guest_pcs(REPORT_PC_COUNT));
    log!(
        "  this run's log: {} (the previous run is kept as {})",
        crate::log::log_file_path().display(),
        crate::log::previous_log_file_path().display(),
    );
    offer_log_sharing(&text);
}

/// The guest is running but nothing reaches the screen. Rendering problem,
/// not a hang.
fn report_no_frames(since_frame: Duration) {
    let text = format!(
        "The emulated CPU is running, but no frame has been presented for \
         {:.1} s. The app itself is alive, so this is a presentation problem \
         (the drawable being rendered is not the one on screen, or the GPU \
         present path produces nothing on this driver), not a stuck app.",
        since_frame.as_secs_f64(),
    );
    log!("{}", text);
    if let Some(name) = last_imported_function() {
        log!("  the last imported function the app called was {name}");
    }
    log!("  recent guest PCs: {}", recent_guest_pcs(REPORT_PC_COUNT));
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
}
