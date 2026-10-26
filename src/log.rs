/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! Logging and terminal output macros.

use std::fs::File;
use std::sync::{LazyLock, Mutex};

/// Path of the log file for the current run.
pub fn log_file_path() -> std::path::PathBuf {
    crate::paths::user_data_base_path().join(crate::paths::LOG_FILE)
}

/// Path of the log file for the previous run (see [crate::paths::PREVIOUS_LOG_FILE]).
pub fn previous_log_file_path() -> std::path::PathBuf {
    crate::paths::user_data_base_path().join(crate::paths::PREVIOUS_LOG_FILE)
}

/// Get a handle to the log file. This is only for use by logging macros!
///
/// All the logging macros print to stderr or (on Android) logcat, but this
/// is not convenient for users who aren't accustomed to command-line tools or
/// who don't have access to ADB, so we also write to a log file.
///
/// The previous run's log is kept as [previous_log_file_path] rather than
/// being deleted, because the interesting failures are often hangs (a game
/// that stops drawing) rather than crashes: the user has to restart the
/// emulator to get back to the app picker, and that restart would otherwise
/// truncate the only evidence of what the game was doing when it stopped.
pub fn get_log_file() -> &'static Mutex<File> {
    static LOG_FILE: LazyLock<Mutex<File>> = LazyLock::new(|| {
        let path = log_file_path();
        let previous = previous_log_file_path();
        if std::fs::metadata(&path).is_ok_and(|meta| meta.len() > 0) {
            // Best-effort: a failure here must not stop the emulator from
            // logging, it only means the old run's log is lost as before.
            let _ = std::fs::rename(&path, &previous);
        }
        let file = File::create(&path).unwrap();
        #[cfg(unix)]
        crate::crash_handler::set_log_fd(std::os::fd::AsRawFd::as_raw_fd(&file));
        Mutex::new(file)
    });

    &LOG_FILE
}

/// Make sure everything written so far has reached the log file.
///
/// Anything that hands the file to another program (sharing it, opening it in
/// a viewer) must call this first, otherwise the last lines — exactly the ones
/// that matter after a hang — may still be sitting in the process's buffer.
pub fn flush_log() {
    if let Ok(mut file) = get_log_file().lock() {
        let _ = std::io::Write::flush(&mut *file);
        // Logcat/stderr are unbuffered, so only the file needs this.
        let _ = file.sync_data();
    }
}

/// Prints a log message unconditionally. Use this for errors or warnings.
///
/// The message is prefixed with the module path, so it is clear where it comes
/// from.
macro_rules! log {
    ($($arg:tt)+) => {
        echo!("{}: {}", module_path!(), format_args!($($arg)+))
    }
}

/// Same as [log], but silently fails on panic instead of
/// panicking.
macro_rules! log_no_panic {
    ($($arg:tt)+) => {
        echo_no_panic!("{}: {}", module_path!(), format_args!($($arg)+));
    }
}

/// Like [log], but prints the message only if debugging is enabled for the
/// module where it is used. This can be used for verbose things only needed
/// when debugging.
macro_rules! log_dbg {
    ($($arg:tt)+) => {
        if $crate::log::ENABLED_MODULES.contains(&module_path!()) {
            log!($($arg)*);
        }
    }
}

/// Like [log], but messages only log once and cannot have formatting.
/// To be used for log messages that are known to spam the log file (like those
/// logged every frame).
macro_rules! log_once {
    ($msg:literal) => {{
        static LOG_ONCE: std::sync::Once = std::sync::Once::new();
        LOG_ONCE.call_once(|| {
            log!("{} [this log will only be shown once]", $msg);
        });
    }};
}

/// Print a message (with implicit newline). This should be used for all
/// touchHLE output that isn't coming from the app itself.
///
/// Prefer use [log] or [log_dbg] for errors and warnings during emulation.
macro_rules! echo {
    ($($arg:tt)+) => {
        {
            let mut formatted_str = format!($($arg)+);

            #[cfg(target_os = "android")]
            {
                sdl2::log::log(&formatted_str);
            }
            #[cfg(not(target_os = "android"))]
            eprintln!("{}", formatted_str);

            if let Ok(mut log_file) = $crate::log::get_log_file().lock() {
                // Append the newline into the same buffer so the line reaches
                // the file in a single write syscall (halves per-line I/O; this
                // matters on slow storage such as Android's app sandbox).
                formatted_str.push('\n');
                let _ = std::io::Write::write_all(&mut *log_file, formatted_str.as_bytes());
            }
        }
    };
    () => {
        {
            #[cfg(target_os = "android")]
            {
                sdl2::log::log("");
            }
            #[cfg(not(target_os = "android"))]
            eprintln!("");

            if let Ok(mut log_file) = $crate::log::get_log_file().lock() {
                let _ = std::io::Write::write_all(&mut *log_file, b"\n");
                let _ = std::io::Write::flush(&mut *log_file);
            }
        }
    }
}

/// Like [echo], but only writes to the in-file log: nothing is sent to
/// logcat/stderr. Intended for opt-in tracing of extremely hot paths (e.g.
/// per-call `--verbose-gles` output), where the logcat round-trip alone is a
/// measurable frame-time cost on Android.
macro_rules! echo_file_only {
    ($($arg:tt)+) => {
        {
            let formatted_str = format!($($arg)+);

            if let Ok(mut log_file) = $crate::log::get_log_file().lock() {
                let _ = std::io::Write::write_all(&mut *log_file, formatted_str.as_bytes());
                let _ = std::io::Write::write_all(&mut *log_file, b"\n");
            }
        }
    };
}

/// Like [log], but only writes to the in-file log (never logcat/stderr). To
/// be used for per-call traces that can emit thousands of lines per second.
macro_rules! log_file_only {
    ($($arg:tt)+) => {
        echo_file_only!("{}: {}", module_path!(), format_args!($($arg)+))
    };
}

/// Same as [echo], but silently fails on panic instead of
/// panicking.
macro_rules! echo_no_panic {
    ($($arg:tt)*) => {
        {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                echo!($($arg)*);
            }));
        }
    }
}

/// Put modules to enable [log_dbg] for here, e.g. "touchHLE::mem" to see when
/// memory is allocated and freed. Keep the default empty to avoid hot-path log
/// traffic in normal builds.
#[cfg(not(target_os = "ios"))]
pub const ENABLED_MODULES: &[&str] = &["touchHLE::frameworks::media_player::movie_player"];
// NOTE: keep this EMPTY for builds that run on an iOS device. Every
// `log_dbg!` does two blocking writes (stderr, which the iOS host redirects to
// a file, and touchHLE_log.txt). On the main thread that starves UIKit of the
// run-loop time it needs to dispatch touches.
#[cfg(target_os = "ios")]
pub const ENABLED_MODULES: &[&str] = &[];
