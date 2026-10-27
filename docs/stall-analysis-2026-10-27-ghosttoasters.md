# Ghost Toasters: what the stall report actually says

Analysis of `touchHLE_log.txt` from UltraHLE Rebirth v0.2.3 (build `2990965-dirty`),
Ghost Toasters - Regular Show 1.1.4, iPhone3GS profile, ANGLE ES 2.0 on Adreno 642L.
Line numbers refer to that log.

## Short version

This is not a graphics problem, even though the dialog says it is. The app's
startup code blocks (or spin-waits) inside its own initialisation, at the point
where it talks to StoreKit / GameKit / PlayHaven / Crittercism, and the thread
that presents frames never comes back. The watchdog's "...this is a presentation
problem" sentence is its default wording for "the CPU counter moved but the frame
counter did not"; both counters are process-wide, so it cannot tell which thread
stopped. In this log the evidence says *the app stopped the thread that presents*,
not that presenting stopped working.

## What the log proves

1. Presented frames are counted at a single choke point. Every presenter path
   (`--present-mode=direct` GPU copy included) goes through
   `Window::swap_window` (`src/window.rs:2671`), which is where the watchdog's
   counter is bumped (`src/frameworks/opengles/eagl.rs:2706/2800/3694`). So
   "no frame presented" means the guest really did stop calling
   `presentRenderbuffer:` - it is not a counting artifact and not a dropped frame.

2. Guest thread 0 is the frame-presenting thread (`223`,
   "EAGL event polling first reached on guest thread 0"), and it is also the
   thread that runs the run loop (`152`, "Main run loop: now dispatching
   events, timers and composition").

3. Everything the app itself logged after the first frame is startup code, and all
   of it is on thread 0: `Current screen height: 320 Width:480` (`240`),
   `Platform 2x` / `Platform 1x` (`245`, `250`), `Load Products` (`255`),
   `INITIATED PRODUCTS` (`260`), `Load iap products` (`262`). The two
   `The referenced script on this Behaviour is missing!` lines are on thread 8
   (`230`, `235`): Unity's preload/loader thread was loading the first scene at
   that moment, i.e. other guest threads were alive and working.

4. The last framework activity is a run of stubbed SDK initialisation calls, with
   no app log line after them:

   - `SKProductsRequest initWithProductIdentifiers: stubbed (IAP emulation off)`
     (`261`) - with emulation off this returns `nil` and `start` is never called,
     so `productsRequest:didReceiveResponse:` can never fire
     (`src/frameworks/store_kit/sk_product.rs`);
   - `GKLocalPlayer localPlayer: singleton created` (`263`);
   - `UIWebView overlay skipped: view has no on-screen extent yet` and
     `deferring HTML load ... (frame 1.4e-45, no superview yet)` (`264`, `265`) -
     the PlayHaven/ad web view was created with no frame and no superview, so its
     content was deferred;
   - `NSPersistentStoreCoordinator addPersistentStoreWithType:SQLite ... stubbed`
     and `NSManagedObjectContext executeFetchRequest:error: stubbed -> empty
     array` (`266`, `267`).

5. Then 30 s of nothing presented, while the emulated CPU kept running. The PC
   ring (`270`) holds only seven distinct addresses in sixteen entries:
   `0x013e5f04` nine times, the `_objc_msgSend` stub `0x36461004` four times,
   `0x013e5ef4` (sixteen bytes below `0x013e5f04`, i.e. the next stub entry in the
   same section), `0x013e2964`, and `0x30008cc8` / `0x30008cf8` in another image.
   The ring records one entry per guest-to-host dispatch, so this is *the same few
   host calls being made over and over*, not a load that is walking through many
   different libc/framework entry points. Whatever the app is doing, it is looping.

6. The deferred WebView load was never retried. The code logs
   `UIWebView: deferred load still waiting (frame ...); polling on a timer` on the
   first 0.6 s retry (`src/frameworks/uikit/ui_view/ui_web_view.rs`), and that line
   is absent. Either the thread that started the load never returned to a run loop
   after `265`, or the load was started on a thread whose run loop nothing pumps.
   Either way the `webViewDidFinishLoad:` the SDK may be waiting for was not
   delivered.

## What it does not prove

The log cannot name the exact function the app is stuck in, because the watchdog
only keeps a single process-wide PC ring and a single "last host function" slot.
With a loader thread and an SDK worker thread running alongside the stuck main
thread, those get overwritten by the workers. That is precisely the ambiguity this
tree now fixes (see below).

## Ranked explanations

**A. Startup wait-loop on a callback that can never arrive (most likely).**
The app's last own words are `Load iap products`, and the product request is a
stub that answers `nil` - no delegate callback, ever. The same shape applies to
the deferred PlayHaven web view (finished-load callback depends on a run loop that
is not being pumped) and to Crittercism's Core Data store (stubbed, empty fetch at
`267`). A Unity-side `while (!productsLoaded)` / retry loop produces exactly the
observed signature: the main thread never returns to its run loop (no frames, no
timers), it keeps making host calls (`_objc_msgSend` in a loop), and other threads
keep the CPU counter alive.

**B. Blocked worker + spinning main thread.**
The main thread polls a flag that only a worker can set, and the worker is stuck
inside a host call. The two `0x30008cc8` / `0x30008cf8` entries suggest a second
thread also doing host calls around the report. Distinguishing A from B is what
the new per-thread reporting is for.

**C. A genuinely long first-scene load (> 30 s).**
Possible on an emulated armv7 + software PVRTC decode (`19`, `80`), but the narrow
stub set in `270` and the complete absence of further one-time GL/framework lines
while the first real frame is built argue against it.

## Not the cause (ruled out)

- **The present path / `--present-mode=direct`.** Frames would still be counted;
  see (1).
- **DNS for the dead analytics hosts.** Both lookups were answered locally
  (`158`-`161`, `Could not resolve api2/media.playhaven.com`), which is the
  intended `--legacy-analytics=mock` behaviour.
- **The zero-initialised pthread mutex** (`229`). The emulator lazily registers a
  default mutex; it is a real app bug, but not a hang by itself.
- **The `_touchHLE_NSString` use-after-free and the missing
  `NSURLRequest initWithCoder:`/`encodeWithCoder:`** (`114`-`118`). Crittercism's
  request archival is broken (`NSURLRequest` should conform to `NSCoding`), and the
  double release in an autorelease pool drain is a real bug, but neither spins.

## What to do next

1. Rebuild with this tree and re-run. The stall report now says which guest thread
   is stuck, in which host function, and which threads are still running, and it
   dumps the PC/LR/FP/SP plus every active thread's guest stack into the log
   (`STALL-DIAG`, above the report). Those stacks name the app-side function
   directly, which is the one thing this log was missing.
2. Quick A/Bs from the app's own options file (`.../files/touchHLE_options.txt`,
   currently absent - `62`, see `61` for the defaults in use):
   - `com.turner.ghosttoasters: --xml-dom-mirror --present-mode=direct --legacy-analytics=fail`
     changes the analytics answer from a synthetic 503 to `NSURLErrorCannotFindHost`,
     which the SDKs treat differently (fast failure instead of a retryable error).
   - On desktop, `TOUCHHLE_IAP_EMULATION=1` makes `SKProductsRequest` really answer,
     which directly tests explanation A. Android apps cannot be given environment
     variables, so on the device this one is not currently reachable.
