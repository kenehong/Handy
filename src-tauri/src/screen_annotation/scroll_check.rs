use super::*;
use serde_json::{json, Value};
use std::sync::mpsc::{self, Receiver};
use std::time::Instant;
use tauri::{LogicalPosition, LogicalSize, WebviewUrl};
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetFocus, GetKeyState, GetKeyboardState, SetKeyboardState, VK_LMENU, VK_MENU, VK_SPACE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetAncestor, GetForegroundWindow, GetWindowRect, IsWindowVisible, SetWindowPos,
    WindowFromPoint, GA_ROOT, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL,
};

const REPORT_SCRIPT: &str = r#"
(() => {
  const report = data => fetch('/__report?data=' + encodeURIComponent(JSON.stringify(data)));
  document.addEventListener('wheel', e => report({
    role: 'wheel', alt: e.altKey, ctrl: e.ctrlKey, delta: e.deltaY
  }), {capture: true});
  window.annotationCheckFrame = () => {
    const canvas = document.querySelector('canvas');
    if (!canvas || canvas.width < 2) return;
    const pixels = canvas.getContext('2d').getImageData(0, 0, canvas.width, canvas.height).data;
    let hash = 2166136261;
    let ink = 0;
    for (let i = 0; i < pixels.length; i += 4)
    {
      hash = Math.imul(hash ^ pixels[i] ^ (pixels[i + 1] << 8) ^ (pixels[i + 2] << 16), 16777619);
      if (pixels[i] === 255 && pixels[i + 1] === 45 && pixels[i + 2] === 85) ink++;
    }
    const x = Math.floor(canvas.width * 3 / 4);
    let markerY = -1;
    for (let y = 0; y < canvas.height; y++) {
      const i = (y * canvas.width + x) * 4;
      if (pixels[i] >= 180 && pixels[i] <= 235 && pixels[i + 1] <= 20 && pixels[i + 2] >= 230) {
        markerY = y; break;
      }
    }
    report({role: 'frame', hash, markerY, ink, time: Date.now(),
      browsing: document.querySelector('main').classList.contains('annotation-browsing'),
      busy: document.querySelector('main').getAttribute('aria-busy')});
  };
  report({role: 'instrumented'});
})();
"#;

const SOURCE: &[u8] = br#"<!doctype html><html><body style="margin:0;background:white">
<div style="height:3000px;background:#143aa9">
<h1>Synthetic source only</h1></div>
<div id="marker" style="position:absolute;left:0;top:220px;width:100%;height:8px;background:rgb(212,0,255)"></div>
<output style="position:fixed;top:80px;background:white"></output>
<script>
const pane = location.pathname.endsWith('right') ? 'right' : 'left';
const report = (settled = false) => {
 document.querySelector('output').textContent = pane + ' ' + Math.round(scrollY);
 fetch('/__report?data=' + encodeURIComponent(JSON.stringify({
   role: 'source', pane, offset: Math.round(scrollY), time: Date.now(), dpr: devicePixelRatio,
   visibility: document.visibilityState, width: innerWidth, height: innerHeight,
   markerTop: document.querySelector('#marker').getBoundingClientRect().top, settled
 })));
};
addEventListener('load', () => report());
addEventListener('scroll', () => report());
addEventListener('scrollend', () => report(true));
addEventListener('pointerdown', () => fetch('/__report?data=' + encodeURIComponent(JSON.stringify({
 role: 'source-press', pane
}))));
</script></body></html>"#;

fn captured_marker_y(png: &[u8]) -> Result<i64, String> {
    let mut reader = png::Decoder::new(std::io::Cursor::new(png))
        .read_info()
        .map_err(|error| error.to_string())?;
    let mut pixels = vec![0; reader.output_buffer_size()];
    let info = reader
        .next_frame(&mut pixels)
        .map_err(|error| error.to_string())?;
    if info.color_type != png::ColorType::Rgba {
        return Err("The synthetic capture was not RGBA".into());
    }
    let x = info.width as usize * 3 / 4;
    (0..info.height as usize)
        .find(|y| {
            let i = (y * info.width as usize + x) * 4;
            (180..=235).contains(&pixels[i]) && pixels[i + 1] <= 20 && pixels[i + 2] >= 230
        })
        .map(|y| y as i64)
        .ok_or("The known synthetic marker is absent from the capture".into())
}

fn on_main<T: Send + 'static>(
    app: &AppHandle,
    action: impl FnOnce(AppHandle) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let (sender, receiver) = mpsc::sync_channel(1);
    let handle = app.clone();
    app.run_on_main_thread(move || {
        let _ = sender.send(action(handle));
    })
    .map_err(|error| error.to_string())?;
    receiver
        .recv_timeout(Duration::from_secs(8))
        .map_err(|error| error.to_string())?
}

fn wait_report(
    receiver: &Receiver<Value>,
    reports: &mut Vec<Value>,
    description: &str,
    predicate: impl Fn(&Value) -> bool,
) -> Result<Value, String> {
    if let Some(report) = reports.iter().find(|report| predicate(report)) {
        return Ok(report.clone());
    }
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let report = receiver
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|error| format!("{description}: {error}"))?;
        eprintln!("annotation check: {report}");
        let matched = predicate(&report);
        reports.push(report.clone());
        if matched {
            return Ok(report);
        }
    }
}

pub fn run(directory: &Path) -> Result<(), String> {
    if let Ok(window) = unsafe {
        windows::Win32::UI::WindowsAndMessaging::FindWindowW(
            None,
            windows::core::w!("Screen annotation"),
        )
    } {
        if unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindowVisible(window) }.as_bool() {
            return Err("An annotation window is already visible; leaving it untouched".into());
        }
    }
    let profile = directory.join("webview");
    std::fs::create_dir_all(&profile).map_err(|error| error.to_string())?;
    let previous_profile = std::env::var_os("WEBVIEW2_USER_DATA_FOLDER");
    std::env::set_var("WEBVIEW2_USER_DATA_FOLDER", &profile);
    let dist = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("dist");
    let cache = directory.join("captures");
    let (report_sender, report_receiver) = mpsc::channel();
    let command_reports = report_sender.clone();
    let (source_sender, source_receiver) = mpsc::channel();
    let held_keys = std::sync::Arc::new(Mutex::new(None::<[u8; 256]>));
    let cleanup_keys = held_keys.clone();
    let mut context = tauri::generate_context!();
    context.config_mut().app.windows.clear();
    context.config_mut().identifier = "computer.handy.annotation-scroll-check".into();
    context.config_mut().build.dev_url = Some(
        "http://annotation-fixture.localhost/"
            .parse()
            .map_err(|error| format!("{error}"))?,
    );
    let commands: fn(tauri::ipc::Invoke<tauri::Wry>) -> bool = tauri::generate_handler![
        get_screen_annotation_snapshot,
        show_screen_annotation,
        scroll_screen_annotation,
        freeze_screen_annotation,
        submit_screen_annotation
    ];
    let app = tauri::Builder::default()
        .any_thread()
        .plugin(
            tauri_plugin_log::Builder::new()
                .clear_targets()
                .target(tauri_plugin_log::Target::new(
                    tauri_plugin_log::TargetKind::Stderr,
                ))
                .level(log::LevelFilter::Info)
                .build(),
        )
        .invoke_handler(move |invoke| {
            if invoke.message.command() == "get_app_settings" {
                invoke.resolver.resolve(json!({"app_language": "en"}));
                return true;
            }
            if invoke.message.command() == "get_screen_annotation_snapshot"
                && invoke
                    .message
                    .webview()
                    .state::<ScreenAnnotationState>()
                    .session
                    .lock()
                    .unwrap()
                    .is_none()
            {
                let _ = command_reports.send(json!({"role": "overlay-ready"}));
            }
            let showing = invoke.message.command() == "show_screen_annotation";
            let handled = commands(invoke);
            if showing {
                let _ = command_reports.send(json!({"role": "overlay-shown"}));
            }
            handled
        })
        .register_uri_scheme_protocol("annotation-fixture", move |_, request| {
            let path = request.uri().path();
            if path == "/__report" {
                let url = tauri::Url::parse(&request.uri().to_string()).expect("fixture URL");
                let data = url
                    .query_pairs()
                    .find(|(key, _)| key == "data")
                    .expect("report");
                let value = serde_json::from_str(&data.1).expect("fixture report");
                let _ = report_sender.send(value);
            }
            let (body, content_type) = if path.starts_with("/fixture-") {
                (SOURCE.to_vec(), "text/html")
            } else if path == "/__report" || path == "/favicon.ico" {
                (Vec::new(), "text/plain")
            } else {
                assert!(!path.contains(".."), "fixture assets stay inside dist");
                let file = dist.join(path.trim_start_matches('/'));
                let mime = match file.extension().and_then(|value| value.to_str()) {
                    Some("js") => "text/javascript",
                    Some("css") => "text/css",
                    Some("html") => "text/html",
                    _ => "application/octet-stream",
                };
                match std::fs::read(&file) {
                    Ok(bytes) => (bytes, mime),
                    Err(error) => {
                        eprintln!("Fixture asset failure at {}: {error}", file.display());
                        return tauri::http::Response::builder()
                            .status(404)
                            .body(error.to_string().into_bytes())
                            .expect("asset failure response");
                    }
                }
            };
            tauri::http::Response::builder()
                .header("Content-Type", content_type)
                .body(body)
                .expect("fixture response")
        })
        .on_page_load(|webview, payload| {
            if webview.label() == WINDOW_LABEL
                && matches!(payload.event(), tauri::webview::PageLoadEvent::Finished)
            {
                webview
                    .eval(REPORT_SCRIPT)
                    .expect("observe the real annotation canvas");
            }
        })
        .setup(move |app| {
            initialize(app.handle());
            let source = tauri::window::WindowBuilder::new(app, "source_host")
                .title("Synthetic multi-webview source")
                .inner_size(640.0, 320.0)
                .decorations(false)
                .shadow(false)
                .skip_taskbar(true)
                .focused(false)
                .visible(false)
                .build()?;
            source.show()?;
            let _coordinates = PhysicalPixelContext::enter()?;
            let hwnd = source.hwnd()?;
            let original = unsafe { GetForegroundWindow() }.0 as isize;
            unsafe {
                SetWindowPos(
                    hwnd,
                    Some(HWND_TOPMOST),
                    250,
                    250,
                    0,
                    0,
                    SWP_NOACTIVATE | SWP_NOSIZE,
                )?;
                let mut rect = RECT::default();
                GetWindowRect(hwnd, &mut rect)?;
                let mut client = RECT::default();
                windows::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut client)?;
                eprintln!(
                    "Source host: [{}] rect={rect:?} client={client:?}",
                    window_metadata(hwnd)
                );
                source_sender.send((
                    hwnd.0 as isize,
                    original,
                    ScreenBounds {
                        x: rect.left + 8,
                        y: rect.top + 8,
                        width: (rect.right - rect.left - 16) as u32,
                        height: (rect.bottom - rect.top - 16) as u32,
                    },
                ))?;
            }
            Ok(())
        })
        .build(context)
        .map_err(|error| error.to_string())?;
    let handle = app.handle().clone();
    let worker = std::thread::spawn(move || {
        let mut original = 0;
        let mut source = 0;
        let result = (|| {
            let (source_window, original_window, bounds) = source_receiver
                .recv_timeout(Duration::from_secs(10))
                .map_err(|error| error.to_string())?;
            original = original_window;
            source = source_window;
            let mut reports = Vec::new();
            on_main(&handle, |app| {
                app.get_window("source_host")
                    .ok_or("Source disappeared")?
                    .add_child(
                        tauri::webview::WebviewBuilder::new(
                            "left",
                            WebviewUrl::App("fixture-left".into()),
                        )
                        .focused(false),
                        LogicalPosition::new(0.0, 0.0),
                        LogicalSize::new(320.0, 320.0),
                    )
                    .map_err(|error| error.to_string())?;
                Ok(())
            })?;
            wait_report(&report_receiver, &mut reports, "left source ready", |r| {
                r["role"] == "source" && r["pane"] == "left" && r["offset"] == 0
            })?;
            on_main(&handle, |app| {
                app.get_window("source_host")
                    .ok_or("Source disappeared")?
                    .add_child(
                        tauri::webview::WebviewBuilder::new(
                            "right",
                            WebviewUrl::App("fixture-right".into()),
                        )
                        .focused(false),
                        LogicalPosition::new(320.0, 0.0),
                        LogicalSize::new(320.0, 320.0),
                    )
                    .map_err(|error| error.to_string())?;
                Ok(())
            })?;
            wait_report(&report_receiver, &mut reports, "right source ready", |r| {
                r["role"] == "source" && r["pane"] == "right" && r["offset"] == 0
            })?;
            on_main(&handle, |app| {
                create_window(&app);
                Ok(())
            })?;
            for role in ["overlay-ready", "instrumented"] {
                wait_report(&report_receiver, &mut reports, role, |r| r["role"] == role)?;
            }
            on_main(&handle, move |app| {
                let _coordinates = PhysicalPixelContext::enter()?;
                let source = HWND(source_window as *mut _);
                for label in ["left", "right"] {
                    app.get_webview(label)
                        .ok_or("Source child disappeared")?
                        .with_webview(move |webview| {
                            let mut visible = Default::default();
                            unsafe { webview.controller().IsVisible(&mut visible) }
                                .expect("controller visibility");
                            eprintln!(
                                "Source child {label} controller visible={}",
                                visible.as_bool()
                            );
                        })
                        .map_err(|error| error.to_string())?;
                }
                let foreground = unsafe { GetForegroundWindow() };
                if foreground.0 as isize != original_window && foreground != source {
                    return Err(format!(
                        "The paste destination changed; refusing the synthetic check: original=[{}] current=[{}]",
                        window_metadata(HWND(original_window as *mut _)), window_metadata(foreground)
                    ));
                }
                for x in [bounds.x, bounds.x + bounds.width as i32 - 1] {
                    for y in [bounds.y, bounds.y + bounds.height as i32 - 1] {
                        let owner =
                            unsafe { GetAncestor(WindowFromPoint(POINT { x, y }), GA_ROOT) };
                        if owner != source {
                            return Err(format!(
                                "Synthetic source does not cover ({x},{y}); refusing capture: source=[{}] owner=[{}]",
                                window_metadata(source), window_metadata(owner)
                            ));
                        }
                    }
                }
                let selected = source_window_at_point(POINT {
                    x: bounds.x + bounds.width as i32 * 3 / 4,
                    y: bounds.y + bounds.height as i32 / 2,
                })?;
                begin_capture(
                    &app,
                    "transcribe_with_annotation",
                    bounds,
                    original_window,
                    selected.0 as isize,
                    &cache,
                )
            })?;
            wait_report(
                &report_receiver,
                &mut reports,
                "actual overlay shown",
                |r| r["role"] == "overlay-shown",
            )?;
            on_main(&handle, |app| {
                app.get_webview_window(WINDOW_LABEL)
                    .unwrap()
                    .eval("window.annotationCheckFrame(); setInterval(window.annotationCheckFrame, 100)")
                    .map_err(|error| error.to_string())
            })?;
            let first = wait_report(&report_receiver, &mut reports, "first canvas", |r| {
                r["role"] == "frame" && r["busy"] == "false"
            })?;
            let first_marker = {
                let state = handle.state::<ScreenAnnotationState>();
                let session = state.session.lock().map_err(|error| error.to_string())?;
                let session = session.as_ref().ok_or("No synthetic capture")?;
                let mut reader = png::Decoder::new(std::io::Cursor::new(&session.png))
                    .read_info()
                    .map_err(|e| e.to_string())?;
                let mut pixels = vec![0; reader.output_buffer_size()];
                reader.next_frame(&mut pixels).map_err(|e| e.to_string())?;
                let mut colors = std::collections::BTreeMap::new();
                for pixel in pixels.chunks_exact(4) {
                    *colors
                        .entry([pixel[0], pixel[1], pixel[2]])
                        .or_insert(0_u32) += 1;
                }
                let mut colors = colors.into_iter().collect::<Vec<_>>();
                colors.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
                eprintln!(
                    "Owned source capture palette: {:?}",
                    &colors[..colors.len().min(5)]
                );
                captured_marker_y(&session.png)?
            };
            if first["markerY"].as_i64() != Some(first_marker) {
                return Err(format!(
                    "Initial native marker {first_marker} does not match canvas {first}"
                ));
            }
            on_main(&handle, move |app| {
                let overlay = app
                    .get_webview_window(WINDOW_LABEL)
                    .ok_or("Overlay disappeared")?
                    .hwnd()
                    .map_err(|error| error.to_string())?;
                app.get_webview_window(WINDOW_LABEL)
                    .ok_or("Overlay disappeared")?
                    .with_webview(|webview| {
                        let mut visible = Default::default();
                        match unsafe { webview.controller().IsVisible(&mut visible) } {
                            Ok(()) => {
                                eprintln!("Actual overlay controller visible={}", visible.as_bool())
                            }
                            Err(error) => {
                                eprintln!("Cannot inspect controller visibility: {error}")
                            }
                        }
                    })
                    .map_err(|error| error.to_string())?;
                unsafe {
                    let focus = GetFocus();
                    eprintln!(
                        "Actual overlay: {}; focus: {}",
                        window_metadata(overlay),
                        window_metadata(focus)
                    );
                    let mut keys = [0; 256];
                    GetKeyboardState(&mut keys).map_err(|error| error.to_string())?;
                    *held_keys.lock().unwrap() = Some(keys);
                    for key in [VK_MENU, VK_LMENU, VK_SPACE] {
                        keys[key.0 as usize] = 0x80;
                    }
                    SetKeyboardState(&keys).map_err(|error| error.to_string())?;
                    eprintln!(
                        "Thread-local held state: Alt={} Space={}",
                        GetKeyState(VK_MENU.0 as i32) < 0,
                        GetKeyState(VK_SPACE.0 as i32) < 0
                    );
                }
                app.get_webview_window(WINDOW_LABEL).ok_or("Overlay disappeared")?
                    .eval("document.querySelector('canvas').dispatchEvent(new WheelEvent('wheel', {clientX: innerWidth * .75, clientY: innerHeight * .5, deltaY: 100, altKey: true, bubbles: true, cancelable: true}))")
                    .map_err(|error| error.to_string())?;
                Ok(())
            })?;
            let wheel = wait_report(&report_receiver, &mut reports, "overlay DOM wheel", |r| {
                r["role"] == "wheel"
            })?;
            if wheel["alt"] != true {
                return Err("The overlay wheel lost its held Alt state".into());
            }
            let first_scroll = wait_report(
                &report_receiver,
                &mut reports,
                "right embedded source actually scrolled",
                |r| {
                    r["role"] == "source"
                        && r["pane"] == "right"
                        && r["settled"] == true
                        && r["offset"].as_i64().is_some_and(|offset| offset > 0)
                },
            )?;
            on_main(&handle, move |_| {
                if !browse_input::check_input(
                    WM_MOUSEWHEEL,
                    bounds.x + bounds.width as i32 * 3 / 4,
                    bounds.y + bounds.height as i32 / 2,
                    -120,
                ) {
                    return Err("The live native wheel was not intercepted".into());
                }
                Ok(())
            })?;
            let scrolled = wait_report(
                &report_receiver,
                &mut reports,
                "second wheel through the installed native hook",
                |r| {
                    r["role"] == "source"
                        && r["pane"] == "right"
                        && r["settled"] == true
                        && r["offset"].as_i64() > first_scroll["offset"].as_i64()
                },
            )?;
            wait_report(
                &report_receiver,
                &mut reports,
                "live browse canvas cleared",
                |r| {
                    r["role"] == "frame"
                        && r["browsing"] == true
                        && r["busy"] == "false"
                        && r["markerY"] == -1
                },
            )?;
            let live_marker = on_main(&handle, move |app| {
                let _coordinates = PhysicalPixelContext::enter()?;
                let window = app
                    .get_webview_window(WINDOW_LABEL)
                    .ok_or("Overlay disappeared")?;
                let overlay = window.hwnd().map_err(|e| e.to_string())?;
                // Keep the opaque owned source above other apps for sampling.
                unsafe {
                    SetWindowPos(
                        HWND(source_window as *mut _),
                        Some(overlay),
                        0,
                        0,
                        0,
                        0,
                        SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE,
                    )
                    .map_err(|e| e.to_string())?;
                    windows::Win32::Graphics::Dwm::DwmFlush().map_err(|e| e.to_string())?;
                }
                for x in [bounds.x, bounds.x + bounds.width as i32 - 1] {
                    for y in [bounds.y, bounds.y + bounds.height as i32 - 1] {
                        if unsafe { GetAncestor(WindowFromPoint(POINT { x, y }), GA_ROOT) }
                            != HWND(source_window as *mut _)
                        {
                            return Err("The owned source no longer covers the sample".into());
                        }
                    }
                }
                captured_marker_y(&encode_png(
                    &capture_screen(bounds)?,
                    bounds.width,
                    bounds.height,
                )?)
            })?;
            if live_marker >= first_marker
                || first_marker - live_marker
                    != (scrolled["offset"].as_f64().ok_or("No source offset")?
                        * scrolled["dpr"].as_f64().ok_or("No source scale")?)
                    .round() as i64
            {
                return Err(format!("Live desktop marker {first_marker} -> {live_marker} disagrees with source {scrolled}"));
            }
            eprintln!("Live browse: source offset {}; visible desktop marker {first_marker} -> {live_marker}; overlay hidden", scrolled["offset"]);
            on_main(&handle, move |app| {
                let _coordinates = PhysicalPixelContext::enter()?;
                let overlay = app
                    .get_webview_window(WINDOW_LABEL)
                    .ok_or("Overlay disappeared")?
                    .hwnd()
                    .map_err(|e| e.to_string())?;
                if unsafe { IsWindowVisible(overlay) }.as_bool() {
                    return Err("Browse did not release the visible overlay".into());
                }
                let point = POINT {
                    x: bounds.x + bounds.width as i32 * 3 / 4,
                    y: bounds.y + bounds.height as i32 * 4 / 5,
                };
                for (message, dx, dy) in [(WM_LBUTTONDOWN, 0, 0), (WM_MOUSEMOVE, 30, 15)] {
                    if !browse_input::check_input(message, point.x + dx, point.y + dy, 0) {
                        return Err(
                            "The installed native hook did not swallow the first gesture".into(),
                        );
                    }
                }
                Ok(())
            })?;
            wait_report(
                &report_receiver,
                &mut reports,
                "first native press freezes and draws",
                |r| {
                    r["role"] == "frame"
                        && r["browsing"] == false
                        && r["busy"] == "false"
                        && r["ink"].as_u64().is_some_and(|ink| ink > 0)
                        && r["markerY"]
                            .as_i64()
                            .is_some_and(|y| y >= 0 && y != first_marker)
                },
            )?;
            on_main(&handle, move |app| {
                let x = bounds.x + bounds.width as i32 * 3 / 4 + 30;
                let y = bounds.y + bounds.height as i32 * 4 / 5 + 15;
                if !browse_input::check_input(WM_MOUSEMOVE, x, y, 0)
                    || !browse_input::check_input(WM_LBUTTONUP, x, y, 0)
                    || browse_input::check_input(WM_MOUSEMOVE, x, y, 0)
                {
                    return Err(
                        "The held first gesture was not protected through overlay display".into(),
                    );
                }
                browse_input::start(&app, 1, bounds)?;
                if !browse_input::check_input(WM_LBUTTONDOWN, x, y, 0) {
                    return Err("Drain check did not intercept down".into());
                }
                browse_input::stop();
                if !browse_input::check_input(WM_LBUTTONUP, x, y, 0)
                    || browse_input::check_input(WM_MOUSEMOVE, x, y, 0)
                {
                    return Err("Cancel did not drain its swallowed down before unhooking".into());
                }
                Ok(())
            })?;
            if reports.iter().any(|r| {
                r["role"] == "source"
                    && r["pane"] == "left"
                    && r["offset"].as_i64().is_some_and(|offset| offset > 0)
            }) {
                return Err("The wrong embedded webview was scrolled".into());
            }
            if reports.iter().any(|r| r["role"] == "source-press") {
                return Err("The first drawing press leaked into a source webview".into());
            }
            let state = handle.state::<ScreenAnnotationState>();
            let session = state.session.lock().unwrap();
            let session = session
                .as_ref()
                .ok_or("The annotation unexpectedly ended")?;
            if session.snapshot.capture_id == 0 || session.point_starts != [0] {
                return Err("Recapture did not advance or changed audio/point boundaries".into());
            }
            let captured_marker = captured_marker_y(&session.png)?;
            let displayed = reports
                .iter()
                .rev()
                .find(|r| r["role"] == "frame" && r["busy"] == "false")
                .ok_or("No final canvas")?;
            if captured_marker == first_marker
                || displayed["markerY"].as_i64() != Some(captured_marker)
            {
                return Err(format!("Source moved, but native marker {first_marker} -> {captured_marker} and displayed marker {} do not agree", displayed["markerY"]));
            }
            eprintln!("PASS: source scroll -> visible live marker {live_marker} -> intercepted native press -> native capture marker {captured_marker} -> same displayed marker and first stroke; capture {}; source received no pointer press, left child and audio boundaries unchanged. Wheel was dispatched to the real DOM, not injected globally.", session.snapshot.capture_id);
            Ok(())
        })();
        if let Err(error) = &result {
            eprintln!("FAIL: {error}");
        }
        let cleanup = on_main(&handle, move |app| {
            browse_input::stop();
            if let Some(keys) = cleanup_keys.lock().unwrap().take() {
                unsafe { SetKeyboardState(&keys) }.map_err(|error| error.to_string())?;
            }
            let overlay = app
                .get_webview_window(WINDOW_LABEL)
                .map(|w| w.hwnd())
                .transpose()
                .map_err(|error| error.to_string())?;
            let foreground = unsafe { GetForegroundWindow() };
            app.state::<ScreenAnnotationState>()
                .session
                .lock()
                .unwrap()
                .take();
            if original != 0 && (overlay == Some(foreground) || foreground.0 as isize == source) {
                restore_target_window(original);
            }
            Ok(())
        });
        handle.exit(if result.is_ok() && cleanup.is_ok() {
            0
        } else {
            1
        });
        result.and(cleanup)
    });
    let _exit_code = app.run_return(|_, _| {});
    let result = worker
        .join()
        .map_err(|_| "Annotation check worker panicked")?;
    match previous_profile {
        Some(value) => std::env::set_var("WEBVIEW2_USER_DATA_FOLDER", value),
        None => std::env::remove_var("WEBVIEW2_USER_DATA_FOLDER"),
    }
    result
}

#[test]
fn live_browse_intercepts_first_press_and_refreezes() {
    let directory = tempfile::tempdir().expect("isolated annotation fixture");
    run(directory.path()).expect("live browse and first-press capture");
}
