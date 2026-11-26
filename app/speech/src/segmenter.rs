//! Cuts a continuous 16 kHz stream into phrases from Silero VAD probabilities.
//!
//! The caller feeds audio with [`Segmenter::push`], runs the VAD over [`Segmenter::tail`] and
//! hands the probabilities to [`Segmenter::step`], which says what to recognise, if anything.
//! Pure logic: no models here, so it is tested with made-up probabilities.

pub const RATE: usize = 16_000;
/// Samples per Silero window at 16 kHz.
pub const WINDOW: usize = 512;

const fn ms(n: usize) -> usize {
    n * RATE / 1000
}

/// Speech starts above this probability.
const START: f32 = 0.5;
/// And counts as over below this one (hysteresis, so a phrase is not split on every soft sound).
const STOP: f32 = 0.35;
/// Silence that ends a phrase. Longer waits less often mid-sentence but delays every subtitle.
const END_SILENCE: usize = ms(600);
/// Audio kept before the first voiced window: Silero reacts a little late to soft onsets.
const PREROLL: usize = ms(300);
/// Audio kept after the last voiced window.
const PAD: usize = ms(150);
/// A phrase longer than this is cut at its quietest point, so a monologue still gets subtitles.
const MAX_PHRASE: usize = ms(12_000);
/// Where to look for that quietest point: the end of the phrase.
const CUT_SEARCH: usize = ms(3_000);
/// Less voiced audio than this is a click or a cough: Whisper would make up words for it.
const MIN_VOICED: usize = ms(250);
/// How often the unfinished phrase is recognised for the grey preview.
const PARTIAL_EVERY: usize = ms(1_000);
/// How much audio the VAD sees on each step. Silero restarts its state on every call.
pub const VAD_CONTEXT: usize = ms(2_048);

#[derive(Debug, PartialEq)]
pub enum Step {
    Wait,
    /// The phrase so far, still being spoken.
    Partial(Vec<f32>),
    /// A finished phrase, and where it started, in samples since the beginning of the stream.
    Final { audio: Vec<f32>, start: u64 },
}

#[derive(Default)]
pub struct Segmenter {
    /// Audio not yet given away as a final phrase.
    audio: Vec<f32>,
    /// Stream position of `audio[0]`.
    offset: u64,
    /// Start of the current phrase in `audio`, None while nobody speaks.
    phrase: Option<usize>,
    /// Voiced audio in the current phrase.
    voiced: usize,
    /// `audio.len()` at the previous step: windows before it were already counted.
    seen: usize,
    /// `audio.len()` at the previous partial.
    partial_at: usize,
}

impl Segmenter {
    pub fn push(&mut self, samples: &[f32]) {
        self.audio.extend_from_slice(samples);
    }

    /// Audio to run the VAD over: whole windows, ending at the newest sample.
    pub fn tail(&self) -> &[f32] {
        let n = self.audio.len().min(VAD_CONTEXT) / WINDOW * WINDOW;
        &self.audio[self.audio.len() - n..]
    }

    /// Samples since the previous step.
    pub fn fresh(&self) -> usize {
        self.audio.len() - self.seen
    }

    /// `probs`: one per window of [`Segmenter::tail`], taken just before this call.
    pub fn step(&mut self, probs: &[f32]) -> Step {
        let len = self.audio.len();
        let first = len - probs.len() * WINDOW;
        let at = |i: usize| first + i * WINDOW;
        let seen = std::mem::replace(&mut self.seen, len);

        let start = match self.phrase {
            Some(start) => start,
            None => {
                let Some(i) = probs.iter().position(|&p| p >= START) else {
                    self.trim();
                    return Step::Wait;
                };
                let start = at(i).saturating_sub(PREROLL);
                self.phrase = Some(start);
                self.voiced = 0;
                self.partial_at = len;
                start
            }
        };

        // Count voiced windows once, although the VAD sees them on several steps.
        self.voiced += probs
            .iter()
            .enumerate()
            .filter(|&(i, &p)| p >= STOP && at(i) + WINDOW > seen && at(i) >= start)
            .count()
            * WINDOW;

        let silent = probs.iter().rev().take_while(|&&p| p < STOP).count() * WINDOW;
        if silent >= END_SILENCE {
            let end = (len - silent + PAD).min(len);
            return self.finish(start, end, None);
        }
        if len - start >= MAX_PHRASE {
            // The quietest window near the end; the newest one wins a tie.
            let from = probs.len().saturating_sub(CUT_SEARCH / WINDOW);
            let quiet = (from..probs.len())
                .rev()
                .min_by(|&a, &b| probs[a].total_cmp(&probs[b]))
                .map_or(len, |i| at(i) + WINDOW / 2);
            let carried = probs.iter().enumerate().filter(|&(i, &p)| p >= STOP && at(i) >= quiet).count() * WINDOW;
            let step = self.finish(start, quiet.max(start + WINDOW), Some(quiet));
            self.voiced = carried;
            return step;
        }
        if len - self.partial_at >= PARTIAL_EVERY && self.voiced >= MIN_VOICED {
            self.partial_at = len;
            return Step::Partial(self.audio[start..].to_vec());
        }
        Step::Wait
    }

    /// Gives away `audio[start..end]`. `next`: the phrase goes on from there (a forced cut).
    fn finish(&mut self, start: usize, end: usize, next: Option<usize>) -> Step {
        let voiced = std::mem::take(&mut self.voiced);
        let audio = self.audio[start..end].to_vec();
        let stream_start = self.offset + start as u64;
        let keep = next.unwrap_or(end);
        self.audio.drain(..keep);
        self.offset += keep as u64;
        self.seen = self.seen.saturating_sub(keep);
        self.phrase = next.map(|_| 0);
        self.partial_at = self.audio.len();
        if voiced < MIN_VOICED {
            return Step::Wait;
        }
        Step::Final { audio, start: stream_start }
    }

    /// While nobody speaks, keep only what the VAD and the preroll need.
    fn trim(&mut self) {
        let keep = VAD_CONTEXT + PREROLL;
        if self.audio.len() > keep * 2 {
            let cut = (self.audio.len() - keep) / WINDOW * WINDOW;
            self.audio.drain(..cut);
            self.offset += cut as u64;
            self.seen -= cut;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds `windows` windows of audio with probability `p`, a step every 8 windows (~256 ms),
    /// with a fake VAD that remembers the probability of every window it was given.
    struct Feed {
        seg: Segmenter,
        probs: Vec<f32>,
        steps: Vec<Step>,
    }

    impl Feed {
        fn new() -> Feed {
            Feed { seg: Segmenter::default(), probs: vec![], steps: vec![] }
        }

        fn run(&mut self, windows: usize, p: f32) -> &mut Self {
            for _ in 0..windows {
                self.seg.push(&[p; WINDOW]);
                self.probs.push(p);
                if self.seg.fresh() >= 8 * WINDOW {
                    // Every sample carries its window's probability, so the fake VAD just reads it back.
                    let probs: Vec<f32> = self.seg.tail().chunks(WINDOW).map(|w| w[0]).collect();
                    let step = self.seg.step(&probs);
                    if step != Step::Wait {
                        self.steps.push(step);
                    }
                }
            }
            self
        }

        fn finals(&self) -> Vec<(usize, u64)> {
            self.steps
                .iter()
                .filter_map(|s| match s {
                    Step::Final { audio, start } => Some((audio.len(), *start)),
                    _ => None,
                })
                .collect()
        }

        fn partials(&self) -> usize {
            self.steps.iter().filter(|s| matches!(s, Step::Partial(_))).count()
        }
    }

    const fn windows(ms: usize) -> usize {
        ms * RATE / 1000 / WINDOW
    }

    #[test]
    fn silence_gives_nothing_and_does_not_grow() {
        let mut f = Feed::new();
        f.run(windows(60_000), 0.0);
        assert!(f.steps.is_empty());
        assert!(f.seg.audio.len() <= (VAD_CONTEXT + PREROLL) * 2 + 8 * WINDOW);
    }

    #[test]
    fn one_phrase_is_cut_after_the_pause_with_preroll_and_pad() {
        let mut f = Feed::new();
        f.run(windows(5_000), 0.0).run(windows(2_000), 0.9).run(windows(1_000), 0.0);
        let finals = f.finals();
        assert_eq!(finals.len(), 1);
        let (len, start) = finals[0];
        let speech_start = (windows(5_000) * WINDOW) as u64;
        // Preroll is 300 ms, but the VAD only notices speech at the next step.
        assert!(start <= speech_start - PREROLL as u64 + 8 * WINDOW as u64 && start >= speech_start - PREROLL as u64 - 8 * WINDOW as u64);
        let speech = windows(2_000) * WINDOW;
        assert!(len >= speech + PAD - WINDOW && len <= speech + PREROLL + PAD + 8 * WINDOW, "{len}");
    }

    #[test]
    fn a_short_pause_does_not_split_a_phrase() {
        let mut f = Feed::new();
        f.run(windows(1_500), 0.9).run(windows(300), 0.1).run(windows(1_500), 0.9).run(windows(1_000), 0.0);
        assert_eq!(f.finals().len(), 1);
    }

    #[test]
    fn a_click_is_dropped() {
        let mut f = Feed::new();
        f.run(windows(1_000), 0.0).run(2, 0.9).run(windows(2_000), 0.0);
        assert!(f.steps.is_empty());
    }

    #[test]
    fn partials_come_about_once_a_second_while_speaking() {
        let mut f = Feed::new();
        f.run(windows(4_000), 0.9);
        assert!((3..=4).contains(&f.partials()), "{}", f.partials());
        assert!(f.finals().is_empty());
    }

    #[test]
    fn a_monologue_is_cut_at_the_quietest_point() {
        let mut f = Feed::new();
        f.run(windows(10_000), 0.9).run(3, 0.4).run(windows(5_000), 0.9).run(windows(1_000), 0.0);
        let finals = f.finals();
        assert_eq!(finals.len(), 2, "{finals:?}");
        // The first piece ends in the dip at ~10 s, the second starts right there.
        let dip = (windows(10_000) * WINDOW) as u64;
        assert!((finals[0].0 as u64).abs_diff(dip) < 3 * WINDOW as u64, "{finals:?}");
        assert_eq!(finals[1].1, finals[0].1 + finals[0].0 as u64);
    }
}
