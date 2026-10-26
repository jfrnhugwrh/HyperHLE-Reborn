/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! Re-entrancy accounting for `objc_msgSend`.
//!
//! Every dispatch — host IMP, guest IMP or `super` call — funnels through
//! `objc_msgSend_inner`, which means that function is the only place where the
//! runtime can observe a dispatch *chain*. This module keeps that chain in a
//! thread-local vector so the runtime can tell two very different situations
//! apart:
//!
//! * **A deep but finite chain.** Legitimate UIKit/Cocos/GameKit code happily
//!   nests dozens of dispatches (a `hitTest:withEvent:` walk, a Core Animation
//!   commit that re-enters a guest delegate, a Cocos `visit` recursion). Apple's
//!   runtime has no depth limit at all here, so the emulator must not impose a
//!   small one — the previous implementation bailed out with `nil` at 128
//!   frames, which silently truncated perfectly valid call chains and, when the
//!   truncated message was `-release`, leaked the receiver.
//!
//! * **An actual cycle.** The same selector being sent to the same receiver
//!   while that very dispatch is still on the stack. That can only happen when
//!   the guest app (or a host IMP) re-enters itself unconditionally; on a real
//!   device it exhausts the stack and crashes. The emulator has to break the
//!   cycle, but it should do so *at the cycle*, not at an arbitrary depth, and
//!   it should say exactly which pair cycled so the root cause is findable.
//!
//! The native Rust stack is still finite, so a hard cap remains as a last
//! resort: with no cycle ever repeating, [`MAX_DEPTH`] nested dispatches is far
//! beyond anything real iOS code does (and touchHLE's dispatcher frames are
//! large enough that going much deeper risks exhausting Android's 1 MB thread
//! stacks). Reaching it is reported as a defect, not as normal operation.
//!
//! Nothing in here performs any dispatch, touches guest memory or holds a lock
//! across a callout, so it is trivially unit-testable.

use std::cell::RefCell;

/// Depth at which the runtime starts *warning* about a deep chain without
/// refusing to dispatch.
///
/// This is the depth the previous implementation used as a hard stop. Real iOS
/// nesting (a deep `hitTest:`/`nextResponder` walk is ~50 frames) stays well
/// below it, so a warning here is genuinely unusual and worth logging once.
pub(super) const WARN_DEPTH: usize = 128;

/// Depth at which dispatch is refused.
///
/// The old cut-off of 128 was chosen to stay inside Android's 1 MB default
/// thread stacks (each host dispatch frame is a few KB). Doubling it keeps that
/// safety margin while giving legitimate deep chains headroom, and true cycles
/// are broken much earlier by the cycle check, so this cap is only reached by a
/// chain that keeps growing with distinct frames.
pub(super) const MAX_DEPTH: usize = 256;

/// Minimum depth before the (linear) cycle scan runs at all.
///
/// Scanning the chain on every message would make dispatch O(depth) even in the
/// overwhelmingly common shallow case. A runaway cycle necessarily grows past
/// any fixed depth, so nothing is missed by only starting to look once the chain
/// is deep enough to be interesting.
const CYCLE_SCAN_DEPTH: usize = 16;

/// How many times the same `(receiver, selector)` pair may be active
/// simultaneously before the chain is considered cyclic.
///
/// Two is deliberately small: re-sending `-release`/`-class`/`-dealloc` to an
/// object that is *already* running that method cannot make progress, and every
/// extra repetition only burns host stack. Nested occurrences of the same
/// selector on *different* receivers (`-[NSArray count]` inside
/// `-[NSArray count]` on a different array) are not affected by this check.
const CYCLE_REPEAT_LIMIT: usize = 2;

/// Maximum number of frames retained for diagnostics when dumping a chain.
pub(super) const CHAIN_DUMP_LEN: usize = 24;

/// One active dispatch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Frame {
    pub receiver: u32,
    pub selector: u32,
}

thread_local! {
    /// Active dispatch chain of the current (emulator) thread. Guest threads
    /// are scheduled on host threads by touchHLE, and a dispatch chain can
    /// never span threads, so a thread-local is the right scope.
    static CHAIN: RefCell<Vec<Frame>> = const { RefCell::new(Vec::new()) };
}

/// Outcome of entering one dispatch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Verdict {
    /// Nothing unusual: dispatch normally.
    Ok,
    /// The chain is deeper than [`WARN_DEPTH`] but not cyclic. Dispatch
    /// normally; the caller may log a throttled warning.
    Deep,
    /// The message re-entered an active `(receiver, selector)` dispatch.
    /// The caller must not dispatch, and should unwind the guest call with a
    /// zero return value instead of recursing further.
    Cycle { occurrences: usize },
    /// The chain reached [`MAX_DEPTH`] without any cycle. The caller must not
    /// dispatch.
    TooDeep,
}

/// RAII frame marker for one dispatch.
///
/// Creating it with [`DispatchFrame::enter`] pushes the frame if (and only if)
/// the dispatch is allowed to proceed; dropping it pops the frame again. Using
/// `Drop` rather than a manual `leave()` call means early returns, `?`
/// operators and panics inside the dispatcher all keep the chain balanced.
pub(super) struct DispatchFrame {
    /// Whether this value owns a frame on the chain. `false` for rejected
    /// dispatches, which must not pop somebody else's frame when dropped.
    pushed: bool,
}

impl DispatchFrame {
    /// Account for one dispatch of `selector` to `receiver`.
    pub(super) fn enter(receiver: u32, selector: u32) -> (Self, Verdict) {
        let rejected = DispatchFrame { pushed: false };

        CHAIN.with(|chain| {
            let mut chain = chain.borrow_mut();

            // Cycle detection first: a cycle is a much more useful diagnosis
            // than "too deep", and it is independent of the depth cap.
            let depth = chain.len();
            if depth >= CYCLE_SCAN_DEPTH {
                let occurrences = chain
                    .iter()
                    .filter(|frame| frame.receiver == receiver && frame.selector == selector)
                    .count();
                if occurrences >= CYCLE_REPEAT_LIMIT {
                    return (rejected, Verdict::Cycle { occurrences });
                }
            }

            if depth >= MAX_DEPTH {
                return (rejected, Verdict::TooDeep);
            }

            chain.push(Frame { receiver, selector });
            let verdict = if depth + 1 >= WARN_DEPTH {
                Verdict::Deep
            } else {
                Verdict::Ok
            };
            (DispatchFrame { pushed: true }, verdict)
        })
    }

    /// Whether this frame is actually on the chain.
    #[cfg(test)]
    pub(super) fn is_active(&self) -> bool {
        self.pushed
    }
}

impl Drop for DispatchFrame {
    fn drop(&mut self) {
        if !self.pushed {
            return;
        }
        CHAIN.with(|chain| {
            let mut chain = chain.borrow_mut();
            // Defensive: a mismatched pop would desynchronise the chain for
            // the rest of the thread's life, so verify the top of the stack
            // instead of blindly popping. (`debug_assert` is compiled out in
            // release builds, where losing one frame is preferable to
            // panicking inside `Drop`.)
            debug_assert!(!chain.is_empty(), "dispatch chain underflow");
            if !chain.is_empty() {
                chain.pop();
            }
        });
    }
}

/// Snapshot of the innermost `limit` frames, oldest first.
///
/// Used for diagnostics only; the runtime formats these into class/selector
/// names when it logs a cycle or the depth cap.
pub(super) fn snapshot(limit: usize) -> Vec<Frame> {
    CHAIN.with(|chain| {
        let chain = chain.borrow();
        let start = chain.len().saturating_sub(limit);
        chain[start..].to_vec()
    })
}

/// Current depth of this thread's dispatch chain.
pub(super) fn depth() -> usize {
    CHAIN.with(|chain| chain.borrow().len())
}

/// Clear the chain. Only for tests (and for recovering from a broken chain
/// after a caught panic, see `crate::objc::messages`).
#[cfg(test)]
pub(super) fn reset() {
    CHAIN.with(|chain| chain.borrow_mut().clear());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_are_popped_when_dropped() {
        reset();
        {
            let (frame, verdict) = DispatchFrame::enter(0x1000, 0x2000);
            assert_eq!(verdict, Verdict::Ok);
            assert!(frame.is_active());
            assert_eq!(depth(), 1);
        }
        assert_eq!(depth(), 0);
    }

    #[test]
    fn repeated_receiver_selector_pair_is_a_cycle() {
        reset();
        // The scan only starts at `CYCLE_SCAN_DEPTH`, so prime the chain with
        // distinct frames first (a real cycle grows past that depth anyway).
        let mut prefix = Vec::new();
        for index in 0..CYCLE_SCAN_DEPTH as u32 {
            let (frame, verdict) = DispatchFrame::enter(0x9000 + index * 4, 0x7000);
            assert_eq!(verdict, Verdict::Ok);
            prefix.push(frame);
        }
        let (first, verdict) = DispatchFrame::enter(0x1000, 0x2000);
        assert_eq!(verdict, Verdict::Ok);
        let (second, verdict) = DispatchFrame::enter(0x1000, 0x2000);
        assert_eq!(verdict, Verdict::Ok);
        // The third nested occurrence is refused, before any stack growth.
        let (third, verdict) = DispatchFrame::enter(0x1000, 0x2000);
        assert_eq!(verdict, Verdict::Cycle { occurrences: 2 });
        assert!(!third.is_active());
        assert_eq!(depth(), CYCLE_SCAN_DEPTH + 2);
        drop(second);
        drop(first);
        drop(prefix);
        assert_eq!(depth(), 0);
    }

    #[test]
    fn different_receivers_are_not_a_cycle() {
        reset();
        let mut frames = Vec::new();
        for index in 0..64u32 {
            let (frame, verdict) = DispatchFrame::enter(0x1000 + index, 0x2000);
            assert_eq!(verdict, Verdict::Ok);
            frames.push(frame);
        }
        assert_eq!(depth(), 64);
        drop(frames);
        assert_eq!(depth(), 0);
    }

    #[test]
    fn different_selectors_on_one_receiver_are_not_a_cycle() {
        reset();
        // This is the common case: `-class` inside `-isKindOfClass:`, etc.
        let (a, verdict) = DispatchFrame::enter(0x1000, 0x1);
        assert_eq!(verdict, Verdict::Ok);
        let (b, verdict) = DispatchFrame::enter(0x1000, 0x2);
        assert_eq!(verdict, Verdict::Ok);
        let (c, verdict) = DispatchFrame::enter(0x1000, 0x1);
        assert_eq!(verdict, Verdict::Ok);
        drop(c);
        drop(b);
        drop(a);
        assert_eq!(depth(), 0);
    }

    #[test]
    fn deep_non_cyclic_chain_is_allowed_past_the_old_limit() {
        reset();
        let mut frames = Vec::new();
        for index in 0..200u32 {
            // Same selector, different receivers: a view-hierarchy walk.
            let (frame, verdict) = DispatchFrame::enter(0x10000 + index * 4, 0x3000);
            assert!(
                matches!(verdict, Verdict::Ok | Verdict::Deep),
                "unexpected verdict {:?} at depth {}",
                verdict,
                index
            );
            frames.push(frame);
        }
        assert_eq!(depth(), 200);
        while let Some(frame) = frames.pop() {
            drop(frame);
        }
        assert_eq!(depth(), 0);
    }

    #[test]
    fn depth_cap_is_enforced_without_a_cycle() {
        reset();
        let mut frames = Vec::new();
        for index in 0..MAX_DEPTH as u32 {
            let (frame, verdict) = DispatchFrame::enter(0x20000 + index * 4, 0x3000);
            assert_ne!(verdict, Verdict::TooDeep);
            frames.push(frame);
        }
        let (frame, verdict) = DispatchFrame::enter(0x9999, 0x3000);
        assert_eq!(verdict, Verdict::TooDeep);
        assert!(!frame.is_active());
        assert_eq!(depth(), MAX_DEPTH);
        drop(frames);
        assert_eq!(depth(), 0);
    }

    #[test]
    fn deep_verdict_starts_at_the_warn_depth() {
        reset();
        let mut frames = Vec::new();
        for index in 0..(WARN_DEPTH as u32 - 1) {
            let (frame, verdict) = DispatchFrame::enter(0x30000 + index * 4, 0x4000);
            assert_eq!(verdict, Verdict::Ok);
            frames.push(frame);
        }
        let (frame, verdict) = DispatchFrame::enter(0x40000, 0x4000);
        assert_eq!(verdict, Verdict::Deep);
        frames.push(frame);
        drop(frames);
        assert_eq!(depth(), 0);
    }

    #[test]
    fn snapshot_returns_the_innermost_frames() {
        reset();
        let (a, _) = DispatchFrame::enter(0x1, 0x10);
        let (b, _) = DispatchFrame::enter(0x2, 0x20);
        let frames = snapshot(1);
        assert_eq!(
            frames,
            vec![Frame {
                receiver: 0x2,
                selector: 0x20
            }]
        );
        let frames = snapshot(CHAIN_DUMP_LEN);
        assert_eq!(frames.len(), 2);
        drop(b);
        drop(a);
        assert!(snapshot(4).is_empty());
    }
}
