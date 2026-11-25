//! The overlay window: hidden from screen capture, click-through while inactive.

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering::Relaxed};

use tauri::{
    window::{Color, Effect, EffectsBuilder},
    AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder, WindowEvent,
};
use windows::Win32::{
    Foundation::HWND,
    UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowDisplayAffinity, SetForegroundWindow, SetWindowDisplayAffinity,
        WDA_EXCLUDEFROMCAPTURE,
    },
};

use crate::App;

/// Share of the screen area the window takes.
const SCREEN_SHARE: f64 = 1.0 / 8.0;

#[derive(Default)]
pub struct WindowState {
    pub active: AtomicBool,
    /// Window that had focus before activation, restored on deactivation.
    previous: AtomicIsize,
}

fn hwnd(window: &WebviewWindow) -> tauri::Result<HWND> {
    Ok(HWND(window.hwnd()?.0 as _))
}

/// Excludes the window from every kind of capture. The window must never be shown without this.
fn hide_from_capture(window: &WebviewWindow) -> Result<(), Box<dyn std::error::Error>> {
    let h = hwnd(window)?;
    let mut affinity = 0u32;
    unsafe {
        SetWindowDisplayAffinity(h, WDA_EXCLUDEFROMCAPTURE)?;
        GetWindowDisplayAffinity(h, &mut affinity)?;
    }
    if affinity != WDA_EXCLUDEFROMCAPTURE.0 {
        return Err("Windows did not exclude the window from screen capture".into());
    }
    Ok(())
}

pub fn create(app: &AppHandle) -> Result<WebviewWindow, Box<dyn std::error::Error>> {
    let window = WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
        .title("Sidekick")
        .decorations(false)
        .resizable(false)
        .shadow(false)
        .skip_taskbar(true)
        .always_on_top(true)
        .focused(false)
        // Without WS_EX_NOACTIVATE from the start, show() steals the keyboard from the user's app.
        .focusable(false)
        .visible(false)
        .transparent(true)
        .background_color(Color(0, 0, 0, 0))
        .effects(EffectsBuilder::new().effect(Effect::Acrylic).build())
        .build()?;

    let monitor = window.primary_monitor()?.ok_or("no monitor")?;
    let (area, scale) = (monitor.size(), SCREEN_SHARE.sqrt());
    let size = PhysicalSize::new(
        (area.width as f64 * scale) as u32,
        (area.height as f64 * scale) as u32,
    );
    window.set_size(size)?;
    let corner = PhysicalPosition::new(
        monitor.position().x + area.width as i32 - size.width as i32 - 24,
        monitor.position().y + area.height as i32 - size.height as i32 - 72,
    );
    window.set_position(saved_position(app, &window).unwrap_or(corner))?;

    hide_from_capture(&window)?;
    window.set_ignore_cursor_events(true)?;
    window.show()?;

    let handle = app.clone();
    window.on_window_event(move |event| {
        if let WindowEvent::Moved(p) = event {
            let app = handle.state::<App>();
            let _ = app.db().set_setting("position", &format!("{},{}", p.x, p.y));
        }
    });
    Ok(window)
}

/// The saved position, unless that monitor is no longer there.
fn saved_position(app: &AppHandle, window: &WebviewWindow) -> Option<PhysicalPosition<i32>> {
    let text = app.state::<App>().db().setting("position").ok()??;
    let (x, y) = text.split_once(',')?;
    let (x, y): (i32, i32) = (x.parse().ok()?, y.parse().ok()?);
    let visible = window.available_monitors().ok()?.iter().any(|m| {
        let (p, s) = (m.position(), m.size());
        (p.x - 100..p.x + s.width as i32 - 100).contains(&x) && (p.y..p.y + s.height as i32 - 100).contains(&y)
    });
    visible.then_some(PhysicalPosition::new(x, y))
}

pub fn is_active(app: &AppHandle) -> bool {
    app.state::<App>().window.active.load(Relaxed)
}

/// `view` tells the page what to open; None keeps what was there.
pub fn set_active(app: &AppHandle, active: bool, view: Option<&str>) {
    let Some(window) = app.get_webview_window("main") else { return };
    let state = app.state::<App>();
    let was_active = state.window.active.swap(active, Relaxed);
    if active && !was_active {
        let previous = unsafe { GetForegroundWindow() };
        state.window.previous.store(previous.0 as isize, Relaxed);
    }
    let _ = window.set_ignore_cursor_events(!active);
    let _ = window.set_focusable(active);
    if active {
        let _ = window.set_focus();
    } else if was_active {
        // Give the keyboard back only if we still have it: the user may have clicked elsewhere.
        let ours = hwnd(&window).is_ok_and(|h| unsafe { GetForegroundWindow() } == h);
        let previous = state.window.previous.swap(0, Relaxed);
        if ours && previous != 0 {
            let _ = unsafe { SetForegroundWindow(HWND(previous as _)) };
        }
    }
    let _ = window.emit("active", serde_json::json!({ "active": active, "view": view }));
}
