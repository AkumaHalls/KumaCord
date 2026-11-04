//! The launcher's music: the movie's own track decoded ahead of the first repaint, so the home
//! screen can play it on a loop while the user decides to enter (before any voice session exists).
//!
//! The file is decoded with symphonia (the same pure-Rust decoders the soundboard uses) into
//! interleaved stereo at 48 kHz — the rate every output device path in this crate expects — and
//! handed out as 16-bit PCM, which is what [`super::mixer::Mixer::play_ui_loop`] plays.

use std::io::Cursor;
use std::sync::Arc;

use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use super::resample::LinearResampler;

/// Nothing played by the launcher may be longer than this; the shipped theme is the movie's own
/// track (~3:50), so the cap only guards against a damaged header claiming hours of audio.
pub const MAX_CLIP_SECS: u32 = 300;
/// Everything the mixer plays from a buffer is stereo at this rate.
pub const CLIP_RATE: u32 = 48_000;

/// Decode `bytes` (any format symphonia knows; `ext` only helps the probe) into interleaved
/// stereo 48 kHz PCM. `None` when the file cannot be read as audio (never a panic: the app is
/// built with `panic = "abort"`, and this runs at startup).
pub fn decode_48k_stereo(bytes: &[u8], ext: &str) -> Option<Arc<[i16]>> {
    let source = MediaSourceStream::new(Box::new(Cursor::new(bytes.to_vec())), Default::default());
    let mut hint = Hint::new();
    hint.with_extension(ext);
    let probed = symphonia::default::get_probe().format(&hint, source, &FormatOptions::default(), &MetadataOptions::default()).ok()?;
    let mut format = probed.format;
    let track = format.tracks().iter().find(|t| t.codec_params.codec != CODEC_TYPE_NULL)?;
    let track_id = track.id;
    let rate = track.codec_params.sample_rate.unwrap_or(CLIP_RATE);
    let mut decoder = symphonia::default::get_codecs().make(&track.codec_params, &DecoderOptions::default()).ok()?;
    let max_frames = MAX_CLIP_SECS as u64 * rate as u64;

    let mut interleaved: Vec<f32> = Vec::new();
    let mut buf: Option<SampleBuffer<f32>> = None;
    let mut frames_seen = 0u64;
    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            // The end of the stream is an I/O error for most formats.
            Err(SymError::IoError(_)) | Err(SymError::ResetRequired) => break,
            Err(_) if frames_seen > 0 => break,
            Err(_) => return None,
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            // A damaged packet in the middle of the music is skipped, not fatal.
            Err(_) if frames_seen > 0 => continue,
            Err(_) => return None,
        };
        let spec = *decoded.spec();
        let channels = spec.channels.count().max(1);
        if buf.as_ref().is_none_or(|b| b.capacity() < decoded.capacity()) {
            buf = Some(SampleBuffer::new(decoded.capacity() as u64, *decoded.spec()));
        }
        let sb = buf.as_mut()?;
        sb.copy_interleaved_ref(decoded);
        // Keep the first two channels (or the only one, duplicated): everything is stereo.
        for frame in sb.samples().chunks_exact(channels) {
            let (l, r) = match channels {
                1 => (frame[0], frame[0]),
                _ => (frame[0], frame[1]),
            };
            interleaved.push(l);
            interleaved.push(r);
        }
        frames_seen = interleaved.len() as u64 / 2;
        if frames_seen >= max_frames {
            break;
        }
    }
    if interleaved.len() < 2 {
        return None;
    }
    let samples = if rate == CLIP_RATE {
        interleaved
    } else {
        let mut out = Vec::with_capacity(interleaved.len() * CLIP_RATE as usize / rate as usize + 8);
        LinearResampler::new(rate, CLIP_RATE, 2).process(&interleaved, &mut out);
        out
    };
    let pcm: Vec<i16> = samples.into_iter().map(|s| (s.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16).collect();
    Some(pcm.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal 16-bit PCM WAV in memory (symphonia's `wav` reader takes it).
    fn wav(rate: u32, channels: u16, frames: usize) -> Vec<u8> {
        let data_len = (frames * channels as usize * 2) as u32;
        let mut v = Vec::with_capacity(44 + data_len as usize);
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&(36 + data_len).to_le_bytes());
        v.extend_from_slice(b"WAVEfmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&channels.to_le_bytes());
        v.extend_from_slice(&rate.to_le_bytes());
        v.extend_from_slice(&(rate * channels as u32 * 2).to_le_bytes());
        v.extend_from_slice(&(channels * 2).to_le_bytes());
        v.extend_from_slice(&16u16.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&data_len.to_le_bytes());
        for i in 0..frames {
            let s = ((i as f32 / 8.0).sin() * 8000.0) as i16;
            for _ in 0..channels {
                v.extend_from_slice(&s.to_le_bytes());
            }
        }
        v
    }

    #[test]
    fn mono_at_another_rate_becomes_stereo_48k() {
        // 24 kHz mono, half a second: 48 kHz stereo, the same half second.
        let pcm = decode_48k_stereo(&wav(24_000, 1, 12_000), "wav").expect("decoded");
        assert_eq!(pcm.len() % 2, 0);
        let frames = pcm.len() / 2;
        assert!(frames.abs_diff(24_000) < 200, "expected ~24000 frames, got {frames}");
        // Duplicated channel.
        assert_eq!(pcm[0], pcm[1]);
        // The signal survived (not silence, not clipped).
        assert!(pcm.iter().any(|s| s.unsigned_abs() > 1000));
    }

    #[test]
    fn garbage_is_none_not_a_panic() {
        assert!(decode_48k_stereo(&[0u8; 64], "mp3").is_none());
        assert!(decode_48k_stereo(b"", "mp3").is_none());
    }

    /// The launcher's theme, the real file this build ships: it must decode whole (a decode that
    /// bails early would loop a fragment instead of the full movie's ~3:50).
    #[test]
    fn the_shipped_launcher_theme_decodes_whole() {
        let mp3 = include_bytes!("../../../../app/assets/intro/intro.mp3");
        let pcm = decode_48k_stereo(mp3, "mp3").expect("the shipped mp3 decodes");
        let frames = pcm.len() / 2;
        // The whole track at 48 kHz (3:50 = 230 s): a few ms of fades, no missing minutes.
        assert!(frames > 48_000 * 225, "expected ~11 million frames, got {frames}");
        assert!(frames < 48_000 * 240, "unreasonably long: {frames}");
    }
}
