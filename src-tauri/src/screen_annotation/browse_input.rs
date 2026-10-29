use super::{ScreenBounds, WINDOW_LABEL};
use std::sync::atomic::{
    AtomicBool, AtomicI32, AtomicIsize, AtomicU32, AtomicU64, AtomicU8, Ordering,
};
use std::sync::OnceLock;
use tauri::{AppHandle, Emitter};
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

const LIVE: u8 = 1;
const DRAWING: u8 = 2;
const WAITING: u8 = 3;
const DRAINING: u8 = 4;
static APP: OnceLock<AppHandle> = OnceLock::new();
static HOOK: AtomicIsize = AtomicIsize::new(0);
static MODE: AtomicU8 = AtomicU8::new(0);
static SESSION: AtomicU64 = AtomicU64::new(0);
static LEFT: AtomicI32 = AtomicI32::new(0);
static TOP: AtomicI32 = AtomicI32::new(0);
static WIDTH: AtomicU32 = AtomicU32::new(0);
static HEIGHT: AtomicU32 = AtomicU32::new(0);
static PRESSED: AtomicU32 = AtomicU32::new(0);
static ACTIVE_BUTTON: AtomicU32 = AtomicU32::new(0);
static SHOWN: AtomicBool = AtomicBool::new(false);

#[derive(Clone, serde::Serialize)]
struct Input {
    session_id: u64,
    kind: &'static str,
    x: f64,
    y: f64,
    delta_x: i32,
    delta_y: i32,
}

pub fn start(app: &AppHandle, session: u64, bounds: ScreenBounds) -> Result<(), String> {
    if HOOK.load(Ordering::SeqCst) != 0 {
        return Err("Finish the current drawing gesture before scrolling".into());
    }
    APP.get_or_init(|| app.clone());
    SESSION.store(session, Ordering::SeqCst);
    LEFT.store(bounds.x, Ordering::SeqCst);
    TOP.store(bounds.y, Ordering::SeqCst);
    WIDTH.store(bounds.width, Ordering::SeqCst);
    HEIGHT.store(bounds.height, Ordering::SeqCst);
    PRESSED.store(0, Ordering::SeqCst);
    SHOWN.store(false, Ordering::SeqCst);
    let module = unsafe { GetModuleHandleW(None) }.map_err(|error| error.to_string())?;
    let hook = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(callback), Some(module.into()), 0) }
        .map_err(|error| format!("Cannot protect the first drawing press: {error}"))?;
    HOOK.store(hook.0 as isize, Ordering::SeqCst);
    MODE.store(LIVE, Ordering::SeqCst);
    Ok(())
}

fn remove() {
    MODE.store(0, Ordering::SeqCst);
    let hook = HOOK.swap(0, Ordering::SeqCst);
    if hook != 0 {
        if let Err(error) = unsafe { UnhookWindowsHookEx(HHOOK(hook as *mut _)) } {
            log::error!("Failed to remove annotation mouse hook: {error}");
        }
    }
}

pub fn stop() {
    let Some(app) = APP.get() else {
        return;
    };
    let session = SESSION.load(Ordering::SeqCst);
    if let Err(error) = app.run_on_main_thread(move || {
        if SESSION.load(Ordering::SeqCst) == session {
            stop_on_main();
        }
    }) {
        log::error!("Failed to stop protected annotation input: {error}");
    }
}

fn stop_on_main() {
    if PRESSED.load(Ordering::SeqCst) == 0 {
        remove();
    } else {
        // A swallowed down must not be followed by an unmatched source up.
        MODE.store(DRAINING, Ordering::SeqCst);
    }
}

pub fn shown(session: u64) {
    let Some(app) = APP.get() else {
        return;
    };
    if let Err(error) = app.run_on_main_thread(move || {
        if SESSION.load(Ordering::SeqCst) == session {
            SHOWN.store(true, Ordering::SeqCst);
            if PRESSED.load(Ordering::SeqCst) == 0 {
                remove();
            }
        }
    }) {
        log::error!("Failed to release protected annotation input: {error}");
    }
}

pub fn waiting(session: u64) {
    if SESSION.load(Ordering::SeqCst) == session {
        let _ = MODE.compare_exchange(LIVE, WAITING, Ordering::SeqCst, Ordering::SeqCst);
    }
}

fn send(input: Input) {
    if let Some(app) = APP.get() {
        if let Err(error) = app.emit_to(WINDOW_LABEL, "annotation-browse-input", input) {
            log::error!("Failed to deliver protected annotation input: {error}");
            let _ = app.emit("transcription-error", error.to_string());
            stop_on_main();
        }
    }
}

unsafe extern "system" fn callback(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code != HC_ACTION as i32 || MODE.load(Ordering::SeqCst) == 0 {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }
    let data = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
    if handle(wparam.0 as u32, data) {
        LRESULT(1)
    } else {
        unsafe { CallNextHookEx(None, code, wparam, lparam) }
    }
}

fn handle(message: u32, data: &MSLLHOOKSTRUCT) -> bool {
    let mode = MODE.load(Ordering::SeqCst);
    let width = WIDTH.load(Ordering::SeqCst);
    let height = HEIGHT.load(Ordering::SeqCst);
    if mode == 0 || width == 0 || height == 0 {
        return false;
    }
    let x = f64::from(data.pt.x - LEFT.load(Ordering::SeqCst)) / f64::from(width);
    let y = f64::from(data.pt.y - TOP.load(Ordering::SeqCst)) / f64::from(height);
    let inside = (0.0..1.0).contains(&x) && (0.0..1.0).contains(&y);
    let mut input = Input {
        session_id: SESSION.load(Ordering::SeqCst),
        kind: "move",
        x,
        y,
        delta_x: 0,
        delta_y: 0,
    };
    let button = match message {
        WM_LBUTTONDOWN | WM_LBUTTONUP => 1,
        WM_RBUTTONDOWN | WM_RBUTTONUP => 2,
        WM_MBUTTONDOWN | WM_MBUTTONUP => 4,
        WM_XBUTTONDOWN | WM_XBUTTONUP => 8 << ((data.mouseData >> 16).saturating_sub(1)),
        _ => 0,
    };
    if matches!(
        message,
        WM_LBUTTONUP | WM_RBUTTONUP | WM_MBUTTONUP | WM_XBUTTONUP
    ) && PRESSED.load(Ordering::SeqCst) & button != 0
    {
        PRESSED.fetch_and(!button, Ordering::SeqCst);
        if mode == DRAWING && ACTIVE_BUTTON.load(Ordering::SeqCst) == button {
            MODE.store(WAITING, Ordering::SeqCst);
            input.kind = "up";
            send(input);
        }
        if PRESSED.load(Ordering::SeqCst) == 0 && (mode == DRAINING || SHOWN.load(Ordering::SeqCst))
        {
            remove();
        }
        return true;
    }
    if mode == DRAINING {
        return message == WM_MOUSEMOVE;
    }
    // Protect the captured region until release/cancel, even after focus changes.
    // Capture/show still reject an unrelated foreground application.
    if message == WM_MOUSEMOVE && mode == DRAWING {
        send(input);
        return true;
    }
    if !inside {
        return false;
    }
    if matches!(
        message,
        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN
    ) {
        PRESSED.fetch_or(button, Ordering::SeqCst);
        if mode == LIVE {
            ACTIVE_BUTTON.store(button, Ordering::SeqCst);
            MODE.store(DRAWING, Ordering::SeqCst);
            input.kind = "down";
            send(input);
        }
        return true;
    }
    if matches!(message, WM_MOUSEWHEEL | WM_MOUSEHWHEEL) {
        if mode == LIVE {
            input.kind = "wheel";
            let delta = (data.mouseData >> 16) as u16 as i16 as i32;
            if message == WM_MOUSEWHEEL {
                input.delta_y = delta;
            } else {
                input.delta_x = delta;
            }
            send(input);
        }
        return true;
    }
    false
}

#[cfg(feature = "annotation-scroll-check")]
pub(super) fn check_input(message: u32, x: i32, y: i32, delta: i16) -> bool {
    let data = MSLLHOOKSTRUCT {
        pt: windows::Win32::Foundation::POINT { x, y },
        mouseData: (delta as u16 as u32) << 16,
        ..Default::default()
    };
    HOOK.load(Ordering::SeqCst) != 0
        && unsafe {
            callback(
                HC_ACTION as i32,
                WPARAM(message as usize),
                LPARAM(&data as *const _ as isize),
            )
        }
        .0 == 1
}
