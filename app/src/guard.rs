//! Crash guard. The call's microphone goes through the app (see `gate`): if the app dies, the call
//! hears silence and nobody notices. So `sidekick.exe` starts as a small guard that runs the real
//! app as its own child (`--app`) and starts it again after a crash, with a warning.

use std::{
    io::Write,
    path::PathBuf,
    process::{Command, ExitStatus},
    time::{Duration, Instant},
};

/// Passed by the guard to the process that runs the app.
pub const APP_FLAG: &str = "--app";
/// More crashes than this within `WINDOW` and the guard gives up: something is broken for good.
const MAX_RESTARTS: usize = 3;
const WINDOW: Duration = Duration::from_secs(10 * 60);

/// Runs the app and restarts it after crashes. False: the app could not be started as a child
/// at all, the caller runs it in this process instead.
pub fn run() -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    let mut crashes: Vec<Instant> = vec![];
    loop {
        let status = match Command::new(&exe).arg(APP_FLAG).args(std::env::args_os().skip(1)).status() {
            Ok(status) => status,
            Err(_) if crashes.is_empty() => return false,
            Err(e) => {
                log(&format!("restart failed: {e}"));
                warn(&format!("Sidekick впав і не зміг запуститися знову: {e}.\n\n{SILENCE}"));
                return true;
            }
        };
        // Quit from the tray, or a second copy that handed over to the running one.
        if status.success() {
            return true;
        }
        log(&format!("app exited: {}", describe(status)));
        crashes.retain(|t| t.elapsed() < WINDOW);
        crashes.push(Instant::now());
        if crashes.len() > MAX_RESTARTS {
            warn(&format!(
                "Sidekick впав {} рази за 10 хвилин, тому більше не перезапускається.\n\n{SILENCE}\n\nДеталі: {}",
                crashes.len(),
                log_path().display()
            ));
            return true;
        }
        // Not blocking: the app is back as soon as possible, the box waits for the user.
        std::thread::spawn(|| {
            warn("Sidekick впав і перезапустився сам. Якщо йде дзвінок, там могла бути тиша кілька секунд.")
        });
    }
}

const SILENCE: &str = "Якщо йде дзвінок, там зараз тиша: вибери в програмі дзвінка справжній мікрофон замість «CABLE Output».";

fn describe(status: ExitStatus) -> String {
    match status.code() {
        // Windows exception codes such as 0xC0000005 read better in hex.
        Some(code) if code < 0 => format!("0x{:08X}", code as u32),
        Some(code) => code.to_string(),
        None => status.to_string(),
    }
}

/// Same folder as Tauri's app data dir for `dev.sidekick.app`, without starting Tauri.
pub fn log_path() -> PathBuf {
    let dir = std::env::var_os("SIDEKICK_DATA_DIR").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(std::env::var_os("APPDATA").unwrap_or_default()).join("dev.sidekick.app")
    });
    dir.join("crash.log")
}

/// Best effort: a crash log must never cause a crash of its own.
pub fn log(line: &str) {
    let path = log_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{} {line}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"));
    }
}

/// Writes panics to the crash log: in a release build stderr goes nowhere.
pub fn log_panics() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        log(&format!("panic in {}: {info}", thread.name().unwrap_or("?")));
        default(info);
    }));
}

fn warn(text: &str) {
    #[cfg(windows)]
    {
        use windows::{
            core::HSTRING,
            Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONWARNING, MB_OK, MB_TOPMOST},
        };
        unsafe {
            MessageBoxW(None, &HSTRING::from(text), &HSTRING::from("Sidekick"), MB_OK | MB_ICONWARNING | MB_TOPMOST);
        }
    }
    #[cfg(not(windows))]
    eprintln!("{text}");
}
