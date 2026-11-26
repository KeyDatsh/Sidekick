//! Sidekick's speech worker. A separate process, so that the app builds without CUDA, a crash
//! here does not take the tasks down, and the ~2.5 GB of video memory is free between meetings.
//!
//!   sidekick-speech --models <dir> [--mic <name>]
//!   … [--mic-wav <file>] [--wav <file>…]      files instead of the microphone / the call, for tests
//!
//! Subtitles: listens to the default output device (the other people in the call), cuts it into
//! phrases with Silero VAD, recognises them with Whisper and translates EN→UK.
//! Pause mode: also listens to the microphone and keeps what is said while the pause key is held
//! (`pause_down` / `pause_up` lines on stdin), then recognises it in Ukrainian and translates UK→EN.
//! Muting the call is not done here but in the app, so the call keeps its microphone without us.
//!
//! Writes one JSON object per line to stdout; logs go to stderr. Exits when stdin closes,
//! so it never outlives the app.

mod audio;
mod segmenter;
mod speech;

use std::{
    error::Error,
    io::{BufRead, Write},
    path::PathBuf,
    sync::mpsc::{self, RecvTimeoutError, Sender},
    time::{Duration, Instant},
};

use audio::{Capture, Source, To16k};
use segmenter::{Segmenter, Step, RATE, WINDOW};
use serde::Serialize;
use speech::{Lang, Speech};

type Res<T> = Result<T, Box<dyn Error>>;

/// Without call audio for this long the output device is idle (WASAPI loopback sends nothing then):
/// count the gap as silence, or the last phrase would never end.
const IDLE: Duration = Duration::from_millis(150);
/// Microphone audio kept from before the key goes down: people start talking as they press it.
const PREROLL: usize = RATE * 3 / 10;
/// Shorter than this is an accidental tap.
const MIN_PAUSE: usize = RATE * 3 / 10;
/// How often what is said during a pause is recognised for the preview.
const PAUSE_PARTIAL_EVERY: usize = RATE;

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Msg<'a> {
    Loading,
    /// `mic`: the microphone for pause mode; else `mic_error` says why (subtitles work anyway).
    Ready { device: &'a str, mic: Option<&'a str>, mic_error: Option<&'a str> },
    Partial { en: &'a str, uk: &'a str },
    /// `at_ms`: when the phrase started, since the worker started listening.
    Final { at_ms: u64, en: &'a str, uk: &'a str },
    /// Said during a pause. `partial`: the key is still held.
    Said { partial: bool, at_ms: u64, uk: &'a str, en: &'a str },
    Error { message: &'a str },
}

fn send(msg: Msg) {
    let mut out = std::io::stdout().lock();
    // Nobody to tell if stdout is gone; the stdin watcher ends the process.
    let _ = serde_json::to_writer(&mut out, &msg);
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

enum Event {
    Call(Vec<f32>),
    Mic(Vec<f32>),
    Down,
    Up,
}

/// whisper.cpp logs several lines per VAD call (ten a second): keep only warnings and errors.
struct Quiet;

impl log::Log for Quiet {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= log::Level::Warn
    }
    fn log(&self, r: &log::Record) {
        if self.enabled(r.metadata()) {
            eprintln!("{}: {}", r.level(), r.args().to_string().trim_end());
        }
    }
    fn flush(&self) {}
}

struct Args {
    models: PathBuf,
    call: Source,
    mic: Source,
}

fn args() -> Option<Args> {
    let mut a = Args { models: PathBuf::new(), call: Source::Speakers, mic: Source::Mic(String::new()) };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--models" => a.models = it.next()?.into(),
            "--mic" => a.mic = Source::Mic(it.next()?),
            "--mic-wav" => a.mic = Source::Files(vec![it.next()?.into()]),
            "--wav" => a.call = Source::Files(it.by_ref().map(PathBuf::from).collect()),
            _ => return None,
        }
    }
    (!a.models.as_os_str().is_empty()).then_some(a)
}

fn main() {
    let _ = log::set_logger(&Quiet);
    log::set_max_level(log::LevelFilter::Warn);
    whisper_rs::install_logging_hooks();
    let Some(args) = args() else {
        eprintln!("usage: sidekick-speech --models <dir> [--mic <name> | --mic-wav <file>] [--wav <file>…]");
        std::process::exit(2);
    };
    let (tx, rx) = mpsc::channel();
    let commands = tx.clone();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            // A BOM shows up when a .NET or PowerShell client writes the first line.
            let _ = match line.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}') {
                "pause_down" => commands.send(Event::Down),
                "pause_up" => commands.send(Event::Up),
                other => Ok(eprintln!("unknown command: {other}")),
            };
        }
        std::process::exit(0);
    });
    if let Err(e) = run(&args, tx, rx) {
        eprintln!("{e}");
        send(Msg::Error { message: &e.to_string() });
        std::process::exit(1);
    }
}

fn open(source: &Source, tx: &Sender<Event>, wrap: fn(Vec<f32>) -> Event) -> Res<Capture> {
    let tx = tx.clone();
    audio::capture(source, move |chunk| {
        let _ = tx.send(wrap(chunk));
    })
}

fn run(args: &Args, tx: Sender<Event>, rx: mpsc::Receiver<Event>) -> Res<()> {
    send(Msg::Loading);
    let missing: Vec<&str> = speech::ALL.into_iter().filter(|f| !args.models.join(f).exists()).collect();
    if !missing.is_empty() {
        return Err(format!("У папці {} немає: {}", args.models.display(), missing.join(", ")).into());
    }
    let mut speech = Speech::load(&args.models)?;
    let call = open(&args.call, &tx, Event::Call)?;
    let mic = open(&args.mic, &tx, Event::Mic);
    drop(tx);
    eprintln!("listening to {} at {} Hz", call.device, call.rate);
    let mic_note = match &mic {
        Ok(m) => {
            eprintln!("microphone {} at {} Hz", m.device, m.rate);
            Ok(m.device.clone())
        }
        Err(e) => {
            eprintln!("no microphone: {e}");
            Err(e.to_string())
        }
    };
    send(Msg::Ready { device: &call.device, mic: mic_note.as_deref().ok(), mic_error: mic_note.as_ref().err().map(String::as_str) });

    let mut call16 = To16k::new(call.rate)?;
    let mut mic16 = mic.as_ref().map_or(Ok(None), |m| To16k::new(m.rate).map(Some))?;
    let mut seg = Segmenter::default();
    let mut pause = Pause::default();
    let started = Instant::now();
    let at_ms = || started.elapsed().as_millis() as u64;
    let (mut fed, mut last_call) = (0u64, Instant::now());
    loop {
        // Everything that queued up while the GPU was busy, then decide once.
        let mut events = vec![];
        match rx.recv_timeout(IDLE) {
            Ok(e) => events.push(e),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Err("Звук пропав: пристрій вимкнено?".into()),
        }
        events.extend(rx.try_iter());
        let mut said = vec![];
        for event in events {
            match event {
                Event::Call(chunk) => {
                    let s = call16.push(&chunk)?;
                    fed += s.len() as u64;
                    last_call = Instant::now();
                    seg.push(&s);
                }
                Event::Mic(chunk) => {
                    if let Some(r) = &mut mic16 {
                        pause.push(&r.push(&chunk)?);
                    }
                }
                Event::Down => pause.down(at_ms()),
                Event::Up => said.extend(pause.up()),
            }
        }
        if last_call.elapsed() >= IDLE {
            let due = (started.elapsed().as_secs_f64() * RATE as f64) as u64;
            if due > fed {
                seg.push(&vec![0.0; (due - fed) as usize]);
                fed = due;
            }
        }

        // Pause first: the user is waiting for it with the call on hold.
        for (audio, at) in said {
            let t = Instant::now();
            let uk = speech.recognise(&audio, Lang::Uk)?;
            let en = if uk.is_empty() { String::new() } else { speech.translate(&uk, Lang::Uk)? };
            eprintln!("{:.1} s pause: {} ms", audio.len() as f32 / RATE as f32, t.elapsed().as_millis());
            send(Msg::Said { partial: false, at_ms: at, uk: &uk, en: &en });
        }
        if let Some((audio, at)) = pause.partial() {
            let uk = speech.recognise(&audio, Lang::Uk)?;
            if !uk.is_empty() {
                send(Msg::Said { partial: true, at_ms: at, uk: &uk, en: &speech.translate(&uk, Lang::Uk)? });
            }
        }

        // Silero needs whole windows; a step every ~100 ms is plenty.
        if seg.fresh() < 3 * WINDOW {
            continue;
        }
        let probs = speech.vad(seg.tail())?;
        match seg.step(&probs) {
            Step::Wait => {}
            Step::Partial(audio) => {
                let en = speech.recognise(&audio, Lang::En)?;
                if !en.is_empty() {
                    send(Msg::Partial { en: &en, uk: &speech.translate(&en, Lang::En)? });
                }
            }
            Step::Final { audio, start } => {
                let t = Instant::now();
                let en = speech.recognise(&audio, Lang::En)?;
                let asr = t.elapsed();
                let uk = if en.is_empty() { String::new() } else { speech.translate(&en, Lang::En)? };
                eprintln!(
                    "{:.1} s phrase: recognition {} ms, translation {} ms",
                    audio.len() as f32 / RATE as f32,
                    asr.as_millis(),
                    (t.elapsed() - asr).as_millis()
                );
                // An empty final still goes out: it clears the grey preview.
                send(Msg::Final { at_ms: start * 1000 / RATE as u64, en: &en, uk: &uk });
            }
        }
    }
}

/// The microphone while the pause key is held, with a little from before.
#[derive(Default)]
struct Pause {
    preroll: Vec<f32>,
    /// Some while the key is held, with when it went down.
    held: Option<(Vec<f32>, u64)>,
    partial_at: usize,
}

impl Pause {
    fn push(&mut self, samples: &[f32]) {
        match &mut self.held {
            Some((audio, _)) => audio.extend_from_slice(samples),
            None => {
                self.preroll.extend_from_slice(samples);
                let extra = self.preroll.len().saturating_sub(PREROLL);
                self.preroll.drain(..extra);
            }
        }
    }

    fn down(&mut self, at_ms: u64) {
        if self.held.is_none() {
            self.held = Some((std::mem::take(&mut self.preroll), at_ms));
            self.partial_at = 0;
        }
    }

    /// The finished recording, unless it was a tap.
    fn up(&mut self) -> Option<(Vec<f32>, u64)> {
        let (audio, at) = self.held.take()?;
        (audio.len() >= PREROLL + MIN_PAUSE).then_some((audio, at))
    }

    /// The recording so far, about once a second while the key is held.
    fn partial(&mut self) -> Option<(Vec<f32>, u64)> {
        let (audio, at) = self.held.as_ref()?;
        if audio.len() < PREROLL + PAUSE_PARTIAL_EVERY || audio.len() < self.partial_at + PAUSE_PARTIAL_EVERY {
            return None;
        }
        self.partial_at = audio.len();
        Some((audio.clone(), *at))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pause_keeps_the_preroll_and_drops_taps() {
        let mut p = Pause::default();
        p.push(&vec![1.0; RATE]);
        p.down(5);
        p.push(&vec![2.0; RATE / 10]);
        assert!(p.up().is_none(), "a tap");
        p.push(&vec![3.0; RATE]);
        p.down(7);
        assert!(p.partial().is_none());
        p.push(&vec![4.0; RATE]);
        let (partial, _) = p.partial().unwrap();
        assert!(p.partial().is_none(), "once a second");
        let (audio, at) = p.up().unwrap();
        assert_eq!((audio.len(), at), (PREROLL + RATE, 7));
        assert_eq!(audio, partial);
        assert_eq!(audio[0], 3.0);
        assert!(p.up().is_none());
    }
}
