//! Audio in: WASAPI loopback of the speakers, the microphone, or wav files for tests.

use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    Device, SampleFormat, Stream, StreamConfig,
};
use rubato::{FftFixedInOut, Resampler};

use crate::{segmenter::RATE, Res};

pub fn name(d: &Device) -> String {
    d.description().map(|d| d.to_string()).unwrap_or_else(|_| "?".into())
}

/// Where the sound comes from.
pub enum Source {
    /// What the default output device plays: the other people in the call.
    Speakers,
    /// A microphone: the first whose name contains this, or the default one when empty.
    Mic(String),
    /// Test files, played at real speed.
    Files(Vec<PathBuf>),
}

/// A running capture. Dropping it stops the sound.
pub struct Capture {
    _stream: Option<Stream>,
    pub device: String,
    pub rate: u32,
}

/// Starts capturing; `sink` gets mono chunks at `Capture::rate` on an audio thread.
pub fn capture(source: &Source, mut sink: impl FnMut(Vec<f32>) + Send + 'static) -> Res<Capture> {
    let host = cpal::default_host();
    let (device, loopback) = match source {
        Source::Files(files) => {
            let rate = play(files, sink)?;
            return Ok(Capture { _stream: None, device: "test files".into(), rate });
        }
        Source::Speakers => (host.default_output_device().ok_or("Немає пристрою виводу звуку")?, true),
        Source::Mic(want) if want.is_empty() => (host.default_input_device().ok_or("Немає мікрофона")?, false),
        Source::Mic(want) => {
            let needle = want.to_lowercase();
            let found = host.input_devices()?.find(|d| name(d).to_lowercase().contains(&needle));
            (found.ok_or_else(|| format!("Немає мікрофона «{want}»"))?, false)
        }
    };
    let device_name = name(&device);
    if !loopback && device_name.to_lowercase().contains("cable") {
        // The call's side of the virtual cable: we would only hear ourselves.
        return Err(format!("Мікрофон «{device_name}» це віртуальний кабель. Обери справжній мікрофон у налаштуваннях.").into());
    }
    let supported = if loopback { device.default_output_config()? } else { device.default_input_config()? };
    if supported.sample_format() != SampleFormat::F32 {
        return Err(format!("{device_name}: формат звуку {} не підтримується", supported.sample_format()).into());
    }
    let config: StreamConfig = supported.into();
    let channels = config.channels as usize;
    let stream = device.build_input_stream(
        &config,
        move |data: &[f32], _: &_| sink(data.chunks(channels).map(|f| f.iter().sum::<f32>() / channels as f32).collect()),
        |e| eprintln!("audio stream: {e}"),
        None,
    )?;
    stream.play()?;
    Ok(Capture { _stream: Some(stream), device: device_name, rate: config.sample_rate })
}

/// Sends the files at playback speed, 10 ms at a time, with 1.5 s of silence after each.
/// Then nothing, like an idle output device. All files must share one format.
fn play(files: &[PathBuf], mut sink: impl FnMut(Vec<f32>) + Send + 'static) -> Res<u32> {
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
            sink(piece.to_vec());
        }
    });
    Ok(rate)
}

/// Device rate to 16 kHz, in whatever pieces the device delivers.
pub struct To16k {
    resampler: Option<FftFixedInOut<f32>>,
    pending: Vec<f32>,
}

impl To16k {
    pub fn new(rate: u32) -> Res<To16k> {
        let resampler = (rate as usize != RATE).then(|| FftFixedInOut::new(rate as usize, RATE, 1024, 1)).transpose()?;
        Ok(To16k { resampler, pending: vec![] })
    }

    pub fn push(&mut self, input: &[f32]) -> Res<Vec<f32>> {
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
