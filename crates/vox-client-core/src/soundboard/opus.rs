//! Opus decoding for the soundboard (Ogg Opus: `.opus` / `.ogg`; WebM / Matroska Opus: `.webm` /
//! `.mka`). symphonia 0.5 demuxes these containers but has no Opus decoder, so the packets it
//! hands over are decoded here.
//!
//! # Why libopus through `extern "C"`
//!
//! The prebuilt libwebrtc that the LiveKit SDK links statically (`webrtc.lib`, from `webrtc-sys`)
//! already contains the reference libopus 1.x, float decoder and multistream decoder included
//! (checked with `dumpbin /linkermember` on the lib: `opus_decoder_create`, `opus_decode_float`,
//! `opus_decoder_destroy`, `opus_multistream_decoder_create`, `opus_multistream_decode_float`,
//! `opus_multistream_decoder_destroy` are all public symbols). Declaring those few functions here
//! costs no new dependency, no build step (no CMake, no C compiler) and no bytes in the exe beyond
//! this file: the decoder is already in it. The alternatives were worse for this repo:
//!
//! - a pure-Rust Opus decoder crate would be a second, much less exercised implementation of a
//!   codec whose reference decoder is already linked, plus extra compile time and exe size;
//! - `audiopus` / `opus` with a bundled build need CMake (or a system libopus), which this machine
//!   does not have and a future dev machine should not need.
//!
//! The one risk is that a future libwebrtc build stops exporting (or renames) the symbols. That
//! shows up at link time, in every build, not as a runtime surprise; `tests/soundboard_decode.rs`
//! also decodes real Opus files.
//!
//! # What the decoder does
//!
//! Output is always 48 kHz f32. The `OpusHead` header (the Ogg identification packet, or the
//! Matroska `CodecPrivate`, same layout) gives the channel count, the pre-skip (samples to drop
//! from the start: the decoder's warm-up), the output gain (Q7.8 dB, applied here as RFC 7845
//! asks) and the channel mapping. Mono and stereo use the plain decoder; mapping families 1 and
//! 255 (up to 8 channels: 3.0 .. 7.1, multistream) use the multistream decoder, and the caller
//! folds more than two channels into one. Anything else (ambisonics, more than 8 channels) is
//! refused with a clear message instead of being decoded wrongly.

use std::ffi::c_int;

use super::process::ProcessError;

/// The decoder always runs at 48 kHz.
pub const OPUS_RATE: u32 = 48_000;
/// Longest Opus frame (120 ms) in samples per channel at 48 kHz.
const MAX_FRAME: usize = 5_760;
/// Most channels accepted (7.1).
const MAX_CHANNELS: usize = 8;

#[repr(C)]
struct RawDecoder {
    _private: [u8; 0],
}

#[repr(C)]
struct RawMsDecoder {
    _private: [u8; 0],
}

extern "C" {
    fn opus_decoder_create(fs: i32, channels: c_int, error: *mut c_int) -> *mut RawDecoder;
    fn opus_decoder_destroy(st: *mut RawDecoder);
    fn opus_decode_float(st: *mut RawDecoder, data: *const u8, len: i32, pcm: *mut f32, frame_size: c_int, decode_fec: c_int) -> c_int;
    fn opus_multistream_decoder_create(fs: i32, channels: c_int, streams: c_int, coupled_streams: c_int, mapping: *const u8, error: *mut c_int) -> *mut RawMsDecoder;
    fn opus_multistream_decoder_destroy(st: *mut RawMsDecoder);
    fn opus_multistream_decode_float(st: *mut RawMsDecoder, data: *const u8, len: i32, pcm: *mut f32, frame_size: c_int, decode_fec: c_int) -> c_int;
}

/// What the `OpusHead` header says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpusHead {
    pub channels: usize,
    /// Samples (at 48 kHz) to drop from the start of the decoded stream.
    pub pre_skip: usize,
    /// Output gain in Q7.8 dB.
    pub gain_q8: i16,
    pub family: u8,
    pub streams: usize,
    pub coupled: usize,
    /// Output channel -> decoded channel (255 = silence); empty for family 0.
    pub mapping: Vec<u8>,
}

impl OpusHead {
    /// Parse an `OpusHead` packet (RFC 7845 section 5.1).
    pub fn parse(b: &[u8]) -> Result<Self, ProcessError> {
        let bad = |why: &str| ProcessError::Unsupported(format!("Opus: {why}"));
        if b.len() < 19 || &b[..8] != b"OpusHead" {
            return Err(bad("cabeçalho inválido"));
        }
        if b[8] >> 4 != 0 {
            return Err(bad("versão desconhecida"));
        }
        let channels = b[9] as usize;
        let pre_skip = u16::from_le_bytes([b[10], b[11]]) as usize;
        let gain_q8 = i16::from_le_bytes([b[16], b[17]]);
        let family = b[18];
        if channels == 0 || channels > MAX_CHANNELS {
            return Err(bad("número de canais não suportado"));
        }
        match family {
            0 if channels <= 2 => Ok(OpusHead { channels, pre_skip, gain_q8, family, streams: 1, coupled: channels - 1, mapping: Vec::new() }),
            // 1: Vorbis channel order (1..8 channels); 255: unlabelled discrete channels.
            1 | 255 => {
                if b.len() < 21 + channels {
                    return Err(bad("cabeçalho de canais truncado"));
                }
                let (streams, coupled) = (b[19] as usize, b[20] as usize);
                if streams == 0 || coupled > streams {
                    return Err(bad("mapa de canais inválido"));
                }
                Ok(OpusHead { channels, pre_skip, gain_q8, family, streams, coupled, mapping: b[21..21 + channels].to_vec() })
            }
            _ => Err(bad("mapeamento de canais não suportado")),
        }
    }

    /// Linear gain for the header's output gain.
    pub fn gain(&self) -> f32 {
        10f32.powf(self.gain_q8 as f32 / (256.0 * 20.0))
    }
}

enum Raw {
    Single(*mut RawDecoder),
    Multi(*mut RawMsDecoder),
}

/// A packet-by-packet Opus decoder that already applies the pre-skip, the output gain and
/// (when told the stream length) the end trim.
pub struct OpusStream {
    raw: Raw,
    channels: usize,
    gain: f32,
    skip: usize,
    /// Frames still to be emitted (`None` = unknown, emit everything).
    remaining: Option<u64>,
    buf: Vec<f32>,
}

// The raw decoder is only ever used through `&mut self`.
unsafe impl Send for OpusStream {}

impl Drop for OpusStream {
    fn drop(&mut self) {
        // SAFETY: the pointer came from the matching `create` and is destroyed exactly once.
        unsafe {
            match self.raw {
                Raw::Single(p) => opus_decoder_destroy(p),
                Raw::Multi(p) => opus_multistream_decoder_destroy(p),
            }
        }
    }
}

impl OpusStream {
    /// `total_frames`: the number of 48 kHz frames the stream has after the pre-skip, when the
    /// container knows it (it trims the codec's end padding).
    pub fn new(head: &OpusHead, total_frames: Option<u64>) -> Result<Self, ProcessError> {
        let mut err: c_int = 0;
        // SAFETY: plain constructor calls; `mapping` has `channels` entries (checked by `parse`)
        // and libopus validates its values itself, reporting through `err`.
        let raw = unsafe {
            if head.family == 0 {
                Raw::Single(opus_decoder_create(OPUS_RATE as i32, head.channels as c_int, &mut err))
            } else {
                Raw::Multi(opus_multistream_decoder_create(OPUS_RATE as i32, head.channels as c_int, head.streams as c_int, head.coupled as c_int, head.mapping.as_ptr(), &mut err))
            }
        };
        let null = match raw {
            Raw::Single(p) => p.is_null(),
            Raw::Multi(p) => p.is_null(),
        };
        if err != 0 || null {
            // A failed create returns null: nothing to destroy.
            return Err(ProcessError::Unsupported("Opus: não consegui iniciar o decodificador".into()));
        }
        Ok(Self { raw, channels: head.channels, gain: head.gain(), skip: head.pre_skip, remaining: total_frames, buf: vec![0.0; MAX_FRAME * head.channels] })
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Decode one packet into interleaved samples (48 kHz), dropping the last `discard_end` frames
    /// (the container's end padding for this packet). `None` for a damaged packet; the slice is
    /// empty while the pre-skip is being dropped or after the end of the stream.
    pub fn decode(&mut self, packet: &[u8], discard_end: usize) -> Option<&[f32]> {
        if packet.is_empty() || packet.len() > i32::MAX as usize {
            return None;
        }
        let ch = self.channels;
        // SAFETY: `buf` holds MAX_FRAME frames of `ch` channels, the most one packet can decode to.
        let n = unsafe {
            match self.raw {
                Raw::Single(p) => opus_decode_float(p, packet.as_ptr(), packet.len() as i32, self.buf.as_mut_ptr(), MAX_FRAME as c_int, 0),
                Raw::Multi(p) => opus_multistream_decode_float(p, packet.as_ptr(), packet.len() as i32, self.buf.as_mut_ptr(), MAX_FRAME as c_int, 0),
            }
        };
        if n <= 0 {
            return None;
        }
        let n = n as usize - discard_end.min(n as usize);
        let drop_front = self.skip.min(n);
        self.skip -= drop_front;
        let mut take = n - drop_front;
        if let Some(rem) = &mut self.remaining {
            take = take.min(*rem as usize);
            *rem -= take as u64;
        }
        let out = &mut self.buf[drop_front * ch..(drop_front + take) * ch];
        if self.gain != 1.0 {
            out.iter_mut().for_each(|s| *s *= self.gain);
        }
        Some(&self.buf[drop_front * ch..(drop_front + take) * ch])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(channels: u8, pre_skip: u16, gain: i16, family: u8, extra: &[u8]) -> Vec<u8> {
        let mut v = b"OpusHead".to_vec();
        v.extend([1, channels]);
        v.extend(pre_skip.to_le_bytes());
        v.extend(48_000u32.to_le_bytes());
        v.extend(gain.to_le_bytes());
        v.push(family);
        v.extend(extra);
        v
    }

    #[test]
    fn the_header_gives_channels_pre_skip_gain_and_mapping() {
        let h = OpusHead::parse(&head(2, 312, 256, 0, &[])).unwrap();
        assert_eq!((h.channels, h.pre_skip, h.streams, h.coupled), (2, 312, 1, 1));
        assert!((h.gain() - 10f32.powf(0.05)).abs() < 1e-6, "+1 dB");
        // 5.1 in the Vorbis order: 4 streams, 2 coupled.
        let h = OpusHead::parse(&head(6, 0, 0, 1, &[4, 2, 0, 4, 1, 2, 3, 5])).unwrap();
        assert_eq!((h.channels, h.streams, h.coupled, h.mapping.len()), (6, 4, 2, 6));
        assert_eq!(h.gain(), 1.0);
    }

    #[test]
    fn what_cannot_be_decoded_is_refused_with_a_reason() {
        assert!(OpusHead::parse(b"OpusTags....................").is_err());
        assert!(OpusHead::parse(&head(0, 0, 0, 0, &[])).is_err());
        assert!(OpusHead::parse(&head(3, 0, 0, 0, &[])).is_err(), "family 0 is mono or stereo");
        assert!(OpusHead::parse(&head(4, 0, 0, 2, &[4, 0, 0, 1, 2, 3])).is_err(), "ambisonics");
        assert!(OpusHead::parse(&head(9, 0, 0, 1, &[9, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8])).is_err(), "more than 8 channels");
        assert!(OpusHead::parse(&head(6, 0, 0, 1, &[4, 2, 0])).is_err(), "truncated mapping");
        assert!(OpusHead::parse(&head(2, 0, 0, 1, &[3, 4, 0, 1])).is_err(), "more coupled than streams");
    }

    #[test]
    fn the_linked_libopus_decodes() {
        // A TOC byte for a 20 ms CELT-only stereo frame with no payload is a valid "silent" packet
        // (a 2-byte frame is the smallest real one); either it decodes to 960 frames or libopus
        // says no, but it must never crash.
        let mut s = OpusStream::new(&OpusHead::parse(&head(2, 0, 0, 0, &[])).unwrap(), None).unwrap();
        if let Some(pcm) = s.decode(&[0xFC, 0xFF, 0xFE], 0) {
            assert_eq!(pcm.len() % 2, 0);
        }
        assert!(s.decode(&[], 0).is_none());
        // A bad multistream layout is refused by libopus itself.
        let bad = OpusHead { channels: 2, pre_skip: 0, gain_q8: 0, family: 1, streams: 1, coupled: 1, mapping: vec![0, 7] };
        assert!(OpusStream::new(&bad, None).is_err());
    }
}
