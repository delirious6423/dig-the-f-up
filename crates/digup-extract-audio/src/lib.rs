//! Audio extraction for DigUp: decode any common audio file (or the soundtrack of a media file symphonia can
//! demux) to 16 kHz mono `f32`, then cut it into overlapping model windows.
//!
//! Port of `AudioExtractor.windows` (Swift `MediaExtractors.swift:34-78`, spec section 3.7) with two differences:
//! windows are returned in memory (no temporary WAV files) and buffering is a ring buffer (`VecDeque`), so memory
//! stays at about one window even for a multi-hour file. Decoding is incremental: packets are decoded, downmixed,
//! resampled and appended to the ring one at a time.
//!
//! No external process (ffmpeg) is ever used; a file symphonia cannot demux gives a typed [`AudioError`].

use std::collections::VecDeque;
use std::fmt;
use std::fs::File;
use std::path::Path;

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Indexing, Resampler};
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

/// Output sample rate expected by the embedding model.
pub const TARGET_RATE: u32 = 16_000;
/// Window length in seconds (the model ignores audio past 30 s).
pub const WINDOW_SECS: f64 = 30.0;
/// Hop between window starts in seconds (5 s overlap).
pub const HOP_SECS: f64 = 25.0;
/// Shortest tail worth embedding, in seconds of new audio.
pub const MIN_TAIL_SECS: f64 = 2.0;
/// Windows with RMS at or below this (samples scaled to [-1, 1]) are silent and skipped. About -50 dBFS.
pub const SILENCE_RMS: f64 = 0.003;

/// Input frames per resampler call.
const RESAMPLE_CHUNK: usize = 4096;

/// Everything that can go wrong while reading audio.
#[derive(Debug)]
pub enum AudioError {
    /// The file could not be opened.
    Io(std::io::Error),
    /// symphonia does not recognise the container.
    UnsupportedFormat(String),
    /// The container has no audio track.
    NoAudioTrack,
    /// The audio track uses a codec symphonia cannot decode.
    UnsupportedCodec(String),
    /// Unrecoverable demux or decode failure.
    Decode(String),
    /// Resampler construction or processing failed.
    Resample(String),
}

impl fmt::Display for AudioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AudioError::Io(e) => write!(f, "cannot open audio: {e}"),
            AudioError::UnsupportedFormat(e) => write!(f, "unsupported media format: {e}"),
            AudioError::NoAudioTrack => write!(f, "no audio track"),
            AudioError::UnsupportedCodec(e) => write!(f, "unsupported audio codec: {e}"),
            AudioError::Decode(e) => write!(f, "audio decoding failed: {e}"),
            AudioError::Resample(e) => write!(f, "resampling failed: {e}"),
        }
    }
}

impl std::error::Error for AudioError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            AudioError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for AudioError {
    fn from(e: std::io::Error) -> Self {
        AudioError::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, AudioError>;

/// One model input: up to 30 s of 16 kHz mono audio starting `start_secs` into the file.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioWindow {
    pub start_secs: f64,
    pub samples: Vec<f32>,
}

impl AudioWindow {
    pub fn end_secs(&self) -> f64 {
        self.start_secs + self.samples.len() as f64 / TARGET_RATE as f64
    }
}

/// Window geometry. `Default` is the DigUp setting: 30 s windows, 25 s hop, 2 s minimum tail.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindowConfig {
    pub window_secs: f64,
    pub hop_secs: f64,
    pub min_tail_secs: f64,
}

impl Default for WindowConfig {
    fn default() -> Self {
        WindowConfig { window_secs: WINDOW_SECS, hop_secs: HOP_SECS, min_tail_secs: MIN_TAIL_SECS }
    }
}

/// A source of 16 kHz mono samples, delivered incrementally.
pub trait ChunkSource {
    /// Append the next chunk of samples to `out`. Returns `Ok(false)` at end of stream (nothing appended).
    fn next_chunk(&mut self, out: &mut Vec<f32>) -> Result<bool>;
}

/// RMS of samples already scaled to [-1, 1].
pub fn rms(samples: &[f32]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
    (sum / samples.len() as f64).sqrt()
}

fn rms_deque(buf: &VecDeque<f32>, len: usize) -> f64 {
    let (a, b) = buf.as_slices();
    let n_a = a.len().min(len);
    let n_b = len - n_a;
    let sum: f64 = a[..n_a].iter().chain(b[..n_b].iter()).map(|&s| f64::from(s) * f64::from(s)).sum();
    if len == 0 {
        0.0
    } else {
        (sum / len as f64).sqrt()
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Windowing

/// Streaming iterator over the non-silent windows of a [`ChunkSource`].
///
/// Memory is bounded by one window plus one source chunk. After the iterator ends (or errors), [`Self::total_secs`]
/// is the length of the audio that was read.
pub struct Windows<S: ChunkSource> {
    source: S,
    window: usize,
    hop: usize,
    min_tail: usize,
    ring: VecDeque<f32>,
    chunk: Vec<f32>,
    /// Sample index (in the file) of `ring[0]`.
    offset: usize,
    consumed: usize,
    eof: bool,
    done: bool,
}

impl<S: ChunkSource> Windows<S> {
    pub fn new(source: S, config: WindowConfig) -> Self {
        let rate = f64::from(TARGET_RATE);
        let window = ((config.window_secs * rate) as usize).max(1);
        let hop = ((config.hop_secs * rate) as usize).clamp(1, window);
        let min_tail = (config.min_tail_secs * rate) as usize;
        Windows {
            source,
            window,
            hop,
            min_tail,
            ring: VecDeque::with_capacity(window + RESAMPLE_CHUNK * 4),
            chunk: Vec::new(),
            offset: 0,
            consumed: 0,
            eof: false,
            done: false,
        }
    }

    /// Seconds of audio read from the source so far.
    pub fn total_secs(&self) -> f64 {
        self.consumed as f64 / f64::from(TARGET_RATE)
    }

    /// Upper bound on samples currently buffered (for tests and diagnostics).
    pub fn buffered_samples(&self) -> usize {
        self.ring.len()
    }

    fn emit(&self, count: usize) -> Option<AudioWindow> {
        if rms_deque(&self.ring, count) <= SILENCE_RMS {
            return None;
        }
        let samples: Vec<f32> = self.ring.iter().take(count).copied().collect();
        Some(AudioWindow { start_secs: self.offset as f64 / f64::from(TARGET_RATE), samples })
    }
}

impl<S: ChunkSource> Iterator for Windows<S> {
    type Item = Result<AudioWindow>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        loop {
            // A full window is buffered: emit it (if loud enough), then advance by one hop either way.
            if self.ring.len() >= self.window {
                let w = self.emit(self.window);
                self.ring.drain(..self.hop);
                self.offset += self.hop;
                if let Some(w) = w {
                    return Some(Ok(w));
                }
                continue;
            }
            if self.eof {
                self.done = true;
                // Tail: audio beyond the previous window's overlap, or the whole file if shorter than a window.
                let covered = if self.offset == 0 { 0 } else { self.window - self.hop };
                if self.ring.len().saturating_sub(covered) >= self.min_tail.max(1) {
                    return self.emit(self.ring.len()).map(Ok);
                }
                return None;
            }
            self.chunk.clear();
            match self.source.next_chunk(&mut self.chunk) {
                Ok(true) => {
                    self.consumed += self.chunk.len();
                    self.ring.extend(self.chunk.iter().copied());
                }
                Ok(false) => self.eof = true,
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Resampling

/// Streaming mono resampler to [`TARGET_RATE`] (pass-through when the rate already matches).
struct MonoResampler {
    inner: Option<Fft<f32>>,
    ratio: f64,
    pending: Vec<f32>,
    scratch_in: Vec<f32>,
    scratch_out: Vec<f32>,
    to_trim: usize,
    total_in: usize,
    emitted: usize,
}

impl MonoResampler {
    fn new(rate_in: u32) -> Result<Self> {
        let err = |e: &dyn fmt::Display| AudioError::Resample(e.to_string());
        let (inner, scratch_out, to_trim) = if rate_in == TARGET_RATE {
            (None, Vec::new(), 0)
        } else {
            let r = Fft::<f32>::new(rate_in as usize, TARGET_RATE as usize, RESAMPLE_CHUNK, 1, FixedSync::Input)
                .map_err(|e| err(&e))?;
            let out = vec![0.0; r.output_frames_max()];
            let delay = r.output_delay();
            (Some(r), out, delay)
        };
        Ok(MonoResampler {
            inner,
            ratio: f64::from(TARGET_RATE) / f64::from(rate_in),
            pending: Vec::new(),
            scratch_in: Vec::new(),
            scratch_out,
            to_trim,
            total_in: 0,
            emitted: 0,
        })
    }

    /// Run one resampler step over `scratch_in` (already sized to `input_frames_next`).
    fn step(&mut self, partial: Option<usize>, out: &mut Vec<f32>) -> Result<()> {
        let r = self.inner.as_mut().expect("resampler present");
        let err = |e: &dyn fmt::Display| AudioError::Resample(e.to_string());
        let frames_in = self.scratch_in.len();
        let input = InterleavedSlice::new(&self.scratch_in[..], 1, frames_in).map_err(|e| err(&e))?;
        let cap = self.scratch_out.len();
        let mut output = InterleavedSlice::new_mut(&mut self.scratch_out[..], 1, cap).map_err(|e| err(&e))?;
        let indexing = Indexing { input_offset: 0, output_offset: 0, partial_len: partial, active_channels_mask: None };
        let (_, n_out) = r.process_into_buffer(&input, &mut output, Some(&indexing)).map_err(|e| err(&e))?;
        let skip = self.to_trim.min(n_out);
        self.to_trim -= skip;
        out.extend_from_slice(&self.scratch_out[skip..n_out]);
        self.emitted += n_out - skip;
        Ok(())
    }

    fn push(&mut self, input: &[f32], out: &mut Vec<f32>) -> Result<()> {
        self.total_in += input.len();
        if self.inner.is_none() {
            out.extend_from_slice(input);
            return Ok(());
        }
        self.pending.extend_from_slice(input);
        loop {
            let need = self.inner.as_ref().expect("resampler present").input_frames_next();
            if self.pending.len() < need {
                break;
            }
            self.scratch_in.clear();
            self.scratch_in.extend_from_slice(&self.pending[..need]);
            self.pending.drain(..need);
            self.step(None, out)?;
        }
        Ok(())
    }

    /// Flush the tail, trim the filter delay and cut to exactly `ceil(ratio * input)` frames.
    fn finish(&mut self, out: &mut Vec<f32>) -> Result<()> {
        if self.inner.is_none() || self.total_in == 0 {
            return Ok(());
        }
        let expected = (self.ratio * self.total_in as f64).ceil() as usize;
        let need = self.inner.as_ref().expect("resampler present").input_frames_next();
        let rest = self.pending.len();
        if rest > 0 {
            self.scratch_in.clear();
            self.scratch_in.extend_from_slice(&self.pending);
            self.scratch_in.resize(need, 0.0);
            self.pending.clear();
            self.step(Some(rest), out)?;
        }
        let mut guard = 0;
        while self.emitted < expected && guard < 64 {
            let need = self.inner.as_ref().expect("resampler present").input_frames_next();
            self.scratch_in.clear();
            self.scratch_in.resize(need, 0.0);
            self.step(Some(0), out)?;
            guard += 1;
        }
        if self.emitted > expected {
            let excess = self.emitted - expected;
            out.truncate(out.len().saturating_sub(excess));
            self.emitted = expected;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Decoding

/// Incremental decoder: container -> codec -> downmix -> resample, yielding 16 kHz mono chunks.
pub struct AudioStream {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    resampler: Option<MonoResampler>,
    interleaved: Vec<f32>,
    mono: Vec<f32>,
    finished: bool,
}

impl AudioStream {
    /// Open an audio file, or the first audio track of any media file symphonia can demux.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let file = File::open(path)?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }
        let format = symphonia::default::get_probe()
            .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
            .map_err(|e| AudioError::UnsupportedFormat(e.to_string()))?;
        let track = format.default_track(TrackType::Audio).ok_or(AudioError::NoAudioTrack)?;
        let params = track
            .codec_params
            .as_ref()
            .and_then(|p| p.audio())
            .ok_or_else(|| AudioError::UnsupportedCodec("missing codec parameters".into()))?;
        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(params, &AudioDecoderOptions::default())
            .map_err(|e| AudioError::UnsupportedCodec(e.to_string()))?;
        let track_id = track.id;
        Ok(AudioStream {
            format,
            decoder,
            track_id,
            resampler: None,
            interleaved: Vec::new(),
            mono: Vec::new(),
            finished: false,
        })
    }

    /// Open the soundtrack of a media file (video or audio container). Same as [`Self::open`]; a file without an
    /// audio track gives [`AudioError::NoAudioTrack`] and one symphonia cannot demux gives
    /// [`AudioError::UnsupportedFormat`].
    pub fn open_soundtrack(path: impl AsRef<Path>) -> Result<Self> {
        Self::open(path)
    }

    /// Stream the non-silent windows of this audio with the DigUp geometry.
    pub fn windows(self) -> Windows<Self> {
        Windows::new(self, WindowConfig::default())
    }

    /// Stream the non-silent windows with a custom geometry.
    pub fn windows_with(self, config: WindowConfig) -> Windows<Self> {
        Windows::new(self, config)
    }
}

impl ChunkSource for AudioStream {
    fn next_chunk(&mut self, out: &mut Vec<f32>) -> Result<bool> {
        while !self.finished {
            let packet = match self.format.next_packet() {
                Ok(Some(p)) => p,
                Ok(None) => {
                    self.finished = true;
                    break;
                }
                Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    self.finished = true;
                    break;
                }
                Err(e) => return Err(AudioError::Decode(e.to_string())),
            };
            if packet.track_id != self.track_id {
                continue;
            }
            let decoded = match self.decoder.decode(&packet) {
                Ok(d) => d,
                // A bad packet is skipped, as the symphonia examples do.
                Err(SymError::IoError(_)) | Err(SymError::DecodeError(_)) => continue,
                Err(e) => return Err(AudioError::Decode(e.to_string())),
            };
            if decoded.frames() == 0 {
                continue;
            }
            let spec = decoded.spec();
            let channels = spec.channels().count().max(1);
            let rate = spec.rate();
            decoded.copy_to_vec_interleaved::<f32>(&mut self.interleaved);
            self.mono.clear();
            if channels == 1 {
                self.mono.extend_from_slice(&self.interleaved);
            } else {
                let inv = 1.0 / channels as f32;
                self.mono
                    .extend(self.interleaved.chunks_exact(channels).map(|f| f.iter().sum::<f32>() * inv));
            }
            if self.resampler.is_none() {
                self.resampler = Some(MonoResampler::new(rate)?);
            }
            let before = out.len();
            self.resampler.as_mut().expect("set above").push(&self.mono, out)?;
            if out.len() > before {
                return Ok(true);
            }
        }
        // End of stream: flush the resampler once.
        if let Some(mut r) = self.resampler.take() {
            let before = out.len();
            r.finish(out)?;
            if out.len() > before {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// Decode a whole (short) file to 16 kHz mono `f32`. Holds everything in memory: use [`AudioStream::windows`] for
/// long files.
pub fn decode_all(path: impl AsRef<Path>) -> Result<Vec<f32>> {
    let mut stream = AudioStream::open(path)?;
    let mut all = Vec::new();
    while stream.next_chunk(&mut all)? {}
    Ok(all)
}

/// Streaming windows of a file with the DigUp geometry (30 s / 25 s / 2 s tail, silence skipped).
pub fn windows(path: impl AsRef<Path>) -> Result<Windows<AudioStream>> {
    Ok(AudioStream::open(path)?.windows())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const R: usize = TARGET_RATE as usize;

    fn sine(rate: u32, secs: f64, freq: f64, amp: f32) -> Vec<f32> {
        let n = (secs * f64::from(rate)) as usize;
        (0..n)
            .map(|i| amp * (2.0 * std::f64::consts::PI * freq * i as f64 / f64::from(rate)).sin() as f32)
            .collect()
    }

    fn write_wav(path: &Path, rate: u32, channels: u16, interleaved: &[f32]) {
        let spec = hound::WavSpec {
            channels,
            sample_rate: rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for &s in interleaved {
            w.write_sample((s * 32767.0).round() as i16).unwrap();
        }
        w.finalize().unwrap();
    }

    /// Synthetic 16 kHz source: `total` samples of constant `level`, in chunks of `chunk`.
    struct Const {
        left: usize,
        chunk: usize,
        level: f32,
    }
    impl ChunkSource for Const {
        fn next_chunk(&mut self, out: &mut Vec<f32>) -> Result<bool> {
            if self.left == 0 {
                return Ok(false);
            }
            let n = self.left.min(self.chunk);
            out.extend(std::iter::repeat_n(self.level, n));
            self.left -= n;
            Ok(true)
        }
    }

    fn collect(total_secs: f64, level: f32) -> Vec<AudioWindow> {
        let src = Const { left: (total_secs * R as f64) as usize, chunk: 1000, level };
        Windows::new(src, WindowConfig::default()).map(|w| w.unwrap()).collect()
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("digup-audio-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn resample_44100_and_48000_length_and_tone() {
        for rate in [44_100u32, 48_000, 22_050, 8_000] {
            let p = tmp(&format!("tone_{rate}.wav"));
            write_wav(&p, rate, 1, &sine(rate, 3.0, 440.0, 0.5));
            let out = decode_all(&p).unwrap();
            assert_eq!(out.len(), 3 * R, "rate {rate}");
            // Middle of the clip: amplitude preserved (RMS of sine 0.5 = 0.3536) and frequency intact.
            let mid = &out[R..2 * R];
            assert!((rms(mid) - 0.5 / 2f64.sqrt()).abs() < 0.01, "rate {rate} rms {}", rms(mid));
            let crossings = mid.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
            assert!((crossings as i32 - 440).abs() <= 2, "rate {rate} crossings {crossings}");
        }
    }

    #[test]
    fn passthrough_16k_is_exact_length() {
        let p = tmp("pass.wav");
        write_wav(&p, 16_000, 1, &sine(16_000, 1.5, 300.0, 0.4));
        assert_eq!(decode_all(&p).unwrap().len(), R * 3 / 2);
    }

    #[test]
    fn stereo_downmix_averages_channels() {
        let p = tmp("stereo.wav");
        let n = R * 2;
        let mut data = Vec::with_capacity(n * 2);
        for _ in 0..n {
            data.push(0.8f32);
            data.push(-0.2f32);
        }
        write_wav(&p, 16_000, 2, &data);
        let out = decode_all(&p).unwrap();
        assert_eq!(out.len(), n);
        assert!(out.iter().all(|&s| (s - 0.3).abs() < 1e-3));
        // Opposite phase cancels.
        let p2 = tmp("stereo_cancel.wav");
        let data: Vec<f32> = (0..n).flat_map(|_| [0.5f32, -0.5]).collect();
        write_wav(&p2, 16_000, 2, &data);
        assert!(rms(&decode_all(&p2).unwrap()) < 1e-3);
    }

    #[test]
    fn windows_of_61s_signal() {
        let ws = collect(61.0, 0.1);
        let starts: Vec<f64> = ws.iter().map(|w| w.start_secs).collect();
        // 0-30, 25-55, tail 50-61 (new audio 61 - 55 = 6 s >= 2 s).
        assert_eq!(starts, vec![0.0, 25.0, 50.0]);
        assert_eq!(ws[0].samples.len(), 30 * R);
        assert_eq!(ws[1].samples.len(), 30 * R);
        assert_eq!(ws[2].samples.len(), 11 * R);
        assert!((ws[2].end_secs() - 61.0).abs() < 1e-9);
    }

    #[test]
    fn short_clip_single_window_and_too_short_dropped() {
        let ws = collect(10.0, 0.1);
        assert_eq!(ws.len(), 1);
        assert_eq!(ws[0].samples.len(), 10 * R);
        assert_eq!(collect(2.0, 0.1).len(), 1);
        assert!(collect(1.99, 0.1).is_empty());
    }

    #[test]
    fn tail_rule() {
        // 56.9 s: windows at 0 and 25; remaining new audio 56.9 - 55 = 1.9 s < 2 s: no tail.
        assert_eq!(collect(56.9, 0.1).len(), 2);
        // 57 s: new audio exactly 2 s: tail emitted, covering 25..57 minus... = starts at 50.
        let ws = collect(57.0, 0.1);
        assert_eq!(ws.len(), 3);
        assert_eq!(ws[2].start_secs, 50.0);
        assert_eq!(ws[2].samples.len(), 7 * R);
        // Exactly 30 s: one full window, then buffer 5 s overlap is all covered: nothing more.
        assert_eq!(collect(30.0, 0.1).len(), 1);
    }

    #[test]
    fn silent_windows_are_skipped_but_offsets_advance() {
        // 0-30 s silence, then loud to 61 s. Window 0 skipped; windows at 25 and 50 kept.
        struct Gap(usize, usize);
        impl ChunkSource for Gap {
            fn next_chunk(&mut self, out: &mut Vec<f32>) -> Result<bool> {
                if self.0 >= self.1 {
                    return Ok(false);
                }
                let n = 1600.min(self.1 - self.0);
                for i in self.0..self.0 + n {
                    out.push(if i < 30 * R { 0.0 } else { 0.2 });
                }
                self.0 += n;
                Ok(true)
            }
        }
        let ws: Vec<_> = Windows::new(Gap(0, 61 * R), WindowConfig::default()).map(|w| w.unwrap()).collect();
        let starts: Vec<f64> = ws.iter().map(|w| w.start_secs).collect();
        assert_eq!(starts, vec![25.0, 50.0]);
        assert!(collect(61.0, 0.0).is_empty());
    }

    #[test]
    fn rms_gate_boundary() {
        // RMS exactly at/below the gate is silent; just above is kept. Constant level => rms == level.
        // 0.003f32 is 0.00300000004..., a hair above the gate; its predecessor is below it.
        let below = f32::from_bits(0.003f32.to_bits() - 1);
        assert!(f64::from(below) < SILENCE_RMS && f64::from(0.003f32) > SILENCE_RMS);
        assert!(collect(5.0, below).is_empty());
        assert!(collect(5.0, 0.0029).is_empty());
        assert_eq!(collect(5.0, 0.003).len(), 1);
        assert_eq!(collect(5.0, 0.0031).len(), 1);
        assert_eq!(collect(5.0, -0.0031).len(), 1);
        assert!((rms(&[0.003; 8]) - 0.003).abs() < 1e-9);
    }

    #[test]
    fn three_hour_stream_bounded_memory_and_fast() {
        let total = 3 * 3600 * R;
        let src = Const { left: total, chunk: 4800, level: 0.1 };
        let mut it = Windows::new(src, WindowConfig::default());
        let t0 = std::time::Instant::now();
        let (mut n, mut max_buf, mut last_start) = (0usize, 0usize, 0.0f64);
        while let Some(w) = it.next() {
            let w = w.unwrap();
            assert!(w.samples.len() <= 30 * R);
            last_start = w.start_secs;
            n += 1;
            max_buf = max_buf.max(it.buffered_samples());
        }
        // ceil((10800 - 30) / 25) + 1 regular windows, then maybe a tail: starts are multiples of 25 s.
        assert!((431..=433).contains(&n), "n = {n}");
        assert_eq!(last_start % 25.0, 0.0);
        assert!(max_buf <= 30 * R + 4800, "ring grew to {max_buf}");
        assert!((it.total_secs() - 10_800.0).abs() < 1e-6);
        assert!(t0.elapsed().as_secs_f64() < 20.0, "took {:?}", t0.elapsed());
    }

    #[test]
    fn long_wav_through_file_api_matches_expected_windows() {
        // 70 s of 48 kHz stereo through the real decode + resample path.
        let p = tmp("long48.wav");
        let mono = sine(48_000, 70.0, 220.0, 0.3);
        let data: Vec<f32> = mono.iter().flat_map(|&s| [s, s]).collect();
        write_wav(&p, 48_000, 2, &data);
        let mut it = windows(&p).unwrap();
        let ws: Vec<_> = it.by_ref().map(|w| w.unwrap()).collect();
        let starts: Vec<f64> = ws.iter().map(|w| w.start_secs).collect();
        assert_eq!(starts, vec![0.0, 25.0, 50.0]);
        assert_eq!(ws[2].samples.len(), 20 * R);
        assert!((it.total_secs() - 70.0).abs() < 1e-6);
    }

    #[test]
    fn aiff_container_decodes() {
        // Hand-built 16-bit big-endian AIFF, 8 kHz mono, 1 s of 400 Hz tone.
        let rate = 8000u32;
        let tone = sine(rate, 1.0, 400.0, 0.5);
        let mut ssnd = Vec::new();
        ssnd.extend_from_slice(&[0u8; 8]); // offset, block size
        for &s in &tone {
            ssnd.extend_from_slice(&((s * 32767.0) as i16).to_be_bytes());
        }
        // 80-bit extended float for the sample rate (8000 = 0x400B FA00...).
        let ext: [u8; 10] = [0x40, 0x0B, 0xFA, 0, 0, 0, 0, 0, 0, 0];
        let mut comm = Vec::new();
        comm.extend_from_slice(&1u16.to_be_bytes());
        comm.extend_from_slice(&(tone.len() as u32).to_be_bytes());
        comm.extend_from_slice(&16u16.to_be_bytes());
        comm.extend_from_slice(&ext);
        let mut body = Vec::new();
        body.extend_from_slice(b"AIFF");
        body.extend_from_slice(b"COMM");
        body.extend_from_slice(&(comm.len() as u32).to_be_bytes());
        body.extend_from_slice(&comm);
        body.extend_from_slice(b"SSND");
        body.extend_from_slice(&(ssnd.len() as u32).to_be_bytes());
        body.extend_from_slice(&ssnd);
        let p = tmp("tone.aiff");
        let mut f = File::create(&p).unwrap();
        f.write_all(b"FORM").unwrap();
        f.write_all(&(body.len() as u32).to_be_bytes()).unwrap();
        f.write_all(&body).unwrap();
        drop(f);
        let out = decode_all(&p).unwrap();
        assert_eq!(out.len(), R);
        assert!((rms(&out[R / 4..3 * R / 4]) - 0.5 / 2f64.sqrt()).abs() < 0.02);
    }

    #[test]
    fn typed_errors() {
        assert!(matches!(AudioStream::open(tmp("missing.wav")), Err(AudioError::Io(_))));
        let p = tmp("garbage.bin");
        std::fs::write(&p, vec![0x5Au8; 4096]).unwrap();
        assert!(matches!(AudioStream::open_soundtrack(&p), Err(AudioError::UnsupportedFormat(_))));
    }
}
