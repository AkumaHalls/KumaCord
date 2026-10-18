//! The trim editor's model: what the "Adicionar som" dialog needs to let someone pick the part
//! (at most 5 s) of a long recording that becomes the sound.
//!
//! Memory is the design constraint. The recording is decoded once, on a background thread, as a
//! stream: each 10 ms of audio is reduced to one [`Block`] (its lowest and highest sample and its
//! mean power) and thrown away. A 10 minute file therefore costs 60 000 blocks (about 700 KB),
//! never the 115 MB its 48 kHz f32 samples would take, and there is no decoded PCM to free when the
//! dialog closes. The waveform, the "loudest 5 s" suggestion and the zoom are all drawn from those
//! blocks. The audio of the selection (to preview it, and to upload it) is decoded again when
//! needed, dropping the frames before the selection and stopping at its end
//! ([`super::process::prepare_file`]); symphonia's seeking is not used, because it is approximate
//! for MP3 and Vorbis and an exact start matters more than saving a decode pass.
//!
//! Limits: [`MAX_EDIT_SECS`] (10 minutes) of audio and [`MAX_SOURCE_BYTES`] (128 MB) per file, and
//! [`MAX_SELECTION`] (5 s) per sound, which is what the hub accepts.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use symphonia::core::io::MediaSource;
use vox_proto::SOUND_MAX_MS;

use super::process::{stream_decode, Trim, DEFAULT_FADE_MS, MAX_EDIT_SECS, MAX_SOURCE_BYTES};
use super::ProcessError;

/// The longest selection, in seconds (the hub's limit).
pub const MAX_SELECTION: f64 = SOUND_MAX_MS as f64 / 1000.0;
/// The shortest selection the handles allow, in seconds.
pub const MIN_SELECTION: f64 = 0.05;
/// Overview resolution: blocks per second.
const BLOCKS_PER_SEC: u32 = 100;

// ---------------------------------------------------------------- overview

/// 10 ms of the recording, reduced.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Block {
    /// Lowest and highest sample of any channel.
    pub min: f32,
    pub max: f32,
    /// Mean power (mean square of the channels' average).
    pub power: f32,
}

/// The whole recording as a row of [`Block`]s.
#[derive(Debug, Clone, Default)]
pub struct Overview {
    pub rate: u32,
    pub channels: usize,
    /// Frames per block (the last block may hold fewer).
    pub block_frames: usize,
    pub frames: u64,
    pub blocks: Vec<Block>,
}

impl Overview {
    pub fn duration(&self) -> f64 {
        if self.rate == 0 {
            0.0
        } else {
            self.frames as f64 / self.rate as f64
        }
    }

    fn block_secs(&self) -> f64 {
        self.block_frames as f64 / self.rate.max(1) as f64
    }

    /// The highest absolute sample.
    pub fn peak(&self) -> f32 {
        self.blocks.iter().fold(0f32, |m, b| m.max(b.max.abs()).max(b.min.abs()))
    }

    /// `n` columns covering the time span `t0..t1` (seconds): each column's lowest and highest
    /// sample. A column outside the recording (or an empty span) is `(0, 0)`. This is what the
    /// waveform draws, one vertical line per pixel.
    pub fn columns(&self, t0: f64, t1: f64, n: usize) -> Vec<(f32, f32)> {
        let mut out = vec![(0f32, 0f32); n];
        if n == 0 || self.blocks.is_empty() || t1 <= t0 {
            return out;
        }
        let bs = self.block_secs();
        let per_col = (t1 - t0) / n as f64;
        for (c, slot) in out.iter_mut().enumerate() {
            let a = t0 + c as f64 * per_col;
            let b = a + per_col;
            if b <= 0.0 {
                continue;
            }
            let first = (a.max(0.0) / bs).floor() as usize;
            // At least one block, even when a column is narrower than a block.
            let last = ((b / bs).ceil() as usize).max(first + 1).min(self.blocks.len());
            if first >= self.blocks.len() {
                continue;
            }
            let (mut lo, mut hi) = (f32::MAX, f32::MIN);
            for blk in &self.blocks[first..last] {
                lo = lo.min(blk.min);
                hi = hi.max(blk.max);
            }
            *slot = (lo.min(0.0), hi.max(0.0));
        }
        out
    }

    /// Where the loudest `len` seconds start (by total power; the earliest wins a tie, and an
    /// all-silent recording gives 0). A recording no longer than `len` starts at 0.
    pub fn loudest_window(&self, len: f64) -> f64 {
        let dur = self.duration();
        let bs = self.block_secs();
        if dur <= len || bs <= 0.0 || self.blocks.is_empty() {
            return 0.0;
        }
        let k = ((len / bs).round() as usize).clamp(1, self.blocks.len());
        let mut sum: f64 = self.blocks[..k].iter().map(|b| b.power as f64).sum();
        let (mut best, mut best_at) = (sum, 0usize);
        for i in 1..=self.blocks.len() - k {
            sum += self.blocks[i + k - 1].power as f64 - self.blocks[i - 1].power as f64;
            // Powers within 0.01 % are a tie (float noise must not move the pick off the
            // earliest window).
            if sum > best * (1.0 + 1e-4) + 1e-18 {
                best = sum;
                best_at = i;
            }
        }
        (best_at as f64 * bs).min(dur - len).max(0.0)
    }
}

/// Builds an [`Overview`] from interleaved samples fed in any chunk sizes.
pub struct OverviewBuilder {
    rate: u32,
    channels: usize,
    block_frames: usize,
    blocks: Vec<Block>,
    n: usize,
    min: f32,
    max: f32,
    sumsq: f64,
    frames: u64,
}

impl OverviewBuilder {
    pub fn new(rate: u32) -> Self {
        Self {
            rate,
            channels: 0,
            block_frames: (rate / BLOCKS_PER_SEC).max(1) as usize,
            blocks: Vec::new(),
            n: 0,
            min: 0.0,
            max: 0.0,
            sumsq: 0.0,
            frames: 0,
        }
    }

    pub fn frames(&self) -> u64 {
        self.frames
    }

    pub fn push(&mut self, interleaved: &[f32], channels: usize) {
        let channels = channels.max(1);
        self.channels = self.channels.max(channels);
        for frame in interleaved.chunks_exact(channels) {
            let mut sum = 0f32;
            for &s in frame {
                self.min = self.min.min(s);
                self.max = self.max.max(s);
                sum += s;
            }
            let mono = (sum / channels as f32) as f64;
            self.sumsq += mono * mono;
            self.n += 1;
            self.frames += 1;
            if self.n == self.block_frames {
                self.close_block();
            }
        }
    }

    fn close_block(&mut self) {
        if self.n > 0 {
            self.blocks.push(Block { min: self.min, max: self.max, power: (self.sumsq / self.n as f64) as f32 });
        }
        self.n = 0;
        self.min = 0.0;
        self.max = 0.0;
        self.sumsq = 0.0;
    }

    pub fn finish(mut self) -> Overview {
        self.close_block();
        Overview { rate: self.rate, channels: self.channels, block_frames: self.block_frames, frames: self.frames, blocks: self.blocks }
    }
}

// ---------------------------------------------------------------- analysis

/// A file that counts how far into it the decoder has read (for the progress bar).
struct CountingFile {
    file: File,
    len: u64,
    pos: u64,
    furthest: Arc<AtomicU64>,
}

impl Read for CountingFile {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.file.read(buf)?;
        self.pos += n as u64;
        self.furthest.fetch_max(self.pos, Ordering::Relaxed);
        Ok(n)
    }
}

impl Seek for CountingFile {
    fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
        self.pos = self.file.seek(to)?;
        Ok(self.pos)
    }
}

impl MediaSource for CountingFile {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.len)
    }
}

/// Decode the whole file once, as a stream, into an [`Overview`]. `progress` gets 0..=1 as the
/// file is read; setting `cancel` stops it (the result is [`ProcessError::Cancelled`]). Refuses
/// files over [`MAX_SOURCE_BYTES`] and audio over [`MAX_EDIT_SECS`].
pub fn analyze(path: &Path, progress: &dyn Fn(f32), cancel: &AtomicBool) -> Result<Overview, ProcessError> {
    let file = File::open(path).map_err(|_| ProcessError::Unsupported("não consegui abrir o arquivo".into()))?;
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if len > MAX_SOURCE_BYTES as u64 {
        return Err(ProcessError::TooBig);
    }
    let furthest = Arc::new(AtomicU64::new(0));
    let source = CountingFile { file, len, pos: 0, furthest: furthest.clone() };
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
    let mut builder: Option<OverviewBuilder> = None;
    let (mut too_long, mut cancelled) = (false, false);
    stream_decode(
        Box::new(source),
        name.as_deref(),
        |n_frames, rate| {
            // The container often knows the length: refuse a long file before decoding any of it.
            if n_frames.is_some_and(|n| rate > 0 && n > rate as u64 * MAX_EDIT_SECS as u64) {
                return Err(ProcessError::SourceTooLong);
            }
            Ok(())
        },
        |chunk, channels, rate| {
            let b = builder.get_or_insert_with(|| OverviewBuilder::new(rate));
            b.push(chunk, channels);
            if len > 0 {
                progress((furthest.load(Ordering::Relaxed) as f32 / len as f32).min(1.0));
            }
            if cancel.load(Ordering::Relaxed) {
                cancelled = true;
                return false;
            }
            if b.frames() > rate as u64 * MAX_EDIT_SECS as u64 {
                too_long = true;
                return false;
            }
            true
        },
    )?;
    if cancelled {
        return Err(ProcessError::Cancelled);
    }
    if too_long {
        return Err(ProcessError::SourceTooLong);
    }
    progress(1.0);
    builder.map(OverviewBuilder::finish).ok_or_else(|| ProcessError::Unsupported("áudio vazio".into()))
}

// ---------------------------------------------------------------- selection

/// The chosen part of the recording, in seconds. Always inside the recording and never longer than
/// [`MAX_SELECTION`]; every method returns a valid selection whatever it is given.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Selection {
    pub start: f64,
    pub end: f64,
}

impl Selection {
    /// `start..end` made valid for a recording of `duration` seconds: ordered, inside it, at most
    /// 5 s (the end gives way) and at least [`MIN_SELECTION`] (or the whole recording if shorter).
    pub fn new(start: f64, end: f64, duration: f64) -> Self {
        let duration = duration.max(0.0);
        let (mut a, mut b) = if start <= end { (start, end) } else { (end, start) };
        if !a.is_finite() || !b.is_finite() {
            (a, b) = (0.0, MAX_SELECTION);
        }
        let min = MIN_SELECTION.min(duration);
        a = a.clamp(0.0, (duration - min).max(0.0));
        b = b.clamp(a + min, duration.max(a + min)).min(a + MAX_SELECTION).min(duration);
        Selection { start: a, end: b.max(a) }
    }

    /// The first `MAX_SELECTION` seconds (all of a shorter recording).
    pub fn at_start(duration: f64) -> Self {
        Self::new(0.0, MAX_SELECTION, duration)
    }

    pub fn len(&self) -> f64 {
        self.end - self.start
    }

    /// Move the start handle; the end stays, and the start cannot go more than 5 s before it.
    pub fn with_start(self, start: f64, duration: f64) -> Self {
        let lo = (self.end - MAX_SELECTION).max(0.0);
        let hi = (self.end - MIN_SELECTION.min(duration)).max(lo);
        Self::new(start.clamp(lo, hi), self.end, duration)
    }

    /// Move the end handle; the start stays, and the end cannot go more than 5 s after it.
    pub fn with_end(self, end: f64, duration: f64) -> Self {
        let lo = self.start + MIN_SELECTION.min(duration);
        let hi = (self.start + MAX_SELECTION).min(duration).max(lo.min(duration));
        Self::new(self.start, end.clamp(lo.min(hi), hi), duration)
    }

    /// Slide the whole selection by `delta` seconds, keeping its length, inside the recording.
    pub fn shifted(self, delta: f64, duration: f64) -> Self {
        let len = self.len();
        let start = (self.start + delta).clamp(0.0, (duration - len).max(0.0));
        Self::new(start, start + len, duration)
    }

    /// The same length, centred on `t` as far as the recording allows.
    pub fn centered_on(self, t: f64, duration: f64) -> Self {
        let len = self.len();
        self.shifted(t - len / 2.0 - self.start, duration)
    }

    /// What the processing needs: whole milliseconds (never more than 5000 of them).
    pub fn to_trim(&self, fade_in: bool, fade_out: bool, trim_silence: bool) -> Trim {
        let start_ms = (self.start * 1000.0).round() as u32;
        let end_ms = ((self.end * 1000.0).round() as u32).min(start_ms + SOUND_MAX_MS);
        Trim {
            start_ms,
            end_ms: end_ms.max(start_ms + 1),
            fade_in_ms: if fade_in { DEFAULT_FADE_MS } else { 0 },
            fade_out_ms: if fade_out { DEFAULT_FADE_MS } else { 0 },
            trim_silence,
        }
    }
}

/// `m:ss.cc`, for the time readout.
pub fn format_time(secs: f64) -> String {
    let cs = (secs.max(0.0) * 100.0).round() as u64;
    format!("{}:{:02}.{:02}", cs / 6000, cs / 100 % 60, cs % 100)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    /// A mono 16-bit WAV on disk.
    fn write_wav(path: &Path, rate: u32, samples: &[f32]) {
        let data: Vec<u8> = samples.iter().flat_map(|&s| ((s * 32767.0) as i16).to_le_bytes()).collect();
        let mut v = Vec::new();
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        v.extend_from_slice(b"WAVEfmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&rate.to_le_bytes());
        v.extend_from_slice(&(rate * 2).to_le_bytes());
        v.extend_from_slice(&2u16.to_le_bytes());
        v.extend_from_slice(&16u16.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&(data.len() as u32).to_le_bytes());
        v.extend_from_slice(&data);
        std::fs::write(path, v).unwrap();
    }

    /// `total` seconds of silence with a 440 Hz tone at `amp` in each `(from, to)` span.
    fn bursts(rate: u32, total: f32, spans: &[(f32, f32)], amp: f32) -> Vec<f32> {
        (0..(total * rate as f32) as usize)
            .map(|i| {
                let t = i as f32 / rate as f32;
                if spans.iter().any(|&(a, b)| t >= a && t < b) {
                    (t * 440.0 * TAU).sin() * amp
                } else {
                    0.0
                }
            })
            .collect()
    }

    fn overview_of(rate: u32, samples: &[f32]) -> Overview {
        let mut b = OverviewBuilder::new(rate);
        b.push(samples, 1);
        b.finish()
    }

    #[test]
    fn peaks_are_the_lowest_and_highest_sample_of_each_column() {
        let rate = 8_000;
        let mut s = vec![0f32; rate as usize * 10];
        s[rate as usize * 3 + 17] = 0.8; // 3.002 s
        s[rate as usize * 7 + 400] = -0.6; // 7.05 s
        let o = overview_of(rate, &s);
        assert!((o.duration() - 10.0).abs() < 1e-9);
        assert_eq!(o.blocks.len(), 1000);
        assert_eq!(o.peak(), 0.8);
        // 10 columns of 1 s: the spike is in column 3, the dip in column 7, the rest is flat.
        let cols = o.columns(0.0, 10.0, 10);
        for (i, &(lo, hi)) in cols.iter().enumerate() {
            match i {
                3 => assert_eq!((lo, hi), (0.0, 0.8)),
                7 => assert_eq!((lo, hi), (-0.6, 0.0)),
                _ => assert_eq!((lo, hi), (0.0, 0.0), "column {i}"),
            }
            assert!(lo <= hi);
        }
        // Zoomed in on 6.9..7.1 s with 20 columns of 10 ms (one block each): exactly one is hit.
        let zoom = o.columns(6.9, 7.1, 20);
        let hit: Vec<_> = zoom.iter().enumerate().filter(|(_, c)| c.0 < 0.0).collect();
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].0, 15, "7.05 s is 15 blocks into the view");
        // More columns than blocks: a column narrower than a block still shows its block.
        let fine = o.columns(7.0, 7.1, 200);
        assert!(fine.iter().filter(|c| c.0 < 0.0).count() >= 1);
        // Out of range, empty and degenerate spans are flat, never a panic.
        assert!(o.columns(-5.0, 0.0, 4).iter().all(|&c| c == (0.0, 0.0)));
        assert!(o.columns(11.0, 20.0, 4).iter().all(|&c| c == (0.0, 0.0)));
        assert!(o.columns(5.0, 5.0, 4).iter().all(|&c| c == (0.0, 0.0)));
        assert!(o.columns(0.0, 10.0, 0).is_empty());
        // Half in, half out of the recording: the part that exists is drawn.
        let edge = o.columns(-3.0, 4.0, 7);
        assert_eq!(edge[6].1, 0.8);
        assert!(Overview::default().columns(0.0, 1.0, 3).iter().all(|&c| c == (0.0, 0.0)));
    }

    #[test]
    fn the_overview_does_not_depend_on_how_the_audio_arrives() {
        let rate = 22_050; // 220 frames per block: not a divisor of anything convenient
        let s: Vec<f32> = (0..rate as usize * 3).map(|i| ((i as f32 * 0.01).sin() * (i as f32 / 70_000.0)).clamp(-1.0, 1.0)).collect();
        let whole = overview_of(rate, &s);
        let mut chunked = OverviewBuilder::new(rate);
        for c in s.chunks(1013) {
            chunked.push(c, 1);
        }
        let chunked = chunked.finish();
        assert_eq!(whole.blocks, chunked.blocks);
        assert_eq!(whole.frames, chunked.frames);
        // Stereo: each side's extremes count, and the energy is that of the average.
        let mut st = OverviewBuilder::new(8_000);
        let inter: Vec<f32> = (0..800).flat_map(|i| if i == 5 { [0.9, -0.4] } else { [0.1, 0.1] }).collect();
        st.push(&inter, 2);
        let o = st.finish();
        assert_eq!((o.blocks[0].min, o.blocks[0].max, o.channels), (-0.4, 0.9, 2));
        // Memory: ten minutes of 48 kHz audio is 60 000 blocks.
        let mut big = OverviewBuilder::new(48_000);
        let minute = vec![0.25f32; 48_000 * 60];
        for _ in 0..10 {
            big.push(&minute, 1);
        }
        let big = big.finish();
        assert_eq!(big.blocks.len(), 60_000);
        assert!(std::mem::size_of_val(big.blocks.as_slice()) < 1_000_000);
    }

    #[test]
    fn the_loudest_window_covers_the_loud_part() {
        let rate = 8_000;
        // 30 s: quiet tone everywhere, a loud 4 s burst at 18..22 s.
        let mut s = bursts(rate, 30.0, &[(0.0, 30.0)], 0.05);
        for (i, v) in s.iter_mut().enumerate() {
            let t = i as f32 / rate as f32;
            if (18.0..22.0).contains(&t) {
                *v = (t * 300.0 * TAU).sin() * 0.7;
            }
        }
        let o = overview_of(rate, &s);
        let at = o.loudest_window(5.0);
        assert!((17.0 - 1e-6..=18.0 + 1e-6).contains(&at), "the 5 s window must hold the 4 s burst: {at}");
        // A burst at the very end: the window is clamped to the recording's end.
        let o = overview_of(rate, &bursts(rate, 12.0, &[(10.5, 12.0)], 0.5));
        assert!((o.loudest_window(5.0) - 7.0).abs() < 0.02, "{}", o.loudest_window(5.0));
        // Two equal bursts: the earlier one wins. Silence and short files start at 0.
        let o = overview_of(rate, &bursts(rate, 40.0, &[(5.0, 6.0), (25.0, 26.0)], 0.5));
        assert!(o.loudest_window(5.0) <= 5.0 + 1e-6);
        assert_eq!(overview_of(rate, &vec![0.0; rate as usize * 20]).loudest_window(5.0), 0.0);
        assert_eq!(overview_of(rate, &bursts(rate, 4.0, &[(1.0, 2.0)], 0.5)).loudest_window(5.0), 0.0);
        assert_eq!(Overview::default().loudest_window(5.0), 0.0);
    }

    #[test]
    fn a_selection_is_always_inside_the_recording_and_at_most_five_seconds() {
        let dur = 20.0;
        let s = Selection::new(3.0, 5.0, dur);
        assert_eq!((s.start, s.end), (3.0, 5.0));
        // Too long: the end gives way. Reversed: ordered. Outside: pulled in.
        let s = Selection::new(2.0, 18.0, dur);
        assert_eq!((s.start, s.len()), (2.0, MAX_SELECTION));
        assert_eq!(Selection::new(9.0, 4.0, dur), Selection::new(4.0, 9.0, dur).with_end(9.0, dur));
        let s = Selection::new(-4.0, 3.0, dur);
        assert_eq!((s.start, s.end), (0.0, 3.0));
        let s = Selection::new(19.0, 40.0, dur);
        assert!(s.end <= dur && s.start >= 0.0 && s.len() >= MIN_SELECTION - 1e-9, "{s:?}");
        let s = Selection::new(25.0, 30.0, dur);
        assert_eq!(s.end, dur);
        // Not-a-number and infinity fall back to the start.
        let s = Selection::new(f64::NAN, f64::INFINITY, dur);
        assert_eq!((s.start, s.end), (0.0, MAX_SELECTION));
        // Short recordings: the selection is the whole thing; the empty one is empty but valid.
        let s = Selection::at_start(3.0);
        assert_eq!((s.start, s.end), (0.0, 3.0));
        assert_eq!(Selection::at_start(0.0), Selection { start: 0.0, end: 0.0 });
        let s = Selection::new(0.0, 1.0, 0.02);
        assert_eq!((s.start, s.end), (0.0, 0.02));
        // Dragging the handles: the other end stays, 5 s is the limit, 50 ms the minimum.
        let s = Selection::new(10.0, 12.0, dur);
        let a = s.with_start(1.0, dur);
        assert_eq!((a.start, a.end), (7.0, 12.0), "the start stops 5 s before the end");
        let a = s.with_start(11.99, dur);
        assert!((a.len() - MIN_SELECTION).abs() < 1e-9, "{a:?}");
        let b = s.with_end(19.0, dur);
        assert_eq!((b.start, b.end), (10.0, 15.0), "the end stops 5 s after the start");
        let b = s.with_end(30.0, 12.5);
        assert_eq!(b.end, 12.5);
        let b = s.with_end(10.0, dur);
        assert!((b.len() - MIN_SELECTION).abs() < 1e-9);
        // Moving keeps the length and stops at the ends.
        let m = s.shifted(100.0, dur);
        assert_eq!((m.len(), m.end), (2.0, dur));
        let m = s.shifted(-100.0, dur);
        assert_eq!((m.start, m.len()), (0.0, 2.0));
        let m = s.centered_on(1.0, dur);
        assert_eq!(m.start, 0.0);
        let m = s.centered_on(15.0, dur);
        assert!((m.start - 14.0).abs() < 1e-9 && (m.end - 16.0).abs() < 1e-9);
        // Whatever sequence of drags, the invariants hold.
        let mut x = 12345u64;
        let mut rnd = move || {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (x >> 33) as f64 / (1u64 << 31) as f64
        };
        for d in [0.0, 0.01, 0.04, 0.3, 4.9, 5.0, 5.01, 20.0, 600.0] {
            let mut s = Selection::at_start(d);
            for _ in 0..500 {
                let t = rnd() * (d + 10.0) - 5.0;
                s = match (rnd() * 5.0) as u32 {
                    0 => s.with_start(t, d),
                    1 => s.with_end(t, d),
                    2 => s.shifted(t, d),
                    3 => s.centered_on(t, d),
                    _ => Selection::new(t, t + rnd() * 9.0 - 2.0, d),
                };
                assert!(s.start >= 0.0 && s.end <= d + 1e-9 && s.start <= s.end, "d={d}: {s:?}");
                assert!(s.len() <= MAX_SELECTION + 1e-9, "d={d}: {s:?}");
                assert!(s.len() >= MIN_SELECTION.min(d) - 1e-9, "d={d}: {s:?}");
                let tr = s.to_trim(true, true, true);
                assert!(tr.end_ms > tr.start_ms && tr.len_ms() <= SOUND_MAX_MS, "{tr:?}");
            }
        }
    }

    #[test]
    fn the_selection_becomes_whole_milliseconds_and_fades() {
        let t = Selection::new(5.0, 8.2, 20.0).to_trim(true, false, true);
        assert_eq!((t.start_ms, t.end_ms, t.fade_in_ms, t.fade_out_ms, t.trim_silence), (5000, 8200, 10, 0, true));
        let t = Selection { start: 1.0004, end: 6.0006 }.to_trim(false, true, false);
        assert_eq!(t.len_ms(), 5000, "rounding never gives more than 5 s");
        assert_eq!((t.fade_in_ms, t.fade_out_ms, t.trim_silence), (0, 10, false));
    }

    #[test]
    fn times_read_as_minutes_seconds_centiseconds() {
        assert_eq!(format_time(0.0), "0:00.00");
        assert_eq!(format_time(3.2), "0:03.20");
        assert_eq!(format_time(75.456), "1:15.46");
        assert_eq!(format_time(-1.0), "0:00.00");
    }

    #[test]
    fn a_twenty_second_file_is_analysed_with_progress_and_the_loud_burst_is_found() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rajadas.wav");
        let rate = 16_000;
        // 20 s: quiet bursts, and one loud at 12..15 s.
        let mut s = bursts(rate, 20.0, &[(1.0, 3.0), (6.0, 8.0)], 0.1);
        for (i, v) in s.iter_mut().enumerate() {
            let t = i as f32 / rate as f32;
            if (12.0..15.0).contains(&t) {
                *v = (t * 440.0 * TAU).sin() * 0.8;
            }
        }
        write_wav(&path, rate, &s);
        let seen = std::sync::Mutex::new(Vec::new());
        let o = analyze(&path, &|p| seen.lock().unwrap().push(p), &AtomicBool::new(false)).unwrap();
        assert!((o.duration() - 20.0).abs() < 0.02, "{}", o.duration());
        assert_eq!((o.rate, o.channels), (rate, 1));
        let seen = seen.into_inner().unwrap();
        assert!(seen.len() > 3 && seen.windows(2).all(|w| w[0] <= w[1] + 1e-6) && *seen.last().unwrap() == 1.0, "{seen:?}");
        let at = o.loudest_window(5.0);
        assert!((9.98..=12.02).contains(&at), "the 5 s window holds the 3 s burst at 12..15 s: {at}");
        assert!(o.peak() > 0.79 && o.peak() < 0.81);
    }

    #[test]
    fn analysis_can_be_cancelled_and_refuses_what_is_too_long() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.wav");
        write_wav(&path, 8_000, &bursts(8_000, 5.0, &[(0.0, 5.0)], 0.3));
        let cancel = AtomicBool::new(true);
        assert_eq!(analyze(&path, &|_| {}, &cancel).unwrap_err(), ProcessError::Cancelled);
        // Eleven minutes (the container says so up front) and not audio at all.
        let long = dir.path().join("longo.wav");
        write_wav(&long, 8_000, &vec![0.01f32; 8_000 * 660]);
        assert_eq!(analyze(&long, &|_| {}, &AtomicBool::new(false)).unwrap_err(), ProcessError::SourceTooLong);
        let msg = ProcessError::SourceTooLong.to_string();
        assert!(msg.contains("10 minutos"), "{msg}");
        let junk = dir.path().join("x.mp3");
        std::fs::write(&junk, b"definitely not audio, just words").unwrap();
        assert!(matches!(analyze(&junk, &|_| {}, &AtomicBool::new(false)), Err(ProcessError::Unsupported(_))));
        assert!(matches!(analyze(&dir.path().join("nao-existe.wav"), &|_| {}, &AtomicBool::new(false)), Err(ProcessError::Unsupported(_))));
        assert!(ProcessError::TooBig.to_string().contains("128 MB"));
    }

    #[test]
    fn processing_a_selection_gives_that_duration() {
        use super::super::process::prepare_file;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rajadas.wav");
        let rate = 22_050;
        // 20 s with 4 s tone bursts at 1..5, 8..12, 15..19.
        write_wav(&path, rate, &bursts(rate, 20.0, &[(1.0, 5.0), (8.0, 12.0), (15.0, 19.0)], 0.3));
        let sel = Selection::new(8.5, 11.7, 20.0); // 3.2 s inside the second burst
        let p = prepare_file(&path, Some(&sel.to_trim(true, true, true))).unwrap();
        assert!((3_195..=3_200).contains(&p.duration_ms), "{} ms", p.duration_ms);
        assert!(p.wav.len() < 400_000, "mono 3.2 s: {} bytes", p.wav.len());
        assert!(vox_proto::parse_wav(&p.wav).is_ok());
        // Not trimming the silence gives the same for a selection that has none to cut.
        let q = prepare_file(&path, Some(&sel.to_trim(true, true, false))).unwrap();
        assert_eq!(q.duration_ms, p.duration_ms);
        // A selection that straddles the end of a burst: the silence after it is cut when asked,
        // kept when not.
        let edge = Selection::new(10.0, 14.0, 20.0);
        let cut = prepare_file(&path, Some(&edge.to_trim(true, true, true))).unwrap();
        let kept = prepare_file(&path, Some(&edge.to_trim(true, true, false))).unwrap();
        assert!((2_000..=2_040).contains(&cut.duration_ms), "{} ms", cut.duration_ms);
        assert!((3_995..=4_000).contains(&kept.duration_ms), "{} ms", kept.duration_ms);
        // Exactly 5 s is fine; a start that is not at the beginning really starts there.
        let five = prepare_file(&path, Some(&Selection::new(8.0, 13.0, 20.0).to_trim(false, false, false))).unwrap();
        assert!(five.duration_ms <= 5_000 && five.duration_ms >= 4_995, "{} ms", five.duration_ms);
        // Only silence: said so. Out of range or too long: refused.
        assert_eq!(prepare_file(&path, Some(&Selection::new(5.5, 7.5, 20.0).to_trim(true, true, true))).unwrap_err(), ProcessError::Silent);
        let bad = Trim { start_ms: 30_000, end_ms: 32_000, fade_in_ms: 0, fade_out_ms: 0, trim_silence: true };
        assert_eq!(prepare_file(&path, Some(&bad)).unwrap_err(), ProcessError::BadSelection);
        let long = Trim { start_ms: 0, end_ms: 5_001, ..bad };
        assert_eq!(prepare_file(&path, Some(&long)).unwrap_err(), ProcessError::BadSelection);
        let empty = Trim { start_ms: 1_000, end_ms: 1_000, ..bad };
        assert_eq!(prepare_file(&path, Some(&empty)).unwrap_err(), ProcessError::BadSelection);
        // The whole 20 s file without a selection is still too long, as before.
        assert!(matches!(prepare_file(&path, None), Err(ProcessError::TooLong { .. })));
    }

    #[test]
    fn the_selection_starts_and_ends_without_a_click() {
        use super::super::process::prepare_file;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tom.wav");
        // A loud tone through the whole file: any cut lands in the middle of a wave.
        write_wav(&path, 48_000, &bursts(48_000, 10.0, &[(0.0, 10.0)], 0.8));
        let sel = Selection::new(2.0013, 5.0013, 10.0);
        let samples = |p: &super::super::Prepared| {
            let info = vox_proto::parse_wav(&p.wav).unwrap();
            vox_proto::wav_samples(&p.wav, &info)
        };
        let faded = samples(&prepare_file(&path, Some(&sel.to_trim(true, true, false))).unwrap());
        let hard = samples(&prepare_file(&path, Some(&sel.to_trim(false, false, false))).unwrap());
        assert_eq!((faded[0], faded[faded.len() - 1]), (0, 0), "faded edges are exactly silent");
        // The fade is a smooth ramp over about 10 ms: no sample jumps by more than a few percent
        // of full scale in the first and last 20 ms.
        let step = |s: &[i16]| s.windows(2).map(|w| (w[1] as i32 - w[0] as i32).abs()).max().unwrap();
        let edge = 48_000 / 50;
        assert!(step(&faded[..edge]) < 3_000 && step(&faded[faded.len() - edge..]) < 3_000, "{} {}", step(&faded[..edge]), step(&faded[faded.len() - edge..]));
        // Without the fades a loud tone cut mid-wave begins with a jump from silence.
        assert!(hard[0].unsigned_abs() > 500 || hard[hard.len() - 1].unsigned_abs() > 500, "{} {}", hard[0], hard[hard.len() - 1]);
        // The fade-in is 10 ms (480 frames): a quarter of the way in, the level is still low.
        let mid = faded[2_000..4_000].iter().map(|s| s.unsigned_abs()).max().unwrap();
        let first10 = faded[..120].iter().map(|s| s.unsigned_abs()).max().unwrap();
        assert!(first10 < mid / 2, "{first10} vs {mid}");
    }
}
