//! The call's microphone: real microphone → VB-Cable ("CABLE Input"), silent while the pause key
//! is held. Lives in the app, not in the speech worker, so the call keeps its microphone whether
//! or not a meeting is running and even if recognition crashes. Ported from prototypes/mic-gate
//! (measured there: ~40 ms through, mute within ~27 ms).

use std::sync::{
    atomic::{AtomicBool, AtomicU32, Ordering::Relaxed},
    Arc, Mutex,
};

use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    BufferSize, Device, SampleFormat, Stream, StreamConfig,
};
use rtrb::{Consumer, Producer, RingBuffer};
use serde::Serialize;

/// Audio kept between the two devices. Less is faster but glitches sooner.
const TARGET_MS: f32 = 20.0;
/// Gate ramp: fast enough to be instant, slow enough not to click.
const RAMP_MS: f32 = 5.0;

#[derive(Default)]
struct Shared {
    held: AtomicBool,
    underruns: AtomicU32,
    /// Set by the stream error callbacks, e.g. when a device is unplugged.
    failed: AtomicBool,
}

#[derive(Serialize, Clone, Default, Debug)]
pub struct Status {
    pub on: bool,
    /// The devices, or why the gate is off.
    pub note: String,
}

#[derive(Serialize)]
pub struct Devices {
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
}

pub fn devices() -> Devices {
    let host = cpal::default_host();
    Devices {
        inputs: host.input_devices().map(|d| d.map(|d| name(&d)).collect()).unwrap_or_default(),
        outputs: host.output_devices().map(|d| d.map(|d| name(&d)).collect()).unwrap_or_default(),
    }
}

fn name(d: &Device) -> String {
    d.description().map(|d| d.to_string()).unwrap_or_default()
}

fn find(devices: impl Iterator<Item = Device>, needle: &str) -> Option<Device> {
    let needle = needle.to_lowercase();
    devices.into_iter().find(|d| name(d).to_lowercase().contains(&needle))
}

struct Running {
    _input: Stream,
    _output: Stream,
    shared: Arc<Shared>,
}

// cpal streams are not Send on every platform; on WASAPI they are only touched under the mutex.
unsafe impl Send for Running {}

#[derive(Default)]
pub struct Gate {
    running: Mutex<Option<Running>>,
    status: Mutex<Status>,
}

impl Gate {
    /// (Re)starts with these devices. `cable`: part of the VB-Cable output's name;
    /// `mic`: part of the microphone's name, empty for the default one.
    pub fn start(&self, cable: &str, mic: &str) -> Status {
        let mut running = self.running.lock().unwrap_or_else(|e| e.into_inner());
        *running = None;
        let status = match open(cable, mic) {
            Ok((r, note)) => {
                *running = Some(r);
                Status { on: true, note }
            }
            Err(note) => Status { on: false, note },
        };
        *self.status.lock().unwrap_or_else(|e| e.into_inner()) = status.clone();
        status
    }

    pub fn status(&self) -> Status {
        let running = self.running.lock().unwrap_or_else(|e| e.into_inner());
        let mut status = self.status.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if running.as_ref().is_some_and(|r| r.shared.failed.load(Relaxed)) {
            status = Status { on: false, note: "Пристрій звуку зник. Перевір мікрофон і збережи налаштування ще раз.".into() };
        }
        status
    }

    pub fn hold(&self, held: bool) {
        if let Some(r) = &*self.running.lock().unwrap_or_else(|e| e.into_inner()) {
            r.shared.held.store(held, Relaxed);
        }
    }
}

fn open(cable: &str, mic: &str) -> Result<(Running, String), String> {
    let host = cpal::default_host();
    let e = |e: &dyn std::fmt::Display| e.to_string();
    let out = find(host.output_devices().map_err(|x| e(&x))?, cable)
        .ok_or_else(|| format!("Немає пристрою «{cable}»: VB-Cable не встановлено? Режим паузи вимкнено."))?;
    let input = match mic.trim() {
        "" => host.default_input_device().ok_or("Немає мікрофона")?,
        m => find(host.input_devices().map_err(|x| e(&x))?, m).ok_or_else(|| format!("Немає мікрофона «{m}»"))?,
    };
    if name(&input).to_lowercase().contains("cable") {
        return Err(format!(
            "Мікрофон за замовчуванням «{}» це сам VB-Cable. Обери справжній мікрофон у налаштуваннях.",
            name(&input)
        ));
    }
    let shared = Arc::new(Shared::default());
    let (tx, rx) = RingBuffer::new(48_000);
    let (input_stream, rate) = open_input(&input, tx, shared.clone()).map_err(|x| e(&x))?;
    let output_stream = open_output(&out, rx, rate, shared.clone()).map_err(|x| e(&x))?;
    input_stream.play().map_err(|x| e(&x))?;
    output_stream.play().map_err(|x| e(&x))?;
    let note = format!("{} → {}", name(&input), name(&out));
    Ok((Running { _input: input_stream, _output: output_stream, shared }, note))
}

fn open_input(dev: &Device, mut ring: Producer<f32>, shared: Arc<Shared>) -> Result<(Stream, u32), Box<dyn std::error::Error>> {
    let supported = dev.default_input_config()?;
    if supported.sample_format() != SampleFormat::F32 {
        return Err(format!("{}: формат {} не підтримується", name(dev), supported.sample_format()).into());
    }
    let cfg = StreamConfig { buffer_size: BufferSize::Default, ..supported.into() };
    let channels = cfg.channels as usize;
    let failed = shared.clone();
    let stream = dev.build_input_stream(
        &cfg,
        move |data: &[f32], _: &_| {
            for frame in data.chunks(channels) {
                // A full ring means the output stopped pulling; dropping is all we can do.
                let _ = ring.push(frame.iter().sum::<f32>() / channels as f32);
            }
        },
        move |err| {
            eprintln!("microphone: {err}");
            failed.failed.store(true, Relaxed);
        },
        None,
    )?;
    Ok((stream, cfg.sample_rate))
}

fn open_output(dev: &Device, ring: Consumer<f32>, in_rate: u32, shared: Arc<Shared>) -> Result<Stream, Box<dyn std::error::Error>> {
    let supported = dev.default_output_config()?;
    if supported.sample_format() != SampleFormat::F32 {
        return Err(format!("{}: формат {} не підтримується", name(dev), supported.sample_format()).into());
    }
    let cfg: StreamConfig = supported.into();
    let failed = shared.clone();
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
    Ok(dev.build_output_stream(
        &cfg,
        move |out: &mut [f32], _: &_| output.fill(out),
        move |err| {
            eprintln!("virtual cable: {err}");
            failed.failed.store(true, Relaxed);
        },
        None,
    )?)
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
