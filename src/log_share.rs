/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! Getting the emulator log off the device.
//!
//! On Android the log lives in the app's external files directory
//! (`Android/data/<package>/files/touchHLE_log.txt`), which modern Android
//! deliberately hides from file managers. The result is that the one artifact
//! needed to diagnose a hang ("black screen after the logo") is also the one
//! artifact a user cannot hand over: ADB is not available to most people, and
//! the app picker's "open the touchHLE directory" action needs a file manager
//! that is allowed to browse `Android/data` at all.
//!
//! So the log is shared instead of located: the file is first flushed (the
//! interesting lines are the *last* ones, and they may still be buffered),
//! then handed to the system share sheet as a `content://` URI served by the
//! app's own [DocumentsProvider], so it can be sent by mail, chat or to cloud
//! storage without any storage permission.
//!
//! The same URI is used by the stall watchdog's dialog (see
//! [crate::stall_watchdog]), which is what makes a hang reportable *while it is
//! happening* — the user does not have to restart the emulator (and lose the
//! log) to get a chance to send it.

/// Subject line / title used when the log is shared.
///
/// Android only: that is the only platform with a share sheet. Kept as a
/// constant so the watchdog dialog and the share action cannot disagree.
#[cfg(target_os = "android")]
pub const SHARE_SUBJECT: &str = "UltraHLE Rebirth log";

/// A `content://` URI for the current run's log file, if this platform can
/// produce one.
///
/// Android's documents provider exposes the user data directory as the
/// document id `root/<relative path>`, which is what
/// [crate::paths::android_document_uri] expects.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub fn log_document_uri() -> Option<String> {
    #[cfg(target_os = "android")]
    {
        Some(crate::paths::android_document_uri(&format!(
            "root/{}",
            crate::paths::LOG_FILE
        )))
    }
    #[cfg(not(target_os = "android"))]
    {
        None
    }
}

/// Flush the log and hand it to the system share sheet.
///
/// Returns `Err` with a user-presentable message when the platform cannot
/// share files (desktop, or a Java bridge that is not available).
pub fn share_log() -> Result<(), String> {
    // The report that explains the hang is written immediately before
    // sharing, so it may still be in the process's buffer.
    crate::log::flush_log();

    #[cfg(target_os = "android")]
    {
        let uri = log_document_uri().expect("Android always has a document URI");
        if crate::android_web_view::share_log_file(SHARE_SUBJECT, &uri) {
            Ok(())
        } else {
            Err("the Android share bridge is not available in this build".to_string())
        }
    }
    #[cfg(not(target_os = "android"))]
    {
        Err(format!(
            "sharing the log is only implemented on Android; the log is at {}",
            crate::log::log_file_path().display()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flush_does_not_panic_before_anything_was_logged() {
        // Sharing is invoked from a watchdog thread that may run before the
        // first log line; flushing must be safe in that state.
        crate::log::flush_log();
    }

    #[test]
    fn the_share_uri_names_the_log_file() {
        #[cfg(target_os = "android")]
        {
            let uri = log_document_uri().expect("Android has a document URI");
            assert!(uri.starts_with("content://"), "unexpected URI: {uri}");
            assert!(uri.contains(crate::paths::LOG_FILE), "unexpected URI: {uri}");
        }
        #[cfg(not(target_os = "android"))]
        {
            assert!(log_document_uri().is_none());
        }
    }

    #[test]
    fn sharing_on_desktop_explains_where_the_log_is() {
        #[cfg(not(target_os = "android"))]
        {
            let err = share_log().expect_err("desktop has no share sheet");
            assert!(err.contains(crate::paths::LOG_FILE), "unhelpful: {err}");
        }
    }
}
