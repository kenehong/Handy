use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Listener, Manager};

#[cfg(all(target_os = "windows", feature = "annotation-scroll-check"))]
pub(crate) mod scroll_check;

#[cfg(target_os = "windows")]
mod browse_input;

const WINDOW_LABEL: &str = "screen_annotation";
const FINALIZE_TIMEOUT: Duration = Duration::from_secs(10);
static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, serde::Serialize, specta::Type)]
pub struct ScreenAnnotationSnapshot {
    session_id: u64,
    capture_id: u64,
    path: String,
    width: u32,
    height: u32,
    point_count: usize,
}

pub fn remove_audio_segments(wav_path: &Path) {
    if let Err(error) = std::fs::remove_file(audio_segments_path(wav_path)) {
        if error.kind() != std::io::ErrorKind::NotFound {
            log::error!("Failed to remove annotation audio boundaries: {error}");
        }
    }
}

#[derive(Clone, Copy)]
struct ScreenBounds {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

struct AnnotationSession {
    snapshot: ScreenAnnotationSnapshot,
    snapshot_paths: Vec<PathBuf>,
    png: Vec<u8>,
    bounds: ScreenBounds,
    binding_id: String,
    target_window: isize,
    scroll_window: isize,
    phase: AnnotationPhase,
    point_starts: Vec<usize>,
    scrolling: bool,
    browsing: bool,
}

impl Drop for AnnotationSession {
    fn drop(&mut self) {
        for path in &self.snapshot_paths {
            if let Err(error) = std::fs::remove_file(path) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    log::error!("Failed to remove an annotation capture: {error}");
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AnnotationPhase {
    Recording,
    Finalizing,
    Finalized,
    Failed,
}

impl AnnotationPhase {
    fn can_show(self) -> bool {
        self == Self::Recording
    }

    fn should_wait(self) -> bool {
        self == Self::Finalizing
    }

    fn can_submit(self) -> bool {
        self == Self::Finalizing
    }
}

#[derive(Default)]
pub struct ScreenAnnotationState {
    session: Mutex<Option<AnnotationSession>>,
    finalized: Condvar,
}

pub fn initialize(app: &AppHandle) {
    app.manage(ScreenAnnotationState::default());
    #[derive(serde::Deserialize)]
    struct PreviewDrawn {
        session_id: u64,
        capture_id: u64,
    }
    app.listen(
        "screen-annotation-preview-drawn",
        |event| match serde_json::from_str::<PreviewDrawn>(event.payload()) {
            Ok(frame) => log::info!(
                "Annotation preview drawn: session={} capture={}",
                frame.session_id,
                frame.capture_id
            ),
            Err(error) => log::warn!("Invalid annotation preview metadata: {error}"),
        },
    );
}

pub fn create_window(app: &AppHandle) {
    if !enabled() || app.get_webview_window(WINDOW_LABEL).is_some() {
        return;
    }

    #[cfg(target_os = "windows")]
    {
        let mut builder = tauri::WebviewWindowBuilder::new(
            app,
            WINDOW_LABEL,
            tauri::WebviewUrl::App("src/annotation/index.html".into()),
        )
        .title("Screen annotation")
        .decorations(false)
        .resizable(false)
        .maximizable(false)
        .minimizable(false)
        .closable(false)
        .shadow(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .focusable(true)
        .focused(false)
        .visible(false);

        if let Some(data_dir) = crate::portable::data_dir() {
            builder = builder.data_directory(data_dir.join("webview"));
        }

        if let Err(error) = builder.build() {
            log::error!("Failed to create screen annotation window: {error}");
        }
    }
}

pub fn enabled() -> bool {
    #[cfg(target_os = "windows")]
    {
        match std::env::var("HANDY_SCREEN_ANNOTATION") {
            Ok(value) => !matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "" | "0" | "false" | "no" | "off"
            ),
            Err(_) => true,
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        false
    }
}

pub fn begin(app: &AppHandle, binding_id: &str) -> bool {
    if !enabled() {
        return false;
    }

    #[cfg(target_os = "windows")]
    {
        match begin_windows(app, binding_id) {
            Ok(()) => true,
            Err(error) => {
                log::warn!("Screen annotation unavailable: {error}");
                false
            }
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = app;
        false
    }
}

pub fn finish(app: &AppHandle) {
    if !enabled() {
        return;
    }

    let state = app.state::<ScreenAnnotationState>();
    {
        let Ok(mut session) = state.session.lock() else {
            log::error!("Failed to lock screen annotation state");
            return;
        };
        let Some(session) = session.as_mut() else {
            return;
        };
        session.phase = AnnotationPhase::Finalizing;
    }
    #[cfg(target_os = "windows")]
    browse_input::stop();

    let Some(window) = app.get_webview_window(WINDOW_LABEL) else {
        mark_finalization_failed(&state);
        return;
    };

    if let Err(error) = window.emit("finish-screen-annotation", ()) {
        log::warn!("Failed to finalize screen annotation: {error}");
        mark_finalization_failed(&state);
        hide_and_restore(app);
    } else {
        // Dismiss immediately; PNG encoding and transcription can finish while
        // the hidden webview remains alive.
        hide_and_restore(app);
    }
}

pub fn cancel(app: &AppHandle) {
    #[cfg(target_os = "windows")]
    browse_input::stop();
    let state = app.state::<ScreenAnnotationState>();
    let session = state
        .session
        .lock()
        .ok()
        .and_then(|mut session| session.take());
    state.finalized.notify_all();

    if let Some(session) = session {
        restore_target_window(session.target_window);
    }

    if let Some(window) = app.get_webview_window(WINDOW_LABEL) {
        let _ = window.hide();
    }
}

fn is_recording_session(app: &AppHandle, session_id: u64) -> bool {
    app.try_state::<ScreenAnnotationState>()
        .and_then(|state| {
            state.session.lock().ok().map(|session| {
                session.as_ref().is_some_and(|session| {
                    session.snapshot.session_id == session_id && session.phase.can_show()
                })
            })
        })
        .unwrap_or(false)
}

pub fn take_annotated_image(app: &AppHandle) -> Result<Option<Vec<u8>>, String> {
    let state = app.state::<ScreenAnnotationState>();
    let mut guard = state.session.lock().map_err(|error| error.to_string())?;

    if guard
        .as_ref()
        .is_some_and(|session| session.phase.should_wait())
    {
        let (new_guard, wait_result) = state
            .finalized
            .wait_timeout_while(guard, FINALIZE_TIMEOUT, |session| {
                session
                    .as_ref()
                    .is_some_and(|session| session.phase.should_wait())
            })
            .map_err(|error| error.to_string())?;
        guard = new_guard;
        if wait_result.timed_out() {
            return Err("Timed out waiting for the numbered screenshot".to_string());
        }
    }

    let Some(session) = guard.as_ref() else {
        return Ok(None);
    };
    if session.phase != AnnotationPhase::Finalized {
        return Err("The numbered screenshot is not ready".to_string());
    }
    let mut session = guard.take().ok_or("The annotation session ended")?;
    drop(guard);

    restore_target_window(session.target_window);
    if let Some(window) = app.get_webview_window(WINDOW_LABEL) {
        let _ = window.hide();
    }
    Ok(Some(std::mem::take(&mut session.png)))
}

pub fn audio_segment_starts(app: &AppHandle) -> Result<Option<Vec<usize>>, String> {
    let state = app.state::<ScreenAnnotationState>();
    let session = state.session.lock().map_err(|error| error.to_string())?;
    Ok(session.as_ref().map(|session| session.point_starts.clone()))
}

pub fn request_next_point(app: &AppHandle) {
    let result = advance_point(app, None).and_then(|snapshot| {
        app.get_webview_window(WINDOW_LABEL)
            .ok_or_else(|| "The annotation window is unavailable".to_string())
            .and_then(|window| {
                window
                    .emit("annotation-point-changed", snapshot)
                    .map_err(|error| error.to_string())
            })
    });
    if let Err(error) = result {
        log::error!("Failed to advance the annotation point: {error}");
        let _ = app.emit("transcription-error", error);
    }
}

#[tauri::command]
#[specta::specta]
pub async fn advance_screen_annotation(
    app: AppHandle,
    session_id: u64,
    point_number: usize,
) -> Result<usize, String> {
    tauri::async_runtime::spawn_blocking(move || {
        advance_point(&app, Some((session_id, point_number))).map(|snapshot| snapshot.point_count)
    })
    .await
    .map_err(|error| format!("Failed to start the next annotation point: {error}"))?
}

fn advance_point(
    app: &AppHandle,
    expected: Option<(u64, usize)>,
) -> Result<ScreenAnnotationSnapshot, String> {
    let state = app.state::<ScreenAnnotationState>();
    let mut session = state.session.lock().map_err(|error| error.to_string())?;
    let session = session.as_mut().ok_or("No screen annotation is active")?;
    let (session_id, point_number) =
        expected.unwrap_or((session.snapshot.session_id, session.point_starts.len()));
    validate_point_transition(session, session_id, point_number)?;
    // Serialize the boundary with finish/cancel and other point requests.
    let position = app
        .state::<std::sync::Arc<crate::managers::audio::AudioRecordingManager>>()
        .recording_position(&session.binding_id)?;
    session.point_starts.push(position);
    session.snapshot.point_count = session.point_starts.len();
    Ok(session.snapshot.clone())
}

fn validate_point_transition(
    session: &AnnotationSession,
    session_id: u64,
    point_number: usize,
) -> Result<(), String> {
    if session.snapshot.session_id != session_id
        || session.phase != AnnotationPhase::Recording
        || session.point_starts.len() != point_number
    {
        return Err("The annotation point is no longer current".to_string());
    }
    Ok(())
}

pub fn audio_segments_path(wav_path: &Path) -> PathBuf {
    wav_path.with_extension("annotation.json")
}

pub fn save_audio_segments(wav_path: &Path, starts: &[usize]) -> anyhow::Result<()> {
    std::fs::write(audio_segments_path(wav_path), serde_json::to_vec(starts)?)?;
    Ok(())
}

pub fn read_audio_segments(wav_path: &Path) -> anyhow::Result<Option<Vec<usize>>> {
    match std::fs::read(audio_segments_path(wav_path)) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub fn transcribe_numbered_segments(
    samples: &[f32],
    starts: &[usize],
    mut transcribe: impl FnMut(Vec<f32>) -> anyhow::Result<String>,
) -> anyhow::Result<String> {
    use anyhow::{ensure, Context};

    ensure!(
        starts.first() == Some(&0)
            && starts.windows(2).all(|pair| pair[0] <= pair[1])
            && starts.last().is_some_and(|start| *start <= samples.len()),
        "Invalid annotation audio boundaries"
    );
    starts
        .iter()
        .enumerate()
        .map(|(index, start)| {
            let end = starts.get(index + 1).copied().unwrap_or(samples.len());
            let audio = &samples[*start..end];
            let text = if audio.iter().all(|sample| *sample == 0.0) {
                String::new()
            } else {
                transcribe(audio.to_vec()).with_context(|| {
                    format!("Failed to transcribe annotation point {}", index + 1)
                })?
            };
            Ok(format!("{}. {}", index + 1, text.trim()))
        })
        .collect::<anyhow::Result<Vec<_>>>()
        .map(|points| points.join("\n"))
}

#[tauri::command]
#[specta::specta]
pub fn complete_screen_annotation(app: AppHandle) -> Result<(), String> {
    let state = app.state::<ScreenAnnotationState>();
    let binding_id = state
        .session
        .lock()
        .map_err(|_| "Failed to lock screen annotation state".to_string())?
        .as_ref()
        .map(|session| session.binding_id.clone())
        .ok_or_else(|| "No screen annotation is active".to_string())?;
    let coordinator = app
        .try_state::<crate::TranscriptionCoordinator>()
        .ok_or_else(|| "Transcription coordinator is unavailable".to_string())?;
    coordinator.send_external_input(&binding_id, "screen annotation done");
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn get_screen_annotation_snapshot(app: AppHandle) -> Result<ScreenAnnotationSnapshot, String> {
    let state = app.state::<ScreenAnnotationState>();
    let session = state
        .session
        .lock()
        .map_err(|_| "Failed to lock screen annotation state".to_string())?;
    session
        .as_ref()
        .map(|session| session.snapshot.clone())
        .ok_or_else(|| "No screen annotation is active".to_string())
}

#[tauri::command]
#[specta::specta]
pub fn show_screen_annotation(
    app: AppHandle,
    session_id: u64,
    capture_id: Option<u64>,
) -> Result<(), String> {
    let state = app.state::<ScreenAnnotationState>();
    let (bounds, target_window, scroll_window, can_show) = state
        .session
        .lock()
        .map_err(|_| "Failed to lock screen annotation state".to_string())?
        .as_ref()
        .filter(|session| {
            session.snapshot.session_id == session_id
                && capture_id.is_none_or(|id| id == session.snapshot.capture_id)
        })
        .map(|session| {
            (
                session.bounds,
                session.target_window,
                session.scroll_window,
                session.phase.can_show(),
            )
        })
        .ok_or_else(|| "No screen annotation is active".to_string())?;
    if !can_show {
        log::debug!("Ignoring annotation show after recording ended: session={session_id}");
        return Ok(());
    }

    let window = app
        .get_webview_window(WINDOW_LABEL)
        .ok_or_else(|| "Screen annotation window is unavailable".to_string())?;

    #[cfg(target_os = "windows")]
    {
        let foreground = unsafe { windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow() };
        let overlay = window.hwnd().map_err(|error| error.to_string())?;
        if foreground != overlay
            && foreground.0 as isize != target_window
            && foreground.0 as isize != scroll_window
        {
            return Err("The original application is no longer active".to_string());
        }
        place_annotation_window(&window, bounds)?;
        browse_input::shown(session_id);
        log::info!(
            "Annotation overlay ready: session={session_id} capture={capture_id:?} overlay=[{}] focus=[{}]",
            window_metadata(overlay),
            window_metadata(unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetFocus() })
        );
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = (bounds, target_window, scroll_window);
        window.show().map_err(|error| error.to_string())?;
        window.set_focus().map_err(|error| error.to_string())?;
    }

    Ok(())
}

#[tauri::command]
#[specta::specta]
pub async fn scroll_screen_annotation(
    app: AppHandle,
    session_id: u64,
    capture_id: u64,
    delta_x: i32,
    delta_y: i32,
    x: f64,
    y: f64,
) -> Result<ScreenAnnotationSnapshot, String> {
    log::info!(
        "Annotation wheel received: session={session_id} capture={capture_id} delta=({delta_x},{delta_y}) position=({x:.3},{y:.3})"
    );
    tauri::async_runtime::spawn_blocking(move || {
        let result = scroll_live_page(&app, session_id, capture_id, delta_x, delta_y, x, y);
        if let Err(error) = &result {
            log::error!("Failed to scroll the annotated page: {error}");
        }
        result
    })
    .await
    .map_err(|error| format!("Failed to scroll the annotated page: {error}"))?
}

fn validate_scroll(
    session: &AnnotationSession,
    session_id: u64,
    capture_id: u64,
) -> Result<(), String> {
    if session.snapshot.session_id != session_id
        || session.snapshot.capture_id != capture_id
        || !session.phase.can_show()
        || session.scrolling
    {
        return Err("The annotation capture is no longer current".to_string());
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn scroll_live_page(
    app: &AppHandle,
    session_id: u64,
    capture_id: u64,
    delta_x: i32,
    delta_y: i32,
    x: f64,
    y: f64,
) -> Result<ScreenAnnotationSnapshot, String> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

    let state = app.state::<ScreenAnnotationState>();
    let window = app
        .get_webview_window(WINDOW_LABEL)
        .ok_or("The annotation window is unavailable")?;
    let overlay = window.hwnd().map_err(|error| error.to_string())?.0 as isize;
    ensure_capture_exclusion_supported()?;
    let (bounds, source, entering) = {
        let mut guard = state.session.lock().map_err(|error| error.to_string())?;
        let session = guard.as_mut().ok_or("No screen annotation is active")?;
        validate_scroll(session, session_id, capture_id)?;
        let foreground = unsafe { GetForegroundWindow() };
        if foreground.0 as isize != overlay
            && foreground.0 as isize != session.target_window
            && foreground.0 as isize != session.scroll_window
        {
            return Err("The original application is no longer active".to_string());
        }
        // Serialize wheel delivery with release/cancel. No modifier key is
        // synthesized: holding Ctrl must advance points, not zoom the source.
        let entering = !session.browsing;
        session.browsing = true;
        session.scrolling = true;
        (session.bounds, session.scroll_window, entering)
    };
    let result = (|| {
        if entering {
            let (sender, receiver) = std::sync::mpsc::sync_channel(1);
            let handle = app.clone();
            app.run_on_main_thread(move || {
                let result = (|| {
                    let state = handle.state::<ScreenAnnotationState>();
                    let guard = state.session.lock().map_err(|error| error.to_string())?;
                    let session = guard.as_ref().ok_or("The annotation was cancelled")?;
                    if !scroll_can_complete(session, session_id, capture_id) {
                        return Err("The annotation capture is no longer current".to_string());
                    }
                    let recording = session.phase.can_show();
                    drop(guard);
                    if recording {
                        browse_input::start(&handle, session_id, bounds)?;
                    }
                    let window = handle
                        .get_webview_window(WINDOW_LABEL)
                        .ok_or("No annotation window")?;
                    if let Err(error) = window.hide() {
                        browse_input::stop();
                        return Err(error.to_string());
                    }
                    Ok(())
                })();
                let _ = sender.send(result);
            })
            .map_err(|error| error.to_string())?;
            receiver
                .recv_timeout(Duration::from_secs(2))
                .map_err(|error| format!("Failed to enter live browse: {error}"))??;
        }
        scroll_target(HWND(source as *mut _), bounds, delta_x, delta_y, x, y)?;
        let guard = state.session.lock().map_err(|error| error.to_string())?;
        let session = guard.as_ref().ok_or("The annotation was cancelled")?;
        if !scroll_can_complete(session, session_id, capture_id) {
            return Err("The annotation capture is no longer current".to_string());
        }
        Ok(session.snapshot.clone())
    })();
    let mut guard = state.session.lock().map_err(|error| error.to_string())?;
    if let Some(session) = guard
        .as_mut()
        .filter(|s| s.snapshot.session_id == session_id)
    {
        session.scrolling = false;
        if result.is_err() {
            session.browsing = false;
        }
    }
    result
}

#[tauri::command]
#[specta::specta]
pub async fn freeze_screen_annotation(
    app: AppHandle,
    session_id: u64,
    capture_id: u64,
) -> Result<ScreenAnnotationSnapshot, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let result = freeze_browsed_page(&app, session_id, capture_id);
        if let Err(error) = &result {
            log::error!("Failed to freeze the browsed page: {error}");
            let _ = app.emit("transcription-error", error.clone());
        }
        result
    })
    .await
    .map_err(|error| error.to_string())?
}

#[cfg(target_os = "windows")]
fn freeze_browsed_page(
    app: &AppHandle,
    session_id: u64,
    capture_id: u64,
) -> Result<ScreenAnnotationSnapshot, String> {
    use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
    let state = app.state::<ScreenAnnotationState>();
    let overlay = app
        .get_webview_window(WINDOW_LABEL)
        .ok_or("No annotation window")?
        .hwnd()
        .map_err(|error| error.to_string())?
        .0 as isize;
    let (bounds, target_window, source) = {
        let mut guard = state.session.lock().map_err(|error| error.to_string())?;
        let session = guard.as_mut().ok_or("No annotation is active")?;
        if session.snapshot.session_id != session_id
            || session.snapshot.capture_id != capture_id
            || !session.browsing
            || session.scrolling
            || !matches!(
                session.phase,
                AnnotationPhase::Recording | AnnotationPhase::Finalizing
            )
        {
            return Err("The browsed annotation is no longer current".into());
        }
        session.scrolling = true;
        (session.bounds, session.target_window, session.scroll_window)
    };
    browse_input::waiting(session_id);
    let result = (|| {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let capture_app = app.clone();
        app.run_on_main_thread(move || {
            let result = (|| {
                let state = capture_app.state::<ScreenAnnotationState>();
                let guard = state.session.lock().map_err(|error| error.to_string())?;
                let session = guard.as_ref().ok_or("The annotation was cancelled")?;
                if !scroll_can_complete(session, session_id, capture_id) {
                    return Err("The annotation capture is no longer current".to_string());
                }
                let foreground = unsafe { GetForegroundWindow() };
                if foreground.0 as isize != overlay
                    && foreground.0 as isize != source
                    && foreground.0 as isize != target_window
                {
                    return Err("The original application is no longer active".to_string());
                }
                let window = capture_app
                    .get_webview_window(WINDOW_LABEL)
                    .ok_or("The annotation window is unavailable")?;
                drop(guard);
                capture_behind_annotation(window.hwnd().map_err(|error| error.to_string())?, bounds)
            })();
            let _ = sender.send(result);
        })
        .map_err(|error| error.to_string())?;
        let rgba = receiver
            .recv_timeout(Duration::from_secs(2))
            .map_err(|error| format!("Failed to capture the scrolled page: {error}"))??;
        let png = encode_png(&rgba, bounds.width, bounds.height)?;
        let mut guard = state.session.lock().map_err(|error| error.to_string())?;
        let session = guard.as_mut().ok_or("The annotation was cancelled")?;
        if !scroll_can_complete(session, session_id, capture_id) {
            return Err("The annotation capture is no longer current".to_string());
        }
        let changed = session.png != png;
        if changed {
            let next_capture = capture_id + 1;
            let path = session
                .snapshot_paths
                .first()
                .ok_or("The original screen capture is unavailable")?
                .with_file_name(format!("screen-annotation-{session_id}-{next_capture}.png"));
            std::fs::write(&path, &png).map_err(|error| error.to_string())?;
            session.snapshot_paths.push(path.clone());
            session.snapshot.path = path.to_string_lossy().into_owned();
            session.snapshot.capture_id = next_capture;
            session.png = png;
        }
        session.browsing = false;
        log::info!(
            "Annotation recapture: session={session_id} capture={} changed={changed}",
            session.snapshot.capture_id
        );
        Ok(session.snapshot.clone())
    })();

    let mut guard = state.session.lock().map_err(|error| error.to_string())?;
    if let Some(session) = guard
        .as_mut()
        .filter(|session| session.snapshot.session_id == session_id)
    {
        session.scrolling = false;
        if result.is_err() {
            session.browsing = false;
        }
    }
    result
}

#[cfg(target_os = "windows")]
fn ensure_capture_exclusion_supported() -> Result<(), String> {
    let version = winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE)
        .open_subkey(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion")
        .and_then(|key| key.get_value::<String, _>("CurrentBuildNumber"))
        .map_err(|error| format!("Failed to check Windows capture support: {error}"))?;
    let build = version
        .parse::<u32>()
        .map_err(|error| format!("Failed to check Windows build: {error}"))?;
    if build < 19041 {
        return Err("Source-page scrolling requires Windows 10 version 2004 or later".to_string());
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn capture_behind_annotation(
    hwnd: windows::Win32::Foundation::HWND,
    bounds: ScreenBounds,
) -> Result<Vec<u8>, String> {
    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE, WDA_NONE,
    };
    // Exclusion also protects a late finalization capture if the overlay is visible.
    unsafe { SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE) }
        .map_err(|error| format!("Failed to exclude annotation controls from capture: {error}"))?;
    let capture = (|| {
        unsafe { windows::Win32::Graphics::Dwm::DwmFlush() }
            .map_err(|error| format!("Failed to refresh the display: {error}"))?;
        capture_screen(bounds)
    })();
    let restore = unsafe { SetWindowDisplayAffinity(hwnd, WDA_NONE) }
        .map_err(|error| format!("Failed to restore annotation capture visibility: {error}"));
    match (capture, restore) {
        (Ok(pixels), Ok(())) => Ok(pixels),
        (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
        (Err(capture), Err(restore)) => Err(format!("{capture}; {restore}")),
    }
}

fn scroll_can_complete(session: &AnnotationSession, session_id: u64, capture_id: u64) -> bool {
    session.snapshot.session_id == session_id
        && session.snapshot.capture_id == capture_id
        && session.scrolling
        && matches!(
            session.phase,
            AnnotationPhase::Recording | AnnotationPhase::Finalizing
        )
}

#[cfg(not(target_os = "windows"))]
fn scroll_live_page(
    _app: &AppHandle,
    _session_id: u64,
    _capture_id: u64,
    _delta_x: i32,
    _delta_y: i32,
    _x: f64,
    _y: f64,
) -> Result<ScreenAnnotationSnapshot, String> {
    Err("Screen annotation scrolling is only supported on Windows".to_string())
}

#[cfg(not(target_os = "windows"))]
fn freeze_browsed_page(_: &AppHandle, _: u64, _: u64) -> Result<ScreenAnnotationSnapshot, String> {
    Err("Screen annotation browsing is only supported on Windows".into())
}

#[cfg(target_os = "windows")]
struct PhysicalPixelContext(windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT);

#[cfg(target_os = "windows")]
impl PhysicalPixelContext {
    fn enter() -> Result<Self, String> {
        use windows::Win32::UI::HiDpi::{
            SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        };
        let previous =
            unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        if previous.0.is_null() {
            return Err(format!(
                "Failed to use physical screen coordinates: {}",
                windows::core::Error::from_win32()
            ));
        }
        Ok(Self(previous))
    }
}

#[cfg(target_os = "windows")]
impl Drop for PhysicalPixelContext {
    fn drop(&mut self) {
        if unsafe { windows::Win32::UI::HiDpi::SetThreadDpiAwarenessContext(self.0) }
            .0
            .is_null()
        {
            log::error!(
                "Failed to restore screen coordinate context: {}",
                windows::core::Error::from_win32()
            );
        }
    }
}

#[cfg(target_os = "windows")]
fn window_metadata(hwnd: windows::Win32::Foundation::HWND) -> String {
    use windows::Win32::UI::WindowsAndMessaging::{GetClassNameW, GetWindowThreadProcessId};
    let mut pid = 0;
    let thread = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    let mut class = [0; 128];
    let length = unsafe { GetClassNameW(hwnd, &mut class) };
    format!(
        "hwnd={:#x} pid={pid} thread={thread} class={}",
        hwnd.0 as usize,
        String::from_utf16_lossy(&class[..length.max(0) as usize])
    )
}

#[cfg(target_os = "windows")]
fn source_window_at_point(
    point: windows::Win32::Foundation::POINT,
) -> Result<windows::Win32::Foundation::HWND, String> {
    use windows::Win32::UI::WindowsAndMessaging::{GetAncestor, WindowFromPoint, GA_ROOT};
    let _coordinates = PhysicalPixelContext::enter()?;
    let window = unsafe { GetAncestor(WindowFromPoint(point), GA_ROOT) };
    if window.is_invalid() {
        return Err("No source window is available under the pointer".to_string());
    }
    Ok(window)
}

#[cfg(target_os = "windows")]
fn scroll_target(
    target: windows::Win32::Foundation::HWND,
    bounds: ScreenBounds,
    delta_x: i32,
    delta_y: i32,
    x: f64,
    y: f64,
) -> Result<(), String> {
    use windows::Win32::Foundation::{LPARAM, POINT, RECT, WPARAM};
    use windows::Win32::Graphics::Gdi::ScreenToClient;
    use windows::Win32::UI::WindowsAndMessaging::{
        ChildWindowFromPointEx, GetWindowRect, IsWindow, SendMessageTimeoutW, CWP_SKIPDISABLED,
        CWP_SKIPINVISIBLE, CWP_SKIPTRANSPARENT, SMTO_ABORTIFHUNG, SMTO_BLOCK, WM_MOUSEHWHEEL,
        WM_MOUSEWHEEL,
    };

    let _coordinates = PhysicalPixelContext::enter()?;
    if !x.is_finite()
        || !y.is_finite()
        || !(0.0..=1.0).contains(&x)
        || !(0.0..=1.0).contains(&y)
        || i16::try_from(delta_x).is_err()
        || i16::try_from(delta_y).is_err()
    {
        return Err("The scroll position or distance is invalid".to_string());
    }
    let point = POINT {
        x: bounds.x + (x * f64::from(bounds.width.saturating_sub(1))).round() as i32,
        y: bounds.y + (y * f64::from(bounds.height.saturating_sub(1))).round() as i32,
    };
    let packed = i16::try_from(point.x)
        .and_then(|x| i16::try_from(point.y).map(|y| (x as u16 as u32) | ((y as u16 as u32) << 16)))
        .map_err(|_| "The scroll position is outside Windows wheel coordinates".to_string())?;
    unsafe {
        if !IsWindow(Some(target)).as_bool() {
            return Err("The original application was closed".to_string());
        }
        let mut rect = RECT::default();
        GetWindowRect(target, &mut rect).map_err(|error| error.to_string())?;
        if point.x < rect.left
            || point.x >= rect.right
            || point.y < rect.top
            || point.y >= rect.bottom
        {
            return Err("Move the pointer over the original application to scroll".to_string());
        }
        let mut receiver = target;
        loop {
            let mut local = point;
            if !ScreenToClient(receiver, &mut local).as_bool() {
                return Err("Failed to locate the source scroll area".to_string());
            }
            let child = ChildWindowFromPointEx(
                receiver,
                local,
                CWP_SKIPDISABLED | CWP_SKIPINVISIBLE | CWP_SKIPTRANSPARENT,
            );
            if child.is_invalid() || child == receiver {
                break;
            }
            receiver = child;
        }
        log::info!(
            "Annotation wheel target: source=[{}] receiver=[{}] point=({},{})",
            window_metadata(target),
            window_metadata(receiver),
            point.x,
            point.y
        );
        for (message, delta) in [(WM_MOUSEWHEEL, delta_y), (WM_MOUSEHWHEEL, delta_x)] {
            if delta == 0 {
                continue;
            }
            if SendMessageTimeoutW(
                receiver,
                message,
                WPARAM((delta as i16 as u16 as usize) << 16),
                LPARAM(packed as isize),
                SMTO_ABORTIFHUNG | SMTO_BLOCK,
                100,
                None,
            )
            .0 == 0
            {
                return Err("The original application did not accept scrolling".to_string());
            }
        }
    }
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub async fn submit_screen_annotation(
    app: AppHandle,
    session_id: u64,
    png_data_url: String,
    point_count: usize,
    additional_png_data_urls: Option<Vec<String>>,
) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        submit_pages(
            &app,
            session_id,
            png_data_url,
            point_count,
            additional_png_data_urls,
        )
    })
    .await
    .map_err(|error| format!("Failed to render the annotated screens: {error}"))?
}

fn submit_pages(
    app: &AppHandle,
    session_id: u64,
    png_data_url: String,
    point_count: usize,
    additional_png_data_urls: Option<Vec<String>>,
) -> Result<(), String> {
    let state = app.state::<ScreenAnnotationState>();
    // Validate before decoding potentially large multi-screen output.
    {
        let guard = state.session.lock().map_err(|error| error.to_string())?;
        let session = guard.as_ref().ok_or("No screen annotation is active")?;
        if session.snapshot.session_id != session_id
            || !session.phase.can_submit()
            || session.point_starts.len() != point_count
        {
            return Err("Screen annotation session is stale".to_string());
        }
    }
    let mut pages = vec![png_data_url];
    pages.extend(additional_png_data_urls.unwrap_or_default());
    let png_bytes = join_annotation_pages(&pages)?;
    {
        let mut session = state
            .session
            .lock()
            .map_err(|_| "Failed to lock screen annotation state".to_string())?;
        let session = session
            .as_mut()
            .ok_or_else(|| "No screen annotation is active".to_string())?;
        if session.snapshot.session_id != session_id
            || !session.phase.can_submit()
            || session.point_starts.len() != point_count
        {
            return Err("Screen annotation session is stale".to_string());
        }
        session.png = png_bytes;
        session.phase = AnnotationPhase::Finalized;
    }
    state.finalized.notify_all();
    hide_and_restore(app);
    Ok(())
}

fn join_annotation_pages(pages: &[String]) -> Result<Vec<u8>, String> {
    use base64::Engine;
    use std::io::Write;

    let decoded = pages
        .iter()
        .map(|page| {
            let encoded = page
                .strip_prefix("data:image/png;base64,")
                .ok_or("Annotated screenshot is not a PNG data URL")?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|error| format!("Invalid annotated screenshot encoding: {error}"))?;
            tauri::image::Image::from_bytes(&bytes)
                .map(|image| (bytes, image.to_owned()))
                .map_err(|error| format!("Invalid annotated screenshot: {error}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let first = decoded
        .first()
        .ok_or("No annotation screenshots were submitted")?;
    let width = first.1.width();
    let height = decoded.iter().try_fold(0u32, |height, (_, image)| {
        if image.width() != width || image.height() == 0 || width == 0 {
            return Err("Annotation screenshots must have the same nonzero width");
        }
        height
            .checked_add(image.height())
            .ok_or("The annotated image is too tall")
    })?;
    if decoded.len() == 1 {
        return Ok(first.0.clone());
    }
    // Stream full-size rows instead of asking the webview for an oversized
    // canvas, whose browser-specific height limit can silently erase output.
    let mut output = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut output, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(|error| error.to_string())?;
        let mut stream = writer.stream_writer().map_err(|error| error.to_string())?;
        for (_, image) in decoded {
            stream
                .write_all(image.rgba())
                .map_err(|error| error.to_string())?;
        }
        stream.finish().map_err(|error| error.to_string())?;
    }
    Ok(output)
}

fn mark_finalization_failed(state: &ScreenAnnotationState) {
    if let Ok(mut session) = state.session.lock() {
        if let Some(session) = session.as_mut() {
            session.phase = AnnotationPhase::Failed;
        }
    }
    state.finalized.notify_all();
}

fn hide_and_restore(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(WINDOW_LABEL) {
        let _ = window.hide();
        #[cfg(target_os = "windows")]
        if let Ok(hwnd) = window.hwnd() {
            use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE};
            unsafe {
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
        }
    }

    if let Some(target_window) = app
        .state::<ScreenAnnotationState>()
        .session
        .lock()
        .ok()
        .and_then(|session| {
            session
                .as_ref()
                .filter(|s| !s.browsing)
                .map(|session| session.target_window)
        })
    {
        restore_target_window(target_window);
    }
}

#[cfg(target_os = "windows")]
fn begin_windows(app: &AppHandle, binding_id: &str) -> Result<(), String> {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

    if app.get_webview_window(WINDOW_LABEL).is_none() {
        return Err("Screen annotation window is unavailable".to_string());
    }

    let _coordinates = PhysicalPixelContext::enter()?;
    let target_window = unsafe { GetForegroundWindow().0 as isize };
    let cursor = crate::input::get_cursor_position(app)
        .ok_or_else(|| "Failed to get cursor position".to_string())?;
    // Capture follows the pointer, which may be over an inactive application.
    // Keep the foreground window separately for the existing paste destination.
    let scroll_window = source_window_at_point(POINT {
        x: cursor.0,
        y: cursor.1,
    })?
    .0 as isize;
    let bounds = active_monitor_bounds(app, cursor)?;
    let cache_dir = app
        .path()
        .app_cache_dir()
        .map_err(|error| error.to_string())?;
    begin_capture(
        app,
        binding_id,
        bounds,
        target_window,
        scroll_window,
        &cache_dir,
    )
}

#[cfg(target_os = "windows")]
fn begin_capture(
    app: &AppHandle,
    binding_id: &str,
    bounds: ScreenBounds,
    target_window: isize,
    scroll_window: isize,
    cache_dir: &Path,
) -> Result<(), String> {
    let _coordinates = PhysicalPixelContext::enter()?;
    let rgba = capture_screen(bounds)?;
    let png = encode_png(&rgba, bounds.width, bounds.height)?;
    std::fs::create_dir_all(cache_dir).map_err(|error| error.to_string())?;
    let snapshot_path = cache_dir.join(format!(
        "screen-annotation-{}.png",
        chrono::Utc::now().timestamp_millis()
    ));
    std::fs::write(&snapshot_path, &png).map_err(|error| error.to_string())?;

    let session_id = NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed);
    log::info!(
        "Annotation started: session={session_id} paste=[{}] source=[{}] bounds=({},{},{},{})",
        window_metadata(windows::Win32::Foundation::HWND(target_window as *mut _)),
        window_metadata(windows::Win32::Foundation::HWND(scroll_window as *mut _)),
        bounds.x,
        bounds.y,
        bounds.width,
        bounds.height
    );
    let snapshot = ScreenAnnotationSnapshot {
        session_id,
        capture_id: 0,
        path: snapshot_path.to_string_lossy().into_owned(),
        width: bounds.width,
        height: bounds.height,
        point_count: 1,
    };
    let reset_snapshot = snapshot.clone();

    let state = app.state::<ScreenAnnotationState>();
    if let Ok(mut session) = state.session.lock() {
        session.replace(AnnotationSession {
            snapshot,
            snapshot_paths: vec![snapshot_path],
            png,
            bounds,
            binding_id: binding_id.to_string(),
            target_window,
            scroll_window,
            phase: AnnotationPhase::Recording,
            point_starts: vec![0],
            scrolling: false,
            browsing: false,
        });
    } else {
        return Err("Failed to lock screen annotation state".to_string());
    }

    let app_handle = app.clone();
    app.run_on_main_thread(move || {
        if !is_recording_session(&app_handle, session_id) {
            return;
        }
        if let Some(window) = app_handle.get_webview_window(WINDOW_LABEL) {
            if let Err(error) = window.emit("reset-screen-annotation", reset_snapshot) {
                log::error!("Failed to reset screen annotation window: {error}");
                cancel(&app_handle);
                crate::utils::show_recording_overlay(&app_handle);
            }
        } else {
            cancel(&app_handle);
            crate::utils::show_recording_overlay(&app_handle);
        }
    })
    .map_err(|error| error.to_string())?;

    Ok(())
}

#[cfg(target_os = "windows")]
fn active_monitor_bounds(app: &AppHandle, cursor: (i32, i32)) -> Result<ScreenBounds, String> {
    let monitors = app
        .available_monitors()
        .map_err(|error| error.to_string())?;
    let monitor = monitors
        .into_iter()
        .find(|monitor| {
            let position = monitor.position();
            let size = monitor.size();
            cursor.0 >= position.x
                && cursor.0 < position.x + size.width as i32
                && cursor.1 >= position.y
                && cursor.1 < position.y + size.height as i32
        })
        .or_else(|| app.primary_monitor().ok().flatten())
        .ok_or_else(|| "No display is available".to_string())?;

    Ok(ScreenBounds {
        x: monitor.position().x,
        y: monitor.position().y,
        width: monitor.size().width,
        height: monitor.size().height,
    })
}

#[cfg(target_os = "windows")]
fn capture_screen(bounds: ScreenBounds) -> Result<Vec<u8>, String> {
    use std::ffi::c_void;
    use windows::Win32::Graphics::Gdi::{
        BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC,
        GetDIBits, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CAPTUREBLT,
        DIB_RGB_COLORS, HGDIOBJ, SRCCOPY,
    };

    let width = i32::try_from(bounds.width).map_err(|_| "Display is too wide".to_string())?;
    let height = i32::try_from(bounds.height).map_err(|_| "Display is too tall".to_string())?;

    unsafe {
        let screen_dc = GetDC(None);
        if screen_dc.is_invalid() {
            return Err("GetDC failed".to_string());
        }

        let memory_dc = CreateCompatibleDC(Some(screen_dc));
        if memory_dc.is_invalid() {
            ReleaseDC(None, screen_dc);
            return Err("CreateCompatibleDC failed".to_string());
        }

        let bitmap = CreateCompatibleBitmap(screen_dc, width, height);
        if bitmap.is_invalid() {
            let _ = DeleteDC(memory_dc);
            ReleaseDC(None, screen_dc);
            return Err("CreateCompatibleBitmap failed".to_string());
        }

        let old_object = SelectObject(memory_dc, HGDIOBJ(bitmap.0));
        let capture_result = BitBlt(
            memory_dc,
            0,
            0,
            width,
            height,
            Some(screen_dc),
            bounds.x,
            bounds.y,
            SRCCOPY | CAPTUREBLT,
        );
        SelectObject(memory_dc, old_object);

        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut pixels = vec![0u8; bounds.width as usize * bounds.height as usize * 4];

        let read_lines = if capture_result.is_ok() {
            GetDIBits(
                screen_dc,
                bitmap,
                0,
                bounds.height,
                Some(pixels.as_mut_ptr().cast::<c_void>()),
                &mut info,
                DIB_RGB_COLORS,
            )
        } else {
            0
        };

        let _ = DeleteObject(HGDIOBJ(bitmap.0));
        let _ = DeleteDC(memory_dc);
        ReleaseDC(None, screen_dc);

        capture_result.map_err(|error| format!("BitBlt failed: {error}"))?;
        if read_lines == 0 {
            return Err("GetDIBits failed".to_string());
        }

        for pixel in pixels.chunks_exact_mut(4) {
            pixel.swap(0, 2);
            pixel[3] = 255;
        }
        Ok(pixels)
    }
}

fn encode_png(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>, String> {
    let mut output = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut output, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(|error| error.to_string())?;
        writer
            .write_image_data(rgba)
            .map_err(|error| error.to_string())?;
    }
    Ok(output)
}

#[cfg(target_os = "windows")]
fn place_annotation_window(
    window: &tauri::WebviewWindow,
    bounds: ScreenBounds,
) -> Result<(), String> {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, SetForegroundWindow, SetWindowPos, HWND_TOPMOST, SWP_SHOWWINDOW,
    };

    let hwnd = window.hwnd().map_err(|error| error.to_string())?;
    window.show().map_err(|error| error.to_string())?;
    unsafe {
        SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            bounds.x,
            bounds.y,
            bounds.width as i32,
            bounds.height as i32,
            SWP_SHOWWINDOW,
        )
        .map_err(|error| error.to_string())?;
        let _ = SetForegroundWindow(hwnd);
        if GetForegroundWindow() != hwnd {
            log::warn!("Windows did not grant focus to the screen annotation window");
        } else {
            // Foreground activation alone leaves wheel input on the outer HWND.
            window
                .as_ref()
                .set_focus()
                .map_err(|error| format!("Failed to focus the annotation canvas: {error}"))?;
        }
    }
    Ok(())
}

fn restore_target_window(target_window: isize) {
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow;
        if target_window != 0
            && !unsafe { SetForegroundWindow(HWND(target_window as *mut _)) }.as_bool()
        {
            log::warn!("Windows did not restore focus to the original target window");
        }
    }

    #[cfg(not(target_os = "windows"))]
    let _ = target_window;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_session() -> AnnotationSession {
        AnnotationSession {
            snapshot: ScreenAnnotationSnapshot {
                session_id: 7,
                capture_id: 3,
                path: String::new(),
                width: 1,
                height: 1,
                point_count: 2,
            },
            snapshot_paths: Vec::new(),
            png: Vec::new(),
            bounds: ScreenBounds {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            binding_id: String::new(),
            target_window: 0,
            scroll_window: 0,
            phase: AnnotationPhase::Recording,
            point_starts: vec![0, 100],
            scrolling: false,
            browsing: false,
        }
    }

    #[test]
    fn annotation_scroll_rejects_stale_or_overlapping_requests_but_can_finish_after_release() {
        let mut session = test_session();
        assert!(validate_scroll(&session, 7, 3).is_ok());
        assert!(validate_scroll(&session, 6, 3).is_err());
        assert!(validate_scroll(&session, 7, 2).is_err());
        session.scrolling = true;
        assert!(validate_scroll(&session, 7, 3).is_err());
        assert!(scroll_can_complete(&session, 7, 3));
        session.phase = AnnotationPhase::Finalizing;
        assert!(scroll_can_complete(&session, 7, 3));
        assert!(validate_scroll(&session, 7, 3).is_err());
        assert!(!scroll_can_complete(&session, 6, 3));
        assert!(!scroll_can_complete(&session, 7, 2));
        session.phase = AnnotationPhase::Failed;
        assert!(!scroll_can_complete(&session, 7, 3));
        session.phase = AnnotationPhase::Finalized;
        assert!(!scroll_can_complete(&session, 7, 3));
        assert_eq!(session.point_starts, [0, 100]);
    }

    #[test]
    fn annotation_pages_join_without_resizing_cropping_or_losing_pixels() {
        use base64::Engine;
        let page = |rgba: &[u8], width, height| {
            format!(
                "data:image/png;base64,{}",
                base64::engine::general_purpose::STANDARD
                    .encode(encode_png(rgba, width, height).expect("encode fixture"))
            )
        };
        let first = page(&[255, 0, 0, 255].repeat(6), 3, 2);
        let second = page(&[0, 0, 255, 255].repeat(9), 3, 3);
        let combined = join_annotation_pages(&[first.clone(), second]).expect("join screens");
        let image = tauri::image::Image::from_bytes(&combined).expect("decode output");
        assert_eq!((image.width(), image.height()), (3, 5));
        assert_eq!(
            image.rgba(),
            &[
                &[255, 0, 0, 255].repeat(6)[..],
                &[0, 0, 255, 255].repeat(9)[..]
            ]
            .concat()
        );
        assert_eq!(
            join_annotation_pages(std::slice::from_ref(&first)).expect("single screen"),
            base64::engine::general_purpose::STANDARD
                .decode(first.split(',').nth(1).expect("payload"))
                .expect("png")
        );
        assert!(join_annotation_pages(&[]).is_err());
        assert!(join_annotation_pages(&[first.clone(), page(&[0; 8], 2, 1)]).is_err());
        assert!(join_annotation_pages(&[first, "data:image/png;base64,bad".to_string()]).is_err());
    }

    #[test]
    fn annotation_capture_cleanup_keeps_all_views_until_the_session_ends() {
        let directory = tempfile::tempdir().expect("directory");
        let paths: Vec<_> = (0..3)
            .map(|index| directory.path().join(format!("{index}.png")))
            .collect();
        for path in &paths {
            std::fs::write(path, b"synthetic capture").expect("write fixture");
        }
        let mut session = test_session();
        session.snapshot_paths = paths.clone();
        assert!(paths.iter().all(|path| path.exists()));
        drop(session);
        assert!(paths.iter().all(|path| !path.exists()));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn annotation_scroll_delivers_only_wheel_messages_to_a_synthetic_window_without_modifiers() {
        use windows::core::w;
        use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::HiDpi::{
            AreDpiAwarenessContextsEqual, GetThreadDpiAwarenessContext,
            SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            DPI_AWARENESS_CONTEXT_UNAWARE,
        };
        use windows::Win32::UI::WindowsAndMessaging::*;
        static MESSAGES: Mutex<Vec<(u32, usize, isize)>> = Mutex::new(Vec::new());
        unsafe extern "system" fn procedure(
            hwnd: HWND,
            message: u32,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT {
            if matches!(message, WM_MOUSEWHEEL | WM_MOUSEHWHEEL) {
                MESSAGES
                    .lock()
                    .expect("messages")
                    .push((message, wparam.0, lparam.0));
                LRESULT(0)
            } else {
                unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
            }
        }
        unsafe {
            let original_dpi =
                SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            assert!(!original_dpi.0.is_null());
            let instance = GetModuleHandleW(None).expect("module");
            let class = WNDCLASSW {
                lpfnWndProc: Some(procedure),
                hInstance: instance.into(),
                lpszClassName: w!("HandyAnnotationScrollTest"),
                ..Default::default()
            };
            assert_ne!(RegisterClassW(&class), 0);
            let target = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                class.lpszClassName,
                w!("Synthetic scroll fixture"),
                WS_OVERLAPPEDWINDOW,
                100,
                100,
                500,
                500,
                None,
                None,
                Some(instance.into()),
                None,
            )
            .expect("hidden synthetic window");
            let foreground = GetForegroundWindow();
            ensure_capture_exclusion_supported().expect("Windows supports capture exclusion");
            SetWindowDisplayAffinity(target, WDA_EXCLUDEFROMCAPTURE)
                .expect("exclude only the synthetic overlay");
            let mut affinity = 0;
            GetWindowDisplayAffinity(target, &mut affinity).expect("read capture exclusion");
            assert_eq!(affinity, WDA_EXCLUDEFROMCAPTURE.0);
            assert_eq!(
                GetForegroundWindow(),
                foreground,
                "capture exclusion never moves focus"
            );
            SetWindowDisplayAffinity(target, WDA_NONE).expect("restore fixture capture visibility");
            let bounds = ScreenBounds {
                x: 100,
                y: 100,
                width: 500,
                height: 500,
            };
            let previous_dpi = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_UNAWARE);
            assert!(!previous_dpi.0.is_null());
            let delivered = scroll_target(target, bounds, 120, -240, 0.9, 0.9);
            let invalid_distance = scroll_target(target, bounds, 0, 32768, 0.5, 0.5);
            let invalid_position = scroll_target(target, bounds, 0, 120, f64::NAN, 0.5);
            let restored_dpi = AreDpiAwarenessContextsEqual(
                GetThreadDpiAwarenessContext(),
                DPI_AWARENESS_CONTEXT_UNAWARE,
            );
            SetThreadDpiAwarenessContext(original_dpi);
            delivered.expect("deliver physical-pixel wheel input from a DPI-unaware worker");
            assert!(invalid_distance.is_err());
            assert!(invalid_position.is_err());
            assert!(restored_dpi.as_bool(), "restore the worker's DPI context");
            DestroyWindow(target).expect("destroy fixture");
            assert!(scroll_target(target, bounds, 0, 120, 0.5, 0.5).is_err());
            UnregisterClassW(class.lpszClassName, Some(instance.into()))
                .expect("remove fixture class");
        }
        let messages = MESSAGES.lock().expect("messages");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].0, WM_MOUSEWHEEL);
        assert_eq!(messages[1].0, WM_MOUSEHWHEEL);
        assert_eq!((messages[0].1 >> 16) as u16 as i16, -240);
        assert_eq!((messages[1].1 >> 16) as u16 as i16, 120);
        for (_, modifiers_and_delta, position) in messages.iter() {
            assert_eq!(
                modifiers_and_delta & 0xffff,
                0,
                "Alt/Ctrl/Space are not forwarded"
            );
            assert_eq!(*position as u16 as i16, 549);
            assert_eq!((*position >> 16) as u16 as i16, 549);
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn annotation_recapture_reads_only_its_synthetic_source_not_the_opaque_overlay() {
        use windows::core::{w, PCWSTR};
        use windows::Win32::Foundation::{
            COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM,
        };
        use windows::Win32::Graphics::Gdi::{CreateSolidBrush, DeleteObject, UpdateWindow, HBRUSH};
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::HiDpi::{
            SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT,
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        };
        use windows::Win32::UI::WindowsAndMessaging::*;

        struct Fixture {
            windows: Vec<HWND>,
            classes: Vec<PCWSTR>,
            brushes: Vec<HBRUSH>,
            instance: HINSTANCE,
            dpi: DPI_AWARENESS_CONTEXT,
        }
        impl Drop for Fixture {
            fn drop(&mut self) {
                unsafe {
                    for window in self.windows.iter().rev() {
                        let _ = DestroyWindow(*window);
                    }
                    for class in &self.classes {
                        let _ = UnregisterClassW(*class, Some(self.instance));
                    }
                    for brush in &self.brushes {
                        let _ = DeleteObject((*brush).into());
                    }
                    SetThreadDpiAwarenessContext(self.dpi);
                }
            }
        }
        unsafe extern "system" fn procedure(
            hwnd: HWND,
            message: u32,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT {
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
        unsafe {
            let foreground = GetForegroundWindow();
            let dpi = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            assert!(!dpi.0.is_null(), "use physical pixels only");
            let mut fixture = Fixture {
                windows: Vec::new(),
                classes: Vec::new(),
                brushes: Vec::new(),
                instance: GetModuleHandleW(None).expect("module").into(),
                dpi,
            };
            for (name, color) in [
                (w!("HandySyntheticCaptureSource"), COLORREF(0x0000ff)),
                (w!("HandySyntheticCaptureOverlay"), COLORREF(0x00ff00)),
            ] {
                let brush = CreateSolidBrush(color);
                fixture.brushes.push(brush);
                let class = WNDCLASSW {
                    lpfnWndProc: Some(procedure),
                    hInstance: fixture.instance,
                    lpszClassName: name,
                    hbrBackground: brush,
                    ..Default::default()
                };
                assert_ne!(RegisterClassW(&class), 0);
                fixture.classes.push(name);
                let window = CreateWindowExW(
                    WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                    name,
                    w!("Synthetic pixels only"),
                    WS_POPUP,
                    100,
                    100,
                    96,
                    96,
                    None,
                    None,
                    Some(fixture.instance),
                    None,
                )
                .expect("create synthetic window");
                fixture.windows.push(window);
                SetWindowPos(
                    window,
                    Some(HWND_TOPMOST),
                    100,
                    100,
                    96,
                    96,
                    SWP_NOACTIVATE | SWP_SHOWWINDOW,
                )
                .expect("show synthetic colors without focus");
                assert!(UpdateWindow(window).as_bool());
                windows::Win32::Graphics::Dwm::DwmFlush().expect("compose the owned fixture");
                assert_ne!(window, foreground, "the source does not need focus");
                let selected = source_window_at_point(POINT { x: 148, y: 148 })
                    .expect("select source under pointer");
                let mut rect = windows::Win32::Foundation::RECT::default();
                GetWindowRect(window, &mut rect).expect("fixture bounds");
                eprintln!(
                    "Owned capture fixture: [{}] visible={} rect={rect:?}; selected=[{}]",
                    window_metadata(window),
                    IsWindowVisible(window).as_bool(),
                    window_metadata(selected)
                );
                assert_eq!(
                    selected, window,
                    "scroll the captured source, not the unrelated foreground window"
                );
            }
            windows::Win32::Graphics::Dwm::DwmFlush().expect("compose synthetic windows");
            let overlay = fixture.windows[1];
            // Read only a 2x2 patch entirely covered by both owned, opaque test
            // windows. Never capture the desktop or any user application's data.
            for (x, y) in [(148, 148), (149, 148), (148, 149), (149, 149)] {
                assert_eq!(
                    WindowFromPoint(POINT { x, y }),
                    overlay,
                    "fixture must fully cover the sample"
                );
            }
            let bounds = ScreenBounds {
                x: 148,
                y: 148,
                width: 2,
                height: 2,
            };
            let visible = capture_screen(bounds).expect("sample owned overlay");
            assert_eq!(visible, [0, 255, 0, 255].repeat(4));
            let behind = capture_behind_annotation(overlay, bounds)
                .expect("capture owned source behind overlay");
            assert_eq!(behind, [255, 0, 0, 255].repeat(4));
            let mut affinity = u32::MAX;
            GetWindowDisplayAffinity(overlay, &mut affinity).expect("read restored affinity");
            assert_eq!(affinity, WDA_NONE.0);
            assert_eq!(
                GetForegroundWindow(),
                foreground,
                "sampling never moves focus"
            );
        }
    }

    #[test]
    fn numbered_annotation_keeps_silent_points_and_retry_boundaries() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("recording.wav");
        let starts = [0, 2, 4, 4, 6];
        save_audio_segments(&path, &starts).expect("save boundaries");
        let restored = read_audio_segments(&path)
            .expect("read boundaries")
            .expect("numbered recording");
        assert_eq!(restored, starts);
        let samples = [0.1, 0.2, 0.0, 0.0, 0.3, 0.4];
        let mut calls = Vec::new();
        let text = transcribe_numbered_segments(&samples, &restored, |audio| {
            calls.push(audio);
            Ok(if calls.len() == 1 { "first" } else { "second" }.to_string())
        })
        .expect("transcribe");
        assert_eq!(calls, vec![vec![0.1, 0.2], vec![0.3, 0.4]]);
        assert_eq!(text, "1. first\n2. \n3. \n4. second\n5. ");
        for invalid in [vec![], vec![1], vec![0, 4, 3], vec![0, 7]] {
            assert!(
                transcribe_numbered_segments(&samples, &invalid, |_| Ok(String::new())).is_err()
            );
        }
        let failure =
            transcribe_numbered_segments(&samples, &[0, 2], |_| anyhow::bail!("model failed"))
                .expect_err("model error");
        assert!(failure.to_string().contains("point 1"));
        remove_audio_segments(&path);
        assert!(read_audio_segments(&path)
            .expect("missing boundaries")
            .is_none());
    }

    #[test]
    fn numbered_annotation_rejects_stale_or_finalizing_point_changes() {
        let mut session = AnnotationSession {
            snapshot: ScreenAnnotationSnapshot {
                session_id: 7,
                capture_id: 0,
                path: String::new(),
                width: 1,
                height: 1,
                point_count: 2,
            },
            snapshot_paths: Vec::new(),
            png: Vec::new(),
            bounds: ScreenBounds {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            binding_id: String::new(),
            target_window: 0,
            scroll_window: 0,
            phase: AnnotationPhase::Recording,
            point_starts: vec![0, 100],
            scrolling: false,
            browsing: false,
        };
        assert!(validate_point_transition(&session, 7, 2).is_ok());
        assert!(validate_point_transition(&session, 6, 2).is_err());
        assert!(validate_point_transition(&session, 7, 1).is_err());
        session.phase = AnnotationPhase::Finalizing;
        assert!(validate_point_transition(&session, 7, 2).is_err());
    }

    #[test]
    fn only_recording_sessions_can_show_the_overlay() {
        assert!(AnnotationPhase::Recording.can_show());
        assert!(!AnnotationPhase::Finalizing.can_show());
        assert!(!AnnotationPhase::Finalized.can_show());
        assert!(!AnnotationPhase::Failed.can_show());
    }

    #[test]
    fn only_finalizing_sessions_wait_for_canvas_submission() {
        assert!(!AnnotationPhase::Recording.should_wait());
        assert!(AnnotationPhase::Finalizing.should_wait());
        assert!(!AnnotationPhase::Finalized.should_wait());
        assert!(!AnnotationPhase::Failed.should_wait());
    }

    #[test]
    fn only_finalizing_sessions_accept_canvas_submission() {
        assert!(!AnnotationPhase::Recording.can_submit());
        assert!(AnnotationPhase::Finalizing.can_submit());
        assert!(!AnnotationPhase::Finalized.can_submit());
        assert!(!AnnotationPhase::Failed.can_submit());
    }
}
