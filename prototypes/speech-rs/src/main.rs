//! Stage 0 prototype: whisper-rs (CUDA) and ct2rs in one process, as the app will use them.
//!
//!   speech-rs <uk|en> <whisper model> <Opus-MT dir> <folder with 16 kHz wav files> [count] [cpu]

use std::{error::Error, fs, path::Path, time::Instant};

use ct2rs::{tokenizers::sentencepiece::Tokenizer, ComputeType, Config, Device, TranslationOptions, Translator};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

type Res<T> = Result<T, Box<dyn Error>>;

fn read_wav(path: &Path) -> Res<Vec<f32>> {
    let mut r = hound::WavReader::open(path)?;
    let spec = r.spec();
    if spec.sample_rate != 16_000 || spec.channels != 1 {
        return Err(format!("{}: expected 16 kHz mono", path.display()).into());
    }
    Ok(match spec.sample_format {
        hound::SampleFormat::Float => r.samples::<f32>().collect::<Result<_, _>>()?,
        hound::SampleFormat::Int => r
            .samples::<i32>()
            .map(|s| s.map(|s| s as f32 / (1u32 << (spec.bits_per_sample - 1)) as f32))
            .collect::<Result<_, _>>()?,
    })
}

fn summary(label: &str, mut ms: Vec<f64>) {
    ms.sort_by(f64::total_cmp);
    let p = |q: f64| ms[((ms.len() - 1) as f64 * q) as usize];
    println!("{label:12} p50={:.0} ms  p95={:.0} ms", p(0.5), p(0.95));
}

fn main() -> Res<()> {
    let args: Vec<String> = std::env::args().collect();
    let [_, lang, model, mt_dir, wavs, rest @ ..] = args.as_slice() else {
        return Err("usage: speech-rs <uk|en> <whisper model> <Opus-MT dir> <wav folder> [count] [cpu]".into());
    };
    let count: usize = rest.first().map_or(Ok(60), |n| n.parse())?;

    let mut cparams = WhisperContextParameters::default();
    cparams.flash_attn(true);
    let ctx = WhisperContext::new_with_params(model, cparams)?;
    let mut state = ctx.create_state()?;
    let config = if rest.get(1).is_some_and(|a| a == "cpu") {
        Config { num_threads_per_replica: 4, ..Config::default() }
    } else {
        Config { device: Device::CUDA, compute_type: ComputeType::INT8_FLOAT16, ..Config::default() }
    };
    let translator = Translator::with_tokenizer(mt_dir, Tokenizer::new(mt_dir)?, &config)?;
    let options = TranslationOptions::<String, String> { beam_size: 2, ..Default::default() };
    // Only the multi-target en-zle model needs to be told the target language.
    let prefix: Vec<&str> = if mt_dir.contains("en-zle") { vec![">>ukr<<"] } else { vec![] };

    let mut asr = |x: &[f32]| -> Res<String> {
        let mut p = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        p.set_language(Some(lang));
        p.set_no_timestamps(true);
        p.set_no_context(true);
        p.set_print_progress(false);
        p.set_print_realtime(false);
        p.set_print_timestamps(false);
        state.full(p, x)?;
        let mut text = String::new();
        for segment in state.as_iter() {
            text.push_str(&segment.to_str_lossy()?);
        }
        Ok(text.trim().to_owned())
    };
    let translate = |text: &str| -> Res<String> {
        let out = translator.translate_batch_with_source_prefix(&[text], &[prefix.clone()], &options, None)?;
        Ok(out.into_iter().next().map(|r| r.0).unwrap_or_default())
    };

    for _ in 0..3 {
        asr(&vec![0.0; 48_000])?;
        translate("test")?;
    }

    let mut files: Vec<_> = fs::read_dir(wavs)?.map(|e| e.map(|e| e.path())).collect::<Result<_, _>>()?;
    files.sort();
    let (mut t_asr, mut t_mt, mut t_all) = (vec![], vec![], vec![]);
    for (i, file) in files.iter().take(count).enumerate() {
        let x = read_wav(file)?;
        let start = Instant::now();
        let text = asr(&x)?;
        let a = start.elapsed().as_secs_f64() * 1000.0;
        let translated = translate(&text)?;
        let all = start.elapsed().as_secs_f64() * 1000.0;
        t_asr.push(a);
        t_mt.push(all - a);
        t_all.push(all);
        if i < 3 {
            println!("{text}\n  -> {translated}");
        }
    }
    summary("recognition", t_asr);
    summary("translation", t_mt);
    summary("total", t_all);
    Ok(())
}
