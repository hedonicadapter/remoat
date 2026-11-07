//! Local streaming speech-to-text.
//!
//! Why streaming and not Whisper: dictation types into the target field *as you speak*, so the
//! engine has to emit partial hypotheses while audio is still arriving. Whisper is batch and
//! pads every inference to 30s, so faking that means re-transcribing a growing buffer — slow
//! and O(n²). A zipformer **transducer** decodes frame-synchronously with cached state, so
//! partials come out for free and there is no rework. (It also barely hallucinates on silence,
//! which is why remoat needs no VAD: push-to-talk already gives the utterance boundaries.)
//!
//! The mic is on the *phone*, so nothing here captures audio — PCM arrives over the WebSocket
//! and is fed in. Resampling to the model's 16 kHz uses sherpa's own `LinearResampler`, which
//! is why there is no `rubato` dependency.
//!
//! The engine is optional at runtime: no model on disk => `None`, and the client falls back to
//! the browser's Web Speech API. So a fresh install still dictates, just not locally.

use std::path::{Path, PathBuf};

use sherpa_onnx::{
    LinearResampler, OnlineModelConfig, OnlineRecognizer, OnlineRecognizerConfig, OnlineStream,
    OnlineTransducerModelConfig,
};

/// Model the daemon expects, and what `--fetch-model` downloads.
///
/// Chosen by measurement, not by size (`REMOAT_BENCH=1 cargo test bench_models`, WER over the
/// bundled reference wavs, RTF = decode time / audio duration on 3 threads):
///   20M-2023-02-17  int8 greedy  7.7% WER  0.051 RTF
///   20M-2023-02-17  int8 beam    6.3%      0.060
///   2023-06-26      int8 greedy  0.0%      0.079   <- this one
///   2023-06-26      int8 beam    0.0%      0.095
///   2023-06-26      fp32 beam    0.0%      0.137
/// The first cut shipped the 20M model to keep the download small; it was audibly worse in
/// real use. This one is ~71 MB of int8 weights (296 MB tarball) and still decodes ~10x faster
/// than realtime, which is what matters for typing while you speak. fp32 buys no accuracy here
/// and costs 45% more time, so int8 stays the default.
pub const MODEL_NAME: &str = "sherpa-onnx-streaming-zipformer-en-2023-06-26";
pub const MODEL_URL: &str =
    "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-streaming-zipformer-en-2023-06-26.tar.bz2";

/// The three ONNX graphs of a transducer plus its token table.
pub struct ModelFiles {
    pub encoder: PathBuf,
    pub decoder: PathBuf,
    pub joiner: PathBuf,
    pub tokens: PathBuf,
}

/// Locate a streaming transducer's files by PREFIX, not by exact name. Models in the sherpa zoo
/// disagree wildly — `encoder.onnx` for the NeMo conformers,
/// `encoder-epoch-99-avg-1-chunk-16-left-128.int8.onnx` for the zipformers — and hardcoding one
/// spelling is what would stop a user from dropping in a better model with REMOAT_STT_MODEL.
pub fn find_model(dir: &Path, prefer_int8: bool) -> Option<ModelFiles> {
    let tokens = dir.join("tokens.txt");
    if !tokens.is_file() {
        return None;
    }
    let entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "onnx"))
        .collect();
    let pick = |prefix: &str| -> Option<PathBuf> {
        let mut c: Vec<&PathBuf> = entries
            .iter()
            .filter(|p| {
                p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with(prefix))
            })
            .collect();
        // Prefer the requested precision, but take whatever exists — the NeMo models ship no
        // int8 variant at all, and half a model is worse than a slower one.
        let int8 = |p: &PathBuf| p.to_string_lossy().contains(".int8.");
        c.sort_by_key(|p| (int8(p) != prefer_int8, p.to_string_lossy().len()));
        c.first().map(|p| (*p).clone())
    };
    Some(ModelFiles {
        encoder: pick("encoder")?,
        decoder: pick("decoder")?,
        joiner: pick("joiner")?,
        tokens,
    })
}

/// Sample rate the model's feature extractor expects.
pub const MODEL_RATE: i32 = 16000;

/// Silence fed before and after the real audio, in samples at `MODEL_RATE` (0.5s).
///
/// Not padding-for-safety — measured. The streaming encoder needs warm-up frames before it
/// emits reliably, and without a lead-in the model drops the opening words outright: on the
/// bundled `0.wav`, priming with 0s/0.3s/0.5s gives
///   ""      -> "THE YELLOW LAMPS WOULD LIGHT UP …"        (first three words gone)
///   0.3s    -> "THE YELLOW LAMPS WOULD LIGHT UP …"        (still gone)
///   0.5s    -> "AFTER EARLY NIGHTFALL THE YELLOW LAMPS …" (correct)
/// That matters because a user presses push-to-talk and starts talking immediately, so the
/// lead-in silence a recogniser wants is exactly what live dictation never has. The trailing
/// pad flushes the last token the same way ("BRAFFLEL" -> "BRAFFLELS").
/// Silence decodes almost instantly, so this costs no perceptible latency.
const PAD_SAMPLES: usize = 8000;

/// Where models live. Mirrors `tls.rs`: state, not config, and never the read-only nix store.
pub fn model_root() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("REMOAT_MODEL_DIR") {
        return Some(PathBuf::from(p));
    }
    let base = std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .ok()
        .or_else(|| std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(base.join("remoat").join("models"))
}

/// Which model to run. `REMOAT_STT_MODEL` takes either a bare name under the model root or an
/// absolute path, so a better model can be swapped in without touching the code.
pub fn model_dir() -> Option<PathBuf> {
    match std::env::var("REMOAT_STT_MODEL") {
        Ok(v) if v.starts_with('/') => Some(PathBuf::from(v)),
        Ok(v) if !v.is_empty() => Some(model_root()?.join(v)),
        _ => Some(model_root()?.join(MODEL_NAME)),
    }
}

/// int8 by default — a quarter the size and faster on CPU. `REMOAT_STT_PRECISION=fp32` trades
/// that back for accuracy, which matters more on small models where quantisation bites hardest.
fn prefer_int8() -> bool {
    !std::env::var("REMOAT_STT_PRECISION").is_ok_and(|v| v.eq_ignore_ascii_case("fp32"))
}

/// Every required file present? A half-extracted download must not look installed.
pub fn model_present(dir: &Path) -> bool {
    find_model(dir, prefer_int8()).is_some()
}

pub struct Stt {
    recognizer: OnlineRecognizer,
}

impl Stt {
    /// Load the engine, or `None` if the model isn't installed / fails to load. Never fatal —
    /// the daemon serves fine without it and the phone falls back to Web Speech.
    pub fn load(threads: i32) -> Option<Stt> {
        let dir = model_dir()?;
        if !model_present(&dir) {
            eprintln!(
                "[stt] no model at {} — dictation will use the phone's Web Speech API \
                 (run `remoat --fetch-model` for local, offline STT)",
                dir.display()
            );
            return None;
        }
        let f = find_model(&dir, prefer_int8())?;
        let s = |p: &Path| Some(p.to_string_lossy().into_owned());
        // greedy_search, on the measurements: beam search improved the SMALL model (7.7% ->
        // 6.3% WER) but bought exactly nothing on this one (0.0% either way) while costing ~20%
        // more decode time and re-ranking partials more, which shows up as churn when the
        // output is live keystrokes. It was briefly the default only because hotwords need it,
        // and hotwords are still inert — a cost with no benefit attached.
        let method = std::env::var("REMOAT_STT_DECODING").unwrap_or_else(|_| "greedy_search".into());
        let mut config = OnlineRecognizerConfig {
            model_config: OnlineModelConfig {
                transducer: OnlineTransducerModelConfig {
                    encoder: s(&f.encoder),
                    decoder: s(&f.decoder),
                    joiner: s(&f.joiner),
                },
                tokens: s(&f.tokens),
                num_threads: threads.max(1),
                ..Default::default()
            },
            max_active_paths: 4,
            decoding_method: Some(method),
            ..Default::default()
        };
        // Endpointing is off: push-to-talk already delimits the utterance, and letting the
        // recognizer decide would cut a slow speaker off mid-sentence.
        config.enable_endpoint = false;

        match OnlineRecognizer::create(&config) {
            Some(recognizer) => {
                eprintln!("[stt] local streaming STT ready ({MODEL_NAME}, {} threads)", threads.max(1));
                Some(Stt { recognizer })
            }
            None => {
                eprintln!("[stt] model at {} failed to load — falling back to Web Speech", dir.display());
                None
            }
        }
    }

    /// Start an utterance.
    ///
    /// `hotwords` would bias decoding toward the command vocabulary — the thing Web Speech
    /// could never be told, and the reason `voice.rs` carries a hand-written mishearing table.
    /// It is plumbed but currently INERT: sherpa only applies hotwords under
    /// `modified_beam_search`, and we decode greedily because this runs live while the user
    /// speaks. Switching the command path to beam search means a second recognizer (another
    /// ~40 MB resident), so it is left as a deliberate follow-up rather than a silent no-op —
    /// see "Open threads" in CLAUDE.md.
    pub fn start(&self, rate: i32, hotwords: Option<&str>) -> Utterance {
        let stream = match hotwords {
            Some(h) if !h.trim().is_empty() => self.recognizer.create_stream_with_hotwords(h),
            _ => self.recognizer.create_stream(),
        };
        // Prime the encoder so the user's first word survives (see PAD_SAMPLES). Fed at
        // MODEL_RATE directly — it's silence, so there is nothing to resample.
        stream.accept_waveform(MODEL_RATE, &vec![0.0f32; PAD_SAMPLES]);
        Utterance {
            stream,
            // Only resample when we must; the phone's rate is whatever its AudioContext gave us.
            resampler: (rate != MODEL_RATE)
                .then(|| LinearResampler::create(rate, MODEL_RATE))
                .flatten(),
            typed: Typed::default(),
        }
    }

    /// Feed audio and return the transcript so far. Decoding is CPU-bound — call from
    /// `spawn_blocking`, never on the async runtime.
    pub fn push(&self, u: &mut Utterance, samples: &[f32]) -> String {
        let owned;
        let feed: &[f32] = match &u.resampler {
            Some(r) => {
                owned = r.resample(samples, false);
                &owned
            }
            None => samples,
        };
        if !feed.is_empty() {
            u.stream.accept_waveform(MODEL_RATE, feed);
        }
        while self.recognizer.is_ready(&u.stream) {
            self.recognizer.decode(&u.stream);
        }
        self.recognizer.get_result(&u.stream).map(|r| r.text).unwrap_or_default()
    }

    /// Flush the tail and return the final transcript.
    pub fn finish(&self, u: &mut Utterance) -> String {
        if let Some(r) = &u.resampler {
            let tail = r.resample(&[], true);
            if !tail.is_empty() {
                u.stream.accept_waveform(MODEL_RATE, &tail);
            }
        }
        // Trailing silence flushes the last token, which otherwise comes out clipped.
        u.stream.accept_waveform(MODEL_RATE, &vec![0.0f32; PAD_SAMPLES]);
        u.stream.input_finished();
        while self.recognizer.is_ready(&u.stream) {
            self.recognizer.decode(&u.stream);
        }
        self.recognizer.get_result(&u.stream).map(|r| r.text).unwrap_or_default()
    }
}

/// One push-to-talk press: its decoder stream, its resampler, and what we've already typed.
pub struct Utterance {
    stream: OnlineStream,
    resampler: Option<LinearResampler>,
    typed: Typed,
}

impl Utterance {
    /// Advance the typed text to `next`, returning the keystrokes that get us there.
    pub fn advance(&mut self, next: &str) -> Edit {
        self.typed.advance(next)
    }

    pub fn emitted(&self) -> &str {
        self.typed.emitted()
    }
}

/// What has actually been typed into the target field so far. Split from `Utterance` so the
/// state machine that drives real keystrokes is testable without a decoder attached.
#[derive(Debug, Default)]
pub struct Typed(String);

impl Typed {
    pub fn advance(&mut self, next: &str) -> Edit {
        let e = diff(&self.0, next);
        self.0 = next.to_string();
        e
    }

    pub fn emitted(&self) -> &str {
        &self.0
    }
}

/// Keystrokes that turn one transcript into another.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Edit {
    /// Characters to delete from the end of what we already typed.
    pub backspaces: usize,
    /// Characters to type after deleting.
    pub append: String,
}

impl Edit {
    pub fn is_noop(&self) -> bool {
        self.backspaces == 0 && self.append.is_empty()
    }
}

/// Longest-common-prefix diff, in CHARACTERS not bytes — this drives real backspaces into the
/// user's document, and a byte-wise split would both miscount and panic on a multi-byte
/// boundary. Ported from `streamText()` in app.js, which had to exist because Web Speech
/// rewrites in-flight words; a transducer revises far less, so most edits are pure appends.
pub fn diff(prev: &str, next: &str) -> Edit {
    let mut common = 0;
    for (a, b) in prev.chars().zip(next.chars()) {
        if a != b {
            break;
        }
        common += 1;
    }
    Edit {
        backspaces: prev.chars().count() - common,
        append: next.chars().skip(common).collect(),
    }
}

/// Download and unpack the model. Shells out to `curl` and `tar`, matching how the rest of the
/// daemon reaches platform tooling (ydotool, the compositor CLIs) — that keeps an HTTP client
/// and a bzip2 decoder out of the dependency tree for something run once.
///
/// Deliberately a separate command rather than an automatic first-run download: it is ~122 MB,
/// and surprising someone's phone-remote with that is worse than one explicit step. Until it
/// runs, the daemon simply reports no local STT and the phone uses Web Speech.
pub fn fetch_model() -> Result<(), String> {
    let root = model_root().ok_or("cannot determine a state directory (set REMOAT_MODEL_DIR)")?;
    let dir = model_dir().unwrap();
    if model_present(&dir) {
        println!("model already installed: {}", dir.display());
        return Ok(());
    }
    std::fs::create_dir_all(&root).map_err(|e| format!("create {}: {e}", root.display()))?;

    // Download beside the target, then unpack — a partial file must never look like a model.
    let tmp = root.join(format!("{MODEL_NAME}.tar.bz2.part"));
    println!("downloading {MODEL_URL}");
    let ok = std::process::Command::new("curl")
        .args(["-L", "--fail", "--progress-bar", "-o"])
        .arg(&tmp)
        .arg(MODEL_URL)
        .status()
        .map_err(|e| format!("curl: {e} (is curl installed?)"))?
        .success();
    if !ok {
        let _ = std::fs::remove_file(&tmp);
        return Err("download failed".into());
    }

    println!("unpacking into {}", root.display());
    let ok = std::process::Command::new("tar")
        .arg("-xjf")
        .arg(&tmp)
        .arg("-C")
        .arg(&root)
        .status()
        .map_err(|e| format!("tar: {e} (is tar installed?)"))?
        .success();
    let _ = std::fs::remove_file(&tmp);
    if !ok {
        return Err("unpack failed (needs tar with bzip2 support)".into());
    }
    if !model_present(&dir) {
        return Err(format!("unpacked, but {} is missing expected files", dir.display()));
    }
    println!("installed: {}", dir.display());
    Ok(())
}

/// Normalize a transcript for typing. The model emits uppercase with no punctuation; lowercase
/// reads better inline and matches what `voice.rs::normalize` expects for commands.
pub fn spoken_to_text(s: &str) -> String {
    s.trim().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_only_costs_no_backspaces() {
        assert_eq!(diff("hello", "hello world"), Edit { backspaces: 0, append: " world".into() });
    }

    #[test]
    fn a_revision_deletes_only_the_changed_tail() {
        // The recognizer took back "there" and replaced it with "their".
        assert_eq!(diff("go there", "go their"), Edit { backspaces: 2, append: "ir".into() });
    }

    #[test]
    fn identical_text_is_a_noop() {
        assert!(diff("same", "same").is_noop());
        assert!(diff("", "").is_noop());
    }

    #[test]
    fn shrinking_text_only_backspaces() {
        assert_eq!(diff("hello world", "hello"), Edit { backspaces: 6, append: String::new() });
    }

    #[test]
    fn diff_counts_characters_not_bytes() {
        // Byte-wise slicing would report 3 backspaces for one deleted char, and could split a
        // multi-byte boundary outright.
        assert_eq!(diff("café", "caf"), Edit { backspaces: 1, append: String::new() });
        assert_eq!(diff("caf", "café"), Edit { backspaces: 0, append: "é".into() });
        assert_eq!(diff("naïve", "naïvety"), Edit { backspaces: 0, append: "ty".into() });
    }

    #[test]
    fn typed_state_accumulates_across_partials() {
        // The realistic sequence: a transducer grows its hypothesis, occasionally revising the
        // last word. Only the delta should ever reach the keyboard.
        let mut t = Typed::default();
        assert_eq!(t.advance("HELLO"), Edit { backspaces: 0, append: "HELLO".into() });
        assert_eq!(t.advance("HELLO THE"), Edit { backspaces: 0, append: " THE".into() });
        assert_eq!(t.advance("HELLO THERE"), Edit { backspaces: 0, append: "RE".into() });
        assert_eq!(t.advance("HELLO THEIR"), Edit { backspaces: 2, append: "IR".into() });
        assert_eq!(t.emitted(), "HELLO THEIR");
    }

    #[test]
    fn transcripts_are_lowercased_for_typing() {
        assert_eq!(spoken_to_text("  HELLO THERE  "), "hello there");
    }

    // ---- real decode against the model's own reference wavs ----
    // Skipped when the model isn't installed, so `cargo test` still passes on a clean checkout.
    // Exercises the actual engine, not just argv or arithmetic. Run after `remoat --fetch-model`.

    /// One engine for every real-decode test. Loading a 70MB model per test and running four
    /// recognizers concurrently on a small box just contends for cores — and ONNX intra-op
    /// threading can vary reduction order under load, which is exactly the kind of thing that
    /// makes a WER assertion flaky. `Stt` is Send + Sync, so sharing it is free.
    fn engine() -> Option<&'static Stt> {
        static ENGINE: std::sync::OnceLock<Option<Stt>> = std::sync::OnceLock::new();
        ENGINE
            .get_or_init(|| {
                let dir = model_dir()?;
                if !model_present(&dir) {
                    eprintln!("[test] model not installed — skipping real-decode tests");
                    return None;
                }
                Stt::load(2)
            })
            .as_ref()
    }

    /// Word error rate, so the assertion tolerates a token slip without being meaningless.
    fn wer(reference: &str, got: &str) -> f32 {
        let r: Vec<&str> = reference.split_whitespace().collect();
        let h: Vec<&str> = got.split_whitespace().collect();
        // Levenshtein over words.
        let mut prev: Vec<usize> = (0..=h.len()).collect();
        for (i, rw) in r.iter().enumerate() {
            let mut cur = vec![i + 1];
            for (j, hw) in h.iter().enumerate() {
                let sub = prev[j] + usize::from(rw != hw);
                cur.push(sub.min(prev[j + 1] + 1).min(cur[j] + 1));
            }
            prev = cur;
        }
        prev[h.len()] as f32 / r.len().max(1) as f32
    }

    #[test]
    fn decodes_the_reference_wavs() {
        let Some(stt) = engine() else { return };
        let dir = model_dir().unwrap();
        let expected = [
            ("0.wav", "AFTER EARLY NIGHTFALL THE YELLOW LAMPS WOULD LIGHT UP HERE AND THERE THE SQUALID QUARTER OF THE BROTHELS"),
            ("1.wav", "GOD AS A DIRECT CONSEQUENCE OF THE SIN WHICH MAN THUS PUNISHED HAD GIVEN HER A LOVELY CHILD WHOSE PLACE WAS ON THAT SAME DISHONOURED BOSOM TO CONNECT HER PARENT FOR EVER WITH THE RACE AND DESCENT OF MORTALS AND TO BE FINALLY A BLESSED SOUL IN HEAVEN"),
        ];
        for (name, reference) in expected {
            let path = dir.join("test_wavs").join(name);
            let wave = sherpa_onnx::Wave::read(&path.to_string_lossy()).expect("read wav");
            let mut u = stt.start(wave.sample_rate(), None);
            // Feed in 100ms chunks: this is a *streaming* test, not a batch one — decoding a
            // whole file in one call would not prove partials work.
            let chunk = (wave.sample_rate() / 10) as usize;
            for c in wave.samples().chunks(chunk) {
                stt.push(&mut u, c);
            }
            let got = stt.finish(&mut u);
            let rate = wer(reference, &got);
            assert!(rate < 0.05, "{name}: WER {rate:.2}\n  want: {reference}\n  got:  {got}");
        }
    }

    #[test]
    fn partials_arrive_before_the_audio_ends() {
        // The whole reason for a transducer over Whisper. If this fails, live typing is dead.
        let Some(stt) = engine() else { return };
        let dir = model_dir().unwrap();
        let wave = sherpa_onnx::Wave::read(&dir.join("test_wavs/0.wav").to_string_lossy()).unwrap();
        let mut u = stt.start(wave.sample_rate(), None);
        let chunk = (wave.sample_rate() / 10) as usize;
        let total = wave.samples().len();
        let mut first_text_at = None;
        let mut fed = 0usize;
        for c in wave.samples().chunks(chunk) {
            let text = stt.push(&mut u, c);
            fed += c.len();
            if first_text_at.is_none() && !text.trim().is_empty() {
                first_text_at = Some(fed as f32 / total as f32);
            }
        }
        let at = first_text_at.expect("no partial transcript before end of audio");
        assert!(at < 0.5, "first partial only at {:.0}% of the audio", at * 100.0);
    }

    #[test]
    fn streaming_edits_are_mostly_appends() {
        // Justifies typing directly into the user's document: a transducer should rarely take
        // characters back, unlike Web Speech, whose churn forced the backspace protocol.
        let Some(stt) = engine() else { return };
        let dir = model_dir().unwrap();
        let wave = sherpa_onnx::Wave::read(&dir.join("test_wavs/0.wav").to_string_lossy()).unwrap();
        let mut u = stt.start(wave.sample_rate(), None);
        let chunk = (wave.sample_rate() / 10) as usize;
        let (mut typed, mut deleted) = (0usize, 0usize);
        for c in wave.samples().chunks(chunk) {
            let text = stt.push(&mut u, c);
            let e = u.advance(&text);
            typed += e.append.chars().count();
            deleted += e.backspaces;
        }
        let final_text = stt.finish(&mut u);
        let e = u.advance(&final_text);
        typed += e.append.chars().count();
        deleted += e.backspaces;
        assert!(typed > 50, "barely any text produced ({typed} chars)");
        let churn = deleted as f32 / typed as f32;
        assert!(churn < 0.25, "too many retractions: {deleted} backspaces for {typed} typed");
        eprintln!("[test] streaming churn: {deleted} backspaces / {typed} chars ({:.1}%)", churn * 100.0);
    }

    #[test]
    fn resampling_from_the_phones_rate_still_transcribes() {
        // The phone's AudioContext is typically 48kHz; the model wants 16kHz. Feed the 8k
        // reference wav to exercise the resampler path rather than trusting it.
        let Some(stt) = engine() else { return };
        let dir = model_dir().unwrap();
        let wave = sherpa_onnx::Wave::read(&dir.join("test_wavs/8k.wav").to_string_lossy()).unwrap();
        assert_ne!(wave.sample_rate(), MODEL_RATE, "8k.wav should exercise resampling");
        let mut u = stt.start(wave.sample_rate(), None);
        for c in wave.samples().chunks((wave.sample_rate() / 10) as usize) {
            stt.push(&mut u, c);
        }
        let got = stt.finish(&mut u);
        let reference = "YET THESE THOUGHTS AFFECTED HESTER PRYNNE LESS WITH HOPE THAN APPREHENSION";
        assert!(wer(reference, &got) < 0.15, "resampled decode: {got}");
    }
}

#[cfg(test)]
mod bench {
    use super::*;
    use std::time::Instant;

    fn wer2(reference: &str, got: &str) -> f32 {
        let r: Vec<&str> = reference.split_whitespace().collect();
        let h: Vec<&str> = got.split_whitespace().collect();
        let mut prev: Vec<usize> = (0..=h.len()).collect();
        for (i, rw) in r.iter().enumerate() {
            let mut cur = vec![i + 1];
            for (j, hw) in h.iter().enumerate() {
                let sub = prev[j] + usize::from(rw != hw);
                cur.push(sub.min(prev[j + 1] + 1).min(cur[j] + 1));
            }
            prev = cur;
        }
        prev[h.len()] as f32 / r.len().max(1) as f32
    }

    const REFS: &[(&str, &str)] = &[
        ("0.wav", "AFTER EARLY NIGHTFALL THE YELLOW LAMPS WOULD LIGHT UP HERE AND THERE THE SQUALID QUARTER OF THE BROTHELS"),
        ("1.wav", "GOD AS A DIRECT CONSEQUENCE OF THE SIN WHICH MAN THUS PUNISHED HAD GIVEN HER A LOVELY CHILD WHOSE PLACE WAS ON THAT SAME DISHONOURED BOSOM TO CONNECT HER PARENT FOR EVER WITH THE RACE AND DESCENT OF MORTALS AND TO BE FINALLY A BLESSED SOUL IN HEAVEN"),
        ("8k.wav", "YET THESE THOUGHTS AFFECTED HESTER PRYNNE LESS WITH HOPE THAN APPREHENSION"),
    ];

    #[test]
    fn bench_models() {
        if std::env::var("REMOAT_BENCH").is_err() { return; }
        let root = model_root().unwrap();
        let wavdir = model_root().unwrap().join(MODEL_NAME).join("test_wavs");
        // Whichever models are installed under the model root, or REMOAT_BENCH_MODELS.
        let listed = std::env::var("REMOAT_BENCH_MODELS").unwrap_or_default();
        let models: Vec<String> = if listed.is_empty() {
            let mut v: Vec<String> = std::fs::read_dir(&root)
                .into_iter()
                .flatten()
                .filter_map(|e| e.ok())
                .filter(|e| e.path().is_dir())
                .filter_map(|e| e.file_name().into_string().ok())
                .collect();
            v.sort();
            v
        } else {
            listed.split(',').map(|s| s.trim().to_string()).collect()
        };
        println!("\n{:<52} {:>6} {:>10} {:>7} {:>7}", "model", "prec", "decoding", "WER", "RTF");
        for m in &models {
            let m = m.as_str();
            for prec in ["int8", "fp32"] {
                for dec in ["greedy_search", "modified_beam_search"] {
                    std::env::set_var("REMOAT_STT_MODEL", m);
                    std::env::set_var("REMOAT_STT_PRECISION", prec);
                    std::env::set_var("REMOAT_STT_DECODING", dec);
                    let dir = root.join(m);
                    let Some(files) = find_model(&dir, prec == "int8") else { continue };
                    // Skip a precision the model doesn't actually ship.
                    let has_int8 = files.encoder.to_string_lossy().contains(".int8.");
                    if (prec == "int8") != has_int8 { continue; }
                    // sherpa ABORTS the process (not an Err) if a NeMo transducer is asked
                    // for modified_beam_search — skip rather than lose the whole benchmark.
                    if dec == "modified_beam_search" && m.contains("nemo") { continue; }
                    let Some(stt) = Stt::load(3) else { continue };
                    let (mut tot_wer, mut n, mut audio_s, mut wall) = (0.0f32, 0, 0.0f64, 0.0f64);
                    for (w, reference) in REFS {
                        let Some(wave) = sherpa_onnx::Wave::read(&wavdir.join(w).to_string_lossy()) else { continue };
                        let t0 = Instant::now();
                        let mut u = stt.start(wave.sample_rate(), None);
                        for c in wave.samples().chunks((wave.sample_rate()/10) as usize) { stt.push(&mut u, c); }
                        let got = stt.finish(&mut u);
                        wall += t0.elapsed().as_secs_f64();
                        audio_s += wave.samples().len() as f64 / wave.sample_rate() as f64;
                        tot_wer += wer2(reference, &got); n += 1;
                    }
                    if n > 0 {
                        println!("{:<52} {:>6} {:>10} {:>6.1}% {:>7.3}",
                            m.replace("sherpa-onnx-", ""), prec,
                            if dec.starts_with("greedy") {"greedy"} else {"beam"},
                            tot_wer/n as f32*100.0, wall/audio_s);
                    }
                }
            }
        }
        for v in ["REMOAT_STT_MODEL","REMOAT_STT_PRECISION","REMOAT_STT_DECODING"] { std::env::remove_var(v); }
    }
}
