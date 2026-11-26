//! Meeting mode: runs the speech worker (`sidekick-speech`, built by app/speech/build.sh),
//! forwards its subtitles to the window and keeps the finished ones in the database.

use std::{
    fs::OpenOptions,
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{Mutex, MutexGuard},
};

use chrono::Local;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

use crate::{db::Line, App};

/// Worker output, one JSON object per line (see app/speech/src/main.rs).
#[derive(Deserialize, Serialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Msg {
    Loading,
    Ready { device: String, mic: Option<String>, mic_error: Option<String> },
    Partial { en: String, uk: String },
    Final { at_ms: i64, en: String, uk: String },
    /// Said during a pause, Ukrainian and its English translation.
    Said { partial: bool, at_ms: i64, uk: String, en: String },
    Error { message: String },
}

#[derive(Serialize, Clone, Default, Debug)]
pub struct Status {
    pub running: bool,
    /// What to show next to the controls: loading, the audio device, or what went wrong.
    pub note: String,
    pub error: bool,
}

#[derive(Default)]
pub struct MeetingState {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    child: Option<Child>,
    /// Commands to the worker: `pause_down`, `pause_up`.
    stdin: Option<ChildStdin>,
    id: Option<i64>,
    status: Status,
}

impl MeetingState {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn status(&self) -> Status {
        self.lock().status.clone()
    }

    pub fn id(&self) -> Option<i64> {
        self.lock().id
    }

    /// Tells the worker to start or stop keeping the microphone. False: no meeting is running.
    pub fn pause(&self, held: bool) -> bool {
        let mut inner = self.lock();
        let Some(stdin) = &mut inner.stdin else { return false };
        let line: &[u8] = if held { b"pause_down\n" } else { b"pause_up\n" };
        stdin.write_all(line).and_then(|()| stdin.flush()).is_ok()
    }
}

fn now() -> String {
    Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

fn set_status(handle: &AppHandle, inner: &mut Inner, status: Status) {
    inner.status = status.clone();
    let _ = handle.emit("meeting_status", status);
}

fn worker_path() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("SIDEKICK_SPEECH_BIN") {
        return Ok(path.into());
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let path = exe.with_file_name("sidekick-speech.exe");
    if !path.exists() {
        return Err(format!("Немає {}. Його збирає app/speech/build.sh.", path.display()));
    }
    Ok(path)
}

/// `mic`: part of the microphone's name for pause mode, empty for the default one.
pub fn start(handle: &AppHandle, models: PathBuf, mic: &str, log: PathBuf) -> Result<(), String> {
    let app = handle.state::<App>();
    let mut inner = app.meeting.lock();
    if inner.child.is_some() {
        return Ok(());
    }
    let bin = worker_path()?;
    let log = OpenOptions::new().create(true).append(true).open(&log).map_err(|e| format!("{}: {e}", log.display()))?;
    let mut command = Command::new(bin);
    command.arg("--models").arg(&models);
    if !mic.trim().is_empty() {
        command.arg("--mic").arg(mic.trim());
    }
    command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(log);
    // The worker loads cuBLAS at start; the CUDA installer sets CUDA_PATH but not always PATH.
    if let Some(cuda) = std::env::var_os("CUDA_PATH") {
        let mut paths = vec![PathBuf::from(cuda).join("bin")];
        paths.extend(std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect::<Vec<_>>()).unwrap_or_default());
        if let Ok(path) = std::env::join_paths(paths) {
            command.env("PATH", path);
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command.spawn().map_err(|e| format!("Не вдалося запустити розпізнавання: {e}"))?;
    let stdout = child.stdout.take().ok_or("no stdout")?;
    inner.stdin = child.stdin.take();
    let id = app.db().start_meeting(&now()).map_err(|e| e.to_string())?;
    inner.child = Some(child);
    inner.id = Some(id);
    set_status(handle, &mut inner, Status { running: true, note: "Завантаження моделей…".into(), error: false });
    drop(inner);

    let handle = handle.clone();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            match serde_json::from_str::<Msg>(&line) {
                Ok(msg) => on_message(&handle, id, msg),
                Err(e) => eprintln!("speech worker: {e}: {line}"),
            }
        }
        exited(&handle, id);
    });
    Ok(())
}

fn on_message(handle: &AppHandle, id: i64, msg: Msg) {
    let app = handle.state::<App>();
    let mut inner = app.meeting.lock();
    if inner.id != Some(id) {
        return; // a stopped meeting's last words
    }
    match msg {
        Msg::Loading => {}
        Msg::Ready { device, mic, mic_error } => {
            let pause = match (mic, mic_error) {
                (Some(mic), _) => format!("пауза: {mic}"),
                (None, e) => format!("перекладу в паузі не буде: {}", e.unwrap_or_default()),
            };
            set_status(handle, &mut inner, Status { running: true, note: format!("Слухаю: {device}; {pause}"), error: false })
        }
        Msg::Error { message } => set_status(handle, &mut inner, Status { running: true, note: message, error: true }),
        Msg::Partial { .. } => {
            let _ = handle.emit("subtitle", &msg);
        }
        Msg::Final { at_ms, ref en, ref uk } => {
            if !en.is_empty() {
                let line = Line { at_ms, speaker: "them".into(), original: en.clone(), translation: uk.clone() };
                if let Err(e) = app.db().add_line(id, &line) {
                    eprintln!("meeting line: {e}");
                }
            }
            let _ = handle.emit("subtitle", &msg);
        }
        Msg::Said { partial, at_ms, ref uk, ref en } => {
            if !partial && !uk.is_empty() {
                let line = Line { at_ms, speaker: "me".into(), original: uk.clone(), translation: en.clone() };
                if let Err(e) = app.db().add_line(id, &line) {
                    eprintln!("meeting line: {e}");
                }
            }
            let _ = handle.emit("said", &msg);
        }
    }
}

/// The worker is gone: stopped by us, or it failed.
fn exited(handle: &AppHandle, id: i64) {
    let app = handle.state::<App>();
    let mut inner = app.meeting.lock();
    if inner.id != Some(id) {
        return;
    }
    inner.stdin = None;
    let code = inner.child.take().and_then(|mut c| c.wait().ok()).and_then(|s| s.code());
    let _ = app.db().end_meeting(Some(id), &now());
    inner.id = None;
    // Keep an error the worker reported; otherwise say it ended.
    let mut status = Status { running: false, ..inner.status.clone() };
    if !status.error {
        status.note = format!("Розпізнавання зупинилося (код {}). Подробиці в speech.log.", code.map_or("?".into(), |c| c.to_string()));
        status.error = true;
    }
    set_status(handle, &mut inner, status);
}

pub fn stop(handle: &AppHandle) {
    let app = handle.state::<App>();
    let mut inner = app.meeting.lock();
    inner.stdin = None;
    if let Some(mut child) = inner.child.take() {
        let _ = child.kill();
        let _ = child.wait();
    }
    if let Some(id) = inner.id.take() {
        let _ = app.db().end_meeting(Some(id), &now());
    }
    set_status(handle, &mut inner, Status::default());
}
