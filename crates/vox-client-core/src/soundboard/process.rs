//! Turning whatever file the user picks into the one format the hub stores.
//!
//! WAV, MP3, OGG (Vorbis), FLAC, M4A / MP4 audio (AAC, ALAC) and Opus (in Ogg: `.opus` / `.ogg`; in
//! WebM / Matroska: `.webm` / `.mka`) are decoded (symphonia, pure Rust; Opus through the libopus
//! that libwebrtc already links, see [`super::opus`]), then:
//!
//! 1. mixed down to mono or kept as stereo (a "stereo" file whose channels are the same is mono),
//! 2. trimmed: silence below -50 dBFS at the start and the end goes, with 10 ms of room left,
//! 3. checked against the 5 s limit (after the trim, so a clip with a long silent tail is fine),
//! 4. resampled to 48 kHz with a windowed-sinc filter,
//! 5. loudness-normalised to about -16 LUFS (BS.1770 K-weighting with gating, one gain for the
//!    whole clip, capped so the peaks stay below full scale; a soft limiter catches the rest),
//! 6. written as 16-bit PCM in a plain WAV ([`vox_proto::canonical_wav`]), which is what
//!    `vox_proto::parse_wav` (the hub's check) accepts: at most 480 KB for 5 s of mono.
//!
//! Everything is deterministic and runs in a few tens of milliseconds for a 5 s clip.
//!
//! Robustness: Ogg, WebM (own demuxer, [`super::webm`]) and truncated files of every format give
//! errors, never panics (the app is built with `panic = "abort"`). symphonia's MP4 and AAC code
//! can still panic on rare bit-flipped data (found by fuzzing: about 1 in 5 000 random corruptions,
//! none from truncation), the same class of risk as its MP3 / FLAC decoders.
//!
//! With the trim editor ([`super::trim`]) the same pipeline runs on a part of a long file: the
//! file is streamed from disk (never loaded whole), the frames before the selection are dropped
//! as they are decoded, decoding stops at the selection's end, and only the selection is kept.

use std::f32::consts::PI;
use std::io::Cursor;
use std::path::Path;

use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CodecParameters, Decoder, DecoderOptions, CODEC_TYPE_NULL, CODEC_TYPE_OPUS};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::{FormatOptions, FormatReader, Packet};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use vox_proto::{canonical_wav, SOUND_MAX_MS, SOUND_MIN_MS, SOUND_RATE};

use super::opus::{OpusHead, OpusStream, OPUS_RATE};
use super::webm::{self, WebmOpus};

/// The file extensions the "Adicionar som" dialog offers (lower case, no dot).
pub const SUPPORTED_FORMATS: &[&str] = &["wav", "mp3", "ogg", "opus", "m4a", "aac", "mp4", "webm", "mka", "flac"];

/// Files bigger than this are refused. Files are streamed from disk, so this is not about memory:
/// it lets any 10 minute CD-quality WAV (106 MB) through and refuses the rest as "not a clip".
pub const MAX_SOURCE_BYTES: usize = 128 * 1024 * 1024;
/// The trim editor refuses audio longer than this (its overview is computed by decoding it all).
pub const MAX_EDIT_SECS: u32 = 600;
/// Without a selection, decoding stops after this much audio: nothing that long can end up
/// under 5 s.
const MAX_DECODE_SECS: usize = 90;
/// Loudness target, in LUFS.
pub const TARGET_LUFS: f32 = -16.0;
/// Silence threshold for the trim, as a linear amplitude (-50 dBFS).
const TRIM_THRESHOLD: f32 = 0.003_162_3;
/// Room kept around the sound after the trim.
const TRIM_PAD_MS: usize = 10;
/// Loudness gain limits, in dB.
const MIN_GAIN_DB: f32 = -30.0;
const MAX_GAIN_DB: f32 = 30.0;
/// Peaks may be amplified up to this much above full scale before the soft limiter; keeps a
/// short loud hit from dragging the whole clip's loudness down.
const PEAK_HEADROOM: f32 = 1.9;

/// Why a file could not become a sound. The texts are Portuguese, ready to show.
#[derive(Debug, Clone, PartialEq)]
pub enum ProcessError {
    /// Not a readable audio file in one of [`SUPPORTED_FORMATS`].
    Unsupported(String),
    TooBig,
    /// The editor takes up to [`MAX_EDIT_SECS`] of audio.
    SourceTooLong,
    /// The chosen part is empty or over 5 s.
    BadSelection,
    /// The caller gave up (the dialog was closed or another file chosen).
    Cancelled,
    /// Nothing but silence (or too quiet to be a sound).
    Silent,
    /// Longer than 5 s once the silence is trimmed.
    TooLong { secs: f32 },
}

impl std::fmt::Display for ProcessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProcessError::Unsupported(why) => write!(f, "Não consegui ler esse arquivo de áudio ({why}). Use WAV, MP3, OGG, OPUS, M4A, WEBM ou FLAC."),
            ProcessError::TooBig => write!(f, "Esse arquivo é grande demais (o máximo é {} MB).", MAX_SOURCE_BYTES / (1024 * 1024)),
            ProcessError::SourceTooLong => write!(f, "Esse áudio tem mais de {} minutos. Corte um pedaço menor antes de usar aqui.", MAX_EDIT_SECS / 60),
            ProcessError::BadSelection => write!(f, "O trecho escolhido é inválido: ele precisa ter entre 0,1 e {} segundos.", SOUND_MAX_MS / 1000),
            ProcessError::Cancelled => write!(f, "Cancelado."),
            ProcessError::Silent => write!(f, "Esse áudio está em silêncio."),
            ProcessError::TooLong { secs } => write!(f, "O som tem {secs:.1} s depois de cortar o silêncio; o máximo é {} s.", SOUND_MAX_MS / 1000),
        }
    }
}

impl std::error::Error for ProcessError {}

/// A sound ready to upload, with what was done to it (for logs and the tests).
#[derive(Debug, Clone)]
pub struct Prepared {
    /// The WAV to upload.
    pub wav: Vec<u8>,
    pub channels: u16,
    pub frames: usize,
    pub duration_ms: u32,
    /// What the file was.
    pub source_rate: u32,
    pub source_channels: usize,
    /// Loudness of the trimmed clip before normalising (LUFS), and the gain applied (dB).
    pub loudness_before: f32,
    pub gain_db: f32,
    /// Peak of the result (linear, 0..=1).
    pub peak: f32,
    /// Milliseconds cut from the start and the end.
    pub trimmed_start_ms: u32,
    pub trimmed_end_ms: u32,
}

/// Decoded audio: one `Vec` per channel (1 or 2), at `rate`.
struct Decoded {
    rate: u32,
    source_channels: usize,
    planes: Vec<Vec<f32>>,
}

/// Fade length the editor starts with (long enough to remove a click, short enough not to be
/// heard as a fade).
pub const DEFAULT_FADE_MS: u32 = 10;

/// The part of a file to turn into a sound, and how to treat its edges. Times are on the file's
/// own timeline, in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trim {
    pub start_ms: u32,
    pub end_ms: u32,
    /// Fade-in / fade-out at the edges, in ms (0 = none).
    pub fade_in_ms: u32,
    pub fade_out_ms: u32,
    /// Also cut the silence at the ends of the selection (as is done for a whole file).
    pub trim_silence: bool,
}

impl Trim {
    pub fn len_ms(&self) -> u32 {
        self.end_ms.saturating_sub(self.start_ms)
    }

    fn validate(&self) -> Result<(), ProcessError> {
        if self.end_ms <= self.start_ms || self.len_ms() > SOUND_MAX_MS {
            return Err(ProcessError::BadSelection);
        }
        Ok(())
    }
}

/// How `process` treats the cut edges.
#[derive(Debug, Clone, Copy)]
struct Options {
    trim_silence: bool,
    fade_in_ms: u32,
    fade_out_ms: u32,
}

impl Options {
    /// A whole file: silence goes, and a short fade hides where the trim sliced a decaying tail.
    const WHOLE_FILE: Options = Options { trim_silence: true, fade_in_ms: 3, fade_out_ms: 15 };
}

/// `file_name` only provides the extension hint.
pub fn prepare(bytes: &[u8], file_name: Option<&str>) -> Result<Prepared, ProcessError> {
    if bytes.len() > MAX_SOURCE_BYTES {
        return Err(ProcessError::TooBig);
    }
    let decoded = decode_range(Box::new(Cursor::new(bytes.to_vec())), file_name, 0.0, MAX_DECODE_SECS as f64)?;
    process(decoded, Options::WHOLE_FILE)
}

/// Like [`prepare`] for a file on disk, which is streamed (never loaded whole). With `trim`, only
/// that part of the file is used: the frames before it are dropped while decoding and decoding
/// stops where it ends, so a 10 minute file costs a decode pass but no memory. Without it the file
/// must fit the sound limit after the silence is cut, as with [`prepare`].
pub fn prepare_file(path: &Path, trim: Option<&Trim>) -> Result<Prepared, ProcessError> {
    let file = std::fs::File::open(path).map_err(|_| unsupported("não consegui abrir o arquivo"))?;
    if file.metadata().is_ok_and(|m| m.len() > MAX_SOURCE_BYTES as u64) {
        return Err(ProcessError::TooBig);
    }
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
    match trim {
        None => process(decode_range(Box::new(file), name.as_deref(), 0.0, MAX_DECODE_SECS as f64)?, Options::WHOLE_FILE),
        Some(t) => {
            t.validate()?;
            let d = decode_range(Box::new(file), name.as_deref(), t.start_ms as f64 / 1000.0, t.end_ms as f64 / 1000.0)?;
            process(d, Options { trim_silence: t.trim_silence, fade_in_ms: t.fade_in_ms, fade_out_ms: t.fade_out_ms })
        }
    }
}

fn unsupported(e: impl std::fmt::Display) -> ProcessError {
    ProcessError::Unsupported(e.to_string())
}

/// Decoded audio: interleaved samples, their channel count and rate.
type Chunk<'a> = (&'a [f32], usize, u32);

/// How the chosen track is decoded: symphonia's decoders, or [`OpusStream`] (symphonia demuxes
/// Opus but cannot decode it).
enum Engine {
    Symphonia { decoder: Box<dyn Decoder>, buf: Option<SampleBuffer<f32>> },
    Opus(OpusStream),
}

impl Engine {
    /// One packet decoded into interleaved f32: `Ok(Some((samples, channels, rate)))`, `Ok(None)`
    /// for nothing to hand over (a damaged Opus packet), `Err` for a symphonia error.
    fn decode(&mut self, packet: &Packet) -> Result<Option<Chunk<'_>>, SymError> {
        match self {
            Engine::Symphonia { decoder, buf } => {
                let audio = decoder.decode(packet)?;
                let spec = *audio.spec();
                let (rate, channels) = (spec.rate, spec.channels.count());
                if buf.as_ref().is_none_or(|b| b.capacity() < audio.capacity() * channels.max(1)) {
                    *buf = Some(SampleBuffer::<f32>::new(audio.capacity() as u64, spec));
                }
                let b = buf.as_mut().expect("just set");
                b.copy_interleaved_ref(audio);
                Ok(Some((b.samples(), channels, rate)))
            }
            Engine::Opus(o) => {
                let channels = o.channels();
                Ok(o.decode(&packet.data, packet.trim_end as usize).map(|pcm| (pcm, channels, OPUS_RATE)))
            }
        }
    }
}

/// An Ogg Opus track's decoder, from the `OpusHead` symphonia keeps as the track's extra data.
/// The Ogg demuxer knows the stream length (its frame count includes the pre-skip), which trims the
/// codec's end padding.
fn ogg_opus_engine(params: &CodecParameters) -> Result<(Engine, Option<u64>), ProcessError> {
    let head = OpusHead::parse(params.extra_data.as_deref().ok_or_else(|| unsupported("Opus sem cabeçalho"))?)?;
    let total = params.n_frames.map(|n| n.saturating_sub(params.padding.unwrap_or(0) as u64).saturating_sub(head.pre_skip as u64));
    Ok((Engine::Opus(OpusStream::new(&head, total)?), total))
}

/// Where the packets come from: symphonia's demuxers, or [`WebmOpus`] for WebM / Matroska (whose
/// symphonia demuxer panics on damaged files, which would close the app: see [`super::webm`]).
enum Reader {
    Symphonia(Box<dyn FormatReader>),
    Webm(WebmOpus),
}

impl Reader {
    fn next_packet(&mut self) -> Result<Packet, SymError> {
        match self {
            Reader::Symphonia(f) => f.next_packet(),
            Reader::Webm(w) => w.next_packet(),
        }
    }
}

/// Can this track be decoded here?
fn decodable(params: &CodecParameters) -> bool {
    params.codec == CODEC_TYPE_OPUS || (params.codec != CODEC_TYPE_NULL && symphonia::default::get_codecs().get_codec(params.codec).is_some())
}

/// Decode `source` packet by packet, handing each decoded chunk (interleaved, with its channel
/// count and rate) to `on_chunk`, which says whether to go on. `on_open` sees the container's
/// frame count (when it knows it) and rate before anything is decoded. A damaged packet is
/// skipped; an error after some audio was decoded just ends the stream.
///
/// This is the one place files are decoded, for the editor's overview pass and for the selection's
/// re-decode alike, so every format in [`SUPPORTED_FORMATS`] works in both.
pub(super) fn stream_decode(
    source: Box<dyn MediaSource>,
    file_name: Option<&str>,
    on_open: impl FnOnce(Option<u64>, u32) -> Result<(), ProcessError>,
    mut on_chunk: impl FnMut(&[f32], usize, u32) -> bool,
) -> Result<(), ProcessError> {
    let mut source = source;
    let mut magic = [0u8; 4];
    let sniffed = source.read(&mut magic).unwrap_or(0);
    let matroska = webm::is_matroska(&magic[..sniffed]);
    let (mut reader, mut engine, n_frames, rate_hint, track_id) = if matroska {
        let (w, head) = WebmOpus::open(source)?;
        let engine = Engine::Opus(OpusStream::new(&head, None)?);
        let track = w.track_id();
        (Reader::Webm(w), engine, None, OPUS_RATE, track)
    } else {
        source.rewind().map_err(|_| unsupported("não consegui ler o arquivo"))?;
        let mss = MediaSourceStream::new(source, Default::default());
        symphonia_open(mss, file_name)?
    };
    on_open(n_frames, rate_hint)?;
    let mut got_any = false;
    loop {
        let packet = match reader.next_packet() {
            Ok(p) => p,
            // The end of the stream is an I/O error "unexpected eof" for most formats.
            Err(SymError::IoError(_)) | Err(SymError::ResetRequired) => break,
            Err(e) => {
                if !got_any {
                    return Err(unsupported(e));
                }
                break;
            }
        };
        if packet.track_id() != track_id {
            continue;
        }
        match engine.decode(&packet) {
            Ok(Some((samples, channels, rate))) => {
                if rate < 8_000 || channels == 0 || samples.is_empty() {
                    continue;
                }
                got_any = true;
                if !on_chunk(samples, channels, rate) {
                    break;
                }
            }
            Ok(None) => continue,
            // A damaged packet: skip it.
            Err(SymError::DecodeError(_)) => continue,
            Err(e) => {
                if !got_any {
                    return Err(unsupported(e));
                }
                break;
            }
        }
    }
    if got_any {
        Ok(())
    } else {
        Err(unsupported("áudio vazio"))
    }
}

/// The type of what `stream_decode` sets up: reader, decoder, frame count, rate, track.
type Opened = (Reader, Engine, Option<u64>, u32, u32);

/// Probe a non-Matroska file with symphonia and set up the decoder for its first decodable track.
fn symphonia_open(mss: MediaSourceStream, file_name: Option<&str>) -> Result<Opened, ProcessError> {
    let mut hint = Hint::new();
    if let Some(ext) = file_name.and_then(|n| n.rsplit_once('.')).map(|(_, e)| e.to_ascii_lowercase()) {
        hint.with_extension(&ext);
    }
    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .map_err(|_| unsupported("formato desconhecido"))?;
    let format = probed.format;
    // The first track that can be decoded (an MP4 or WebM may also carry video); failing that, the
    // first with a known codec, which then says "codec não suportado".
    let track = format
        .tracks()
        .iter()
        .find(|t| decodable(&t.codec_params))
        .or_else(|| format.tracks().iter().find(|t| t.codec_params.codec != CODEC_TYPE_NULL))
        .ok_or_else(|| unsupported("sem áudio"))?;
    let track_id = track.id;
    let params = &track.codec_params;
    let (engine, n_frames, rate_hint) = if params.codec == CODEC_TYPE_OPUS {
        let (engine, total) = ogg_opus_engine(params)?;
        (engine, total, OPUS_RATE)
    } else {
        let decoder = symphonia::default::get_codecs().make(params, &DecoderOptions::default()).map_err(|_| unsupported("codec não suportado"))?;
        (Engine::Symphonia { decoder, buf: None }, params.n_frames, params.sample_rate.unwrap_or(0))
    };
    Ok((Reader::Symphonia(format), engine, n_frames, rate_hint, track_id))
}

/// Decode the part of the stream between `start_s` and `end_s` seconds (both on the file's own
/// timeline) into planes (1 or 2 channels; surround is folded into one).
fn decode_range(source: Box<dyn MediaSource>, file_name: Option<&str>, start_s: f64, end_s: f64) -> Result<Decoded, ProcessError> {
    let mut kept: Vec<f32> = Vec::new();
    let mut bounds: Option<(u64, u64)> = None;
    let mut pos = 0u64; // frames seen before the current chunk
    let mut last = (0u32, 0usize);
    stream_decode(
        source,
        file_name,
        |_, _| Ok(()),
        |chunk, channels, rate| {
            last = (rate, channels);
            let (from, to) = *bounds.get_or_insert_with(|| ((start_s * rate as f64).round() as u64, (end_s * rate as f64).round() as u64));
            let frames = (chunk.len() / channels) as u64;
            let (a, b) = (from.max(pos), to.min(pos + frames));
            if a < b {
                kept.extend_from_slice(&chunk[(a - pos) as usize * channels..(b - pos) as usize * channels]);
            }
            pos += frames;
            pos < to
        },
    )?;
    let (rate, channels) = last;
    if kept.is_empty() {
        return Err(ProcessError::BadSelection);
    }
    let frames = kept.len() / channels;
    let plane = |f: &dyn Fn(&[f32]) -> f32| (0..frames).map(|i| f(&kept[i * channels..(i + 1) * channels])).collect::<Vec<f32>>();
    let planes = match channels {
        1 => vec![std::mem::take(&mut kept)],
        2 => vec![plane(&|f| f[0]), plane(&|f| f[1])],
        // Surround: everything folded into one channel.
        n => vec![plane(&|f| f.iter().sum::<f32>() / n as f32)],
    };
    Ok(Decoded { rate, source_channels: channels, planes })
}

/// Raised-cosine fades on the first `fade_in` and the last `fade_out` frames of every plane
/// (each capped at half the clip, so they never overlap). The first and the last sample become
/// exactly zero, so the clip starts and ends without a click.
pub fn apply_fades(planes: &mut [Vec<f32>], fade_in: usize, fade_out: usize) {
    for p in planes {
        let half = p.len() / 2;
        let (fi, fo) = (fade_in.min(half), fade_out.min(half));
        let gain = |i: usize, n: usize| 0.5 - 0.5 * (PI * i as f32 / n as f32).cos();
        for (i, s) in p.iter_mut().take(fi).enumerate() {
            *s *= gain(i, fi);
        }
        for (i, s) in p.iter_mut().rev().take(fo).enumerate() {
            *s *= gain(i, fo);
        }
    }
}

fn process(d: Decoded, opts: Options) -> Result<Prepared, ProcessError> {
    let Decoded { rate, source_channels, mut planes } = d;
    // 1. Mono if both channels carry the same signal (side energy 40 dB under the mid).
    if planes.len() == 2 {
        let (mut mid, mut side) = (0f64, 0f64);
        for (l, r) in planes[0].iter().zip(&planes[1]) {
            mid += ((l + r) as f64 / 2.0).powi(2);
            side += ((l - r) as f64 / 2.0).powi(2);
        }
        if side <= mid * 1e-4 {
            let mono: Vec<f32> = planes[0].iter().zip(&planes[1]).map(|(l, r)| (l + r) / 2.0).collect();
            planes = vec![mono];
        }
    }
    // 2. Trim (a selection keeps its edges when the silence is not to be cut).
    let len = planes[0].len();
    let loud = |i: usize| planes.iter().any(|p| p[i].abs() > TRIM_THRESHOLD);
    let (Some(first), Some(last)) = ((0..len).find(|&i| loud(i)), (0..len).rev().find(|&i| loud(i))) else {
        return Err(ProcessError::Silent);
    };
    let pad = rate as usize * TRIM_PAD_MS / 1000;
    let (start, end) = if opts.trim_silence { (first.saturating_sub(pad), (last + 1 + pad).min(len)) } else { (0, len) };
    // 3. Length (checked on the exact frame count the 48 kHz clip will have).
    let out_frames = ((end - start) as u64 * SOUND_RATE as u64).div_ceil(rate as u64) as usize;
    let max_frames = (SOUND_RATE as u64 * SOUND_MAX_MS as u64 / 1000) as usize;
    if out_frames > max_frames + 2 {
        return Err(ProcessError::TooLong { secs: (end - start) as f32 / rate as f32 });
    }
    let ms = |frames: usize| (frames as u64 * 1000 / rate as u64) as u32;
    let (trimmed_start_ms, trimmed_end_ms) = (ms(start), ms(len - end));
    let mut planes: Vec<Vec<f32>> = planes.into_iter().map(|p| p[start..end].to_vec()).collect();
    // 4. 48 kHz.
    for p in &mut planes {
        *p = resample(p, rate, SOUND_RATE);
        p.truncate(max_frames);
    }
    // Fades at the cut edges: no click where the cut sliced through a sound.
    let frames_of = |ms: u32| SOUND_RATE as usize * ms as usize / 1000;
    apply_fades(&mut planes, frames_of(opts.fade_in_ms), frames_of(opts.fade_out_ms));
    // A click is not a sound: pad to the hub's minimum with silence.
    let min_frames = (SOUND_RATE as usize * SOUND_MIN_MS as usize).div_ceil(1000);
    for p in &mut planes {
        if p.len() < min_frames {
            p.resize(min_frames, 0.0);
        }
    }
    // 5. Loudness.
    let lufs = loudness(&planes);
    if !lufs.is_finite() || lufs < -80.0 {
        return Err(ProcessError::Silent);
    }
    let peak_in = planes.iter().flatten().fold(0f32, |m, s| m.max(s.abs())).max(1e-9);
    let gain_db = (TARGET_LUFS - lufs).clamp(MIN_GAIN_DB, MAX_GAIN_DB);
    let gain = (10f32.powf(gain_db / 20.0)).min(PEAK_HEADROOM / peak_in);
    let mut peak = 0f32;
    for p in &mut planes {
        for s in p.iter_mut() {
            *s = soft_limit(*s * gain);
            peak = peak.max(s.abs());
        }
    }
    // 6. 16-bit PCM in a WAV.
    let channels = planes.len();
    let frames = planes[0].len();
    let mut pcm = Vec::with_capacity(frames * channels);
    for i in 0..frames {
        for p in &planes {
            pcm.push((p[i] * i16::MAX as f32).round().clamp(i16::MIN as f32, i16::MAX as f32) as i16);
        }
    }
    Ok(Prepared {
        wav: canonical_wav(channels as u16, &pcm),
        channels: channels as u16,
        frames,
        duration_ms: (frames as u64 * 1000 / SOUND_RATE as u64) as u32,
        source_rate: rate,
        source_channels,
        loudness_before: lufs,
        gain_db: 20.0 * gain.log10(),
        peak,
        trimmed_start_ms,
        trimmed_end_ms,
    })
}

/// Transparent below 0.8, then bends towards (never reaching) 1.0.
fn soft_limit(x: f32) -> f32 {
    const KNEE: f32 = 0.8;
    let a = x.abs();
    if a <= KNEE {
        x
    } else {
        x.signum() * (KNEE + (1.0 - KNEE) * ((a - KNEE) / (1.0 - KNEE)).tanh())
    }
}

// ---------------------------------------------------------------- resampling

/// Windowed-sinc resampling of one channel (Blackman window, 16 zero crossings each side; the
/// cutoff drops to the target's Nyquist when downsampling). The filter is tabulated at 1024
/// sub-sample positions (error below -55 dB).
pub fn resample(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || input.is_empty() {
        return input.to_vec();
    }
    const PHASES: usize = 1024;
    const ZERO_CROSSINGS: f32 = 16.0;
    let ratio = from as f64 / to as f64; // input samples per output sample
    let cutoff = (to as f32 / from as f32).min(1.0);
    let half = (ZERO_CROSSINGS / cutoff).ceil() as isize; // taps on each side, in input samples
    let width = (2 * half) as usize;
    // table[p][k]: weight of input sample (floor(t) - half + 1 + k) when t has fraction p / PHASES.
    let table: Vec<f32> = (0..PHASES)
        .flat_map(|p| {
            let frac = p as f32 / PHASES as f32;
            (0..width).map(move |k| {
                let x = (-half + 1 + k as isize) as f32 - frac; // input position relative to t
                let u = x / half as f32;
                let window = if u.abs() >= 1.0 { 0.0 } else { 0.42 + 0.5 * (PI * u).cos() + 0.08 * (2.0 * PI * u).cos() };
                let arg = PI * cutoff * x;
                let sinc = if arg.abs() < 1e-6 { 1.0 } else { arg.sin() / arg };
                cutoff * sinc * window
            })
        })
        .collect();
    let out_len = (input.len() as f64 / ratio).ceil() as usize;
    let mut out = Vec::with_capacity(out_len);
    for n in 0..out_len {
        let t = n as f64 * ratio;
        let i0 = t.floor() as isize;
        let phase = (((t - i0 as f64) * PHASES as f64).round() as usize).min(PHASES - 1);
        let taps = &table[phase * width..(phase + 1) * width];
        let base = i0 - half + 1;
        let mut acc = 0f32;
        for (k, w) in taps.iter().enumerate() {
            let idx = base + k as isize;
            if idx >= 0 && (idx as usize) < input.len() {
                acc += input[idx as usize] * w;
            }
        }
        out.push(acc);
    }
    out
}

// ---------------------------------------------------------------- loudness

struct Biquad {
    b: [f64; 3],
    a: [f64; 2],
    z: [f64; 2],
}

impl Biquad {
    fn new(b: [f64; 3], a: [f64; 2]) -> Self {
        Self { b, a, z: [0.0; 2] }
    }

    fn run(&mut self, x: f64) -> f64 {
        let y = self.b[0] * x + self.z[0];
        self.z[0] = self.b[1] * x - self.a[0] * y + self.z[1];
        self.z[1] = self.b[2] * x - self.a[1] * y;
        y
    }
}

/// Integrated loudness in LUFS of 48 kHz `planes` (ITU-R BS.1770-4: K-weighting, 400 ms blocks
/// with 75 % overlap, absolute gate at -70 LUFS and relative gate 10 LU below the mean). A clip
/// shorter than one block is measured whole. `-inf` for digital silence.
pub fn loudness(planes: &[Vec<f32>]) -> f32 {
    let weighted: Vec<Vec<f64>> = planes
        .iter()
        .map(|p| {
            // The 48 kHz coefficients of the standard: a high shelf, then a high-pass.
            let mut shelf = Biquad::new([1.53512485958697, -2.69169618940638, 1.19839281085285], [-1.69065929318241, 0.73248077421585]);
            let mut high = Biquad::new([1.0, -2.0, 1.0], [-1.99004745483398, 0.99007225036621]);
            p.iter().map(|&s| high.run(shelf.run(s as f64))).collect()
        })
        .collect();
    let len = weighted[0].len();
    let block = (SOUND_RATE as usize * 400 / 1000).min(len);
    let hop = (SOUND_RATE as usize * 100 / 1000).max(1);
    let mut powers: Vec<f64> = Vec::new();
    let mut at = 0;
    while at + block <= len {
        let sum: f64 = weighted.iter().map(|w| w[at..at + block].iter().map(|x| x * x).sum::<f64>() / block as f64).sum();
        powers.push(sum);
        at += hop;
    }
    let lufs = |p: f64| -0.691 + 10.0 * p.log10();
    let above: Vec<f64> = powers.iter().copied().filter(|&p| lufs(p) > -70.0).collect();
    if above.is_empty() {
        return f32::NEG_INFINITY;
    }
    let mean = above.iter().sum::<f64>() / above.len() as f64;
    let relative = lufs(mean) - 10.0;
    let gated: Vec<f64> = above.into_iter().filter(|&p| lufs(p) > relative).collect();
    if gated.is_empty() {
        return lufs(mean) as f32;
    }
    lufs(gated.iter().sum::<f64>() / gated.len() as f64) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A PCM16 WAV of any rate / channel count (what a user could have on disk).
    fn wav(rate: u32, channels: u16, samples: &[i16]) -> Vec<u8> {
        let mut v = Vec::new();
        let data = (samples.len() * 2) as u32;
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&(36 + data).to_le_bytes());
        v.extend_from_slice(b"WAVEfmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&channels.to_le_bytes());
        v.extend_from_slice(&rate.to_le_bytes());
        v.extend_from_slice(&(rate * 2 * channels as u32).to_le_bytes());
        v.extend_from_slice(&(2 * channels).to_le_bytes());
        v.extend_from_slice(&16u16.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&data.to_le_bytes());
        for s in samples {
            v.extend_from_slice(&s.to_le_bytes());
        }
        v
    }

    /// `lead` s of silence, `tone` s of a `freq` Hz sine at `amp`, `tail` s of silence.
    fn burst(rate: u32, channels: u16, lead: f32, tone: f32, tail: f32, freq: f32, amp: f32) -> Vec<i16> {
        let n = |s: f32| (s * rate as f32) as usize;
        let mut out = Vec::new();
        for i in 0..n(lead) + n(tone) + n(tail) {
            let v = if i >= n(lead) && i < n(lead) + n(tone) {
                ((i - n(lead)) as f32 * freq * 2.0 * PI / rate as f32).sin() * amp
            } else {
                0.0
            };
            out.extend(std::iter::repeat_n((v * 32767.0) as i16, channels as usize));
        }
        out
    }

    fn samples_of(p: &Prepared) -> Vec<i16> {
        let info = vox_proto::parse_wav(&p.wav).expect("the hub accepts it");
        vox_proto::wav_samples(&p.wav, &info)
    }

    fn rms_db(s: &[i16]) -> f32 {
        let ms = s.iter().map(|&x| (x as f64 / 32768.0).powi(2)).sum::<f64>() / s.len() as f64;
        (10.0 * ms.log10()) as f32
    }

    #[test]
    fn a_sine_burst_in_a_wav_is_trimmed_resampled_and_normalised() {
        let src = wav(44_100, 1, &burst(44_100, 1, 0.5, 1.0, 0.5, 440.0, 0.05));
        let p = prepare(&src, Some("beep.wav")).unwrap();
        assert_eq!((p.channels, p.source_rate), (1, 44_100));
        // 1 s of tone, 10 ms of room each side (the silence around it is gone).
        assert!((1_010..=1_030).contains(&p.duration_ms), "{} ms", p.duration_ms);
        assert!((480..=500).contains(&p.trimmed_start_ms) && (480..=500).contains(&p.trimmed_end_ms), "{} / {}", p.trimmed_start_ms, p.trimmed_end_ms);
        // The hub's own check accepts it.
        let s = samples_of(&p);
        assert_eq!(s.len(), p.frames);
        // Quiet source (-26 dBFS peak) brought to about -16 LUFS. For a steady tone LUFS is about
        // its RMS in dBFS (the K-weighting's +0.7 dB at 1 kHz cancels the standard's -0.691).
        let tone = &s[2_000..s.len() - 2_000];
        assert!((-17.5..-14.5).contains(&rms_db(tone)), "rms {}", rms_db(tone));
        assert!(p.gain_db > 10.0, "a quiet clip is amplified: {}", p.gain_db);
        assert!(p.peak < 1.0 && p.peak > 0.2);
        // The edges are silent (no click) and the pitch survived the resampling: count crossings.
        assert!(s[0].abs() < 50 && s[s.len() - 1].abs() < 50);
        let crossings = tone.windows(2).filter(|w| (w[0] < 0) != (w[1] < 0)).count();
        let hz = crossings as f32 / 2.0 / (tone.len() as f32 / 48_000.0);
        assert!((hz - 440.0).abs() < 8.0, "{hz} Hz");
    }

    #[test]
    fn loud_clips_come_down_and_peaks_never_clip() {
        let src = wav(48_000, 1, &burst(48_000, 1, 0.0, 0.5, 0.0, 1000.0, 0.99));
        let p = prepare(&src, None).unwrap();
        assert!(p.gain_db < -3.0, "a full-scale sine is turned down: {}", p.gain_db);
        let s = samples_of(&p);
        let tone = &s[1_000..s.len() - 1_000];
        assert!((-17.5..-14.5).contains(&rms_db(tone)), "{}", rms_db(tone));
        // A clip made of a very short hit: loudness would ask for more than the peak allows; the
        // result must still stay inside full scale without wrapping.
        let mut hit = vec![0i16; 48_000];
        for (i, v) in hit.iter_mut().enumerate().take(480) {
            *v = (((i as f32) * 0.3).sin() * 20_000.0 * (1.0 - i as f32 / 480.0)) as i16;
        }
        let p = prepare(&wav(48_000, 1, &hit), None).unwrap();
        assert!(p.peak <= 1.0, "{}", p.peak);
        let s = samples_of(&p);
        assert!(s.iter().all(|&x| x > i16::MIN), "no wrapped sample");
    }

    #[test]
    fn stereo_is_kept_and_identical_channels_become_mono() {
        let n = 48_000 / 2;
        let mut left_only = Vec::with_capacity(n * 2);
        for i in 0..n {
            left_only.push(((i as f32 * 0.05).sin() * 8000.0) as i16);
            left_only.push(((i as f32 * 0.09).sin() * 8000.0) as i16);
        }
        let p = prepare(&wav(48_000, 2, &left_only), None).unwrap();
        assert_eq!(p.channels, 2, "different channels stay stereo");
        assert_eq!(samples_of(&p).len(), p.frames * 2);
        let same: Vec<i16> = (0..n).flat_map(|i| {
            let v = ((i as f32 * 0.05).sin() * 8000.0) as i16;
            [v, v]
        }).collect();
        let p = prepare(&wav(48_000, 2, &same), None).unwrap();
        assert_eq!(p.channels, 1, "a stereo file of two equal channels is stored as mono (half the size)");
    }

    #[test]
    fn silence_and_garbage_and_length_are_refused_with_a_reason() {
        let silent = wav(48_000, 1, &vec![0i16; 48_000]);
        assert_eq!(prepare(&silent, None).unwrap_err(), ProcessError::Silent);
        // Almost-silent noise below -50 dBFS counts as silence too.
        let hiss = wav(48_000, 1, &(0..48_000).map(|i| if i % 2 == 0 { 20i16 } else { -20 }).collect::<Vec<_>>());
        assert_eq!(prepare(&hiss, None).unwrap_err(), ProcessError::Silent);
        assert!(matches!(prepare(b"this is not audio at all, just text", Some("x.mp3")), Err(ProcessError::Unsupported(_))));
        assert!(matches!(prepare(&[], None), Err(ProcessError::Unsupported(_))));
        // 6 s of tone is too long, and says by how much; 5 s of tone with a long silent tail is fine.
        let long = wav(48_000, 1, &burst(48_000, 1, 0.0, 6.0, 0.0, 440.0, 0.3));
        match prepare(&long, None).unwrap_err() {
            ProcessError::TooLong { secs } => assert!((6.0..6.1).contains(&secs), "{secs}"),
            e => panic!("{e:?}"),
        }
        let msg = ProcessError::TooLong { secs: 6.04 }.to_string();
        assert!(msg.contains("6.0") && msg.contains("5 s"), "{msg}");
        let tailed = wav(48_000, 1, &burst(48_000, 1, 2.0, 4.5, 8.0, 440.0, 0.3));
        let p = prepare(&tailed, None).unwrap();
        assert!(p.duration_ms <= 4_600, "{}", p.duration_ms);
        // Right at the limit with the pad: 4.99 s of tone + pads is over, 4.97 s fits.
        assert!(prepare(&wav(48_000, 1, &burst(48_000, 1, 1.0, 4.97, 1.0, 440.0, 0.3)), None).unwrap().duration_ms <= 5_000);
        assert!(matches!(prepare(&wav(48_000, 1, &burst(48_000, 1, 1.0, 4.99, 1.0, 440.0, 0.3)), None), Err(ProcessError::TooLong { .. })));
        // A click is padded up to the hub's minimum.
        let click = wav(48_000, 1, &burst(48_000, 1, 0.1, 0.02, 0.1, 440.0, 0.5));
        let p = prepare(&click, None).unwrap();
        assert!(p.duration_ms >= SOUND_MIN_MS, "{}", p.duration_ms);
        assert!(vox_proto::parse_wav(&p.wav).is_ok());
    }

    #[test]
    fn the_result_always_fits_the_upload_limits() {
        // The longest, widest clip: 5 s of different stereo channels.
        let n = 48_000 * 5;
        let mut s = Vec::with_capacity(n * 2);
        for i in 0..n {
            s.push(((i as f32 * 0.03).sin() * 12_000.0) as i16);
            s.push(((i as f32 * 0.07).sin() * 12_000.0) as i16);
        }
        // Exactly 5 s of signal has no room for the pads: the cut keeps it at the limit.
        let p = prepare(&wav(48_000, 2, &s), None);
        match p {
            Ok(p) => assert!(p.wav.len() <= vox_proto::SOUND_MAX_UPLOAD_BYTES && p.duration_ms <= 5_000),
            Err(ProcessError::TooLong { .. }) => {}
            Err(e) => panic!("{e:?}"),
        }
        let s4: Vec<i16> = s[..48_000 * 4 * 2].to_vec();
        let p = prepare(&wav(48_000, 2, &s4), None).unwrap();
        assert!(p.wav.len() <= vox_proto::SOUND_MAX_UPLOAD_BYTES);
        assert!(vox_proto::parse_wav(&p.wav).is_ok());
    }

    #[test]
    fn fades_start_and_end_at_exactly_zero_and_leave_the_middle_alone() {
        let ones = |n: usize| vec![vec![1.0f32; n], vec![-1.0f32; n]];
        let mut p = ones(2_000);
        apply_fades(&mut p, 480, 480);
        for (plane, sign) in p.iter().zip([1.0f32, -1.0]) {
            let v: Vec<f32> = plane.iter().map(|x| x * sign).collect();
            assert_eq!((v[0], v[1_999]), (0.0, 0.0), "no click at either edge");
            assert!(v[..480].windows(2).all(|w| w[1] >= w[0]), "rising");
            assert!(v[1_520..].windows(2).all(|w| w[1] <= w[0]), "falling");
            assert!(v[480..1_520].iter().all(|&x| x == 1.0), "the middle is untouched");
            // Smooth: no step bigger than a few percent of full scale anywhere.
            assert!(v.windows(2).all(|w| (w[1] - w[0]).abs() < 0.01), "steps stay small");
        }
        // Raised cosine: half level halfway through the fade.
        assert!((p[0][240] - 0.5).abs() < 0.01, "{}", p[0][240]);
        // A clip shorter than the fades: they are capped at half the clip each and never overlap.
        let mut short = ones(100);
        apply_fades(&mut short, 480, 480);
        assert_eq!((short[0][0], short[0][99]), (0.0, 0.0));
        assert!(short[0].iter().all(|x| (0.0..=1.0).contains(x)));
        // No fade at all changes nothing, and an empty plane is fine.
        let mut none = ones(100);
        apply_fades(&mut none, 0, 0);
        assert_eq!(none, ones(100));
        apply_fades(&mut [Vec::new()], 480, 480);
    }

    #[test]
    fn resampling_keeps_pitch_and_length() {
        for (from, to) in [(44_100u32, 48_000u32), (22_050, 48_000), (96_000, 48_000), (8_000, 48_000), (48_000, 48_000)] {
            let n = from as usize;
            let tone: Vec<f32> = (0..n).map(|i| (i as f32 * 1_000.0 * 2.0 * PI / from as f32).sin() * 0.5).collect();
            let out = resample(&tone, from, to);
            assert!((out.len() as i64 - to as i64).abs() <= 1, "{from}->{to}: {} samples", out.len());
            let mid = &out[out.len() / 10..out.len() * 9 / 10];
            let crossings = mid.windows(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count() as f32;
            let hz = crossings / 2.0 / (mid.len() as f32 / to as f32);
            assert!((hz - 1_000.0).abs() < 15.0, "{from}->{to}: {hz} Hz");
            let peak = mid.iter().fold(0f32, |m, s| m.max(s.abs()));
            assert!((0.47..0.53).contains(&peak), "{from}->{to}: amplitude {peak}");
        }
        // Downsampling removes what the new rate cannot carry (a 30 kHz tone at 96 kHz -> 48 kHz).
        let high: Vec<f32> = (0..96_000).map(|i| (i as f32 * 30_000.0 * 2.0 * PI / 96_000.0).sin() * 0.5).collect();
        let out = resample(&high, 96_000, 48_000);
        let peak = out[4_800..out.len() - 4_800].iter().fold(0f32, |m, s| m.max(s.abs()));
        assert!(peak < 0.02, "aliasing: {peak}");
    }

    #[test]
    fn loudness_follows_bs1770() {
        // BS.1770 sums the channels' powers: a 997 Hz sine at -20 dBFS RMS is -20 LUFS on its own
        // and -17 LUFS as a stereo pair.
        let sine = |amp: f32| -> Vec<f32> { (0..48_000 * 3).map(|i| (i as f32 * 997.0 * 2.0 * PI / 48_000.0).sin() * amp).collect() };
        let amp = 10f32.powf(-20.0 / 20.0) * 2f32.sqrt(); // -20 dBFS RMS
        let stereo = loudness(&[sine(amp), sine(amp)]);
        assert!((stereo + 17.0).abs() < 0.3, "{stereo}");
        let mono = loudness(&[sine(amp)]);
        assert!((mono + 20.0).abs() < 0.3, "{mono}");
        assert!((stereo - mono - 3.01).abs() < 0.1, "mono {mono} vs stereo {stereo}");
        // Twice the amplitude is 6 dB louder.
        assert!((loudness(&[sine(amp * 2.0)]) - mono - 6.02).abs() < 0.1);
        assert_eq!(loudness(&[vec![0.0; 48_000]]), f32::NEG_INFINITY);
        // Gating: a long quiet stretch does not drag a short loud part down much.
        let mut mixed = vec![0f32; 48_000 * 2];
        mixed.extend(sine(amp).iter().take(48_000));
        let gated = loudness(&[mixed]);
        assert!((gated - mono).abs() < 1.0, "{gated} vs {mono}");
    }
}
