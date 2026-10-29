# Screen Annotation Prototype

This Windows prototype combines speech transcription with a captured screen that
can be annotated while recording. It is intended for local evaluation before a
community discussion and production pull request.

The Windows trial is named **Handy Preview**. Its app icon and all light/dark tray
states include a blue **P** badge; recording, processing, and warning artwork stay
distinct. The application identifier and portable `Data` directory are unchanged,
so existing settings and models are reused. It still shares Handy's single-instance
lock: exit the installed Handy before opening the portable preview.

## Interaction model

Ordinary dictation remains unchanged:

- **Transcribe** (`Ctrl+Space` by default) records and pastes text only.
- **Transcribe + Annotate** (hold **Left Alt+Space** by default) captures the
  monitor containing the pointer, opens a full-screen drawing surface, and
  records speech until Alt or Space is released.

Both bindings are configurable in **Settings > General**. A separate binding is
used instead of a global checkbox so screenshot capture is selected per
invocation. The Left Alt+Space binding always uses hold behavior, independent of
ordinary dictation's activation setting. Other annotation bindings retain the
configured activation behavior and use the Next point button.
When the held binding is selected, **Next point — Ctrl** appears as an indented
child row in settings. Ctrl is fixed and is not an editable shortcut.

While annotating:

- Draw directly on the frozen screen image.
- The first stroke anchors point **1**. Further strokes belong to that point.
- While holding Left Alt+Space, press **Ctrl** to start the next numbered point
  and speech segment without restarting the microphone. Release Ctrl to re-arm;
  holding it or Space auto-repeat never adds extra points. Its first stroke
  anchors the new number. **Next point** remains available as a button.
- Choose an ink color and stroke width.
- Clear all ink without discarding the screen capture, point numbers, or speech.
- Scroll over the captured page to move the original application's scroll area
  under the pointer. The frozen image, ink, and controls disappear while
  scrolling so the live source is visible. The next drawing press captures
  the current view and resumes drawing, including that first drag.
  The source is the window under the pointer when annotation starts, even if a
  different application has keyboard focus. The original paste destination is
  unchanged.
  Keep holding Left Alt+Space; Ctrl still starts the next point and does not
  become Ctrl+wheel zoom in the source application.
- Previous ink stays with its original screen capture, not over the new
  content. The live surface intercepts drawing presses: they never click a
  source link, button, or selection. A point can span several
  captures; its number is repeated on those screens and retains one speech
  segment. Clearing ink affects only the current screen.
- Read the live transcript below the controls when the selected model supports
  streaming. Numbered comments are transcribed locally per point after finishing.
- Release Alt or Space, or press the green check, to finish. Press **Escape**
  while still holding the chord to cancel without pasting.

Finishing hides the annotation window immediately. If browsing, it captures the
last live view before restoring the previously active application, then pastes
one numbered PNG followed by the matching numbered
comments. Silent points keep their number and an empty comment. A point finished
without a drawing keeps its badge in the top-left margin; numbers are never
silently removed or reassigned.

When scrolling changes the captured screen, the result is still **one PNG**:
full-size annotated screens are stacked vertically, in capture order, with
`Screen 1`, `Screen 2`, etc. headers. This is not a seamless full-page screenshot:
overlapping content and fixed page headers are retained, and scrolling back
does not relocate earlier ink onto the live page. No browser extension, document
coordinate inference, or external service is involved.

## Implementation

- `src-tauri/src/screen_annotation.rs` captures the active monitor with Win32 GDI,
  owns the reusable Tauri annotation window, coordinates lifecycle state, and
  restores the target window.
- `src/annotation/` renders the full-screen canvas and live transcript controls.
- `src-tauri/src/actions.rs` starts annotation only for the dedicated
  `transcribe_with_annotation` action.
- `src-tauri/src/clipboard.rs` performs the two-stage image and text paste while
  preserving the previous clipboard content.

Point boundaries are captured in the recorder's 16 kHz sample timeline, not a
webview timer. Annotation capture replaces VAD-rejected frames with silence and
fills delayed speech pre-roll back into its original slots, so silence removal
cannot shift a spoken comment into another point. Ordinary dictation keeps its
existing VAD behavior. Audio history stores a small `.annotation.json` boundary
file beside each numbered WAV so retrying transcription preserves the numbers;
history deletion and retention also remove that boundary file.

The held shortcut reuses handy-keys' blocking keyboard listener on Windows, so
Alt+Space does not also open the window's system menu. Only the chord's Space
events are blocked; ordinary shortcuts remain unchanged when idle. Control
transitions are queued with recording start/stop, and audio boundaries are
committed before a subsequent release can finalize the recording. The overlay
mirrors these numbered updates instead of recording a second boundary.

An untouched former Ctrl+Alt+Space default is upgraded to Left Alt+Space when
there is no conflicting Handy binding. Customized annotation shortcuts and
conflicting assignments are preserved.
While the held binding is configured, settings reject assigning Left Alt+Space
(including generic Alt+Space) to another Handy action, including cancellation.
Conflicting changes are rejected before unregistering the working shortcut.

The screenshot is written to Handy's application cache only long enough for the
webview to load it. The cached file is deleted after paste or cancellation. Speech
and image processing remain local.

The annotation webview is created once and reused. Its lifecycle distinguishes
recording, finalizing, and finalized states so a delayed image-load callback
cannot re-show the overlay after recording stops.
After Windows activates the overlay, the embedded WebView is explicitly focused:
activating only the outer window can leave wheel input outside the canvas.

Scrolling selects the source window under the initial pointer independently of
the foreground paste destination. Source hit-testing, monitor capture, and wheel
routing use physical pixels, including on DPI-unaware worker threads.
It forwards only Win32 wheel messages to that source window's child under the
pointer, without Alt/Ctrl/Space modifier flags or synthesized key events.
Wheel input is serialized. Entering browse preserves the current canvas, installs
a session-scoped `WH_MOUSE_LL` hook on the UI thread, then hides the overlay.
The source is genuinely visible and owns native hit-testing. Keeping an input
shield above Chromium instead caused forwarded wheels to return to the overlay.
The hook consumes wheels in the captured region and forwards them through the
same modifier-free route. It consumes the first drawing press, moves and matching
release before the source receives them. No snapshot is taken per wheel. The
press waits for an in-flight wheel, captures once, and buffers its drag until
image decoding completes. Controls and point markers reappear after refreezing;
the hook stays installed until the protected gesture ends.
The prior annotated view is archived only when the capture changes; unchanged
captures restore its original ink without creating a duplicate screen. Ctrl
boundaries, including undrawn points, are preserved across the transition.
The overlay stays hidden during recapture; Windows' temporary
`WDA_EXCLUDEFROMCAPTURE` affinity also excludes a visible overlay during late
finalization. Held modifier keys are never synthesized or released.
The session/capture IDs reject stale work; release waits for in-flight scroll,
capture, and the buffered first stroke
without sending queued future scrolls, and cancellation prevents delayed
capture callbacks from changing a later session. Capture errors preserve the
previous annotations and show a retry message rather than clearing them.
Hook removal is serialized on the UI thread. Finish/cancel drain a swallowed
button down through its matching up before removing the hook, without leaking an
unmatched release to the source. Focus changes do not disable first-press
protection in the captured region; capture/show still reject unrelated apps.
Full-size screen PNGs are joined natively instead of exceeding browser canvas
height limits. All temporary source captures are removed when the session ends.
Local diagnostic logs correlate wheel receipt, recapture, and canvas draw by
session/capture ID. The draw acknowledgement contains no screen or speech content;
it confirms a canvas update, not that the source application actually scrolled.

## Development

The prototype is available on Windows. Set `HANDY_SCREEN_ANNOTATION=0` before
launching to disable the annotation window and binding behavior.

Run the normal development command:

```powershell
bun run tauri dev
```

On a machine without the Vulkan SDK, a CPU-only local run can be started with:

```powershell
$env:CMAKE_ARGS = "-DTRANSCRIBE_VULKAN=OFF"
$env:CARGO_PROFILE_DEV_DEBUG = "0"
bun run tauri dev
```

Relevant checks:

```powershell
bun run build
bun run lint
cargo test --manifest-path src-tauri\Cargo.toml --lib
```

Targeted numbered-point checks (no microphone or screen capture):

```powershell
cargo test --manifest-path src-tauri\Cargo.toml --lib annotation
bun run test:playwright annotation.spec.ts
```

On Windows with Edge already installed, the browser checks can use it without
downloading Playwright's Chromium:

```powershell
$env:PLAYWRIGHT_CHROMIUM_CHANNEL = "msedge"
bun run test:playwright annotation --workers=2 --reporter=line
```

The browser checks use a synthetic screenshot and mocked Tauri commands; they do
not capture the display, record audio, or paste into another application. Scroll
checks use an isolated synthetic scrollable document. Native tests send wheel
messages only to an owned hidden test window and verify GDI capture exclusion
using a 2-by-2-pixel patch entirely covered by two owned, opaque color-fixture
windows. They also verify pointer-based selection of inactive source windows and
physical-coordinate routing from a DPI-unaware worker. They do not operate or
capture a user app or persist captured pixels.

The opt-in Windows integration check uses the real built annotation frontend and
native overlay above two scrollable child WebViews:

```powershell
bun run build
cargo test --manifest-path src-tauri\Cargo.toml --features annotation-scroll-check --lib live_browse_intercepts_first_press_and_refreezes -- --nocapture
```

It checks wheel receipt with thread-local held Alt/Space state, the pointed
child's actual source offset after scrolling settles, displacement of a known
landmark in live desktop pixels, and native hit-testing of the source while the
overlay is hidden. It drives another wheel and the first gesture through the
installed production hook callback, requiring its blocking return value. It
checks first-stroke replay, native recapture, matching canvas landmark, no source
pointer press, protection through overlay redisplay, and cancel-time button
draining. The
other child and point/audio boundaries must remain unchanged. Captures are
restricted to an opaque, owned
synthetic source; temporary captures and an isolated WebView profile are removed
afterward. No microphone, clipboard, global key injection, or user-app capture is
used. This does not inject a physical mouse gesture or test physical hook
delivery, the global shortcut, or a particular external host such as Copilot's
Browser canvas.
Browser-only success does not establish native click interception or live
desktop visibility; those require the native fixture to complete successfully.

Use an interactive Windows desktop. The check refuses to start while another
annotation window is visible and fails with a bounded timeout rather than
bypassing Windows' foreground protection or requesting manual focus changes.
The `--lib` entry avoids rebuilding a running `handy.exe`. If Tauri's build
script cannot copy locked runtime resources while Preview runs, checks can
temporarily set `$env:TAURI_CONFIG = '{"bundle":{"resources":[]}}'`; remove this
override before a normal application build.

## Prototype constraints

- Windows only.
- One production startup attempt failed with WebView2 `0x80070057` before the
  main UI and shortcuts initialized. An instrumented restart succeeded without a
  product startup change. The failing API and cause remain unconfirmed; the
  diagnostic build's successful startup does not establish normal-build reliability.
- Captures one monitor: the monitor containing the pointer when recording starts.
- The destination application must support pasting images.
- Source scrolling depends on the target accepting standard Windows wheel
  messages. Elevated/protected apps and custom input surfaces may reject them.
  Keep the pointer over the original app; the preview will not scroll a
  different app or reclaim focus after you switch away.
- Source-page scrolling requires Windows 10 version 2004 or later and desktop
  composition; older systems retain frozen-screen annotation but reject scrolling.
- Drawing uses frozen captures; browsing hides the overlay to expose the live
  source. Clicking while browsing starts drawing, not interaction with
  source controls. Animated or slowly loading content may change after the
  first-press capture. Keep the original application active; if you switch away,
  release the held shortcut or press Escape before interacting with another app
  in the captured region. Long sessions
  create tall PNGs, which the destination app must be able to paste.
- Image and text are pasted as two clipboard operations because many applications
  do not consume mixed clipboard formats consistently.
- One local transcription is run per point at completion. This adds processing
  time for long sessions; automatic point detection and point reordering are not
  part of this prototype.
- A production proposal should gather community feedback and decide whether the
  annotation binding is visible by default or placed behind an experimental
  master toggle. The per-invocation binding should remain the primary control.
