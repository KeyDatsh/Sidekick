//! Stage 0 prototype: real microphone -> virtual microphone, muted while a key is held.
//!
//! The assistant always hears the microphone; the call hears it only while the key is up.
//!
//!   mic-gate list
//!   mic-gate run --out "CABLE Input" [--in <mic name>] [--record pause.wav]   hold F8 to pause
//!   mic-gate selftest [--out <device name>]
//!
//! `selftest` replaces the microphone with a quiet tone, so it records nothing: it plays the
//! tone through the same path into an output device, listens to that device via WASAPI
//! loopback and measures the delay and how fast and completely the gate closes.

use std::{
    f32::consts::TAU,
    process::exit,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed},
        Arc, Mutex,
    },
    thread::sleep,
    time::{Duration, Instant},
};

use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    BufferSize, Device, SampleFormat, Stream, StreamConfig,
};
use rtrb::{Consumer, Producer, RingBuffer};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_F8};

type Res<T> = Result<T, Box<dyn std::error::Error>>;

/// Audio kept between the two devices. Less is faster but glitches sooner.
const TARGET_MS: f32 = 20.0;
/// Gate ramp: fast enough to be instant, slow enough not to click.
const RAMP_MS: f32 = 5.0;

#[derive(Default)]
struct Shared {
    held: AtomicBool,
    underruns: AtomicU32,
    overruns: AtomicU32,
    fill_ms: AtomicU32,
    heard_while_held: AtomicU64,
    recording: Mutex<Vec<f32>>,
}

fn name(d: &Device) -> String {
    // Not unique (two outputs can both be "Speakers"); the app should store device ids instead.
    d.description().map(|d| d.to_string()).unwrap_or_default()
}

fn find(devices: impl Iterator<Item = Device>, needle: &str) -> Option<Device> {
    let needle = needle.to_lowercase();
    devices
        .into_iter()
        .find(|d| name(d).to_lowercase().contains(&needle))
}

fn arg(flag: &str) -> Option<String> {
    let mut args = std::env::args();
    args.find(|a| a == flag)?;
    args.next()
}

fn stream_error(e: cpal::StreamError) {
    eprintln!("stream error: {e}");
}

/// Reads the ring, converts the rate, follows clock drift and applies the gate.
struct Output {
    ring: Consumer<f32>,
    shared: Arc<Shared>,
    in_rate: f32,
    out_rate: f32,
    channels: usize,
    pos: f32,
    cur: f32,
    next: f32,
    gain: f32,
    started: bool,
}

impl Output {
    fn fill(&mut self, out: &mut [f32]) {
        let target = self.in_rate * TARGET_MS / 1000.0;
        let available = self.ring.slots() as f32;
        self.shared
            .fill_ms
            .store((available * 1000.0 / self.in_rate) as u32, Relaxed);
        if !self.started {
            // Wait for enough audio for this callback plus the safety margin.
            let need = (out.len() / self.channels) as f32 * self.in_rate / self.out_rate;
            if available < target + need {
                out.fill(0.0);
                return;
            }
            self.started = true;
        }
        // The two devices have independent clocks: nudge the rate by at most 0.5 %.
        let drift = ((available - target) / target).clamp(-1.0, 1.0) * 0.005;
        let step = self.in_rate / self.out_rate * (1.0 + drift);
        let ramp = 1000.0 / (RAMP_MS * self.out_rate);
        let held = self.shared.held.load(Relaxed);
        for frame in out.chunks_mut(self.channels) {
            self.pos += step;
            while self.pos >= 1.0 {
                self.pos -= 1.0;
                self.cur = self.next;
                match self.ring.pop() {
                    Ok(s) => self.next = s,
                    Err(_) => {
                        self.next = 0.0;
                        if self.started {
                            self.started = false;
                            self.shared.underruns.fetch_add(1, Relaxed);
                        }
                    }
                }
            }
            let goal = if held { 0.0 } else { 1.0 };
            self.gain += (goal - self.gain).clamp(-ramp, ramp);
            let s = (self.cur + (self.next - self.cur) * self.pos) * self.gain;
            frame.fill(s);
        }
    }
}

fn open_output(dev: &Device, ring: Consumer<f32>, in_rate: u32, shared: Arc<Shared>) -> Res<Stream> {
    let supported = dev.default_output_config()?;
    if supported.sample_format() != SampleFormat::F32 {
        return Err(format!("{}: expected f32 output", name(dev)).into());
    }
    let cfg: StreamConfig = supported.into();
    println!("output: {} ({} Hz, {} ch)", name(dev), cfg.sample_rate, cfg.channels);
    let mut output = Output {
        ring,
        shared,
        in_rate: in_rate as f32,
        out_rate: cfg.sample_rate as f32,
        channels: cfg.channels as usize,
        pos: 0.0,
        cur: 0.0,
        next: 0.0,
        gain: 1.0,
        started: false,
    };
    Ok(dev.build_output_stream(&cfg, move |out: &mut [f32], _: &_| output.fill(out), stream_error, None)?)
}

/// Opens an input device (or the loopback of an output device) and hands over mono samples.
fn open_input(
    dev: &Device,
    loopback: bool,
    mut sink: impl FnMut(&[f32]) + Send + 'static,
) -> Res<(Stream, u32)> {
    let supported = if loopback { dev.default_output_config()? } else { dev.default_input_config()? };
    if supported.sample_format() != SampleFormat::F32 {
        return Err(format!("{}: expected f32 input", name(dev)).into());
    }
    let cfg = StreamConfig { buffer_size: BufferSize::Default, ..supported.into() };
    let channels = cfg.channels as usize;
    let mut mono = Vec::with_capacity(8192);
    let stream = dev.build_input_stream(
        &cfg,
        move |data: &[f32], _: &_| {
            mono.clear();
            mono.extend(data.chunks(channels).map(|f| f.iter().sum::<f32>() / channels as f32));
            sink(&mono);
        },
        stream_error,
        None,
    )?;
    Ok((stream, cfg.sample_rate))
}

fn push_all(ring: &mut Producer<f32>, samples: &[f32], shared: &Shared) {
    for &s in samples {
        if ring.push(s).is_err() {
            shared.overruns.fetch_add(1, Relaxed);
            break;
        }
    }
}

fn list() -> Res<()> {
    let host = cpal::default_host();
    println!("inputs:");
    for d in host.input_devices()? {
        println!("  {}", name(&d));
    }
    println!("outputs:");
    for d in host.output_devices()? {
        println!("  {}", name(&d));
    }
    Ok(())
}

fn run() -> Res<()> {
    let host = cpal::default_host();
    let out_name = arg("--out").unwrap_or("CABLE Input".into());
    let out = find(host.output_devices()?, &out_name)
        .ok_or_else(|| format!("no output device matching {out_name:?} (is VB-Cable installed?)"))?;
    let mic = match arg("--in") {
        Some(n) => find(host.input_devices()?, &n).ok_or_else(|| format!("no input device matching {n:?}"))?,
        None => host.default_input_device().ok_or("no microphone")?,
    };
    if name(&mic).to_lowercase().contains("cable") {
        return Err("the input is the virtual cable itself; pass the real microphone with --in".into());
    }
    let record = arg("--record");
    let recording = record.is_some();
    let shared = Arc::new(Shared::default());
    let (mut tx, rx) = RingBuffer::new(48_000);
    let s = shared.clone();
    let (input, rate) = open_input(&mic, false, move |mono| {
        push_all(&mut tx, mono, &s);
        if s.held.load(Relaxed) {
            s.heard_while_held.fetch_add(mono.len() as u64, Relaxed);
            if recording {
                if let Ok(mut r) = s.recording.lock() {
                    r.extend_from_slice(mono);
                }
            }
        }
    })?;
    println!("microphone: {} ({rate} Hz)", name(&mic));
    let output = open_output(&out, rx, rate, shared.clone())?;
    input.play()?;
    output.play()?;
    println!("hold F8 to pause the call microphone, Ctrl+C to quit");

    let mut last = Instant::now();
    loop {
        let held = unsafe { GetAsyncKeyState(VK_F8.0 as i32) } < 0;
        if held != shared.held.swap(held, Relaxed) {
            if held {
                println!("PAUSE: the call hears silence");
            } else {
                let heard = shared.heard_while_held.swap(0, Relaxed) as f32 / rate as f32;
                println!("LIVE: the assistant heard {heard:.1} s during the pause");
                if let Some(path) = &record {
                    save_wav(path, &std::mem::take(&mut *shared.recording.lock().unwrap()), rate)?;
                }
            }
        }
        if last.elapsed() > Duration::from_secs(10) {
            last = Instant::now();
            println!(
                "buffer {} ms, underruns {}, overruns {}",
                shared.fill_ms.load(Relaxed),
                shared.underruns.load(Relaxed),
                shared.overruns.load(Relaxed)
            );
        }
        sleep(Duration::from_millis(5));
    }
}

fn save_wav(path: &str, samples: &[f32], rate: u32) -> Res<()> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(path, spec)?;
    for &s in samples {
        w.write_sample(s)?;
    }
    w.finalize()?;
    println!("saved {path}");
    Ok(())
}

fn peak(x: &[f32]) -> f32 {
    x.iter().fold(0.0, |m, s| m.max(s.abs()))
}

fn db(x: f32) -> f32 {
    20.0 * x.max(1e-9).log10()
}

fn selftest() -> Res<()> {
    const RATE: u32 = 16_000; // deliberately different from the output rate
    const LEVEL: f32 = 0.003; // about -50 dBFS
    let host = cpal::default_host();
    let out = match arg("--out") {
        Some(n) => find(host.output_devices()?, &n).ok_or_else(|| format!("no output device matching {n:?}"))?,
        None => host.default_output_device().ok_or("no output device")?,
    };
    let shared = Arc::new(Shared::default());
    let heard: Arc<Mutex<Vec<f32>>> = Arc::default();

    let h = heard.clone();
    let (loopback, lb_rate) = open_input(&out, true, move |m| h.lock().unwrap().extend_from_slice(m))?;
    let (mut tx, rx) = RingBuffer::new(48_000);
    let output = open_output(&out, rx, RATE, shared.clone())?;
    loopback.play()?;
    output.play()?;

    // Tone "microphone": 10 ms packets in real time.
    let tone_on = Arc::new(AtomicBool::new(false));
    let (on, s) = (tone_on.clone(), shared.clone());
    std::thread::spawn(move || {
        let (start, mut sent) = (Instant::now(), 0u64);
        loop {
            let due = (start.elapsed().as_secs_f64() * RATE as f64) as u64;
            while sent + 160 <= due {
                let amp = if on.load(Relaxed) { LEVEL } else { 0.0 };
                let packet: Vec<f32> = (sent..sent + 160)
                    .map(|n| amp * (TAU * 440.0 * (n % RATE as u64) as f32 / RATE as f32).sin())
                    .collect();
                push_all(&mut tx, &packet, &s);
                sent += 160;
            }
            sleep(Duration::from_millis(1));
        }
    });
    // Silence sent by us keeps the loopback running, so its sample count is a clock.
    sleep(Duration::from_millis(1500));
    let len = || heard.lock().unwrap().len();
    let ms = |samples: usize| samples as f32 * 1000.0 / lb_rate as f32;

    let t_on = len();
    tone_on.store(true, Relaxed);
    sleep(Duration::from_secs(1));
    let mut closes = vec![];
    let mut opens = vec![];
    let mut residue = 0.0f32;
    for _ in 0..10 {
        let t = len();
        shared.held.store(true, Relaxed);
        sleep(Duration::from_millis(500));
        let part = heard.lock().unwrap()[t..].to_vec();
        let last = part.iter().rposition(|s| s.abs() > LEVEL * 0.02).unwrap_or(0);
        closes.push(ms(last));
        residue = residue.max(peak(&part[part.len() * 3 / 4..]));
        let t = len();
        shared.held.store(false, Relaxed);
        sleep(Duration::from_millis(500));
        let part = heard.lock().unwrap()[t..].to_vec();
        opens.push(ms(part.iter().position(|s| s.abs() > LEVEL * 0.02).unwrap_or(part.len())));
    }
    println!("soak: 20 s...");
    sleep(Duration::from_secs(20));

    let all = heard.lock().unwrap();
    let first = all[t_on..].iter().position(|s| s.abs() > LEVEL * 0.02);
    let live = peak(&all[all.len() - lb_rate as usize..]);
    println!("pass-through delay (source to loopback): {:.0} ms", first.map_or(f32::NAN, ms));
    let max = |v: &[f32]| v.iter().cloned().fold(0.0, f32::max);
    println!("gate closes within {:.0} ms, opens within {:.0} ms (worst of 10)", max(&closes), max(&opens));
    println!("level live {:.1} dBFS, held {:.1} dBFS", db(live), db(residue));
    println!(
        "underruns {}, overruns {}, buffer {} ms",
        shared.underruns.load(Relaxed),
        shared.overruns.load(Relaxed),
        shared.fill_ms.load(Relaxed)
    );
    Ok(())
}

fn main() {
    let result = match std::env::args().nth(1).as_deref() {
        Some("list") => list(),
        Some("run") => run(),
        Some("selftest") => selftest(),
        _ => Err("usage: mic-gate list | run --out <device> [--in <mic>] [--record file.wav] | selftest [--out <device>]".into()),
    };
    if let Err(e) = result {
        eprintln!("{e}");
        exit(1);
    }
}
