//! Stage 0 prototype: does the Sidekick window idea work at all?
//!
//! * transparent always-on-top window, ~1/8 of the screen, remembers its position;
//! * inactive = click-through, active = normal window, toggled by a global hotkey;
//! * excluded from every kind of screen capture.
//!
//! Flags: `--no-protect` turns capture protection off (control run for the capture test),
//! `--marker` paints the window solid magenta so captures can be checked by pixel count,
//! `--selftest` checks styles and hit-testing in both states, prints the result and exits.

use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicBool, AtomicIsize, Ordering},
    time::Duration,
};

use tauri::{
    window::{Color, Effect, EffectsBuilder},
    AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder, WindowEvent,
};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};
use windows::Win32::{
    Foundation::{HWND, POINT, RECT},
    UI::WindowsAndMessaging::{
        GetAncestor, GetForegroundWindow, GetWindowDisplayAffinity, GetWindowLongPtrW,
        GetWindowRect, SetForegroundWindow, SetWindowDisplayAffinity, WindowFromPoint, GA_ROOT,
        GWL_EXSTYLE, WDA_EXCLUDEFROMCAPTURE, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOPMOST,
        WS_EX_TRANSPARENT,
    },
};

/// Share of the screen area the window takes.
const SCREEN_SHARE: f64 = 1.0 / 8.0;

#[derive(Default)]
struct Overlay {
    active: AtomicBool,
    /// Window that had focus before activation, restored on deactivation.
    previous: AtomicIsize,
}

fn has_flag(name: &str) -> bool {
    std::env::args().any(|a| a == name)
}

fn hwnd(window: &WebviewWindow) -> HWND {
    HWND(window.hwnd().expect("window handle").0 as _)
}

fn position_file(app: &AppHandle) -> Option<PathBuf> {
    Some(app.path().app_config_dir().ok()?.join("position.txt"))
}

fn load_position(app: &AppHandle) -> Option<PhysicalPosition<i32>> {
    let text = fs::read_to_string(position_file(app)?).ok()?;
    let (x, y) = text.trim().split_once(',')?;
    Some(PhysicalPosition::new(x.parse().ok()?, y.parse().ok()?))
}

fn save_position(app: &AppHandle, pos: PhysicalPosition<i32>) {
    if let Some(file) = position_file(app) {
        if let Some(dir) = file.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let _ = fs::write(file, format!("{},{}", pos.x, pos.y));
    }
}

fn set_active(app: &AppHandle, active: bool) {
    let overlay = app.state::<Overlay>();
    let window = app.get_webview_window("main").expect("main window");
    if active {
        let previous = unsafe { GetForegroundWindow() };
        overlay.previous.store(previous.0 as isize, Ordering::Relaxed);
    }
    let _ = window.set_ignore_cursor_events(!active);
    let _ = window.set_focusable(active);
    if active {
        let _ = window.set_focus();
    } else {
        let previous = overlay.previous.load(Ordering::Relaxed);
        if previous != 0 {
            let _ = unsafe { SetForegroundWindow(HWND(previous as _)) };
        }
    }
    overlay.active.store(active, Ordering::Relaxed);
    let _ = window.emit("active", active);
    println!("{}", describe(&window, active));
}

/// One-line summary of everything the prototype has to prove about the window.
fn describe(window: &WebviewWindow, active: bool) -> String {
    let h = hwnd(window);
    let mut affinity = 0u32;
    let mut rect = RECT::default();
    let (style, hit) = unsafe {
        let _ = GetWindowDisplayAffinity(h, &mut affinity);
        let _ = GetWindowRect(h, &mut rect);
        let centre = POINT {
            x: (rect.left + rect.right) / 2,
            y: (rect.top + rect.bottom) / 2,
        };
        (
            GetWindowLongPtrW(h, GWL_EXSTYLE) as u32,
            GetAncestor(WindowFromPoint(centre), GA_ROOT) == h,
        )
    };
    format!(
        "hwnd={} active={active} excluded_from_capture={} topmost={} transparent={} layered={} noactivate={} mouse_hits_window={hit} foreground={} rect={},{} {}x{}",
        h.0 as isize,
        affinity == WDA_EXCLUDEFROMCAPTURE.0,
        style & WS_EX_TOPMOST.0 != 0,
        style & WS_EX_TRANSPARENT.0 != 0,
        style & WS_EX_LAYERED.0 != 0,
        style & WS_EX_NOACTIVATE.0 != 0,
        unsafe { GetForegroundWindow() } == h,
        rect.left,
        rect.top,
        rect.right - rect.left,
        rect.bottom - rect.top,
    )
}

fn create_window(app: &AppHandle) -> tauri::Result<WebviewWindow> {
    let marker = has_flag("--marker");
    let url = if marker { "index.html#marker" } else { "index.html" };
    let mut builder = WebviewWindowBuilder::new(app, "main", WebviewUrl::App(url.into()))
        .title("Sidekick overlay prototype")
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
        .background_color(Color(0, 0, 0, 0));
    if !marker {
        builder = builder.effects(EffectsBuilder::new().effect(Effect::Acrylic).build());
    }
    let window = builder.build()?;

    let monitor = window.primary_monitor()?.expect("primary monitor");
    let scale = SCREEN_SHARE.sqrt();
    let size = PhysicalSize::new(
        (monitor.size().width as f64 * scale) as u32,
        (monitor.size().height as f64 * scale) as u32,
    );
    window.set_size(size)?;
    let pos = load_position(app).unwrap_or(PhysicalPosition::new(
        monitor.size().width as i32 - size.width as i32 - 24,
        monitor.size().height as i32 - size.height as i32 - 72,
    ));
    window.set_position(pos)?;

    // Before the first show(): there must be no frame in which the window is capturable.
    if !has_flag("--no-protect") {
        unsafe { SetWindowDisplayAffinity(hwnd(&window), WDA_EXCLUDEFROMCAPTURE) }
            .expect("SetWindowDisplayAffinity");
    }
    window.set_ignore_cursor_events(true)?;
    window.show()?;

    let handle = app.clone();
    window.on_window_event(move |event| {
        if let WindowEvent::Moved(pos) = event {
            save_position(&handle, *pos);
        }
    });
    Ok(window)
}

fn selftest(app: AppHandle) {
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(2));
        for active in [false, true, false] {
            set_active(&app, active);
            std::thread::sleep(Duration::from_millis(400));
            let window = app.get_webview_window("main").expect("main window");
            println!("settled: {}", describe(&window, active));
        }
        app.exit(0);
    });
}

fn main() {
    let toggle = Shortcut::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::Space);
    tauri::Builder::default()
        .manage(Overlay::default())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, _shortcut, event| {
                    if event.state() == ShortcutState::Pressed {
                        let active = app.state::<Overlay>().active.load(Ordering::Relaxed);
                        set_active(app, !active);
                    }
                })
                .build(),
        )
        .setup(move |app| {
            let window = create_window(app.handle())?;
            println!("{}", describe(&window, false));
            match app.global_shortcut().register(toggle) {
                Ok(()) => println!("hotkey Ctrl+Alt+Space registered"),
                Err(e) => eprintln!("hotkey Ctrl+Alt+Space NOT registered: {e}"),
            }
            if has_flag("--selftest") {
                selftest(app.handle().clone());
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("run overlay prototype");
}
