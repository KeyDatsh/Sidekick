#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod db;
mod gate;
mod guard;
mod llm;
mod meeting;
mod window;
mod work;

use std::{
    path::PathBuf,
    str::FromStr,
    sync::{
        atomic::{AtomicU64, Ordering::Relaxed},
        Mutex, MutexGuard,
    },
};

use chrono::{Local, NaiveDate};
use db::{draft_from_text, iso, DayTab, Db, Line, Project, ReportRow, Task};
use serde::{Deserialize, Serialize};
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, State,
};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

/// Error text shown to the user.
type Cmd<T> = Result<T, String>;

pub struct App {
    db: Mutex<Db>,
    hotkeys: Mutex<Option<Hotkeys>>,
    pub window: window::WindowState,
    pub meeting: meeting::MeetingState,
    gate: gate::Gate,
    data_dir: PathBuf,
}

impl App {
    pub fn db(&self) -> MutexGuard<'_, Db> {
        // A panic while holding the lock cannot leave SQLite half-written.
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[derive(Clone, Copy, PartialEq)]
struct Hotkeys {
    toggle: Shortcut,
    quick: Shortcut,
    /// Held down: the call hears silence and the assistant translates what you say.
    pause: Shortcut,
}

impl Hotkeys {
    fn all(&self) -> [Shortcut; 3] {
        [self.toggle, self.quick, self.pause]
    }
}

#[derive(Serialize, Deserialize, Clone)]
struct Settings {
    toggle_hotkey: String,
    quick_hotkey: String,
    /// Background opacity of the inactive window, percent.
    opacity: u8,
    /// Folder with the speech models; empty means the default, see `models_dir`.
    models_dir: String,
    pause_hotkey: String,
    /// Part of the microphone's name; empty: the Windows default.
    mic_device: String,
    /// Part of the virtual cable's playback device name.
    cable_device: String,
}

impl Settings {
    fn load(db: &Db) -> Settings {
        let get = |key, default: &str| db.setting(key).ok().flatten().unwrap_or_else(|| default.into());
        Settings {
            toggle_hotkey: get("toggle_hotkey", "Ctrl+Alt+Space"),
            // Not the mockup's Ctrl+Shift+V: registered globally it would take "paste as plain text" from every app.
            quick_hotkey: get("quick_hotkey", "Ctrl+Alt+KeyV"),
            opacity: get("opacity", "62").parse().unwrap_or(62),
            models_dir: get("models_dir", ""),
            pause_hotkey: get("pause_hotkey", "Ctrl+Alt+KeyP"),
            mic_device: get("mic_device", ""),
            cable_device: get("cable_device", "CABLE Input"),
        }
    }

    fn hotkeys(&self) -> Cmd<Hotkeys> {
        let parse = |s: &str| Shortcut::from_str(s).map_err(|_| format!("Незрозуміла комбінація: {s}"));
        let keys = Hotkeys {
            toggle: parse(&self.toggle_hotkey)?,
            quick: parse(&self.quick_hotkey)?,
            pause: parse(&self.pause_hotkey)?,
        };
        if keys.toggle == keys.quick || keys.toggle == keys.pause || keys.quick == keys.pause {
            return Err("Дві дії мають однакову комбінацію".into());
        }
        Ok(keys)
    }
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

fn today() -> NaiveDate {
    Local::now().date_naive()
}

fn valid_day(day: &str) -> Cmd<()> {
    day.parse::<NaiveDate>().map(|_| ()).map_err(|_| format!("Невірна дата: {day}"))
}

// State for the page

#[derive(Serialize)]
struct Snapshot {
    today: String,
    tabs: Vec<DayTab>,
    tasks: Vec<Task>,
    /// Today's tasks, for the inactive view and the morning plan.
    today_tasks: Vec<Task>,
    projects: Vec<Project>,
    plan_due: bool,
    active: bool,
    hotkeys_ok: bool,
    settings: Settings,
    meeting: MeetingView,
}

#[derive(Serialize)]
struct MeetingView {
    status: meeting::Status,
    gate: gate::Status,
    /// Finished lines of the running meeting, for a page that was reloaded mid-meeting.
    lines: Vec<Line>,
    default_models_dir: String,
}

#[tauri::command]
fn snapshot(app: State<App>, day: Option<String>) -> Cmd<Snapshot> {
    // Before the database lock: the worker's reader takes the meeting lock, then the database.
    let (status, meeting_id) = (app.meeting.status(), app.meeting.id());
    let db = app.db();
    let now = today();
    let today = iso(now);
    db.carry_over(&today).map_err(err)?;
    Ok(Snapshot {
        tabs: db.day_tabs(now).map_err(err)?,
        tasks: db.tasks(day.as_deref()).map_err(err)?,
        today_tasks: db.tasks(Some(&today)).map_err(err)?,
        projects: db.projects().map_err(err)?,
        plan_due: db.setting("plan_seen").map_err(err)?.as_deref() != Some(&today),
        active: app.window.active.load(Relaxed),
        hotkeys_ok: app.hotkeys.lock().is_ok_and(|k| k.is_some()),
        settings: Settings::load(&db),
        meeting: MeetingView {
            status,
            gate: app.gate.status(),
            lines: match meeting_id {
                Some(id) => db.lines(id, 50).map_err(err)?,
                None => vec![],
            },
            default_models_dir: app.data_dir.join("models").display().to_string(),
        },
        today,
    })
}

#[tauri::command]
fn plan_seen(app: State<App>) -> Cmd<()> {
    app.db().set_setting("plan_seen", &iso(today())).map_err(err)
}

// Projects and tasks

#[tauri::command]
fn add_project(app: State<App>, client: String, name: String) -> Cmd<i64> {
    if client.trim().is_empty() {
        return Err("Вкажи замовника".into());
    }
    app.db().add_project(&client, &name).map_err(err)
}

#[tauri::command]
fn archive_project(app: State<App>, id: i64) -> Cmd<()> {
    app.db().archive_project(id).map_err(err)
}

#[tauri::command]
fn add_task(app: State<App>, title: String, body: String, project: Option<i64>, day: Option<String>) -> Cmd<i64> {
    if title.trim().is_empty() {
        return Err("Задача без заголовка".into());
    }
    if let Some(d) = &day {
        valid_day(d)?;
    }
    app.db().add_task(&title, &body, project, day.as_deref()).map_err(err)
}

#[tauri::command]
fn update_task(app: State<App>, id: i64, title: String, body: String, project: Option<i64>) -> Cmd<()> {
    if title.trim().is_empty() {
        return Err("Задача без заголовка".into());
    }
    app.db().update_task(id, &title, &body, project).map_err(err)
}

#[tauri::command]
fn move_task(app: State<App>, id: i64, day: Option<String>) -> Cmd<()> {
    if let Some(d) = &day {
        valid_day(d)?;
    }
    app.db().move_task(id, day.as_deref()).map_err(err)
}

#[tauri::command]
fn set_done(app: State<App>, id: i64, done: bool) -> Cmd<()> {
    app.db().set_done(id, done, &iso(today())).map_err(err)
}

#[tauri::command]
fn set_started(app: State<App>, id: i64, started: bool) -> Cmd<()> {
    app.db().set_started(id, started, &iso(today())).map_err(err)
}

#[tauri::command]
fn delete_task(app: State<App>, id: i64) -> Cmd<()> {
    app.db().delete_task(id).map_err(err)
}

#[derive(Serialize)]
struct Draft {
    title: String,
    body: String,
}

#[tauri::command]
fn clipboard_draft() -> Draft {
    let text = arboard::Clipboard::new().and_then(|mut c| c.get_text()).unwrap_or_default();
    let (title, body) = draft_from_text(&text);
    Draft { title, body }
}

#[tauri::command]
fn copy_text(text: String) -> Cmd<()> {
    arboard::Clipboard::new().and_then(|mut c| c.set_text(text)).map_err(err)
}

// Report

#[derive(Serialize)]
struct Report {
    rows: Vec<ReportRow>,
    /// Folders with work on that day that are not linked to a project yet.
    unassigned: Vec<work::Folder>,
}

/// Tasks from the database plus Claude Code sessions and commits, grouped by project.
fn build_report(app: &App, day: &str) -> Cmd<Report> {
    let date: NaiveDate = day.parse().map_err(|_| format!("Невірна дата: {day}"))?;
    let (mut rows, folders) = {
        let db = app.db();
        (db.report(day, &iso(today())).map_err(err)?, db.folders().map_err(err)?)
    };
    // Linked repositories are checked for commits even on days without a Claude Code session.
    let repos: Vec<String> = folders.iter().filter(|(_, p)| p.is_some()).map(|(f, _)| f.clone()).collect();
    // Reading sessions and running git takes a while: not under the database lock.
    let work = work::day(date, &repos);
    let mut unassigned = vec![];
    for folder in work {
        // Exact match only: work in a repository's subfolders already comes under its root, and a link
        // on a parent folder (say, the home folder) must not swallow every project inside it.
        let link = folders.iter().find(|(f, _)| work::key(f) == work::key(&folder.path));
        match link {
            None => unassigned.push(folder),
            Some((_, None)) => {}
            Some((_, Some(project))) => match rows.iter_mut().find(|r| r.project_id == *project) {
                Some(row) => row.work.push(folder),
                None => {
                    let mut row = app.db().report_row(day, *project).map_err(err)?;
                    row.work.push(folder);
                    rows.push(row);
                }
            },
        }
    }
    Ok(Report { rows, unassigned })
}

/// `(async)`: reading the sessions must not block the window.
#[tauri::command(async)]
fn report(app: State<App>, day: String) -> Cmd<Report> {
    build_report(&app, &day)
}

/// `project`: None leaves the folder out of reports.
#[tauri::command]
fn assign_folder(app: State<App>, path: String, project: Option<i64>) -> Cmd<()> {
    if path.trim().is_empty() {
        return Err("Порожній шлях".into());
    }
    app.db().set_folder(path.trim(), project).map_err(err)
}

#[derive(Serialize)]
struct Generated {
    report: Report,
    /// False: no API key, the memos are just task titles and commits.
    by_claude: bool,
}

/// `(async)`: the network call must not block the window.
#[tauri::command(async)]
fn generate_report(app: State<App>, day: String) -> Cmd<Generated> {
    let mut report = build_report(&app, &day)?;
    if report.rows.is_empty() {
        return Err("За цей день немає ні задач, ні роботи в прив'язаних папках".into());
    }
    let key = api_key();
    let memos = match &key {
        Some(key) => llm::draft(key, &report.rows)?,
        None => llm::draft_offline(&report.rows),
    };
    let db = app.db();
    for row in &mut report.rows {
        if let Some(memo) = memos.get(&row.project_id) {
            db.save_memo(&day, row.project_id, memo).map_err(err)?;
            row.memo = memo.clone();
        }
    }
    Ok(Generated { report, by_claude: key.is_some() })
}

#[tauri::command]
fn save_memo(app: State<App>, day: String, project: i64, memo: String) -> Cmd<()> {
    valid_day(&day)?;
    app.db().save_memo(&day, project, &memo).map_err(err)
}

#[tauri::command]
fn save_minutes(app: State<App>, day: String, project: i64, minutes: i64) -> Cmd<()> {
    valid_day(&day)?;
    app.db().save_minutes(&day, project, minutes).map_err(err)
}

// Meeting

/// Settings, then SIDEKICK_MODELS, then `models` in the data folder.
fn models_dir(app: &App) -> PathBuf {
    let saved = Settings::load(&app.db()).models_dir;
    if !saved.trim().is_empty() {
        return PathBuf::from(saved.trim());
    }
    std::env::var_os("SIDEKICK_MODELS").map(PathBuf::from).unwrap_or_else(|| app.data_dir.join("models"))
}

#[tauri::command]
fn start_meeting(handle: AppHandle, app: State<App>) -> Cmd<()> {
    let mic = Settings::load(&app.db()).mic_device;
    meeting::start(&handle, models_dir(&app), &mic, app.data_dir.join("speech.log"))
}

#[tauri::command]
fn audio_devices() -> gate::Devices {
    gate::devices()
}

/// Counts pause presses, so the safety release below only undoes its own press.
static PRESSES: AtomicU64 = AtomicU64::new(0);

/// The pause key went down or up.
fn pause(handle: &AppHandle, held: bool) {
    let app = handle.state::<App>();
    app.gate.hold(held);
    let meeting = app.meeting.pause(held);
    let _ = handle.emit("pause", serde_json::json!({ "held": held, "meeting": meeting }));
    let press = PRESSES.fetch_add(1, Relaxed) + 1;
    if held {
        // If the key-up is ever lost, the call must not stay muted.
        let handle = handle.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(60));
            if PRESSES.load(Relaxed) == press {
                pause(&handle, false);
            }
        });
    }
}

#[tauri::command]
fn stop_meeting(handle: AppHandle) {
    meeting::stop(&handle);
}

// Settings

/// The key lives in Windows Credential Manager, never in the database.
fn key_entry() -> Cmd<keyring::Entry> {
    keyring::Entry::new("Sidekick", "anthropic-api-key").map_err(err)
}

fn api_key() -> Option<String> {
    key_entry()
        .and_then(|e| e.get_password().map_err(err))
        .ok()
        .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok())
        .filter(|k| !k.is_empty())
}

#[tauri::command]
fn has_api_key() -> bool {
    api_key().is_some()
}

#[tauri::command]
fn set_api_key(key: String) -> Cmd<()> {
    let entry = key_entry()?;
    match key.trim() {
        "" => match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(err(e)),
        },
        key => entry.set_password(key).map_err(err),
    }
}

#[tauri::command]
fn save_settings(handle: AppHandle, app: State<App>, settings: Settings) -> Cmd<()> {
    set_hotkeys(&handle, settings.hotkeys()?)?;
    let db = app.db();
    db.set_setting("toggle_hotkey", &settings.toggle_hotkey).map_err(err)?;
    db.set_setting("quick_hotkey", &settings.quick_hotkey).map_err(err)?;
    db.set_setting("models_dir", settings.models_dir.trim()).map_err(err)?;
    db.set_setting("pause_hotkey", &settings.pause_hotkey).map_err(err)?;
    let old = Settings::load(&db);
    let (mic, cable) = (settings.mic_device.trim(), settings.cable_device.trim());
    db.set_setting("mic_device", mic).map_err(err)?;
    db.set_setting("cable_device", cable).map_err(err)?;
    drop(db);
    if old.mic_device != mic || old.cable_device != cable || !app.gate.status().on {
        app.gate.start(cable, mic);
    }
    let db = app.db();
    db.set_setting("opacity", &settings.opacity.clamp(20, 100).to_string()).map_err(err)
}

/// Replaces the registered hotkeys; if a new one is taken, the old ones stay.
fn set_hotkeys(handle: &AppHandle, new: Hotkeys) -> Cmd<()> {
    let state = handle.state::<App>();
    let mut current = state.hotkeys.lock().unwrap_or_else(|e| e.into_inner());
    if *current == Some(new) {
        return Ok(());
    }
    let shortcuts = handle.global_shortcut();
    let _ = shortcuts.unregister_all();
    let register = |k: &Hotkeys| -> Cmd<()> {
        for key in k.all() {
            shortcuts
                .register(key)
                .map_err(|_| format!("Комбінацію {key} вже зайняла інша програма"))?;
        }
        Ok(())
    };
    if let Err(e) = register(&new) {
        let _ = shortcuts.unregister_all();
        if let Some(old) = &*current {
            let _ = register(old);
        }
        return Err(e);
    }
    *current = Some(new);
    Ok(())
}

// Window

#[tauri::command]
fn deactivate(handle: AppHandle) {
    window::set_active(&handle, false, None);
}

#[tauri::command]
fn quit(handle: AppHandle) {
    handle.exit(0);
}

fn on_hotkey(handle: &AppHandle, pressed: &Shortcut, state: ShortcutState) {
    let app = handle.state::<App>();
    let keys = *app.hotkeys.lock().unwrap_or_else(|e| e.into_inner());
    if keys.is_some_and(|k| k.pause == *pressed) {
        return pause(handle, state == ShortcutState::Pressed);
    }
    if state != ShortcutState::Pressed {
        return;
    }
    match keys {
        Some(k) if k.quick == *pressed => window::set_active(handle, true, Some("quick")),
        Some(k) if k.toggle == *pressed => window::set_active(handle, !window::is_active(handle), None),
        _ => {}
    }
}

/// The window has no taskbar button: this is the visible way to open it and to quit.
fn create_tray(handle: &AppHandle) -> tauri::Result<()> {
    let open = MenuItem::with_id(handle, "open", "Відкрити", true, None::<&str>)?;
    let quit = MenuItem::with_id(handle, "quit", "Вийти", true, None::<&str>)?;
    let mut tray = TrayIconBuilder::with_id("main")
        .tooltip("Sidekick")
        .menu(&Menu::with_items(handle, &[&open, &quit])?)
        .show_menu_on_left_click(false)
        .on_menu_event(|handle, event| match event.id().as_ref() {
            "open" => window::set_active(handle, true, None),
            "quit" => handle.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                window::set_active(tray.app_handle(), true, None);
            }
        });
    if let Some(icon) = handle.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(handle)?;
    Ok(())
}

fn data_dir(handle: &AppHandle) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let dir = match std::env::var_os("SIDEKICK_DATA_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => handle.path().app_data_dir()?,
    };
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn main() {
    if !std::env::args().any(|a| a == guard::APP_FLAG) && guard::run() {
        return;
    }
    guard::log_panics();
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|handle, _, _| {
            window::set_active(handle, true, None);
        }))
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|handle, shortcut, event| on_hotkey(handle, shortcut, event.state()))
                .build(),
        )
        .invoke_handler(tauri::generate_handler![
            snapshot, plan_seen, add_project, archive_project, add_task, update_task, move_task, set_done,
            set_started, delete_task, clipboard_draft, copy_text, report, assign_folder, generate_report, save_memo,
            save_minutes, has_api_key, set_api_key, save_settings, start_meeting, stop_meeting, audio_devices, deactivate, quit
        ])
        .setup(|app| {
            let handle = app.handle();
            let data_dir = data_dir(handle)?;
            let db = Db::open(&data_dir.join("sidekick.db"))?;
            // Meetings a crash left open.
            db.end_meeting(None, &Local::now().format("%Y-%m-%d %H:%M:%S").to_string())?;
            let settings = Settings::load(&db);
            app.manage(App {
                db: Mutex::new(db),
                hotkeys: Mutex::new(None),
                window: Default::default(),
                meeting: Default::default(),
                gate: Default::default(),
                data_dir,
            });
            // The call's microphone goes through the gate from the start, meeting or not.
            let gate = handle.state::<App>().gate.start(&settings.cable_device, &settings.mic_device);
            eprintln!("gate: {}", gate.note);
            window::create(handle)?;
            create_tray(handle)?;
            if let Err(e) = settings.hotkeys().and_then(|k| set_hotkeys(handle, k)) {
                // Without a hotkey the window could never be activated: open it so the user can pick another one.
                eprintln!("{e}");
                window::set_active(handle, true, Some("settings"));
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("run Sidekick");
}
