//! Sidekick's speech worker. A separate process, so that the app builds without CUDA, a crash
//! here does not take the tasks down, and the ~2.3 GB of video memory is free between meetings.
//!
//!   sidekick-speech --models <dir>
//!   sidekick-speech --models <dir> --wav a.wav b.wav …   files instead of the call, for testing
//!
//! Listens to the default output device (what the other people in the call say), cuts it into
//! phrases with Silero VAD, recognises them with Whisper and translates EN→UK with Opus-MT.
//! Writes one JSON object per line to stdout; logs go to stderr. Exits when stdin closes,
//! so it never outlives the app.

mod segmenter;

use std::{
    error::Error,
    io::{BufRead, Write},
    path::{Path, PathBuf},
    sync::mpsc,
    time::{Duration, Instant},
};

use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    SampleFormat, Stream, StreamConfig,
};
use ct2rs::{tokenizers::sentencepiece::Tokenizer, ComputeType, Config, Device, TranslationOptions, Translator};
use rubato::{FftFixedInOut, Resampler};
use segmenter::{Segmenter, Step, RATE, WINDOW};
use serde::Serialize;
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState, WhisperVadContext,
    WhisperVadContextParams,
};

type Res<T> = Result<T, Box<dyn Error>>;

const WHISPER: &str = "ggml-large-v3-turbo.bin";
const VAD: &str = "ggml-silero-v6.2.0.bin";
const TRANSLATOR: &str = "opus-mt-tc-big-en-zle";
/// Without audio for this long the output device is idle (WASAPI loopback sends nothing then):
/// count the gap as silence, or the last phrase would never end.
const IDLE: Duration = Duration::from_millis(150);

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Msg<'a> {
    Loading,
    Ready { device: &'a str },
    Partial { en: &'a str, uk: &'a str },
    /// `at_ms`: when the phrase started, since the worker started listening.
    Final { at_ms: u64, en: &'a str, uk: &'a str },
    Error { message: &'a str },
}

fn send(msg: Msg) {
    let mut out = std::io::stdout().lock();
    // Nobody to tell if stdout is gone; the stdin watcher ends the process.
    let _ = serde_json::to_writer(&mut out, &msg);
    let _ = out.write_all(b"\n");
    let _ = out.flush();
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

fn main() {
    let _ = log::set_logger(&Quiet);
    log::set_max_level(log::LevelFilter::Warn);
    whisper_rs::install_logging_hooks();
    let mut args = std::env::args().skip(1);
    let models = match (args.next().as_deref(), args.next()) {
        (Some("--models"), Some(dir)) => PathBuf::from(dir),
        _ => {
            eprintln!("usage: sidekick-speech --models <dir> [--wav <file>…]");
            std::process::exit(2);
        }
    };
    let wavs: Vec<PathBuf> = match args.next().as_deref() {
        Some("--wav") => args.map(PathBuf::from).collect(),
        _ => vec![],
    };
    std::thread::spawn(|| {
        // Commands will come here (the pause key); for now only the end of input matters.
        for line in std::io::stdin().lock().lines() {
            if line.is_err() {
                break;
            }
        }
        std::process::exit(0);
    });
    if let Err(e) = run(&models, &wavs) {
        eprintln!("{e}");
        send(Msg::Error { message: &e.to_string() });
        std::process::exit(1);
    }
}

fn run(models: &Path, wavs: &[PathBuf]) -> Res<()> {
    send(Msg::Loading);
    let missing: Vec<&str> = [WHISPER, VAD, TRANSLATOR].into_iter().filter(|f| !models.join(f).exists()).collect();
    if !missing.is_empty() {
        return Err(format!("У папці {} немає: {}", models.display(), missing.join(", ")).into());
    }
    let mut speech = Speech::load(models)?;
    let (tx, rx) = mpsc::channel::<Vec<f32>>();
    let (_stream, device, rate) = if wavs.is_empty() { listen(tx)? } else { (None, "test files".into(), play(wavs, tx)?) };
    eprintln!("listening to {device} at {rate} Hz");
    send(Msg::Ready { device: &device });

    let mut resample = To16k::new(rate)?;
    let mut seg = Segmenter::default();
    let started = Instant::now();
    let mut fed = 0u64;
    loop {
        match rx.recv_timeout(IDLE) {
            Ok(chunk) => {
                let mut all = resample.push(&chunk)?;
                // Take everything that queued up while the GPU was busy, then decide once.
                while let Ok(chunk) = rx.try_recv() {
                    all.extend(resample.push(&chunk)?);
                }
                fed += all.len() as u64;
                seg.push(&all);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let due = (started.elapsed().as_secs_f64() * RATE as f64) as u64;
                if due > fed {
                    let gap = vec![0.0; (due - fed) as usize];
                    fed = due;
                    seg.push(&gap);
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return Err("Звук пропав: пристрій вимкнено?".into()),
        }
        // Silero needs whole windows; a step every ~100 ms is plenty.
        if seg.fresh() < 3 * WINDOW {
            continue;
        }
        let probs = speech.vad(seg.tail())?;
        match seg.step(&probs) {
            Step::Wait => {}
            Step::Partial(audio) => {
                let en = speech.recognise(&audio)?;
                if !en.is_empty() {
                    send(Msg::Partial { en: &en, uk: &speech.translate(&en)? });
                }
            }
            Step::Final { audio, start } => {
                let t = Instant::now();
                let en = speech.recognise(&audio)?;
                let asr = t.elapsed();
                let uk = if en.is_empty() { String::new() } else { speech.translate(&en)? };
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

struct Speech {
    // `state` borrows from the context inside whisper.cpp; keep the context alive alongside.
    _whisper: WhisperContext,
    state: WhisperState,
    vad: WhisperVadContext,
    translator: Translator<Tokenizer>,
}

impl Speech {
    fn load(models: &Path) -> Res<Speech> {
        let path = |f: &str| models.join(f).to_string_lossy().into_owned();
        let mut params = WhisperContextParameters::default();
        params.flash_attn(true);
        let whisper = WhisperContext::new_with_params(&path(WHISPER), params)?;
        let state = whisper.create_state()?;
        let mut vad_params = WhisperVadContextParams::new();
        // Tiny model, called every ~100 ms: GPU launches would cost more than they save.
        vad_params.set_use_gpu(false);
        vad_params.set_n_threads(2);
        let vad = WhisperVadContext::new(&path(VAD), vad_params)?;
        let dir = path(TRANSLATOR);
        let config = Config { device: Device::CUDA, compute_type: ComputeType::INT8_FLOAT16, ..Config::default() };
        let translator = Translator::with_tokenizer(&dir, Tokenizer::new(&dir)?, &config)?;
        let mut speech = Speech { _whisper: whisper, state, vad, translator };
        // The first calls on the GPU are slow: get them out of the way before the meeting starts.
        for _ in 0..2 {
            speech.recognise(&vec![0.0; RATE * 2])?;
            speech.translate("Hello.")?;
        }
        Ok(speech)
    }

    fn vad(&mut self, audio: &[f32]) -> Res<Vec<f32>> {
        self.vad.detect_speech(audio)?;
        Ok(self.vad.probabilities().iter().take(audio.len() / WINDOW).copied().collect())
    }

    fn recognise(&mut self, audio: &[f32]) -> Res<String> {
        let mut p = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        p.set_language(Some("en"));
        p.set_no_timestamps(true);
        p.set_no_context(true);
        p.set_suppress_blank(true);
        // Drops "[Music]", "(laughs)" and the like.
        p.set_suppress_nst(true);
        p.set_print_progress(false);
        p.set_print_realtime(false);
        p.set_print_timestamps(false);
        self.state.full(p, audio)?;
        let mut text = String::new();
        for segment in self.state.as_iter() {
            text.push_str(&segment.to_str_lossy()?);
        }
        Ok(clean(&text))
    }

    fn translate(&self, en: &str) -> Res<String> {
        let options = TranslationOptions::<String, String> { beam_size: 2, ..Default::default() };
        // en-zle covers several languages: the prefix picks Ukrainian.
        let out = self.translator.translate_batch_with_source_prefix(&[en], &[vec![">>ukr<<"]], &options, None)?;
        Ok(out.into_iter().next().map(|r| r.0).unwrap_or_default())
    }
}

/// Whisper's output without what it adds to non-speech: bracketed notes and lone punctuation.
fn clean(text: &str) -> String {
    let mut out = String::new();
    let mut depth = 0;
    for c in text.chars() {
        match c {
            '[' | '(' => depth += 1,
            ']' | ')' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    let mut out = out.split_whitespace().collect::<Vec<_>>().join(" ");
    for p in [".", ",", "?", "!"] {
        out = out.replace(&format!(" {p}"), p);
    }
    if out.chars().any(char::is_alphanumeric) {
        out
    } else {
        String::new()
    }
}

/// Opens WASAPI loopback on the default output device; mono chunks go to `tx`.
fn listen(tx: mpsc::Sender<Vec<f32>>) -> Res<(Option<Stream>, String, u32)> {
    let host = cpal::default_host();
    let device = host.default_output_device().ok_or("Немає пристрою виводу звуку")?;
    let name = device.description().map(|d| d.to_string()).unwrap_or_else(|_| "?".into());
    let supported = device.default_output_config()?;
    if supported.sample_format() != SampleFormat::F32 {
        return Err(format!("{name}: формат звуку {} не підтримується", supported.sample_format()).into());
    }
    let config: StreamConfig = supported.into();
    let channels = config.channels as usize;
    let stream = device.build_input_stream(
        &config,
        move |data: &[f32], _: &_| {
            let mono = data.chunks(channels).map(|f| f.iter().sum::<f32>() / channels as f32).collect();
            let _ = tx.send(mono);
        },
        |e| eprintln!("audio stream: {e}"),
        None,
    )?;
    stream.play()?;
    Ok((Some(stream), name, config.sample_rate))
}

/// Sends the files at playback speed, 10 ms at a time, with 1.5 s of silence after each.
/// Then nothing, like an idle output device. All files must share one format.
fn play(files: &[PathBuf], tx: mpsc::Sender<Vec<f32>>) -> Res<u32> {
    let mut audio = vec![];
    let mut rate = 0;
    for file in files {
        let mut r = hound::WavReader::open(file).map_err(|e| format!("{}: {e}", file.display()))?;
        let spec = r.spec();
        rate = spec.sample_rate;
        let samples: Vec<f32> = match spec.sample_format {
            hound::SampleFormat::Float => r.samples::<f32>().collect::<Result<_, _>>()?,
            hound::SampleFormat::Int => {
                let scale = (1u32 << (spec.bits_per_sample - 1)) as f32;
                r.samples::<i32>().map(|s| s.map(|s| s as f32 / scale)).collect::<Result<_, _>>()?
            }
        };
        let ch = spec.channels as usize;
        audio.extend(samples.chunks(ch).map(|f| f.iter().sum::<f32>() / ch as f32));
        audio.extend(std::iter::repeat_n(0.0, rate as usize * 3 / 2));
    }
    let chunk = rate as usize / 100;
    std::thread::spawn(move || {
        let start = Instant::now();
        for (i, piece) in audio.chunks(chunk).enumerate() {
            let due = start + Duration::from_millis(i as u64 * 10);
            std::thread::sleep(due.saturating_duration_since(Instant::now()));
            if tx.send(piece.to_vec()).is_err() {
                return;
            }
        }
        // Keep the channel open: an idle device sends nothing but is still there.
        std::thread::sleep(Duration::MAX);
    });
    Ok(rate)
}

/// Device rate to 16 kHz, in whatever pieces the device delivers.
struct To16k {
    resampler: Option<FftFixedInOut<f32>>,
    pending: Vec<f32>,
}

impl To16k {
    fn new(rate: u32) -> Res<To16k> {
        let resampler = (rate as usize != RATE).then(|| FftFixedInOut::new(rate as usize, RATE, 1024, 1)).transpose()?;
        Ok(To16k { resampler, pending: vec![] })
    }

    fn push(&mut self, input: &[f32]) -> Res<Vec<f32>> {
        let Some(r) = &mut self.resampler else { return Ok(input.to_vec()) };
        self.pending.extend_from_slice(input);
        let mut out = vec![];
        while self.pending.len() >= r.input_frames_next() {
            let n = r.input_frames_next();
            let chunk = r.process(&[&self.pending[..n]], None)?;
            out.extend_from_slice(&chunk[0]);
            self.pending.drain(..n);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::clean;

    #[test]
    fn whisper_noise_notes_are_dropped() {
        assert_eq!(clean(" [Music] Hello  there (laughs)."), "Hello there.");
        assert_eq!(clean(" [BLANK_AUDIO]"), "");
        assert_eq!(clean(" ..."), "");
    }
}
