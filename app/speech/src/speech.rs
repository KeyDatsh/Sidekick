//! The models: one Whisper for both directions, Silero VAD, and two Opus-MT translators.

use std::path::Path;

use ct2rs::{tokenizers::sentencepiece::Tokenizer, ComputeType, Config, Device, TranslationOptions, Translator};
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState, WhisperVadContext,
    WhisperVadContextParams,
};

use crate::{
    segmenter::{RATE, WINDOW},
    Res,
};

pub const WHISPER: &str = "ggml-large-v3-turbo.bin";
pub const VAD: &str = "ggml-silero-v6.2.0.bin";
/// Subtitles, EN→UK. Covers several languages: the `>>ukr<<` prefix picks Ukrainian.
pub const EN_UK: &str = "opus-mt-tc-big-en-zle";
/// Pause mode, UK→EN.
pub const UK_EN: &str = "opus-mt-tc-big-zle-en";
pub const ALL: [&str; 4] = [WHISPER, VAD, EN_UK, UK_EN];

#[derive(Clone, Copy, PartialEq)]
pub enum Lang {
    En,
    Uk,
}

pub struct Speech {
    // `state` borrows from the context inside whisper.cpp; keep the context alive alongside.
    _whisper: WhisperContext,
    state: WhisperState,
    vad: WhisperVadContext,
    vad_path: String,
    en_uk: Translator<Tokenizer>,
    uk_en: Translator<Tokenizer>,
}

impl Speech {
    pub fn load(models: &Path) -> Res<Speech> {
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
        let config = Config { device: Device::CUDA, compute_type: ComputeType::INT8_FLOAT16, ..Config::default() };
        let translator = |name: &str| -> Res<Translator<Tokenizer>> {
            let dir = path(name);
            Ok(Translator::with_tokenizer(&dir, Tokenizer::new(&dir)?, &config)?)
        };
        let mut speech = Speech {
            _whisper: whisper,
            state,
            vad,
            vad_path: path(VAD),
            en_uk: translator(EN_UK)?,
            uk_en: translator(UK_EN)?,
        };
        // The first calls on the GPU are slow: get them out of the way before the meeting starts.
        for _ in 0..2 {
            speech.recognise(&vec![0.0; RATE * 2], Lang::En)?;
            speech.translate("Hello.", Lang::En)?;
            speech.translate("Привіт.", Lang::Uk)?;
        }
        Ok(speech)
    }

    /// Speech probability per Silero window of `audio`.
    pub fn vad(&mut self, audio: &[f32]) -> Res<Vec<f32>> {
        self.vad.detect_speech(audio)?;
        Ok(self.vad.probabilities().iter().take(audio.len() / WINDOW).copied().collect())
    }

    /// Text in `lang`. For Ukrainian (pause mode) Silero first drops the silence around the
    /// phrase: the key is held before and after speaking, and Whisper invents words for silence.
    pub fn recognise(&mut self, audio: &[f32], lang: Lang) -> Res<String> {
        let mut p = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        p.set_language(Some(if lang == Lang::En { "en" } else { "uk" }));
        p.set_no_timestamps(true);
        p.set_no_context(true);
        p.set_suppress_blank(true);
        // Drops "[Music]", "(laughs)" and the like.
        p.set_suppress_nst(true);
        p.set_print_progress(false);
        p.set_print_realtime(false);
        p.set_print_timestamps(false);
        if lang == Lang::Uk {
            p.set_vad_model_path(Some(&self.vad_path));
            p.enable_vad(true);
        }
        self.state.full(p, audio)?;
        let mut text = String::new();
        for segment in self.state.as_iter() {
            text.push_str(&segment.to_str_lossy()?);
        }
        Ok(clean(&text))
    }

    /// `from`: the language of `text`; the result is in the other one.
    pub fn translate(&self, text: &str, from: Lang) -> Res<String> {
        let options = TranslationOptions::<String, String> { beam_size: 2, ..Default::default() };
        let out = match from {
            Lang::En => self.en_uk.translate_batch_with_source_prefix(&[text], &[vec![">>ukr<<"]], &options, None)?,
            Lang::Uk => self.uk_en.translate_batch(&[text], &options, None)?,
        };
        Ok(out.into_iter().next().map(|r| r.0).unwrap_or_default())
    }
}

/// Whisper's output without what it adds to non-speech: bracketed notes and lone punctuation.
pub fn clean(text: &str) -> String {
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
