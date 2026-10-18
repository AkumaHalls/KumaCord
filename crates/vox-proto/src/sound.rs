//! Server soundboard (v0.5, no protocol bump): wire types, limits, the permission rule and the one
//! audio format the hub stores.
//!
//! Sounds belong to the server. Anyone uploads a clip; the uploader (or a Moderador+) renames or
//! deletes it. Playing is announced by the hub to the members of one voice channel and each app
//! plays the clip locally, so nothing goes through LiveKit.
//!
//! Endpoints (all authenticated; a hub older than the soundboard answers `404`, which is how a
//! client knows to hide the feature):
//!
//! - `GET  /api/sounds`                          -> `Vec<Sound>`
//! - `GET  /api/sounds/{id}/audio?v=<hash>`      -> the clip as WAV ([`parse_wav`] reads it). Immutable
//!   per id and hash: `ETag` is the hash, `Cache-Control: private, max-age=31536000, immutable`.
//! - `POST /api/sounds?name=&emoji=`             body = the WAV bytes (`audio/wav`) -> [`Sound`].
//!   `400` bad name / not the stored format / too long, `413` over [`SOUND_MAX_UPLOAD_BYTES`],
//!   `409` the server already has [`SOUND_MAX_COUNT`] sounds, `429` uploading too fast.
//! - `POST /api/sounds/{id}/rename`              [`SoundRenameRequest`] -> [`Sound`]; `403` unless
//!   [`may_edit_sound`]
//! - `POST /api/sounds/{id}/delete`              -> [`OkResponse`](crate::OkResponse); `403` unless
//!   [`may_edit_sound`]
//!
//! Events: [`ClientEvent::PlaySound`](crate::ClientEvent::PlaySound) (answered with an
//! [`ServerEvent::Error`](crate::ServerEvent::Error) when refused),
//! [`ServerEvent::SoundPlayed`](crate::ServerEvent::SoundPlayed) (to the members of that voice
//! channel, the player included) and [`ServerEvent::SoundsChanged`](crate::ServerEvent::SoundsChanged)
//! (the whole new list, to everybody, after an upload, rename or delete).
//!
//! The *client* turns whatever the user picks (WAV, MP3, OGG, FLAC) into the stored format: 48 kHz,
//! mono or stereo, 16-bit PCM in a plain WAV, trimmed and loudness-normalised. The hub never
//! trusts that: it parses the upload again with [`parse_wav`], checks format, length and size, and
//! stores a canonical re-encoding of the samples ([`canonical_wav`]), dropping every other chunk.

use serde::{Deserialize, Serialize};

use crate::{Role, User, UserId};

pub type SoundId = i64;

/// Longest clip, in milliseconds.
pub const SOUND_MAX_MS: u32 = 5_000;
/// Shortest clip, in milliseconds (a click is not a sound).
pub const SOUND_MIN_MS: u32 = 100;
/// Largest upload body, in bytes (1 MiB; 5 s of 48 kHz stereo is 960 044 bytes).
pub const SOUND_MAX_UPLOAD_BYTES: usize = 1024 * 1024;
/// Most sounds a server holds.
pub const SOUND_MAX_COUNT: usize = 50;
/// Sound names: 1 to this many characters.
pub const SOUND_NAME_MAX: usize = 32;
/// An emoji is at most this many characters (code points; a ZWJ sequence needs several).
pub const SOUND_EMOJI_MAX_CHARS: usize = 12;
/// The only sample rate stored.
pub const SOUND_RATE: u32 = 48_000;

/// The text of the [`ServerEvent::Error`](crate::ServerEvent::Error) the hub answers a
/// `PlaySound` with when the player is too fast (per person, or for the whole room).
pub const ERR_SOUND_RATE_LIMITED: &str = "soundboard rate limited";
/// Answer to a `PlaySound` from someone who is not in a voice channel.
pub const ERR_SOUND_NOT_IN_VOICE: &str = "join a voice channel to play sounds";
/// Answer to a `PlaySound` for a sound that does not exist (any more).
pub const ERR_SOUND_UNKNOWN: &str = "unknown sound";
/// Answer to a `PlaySound` from a server-muted member.
pub const ERR_SOUND_SERVER_MUTED: &str = "you are muted by the server";

pub const AUDIT_SOUND_DELETE: &str = "sound_delete";
pub const AUDIT_SOUND_RENAME: &str = "sound_rename";

/// One soundboard entry (the clip itself is `GET /api/sounds/{id}/audio`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sound {
    pub id: SoundId,
    pub name: String,
    /// One emoji (or a short sequence), or empty.
    #[serde(default)]
    pub emoji: String,
    pub uploader: User,
    pub created_at_ms: i64,
    pub duration_ms: u32,
    /// Size of the stored clip in bytes.
    pub size: u32,
    /// Fingerprint of the stored clip: the cache key (`{id}-{hash}`) and the `ETag`.
    pub hash: String,
}

/// `POST /api/sounds/{id}/rename`; `emoji: None` keeps the emoji, `Some("")` clears it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SoundRenameRequest {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emoji: Option<String>,
}

/// Who may rename or delete a sound: its uploader, and Moderador+ (item 12's matrix: the same
/// people who may delete any chat message).
pub fn may_edit_sound(role: Role, me: UserId, uploader: UserId) -> bool {
    me == uploader || role.can_moderate()
}

/// A sound name, trimmed: 1 to [`SOUND_NAME_MAX`] characters, no control characters.
pub fn clean_sound_name(raw: &str) -> Option<String> {
    let name = raw.trim();
    let n = name.chars().count();
    ((1..=SOUND_NAME_MAX).contains(&n) && !name.chars().any(char::is_control)).then(|| name.to_string())
}

/// A sound emoji, trimmed: empty, or at most [`SOUND_EMOJI_MAX_CHARS`] characters without control
/// characters or spaces.
pub fn clean_sound_emoji(raw: &str) -> Option<String> {
    let e = raw.trim();
    (e.chars().count() <= SOUND_EMOJI_MAX_CHARS && !e.chars().any(|c| c.is_control() || c.is_whitespace())).then(|| e.to_string())
}

// ---------------------------------------------------------------- the stored format

/// Why a clip is not in the stored format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WavError {
    /// Not a RIFF/WAVE file, or truncated.
    NotWav,
    /// Not 16-bit PCM.
    NotPcm16,
    /// Not mono or stereo.
    BadChannels,
    /// Not 48 kHz.
    BadRate,
    /// No `data` chunk, or its length is not a whole number of frames.
    BadData,
    TooShort,
    TooLong,
}

impl std::fmt::Display for WavError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            WavError::NotWav => "not a WAV file",
            WavError::NotPcm16 => "the sound must be 16-bit PCM",
            WavError::BadChannels => "the sound must be mono or stereo",
            WavError::BadRate => "the sound must be 48 kHz",
            WavError::BadData => "the sound data is damaged",
            WavError::TooShort => "the sound is too short (min 0.1 s)",
            WavError::TooLong => "the sound is too long (max 5 s)",
        })
    }
}

impl std::error::Error for WavError {}

/// What [`parse_wav`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WavInfo {
    /// 1 or 2.
    pub channels: u16,
    /// Samples per channel.
    pub frames: u32,
    data_start: usize,
}

impl WavInfo {
    pub fn duration_ms(&self) -> u32 {
        (self.frames as u64 * 1000 / SOUND_RATE as u64) as u32
    }
}

fn le16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn le32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// Check that `bytes` is a WAV in the stored format (48 kHz, 16-bit PCM, mono or stereo, between
/// [`SOUND_MIN_MS`] and [`SOUND_MAX_MS`]); other chunks are skipped.
pub fn parse_wav(bytes: &[u8]) -> Result<WavInfo, WavError> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(WavError::NotWav);
    }
    let mut fmt: Option<(u16, u16, u32, u16)> = None; // format, channels, rate, bits
    let mut at = 12usize;
    while at + 8 <= bytes.len() {
        let id = &bytes[at..at + 4];
        let len = le32(bytes, at + 4).ok_or(WavError::NotWav)? as usize;
        let body = at + 8;
        if id == b"fmt " {
            if len < 16 || body + 16 > bytes.len() {
                return Err(WavError::NotWav);
            }
            let tag = le16(bytes, body).ok_or(WavError::NotWav)?;
            // WAVE_FORMAT_EXTENSIBLE (0xFFFE) carries PCM too; only plain PCM is accepted.
            fmt = Some((tag, le16(bytes, body + 2).ok_or(WavError::NotWav)?, le32(bytes, body + 4).ok_or(WavError::NotWav)?, le16(bytes, body + 14).ok_or(WavError::NotWav)?));
        } else if id == b"data" {
            let (tag, channels, rate, bits) = fmt.ok_or(WavError::NotWav)?;
            if tag != 1 || bits != 16 {
                return Err(WavError::NotPcm16);
            }
            if !(1..=2).contains(&channels) {
                return Err(WavError::BadChannels);
            }
            if rate != SOUND_RATE {
                return Err(WavError::BadRate);
            }
            // A streamed WAV may claim more than it holds; a file that holds less is cut short.
            if body + len > bytes.len() {
                return Err(WavError::BadData);
            }
            let frame = 2 * channels as usize;
            if len == 0 || !len.is_multiple_of(frame) {
                return Err(WavError::BadData);
            }
            let frames = (len / frame) as u32;
            let info = WavInfo { channels, frames, data_start: body };
            return match info.duration_ms() {
                ms if ms < SOUND_MIN_MS => Err(WavError::TooShort),
                // The limit is on the exact length: 5 s is 240 000 frames.
                _ if frames as u64 * 1000 > SOUND_MAX_MS as u64 * SOUND_RATE as u64 => Err(WavError::TooLong),
                _ => Ok(info),
            };
        }
        // Chunks are word aligned.
        at = body.checked_add(len + (len & 1)).ok_or(WavError::NotWav)?;
    }
    Err(WavError::BadData)
}

/// The samples of a clip [`parse_wav`] accepted, interleaved.
pub fn wav_samples(bytes: &[u8], info: &WavInfo) -> Vec<i16> {
    let len = info.frames as usize * info.channels as usize * 2;
    bytes[info.data_start..info.data_start + len].as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b)).collect()
}

/// A plain 44-byte-header WAV of interleaved 48 kHz 16-bit `samples`.
pub fn canonical_wav(channels: u16, samples: &[i16]) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut v = Vec::with_capacity(44 + samples.len() * 2);
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + data_len).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&channels.to_le_bytes());
    v.extend_from_slice(&SOUND_RATE.to_le_bytes());
    v.extend_from_slice(&(SOUND_RATE * 2 * channels as u32).to_le_bytes());
    v.extend_from_slice(&(2 * channels).to_le_bytes());
    v.extend_from_slice(&16u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        v.extend_from_slice(&s.to_le_bytes());
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clip(channels: u16, frames: usize) -> Vec<u8> {
        canonical_wav(channels, &vec![100i16; frames * channels as usize])
    }

    #[test]
    fn canonical_wav_round_trips() {
        for ch in [1u16, 2] {
            let samples: Vec<i16> = (0..9600 * ch as usize).map(|i| (i as i16).wrapping_mul(7)).collect();
            let wav = canonical_wav(ch, &samples);
            let info = parse_wav(&wav).unwrap();
            assert_eq!((info.channels, info.frames, info.duration_ms()), (ch, 9600, 200));
            assert_eq!(wav_samples(&wav, &info), samples);
        }
    }

    #[test]
    fn five_seconds_fit_and_a_bit_more_does_not() {
        assert!(parse_wav(&clip(2, 240_000)).is_ok());
        assert_eq!(parse_wav(&clip(2, 240_001)), Err(WavError::TooLong));
        assert_eq!(parse_wav(&clip(1, 4_799)), Err(WavError::TooShort));
        assert!(parse_wav(&clip(1, 4_800)).is_ok());
        assert!(clip(2, 240_000).len() <= SOUND_MAX_UPLOAD_BYTES, "the longest stereo clip fits the upload limit");
    }

    #[test]
    fn other_formats_are_refused() {
        let mut w = clip(1, 9600);
        w[24..28].copy_from_slice(&44_100u32.to_le_bytes());
        assert_eq!(parse_wav(&w), Err(WavError::BadRate));
        let mut w = clip(1, 9600);
        w[22] = 6;
        assert_eq!(parse_wav(&w), Err(WavError::BadChannels));
        let mut w = clip(1, 9600);
        w[34] = 24;
        assert_eq!(parse_wav(&w), Err(WavError::NotPcm16));
        let mut w = clip(1, 9600);
        w[20] = 3; // IEEE float
        assert_eq!(parse_wav(&w), Err(WavError::NotPcm16));
        assert_eq!(parse_wav(b"OggS\0\x02\0\0\0\0\0\0\0\0\0\0\0\0\0\0"), Err(WavError::NotWav));
        assert_eq!(parse_wav(&[0xff, 0xfb, 0x90, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0]), Err(WavError::NotWav));
        assert_eq!(parse_wav(&[]), Err(WavError::NotWav));
        // Truncated data and a half frame.
        let w = clip(2, 9600);
        assert_eq!(parse_wav(&w[..w.len() - 10]), Err(WavError::BadData));
        let mut w = clip(2, 9600);
        w[40..44].copy_from_slice(&(9600u32 * 4 - 2).to_le_bytes());
        assert_eq!(parse_wav(&w), Err(WavError::BadData));
    }

    #[test]
    fn extra_chunks_are_skipped() {
        let base = clip(1, 9600);
        let mut w = base[..36].to_vec();
        w.extend_from_slice(b"LIST\x03\0\0\0abc\0"); // odd length + pad byte
        w.extend_from_slice(&base[36..]);
        assert_eq!(parse_wav(&w).unwrap().frames, 9600);
    }

    #[test]
    fn names_and_emoji() {
        assert_eq!(clean_sound_name("  bruh  ").as_deref(), Some("bruh"));
        assert_eq!(clean_sound_name(""), None);
        assert_eq!(clean_sound_name("   "), None);
        assert_eq!(clean_sound_name(&"a".repeat(32)).map(|n| n.len()), Some(32));
        assert_eq!(clean_sound_name(&"a".repeat(33)), None);
        assert_eq!(clean_sound_name("a\nb"), None);
        assert!(clean_sound_name(&"ã".repeat(32)).is_some(), "32 characters, not bytes");
        assert_eq!(clean_sound_emoji("").as_deref(), Some(""));
        assert_eq!(clean_sound_emoji("🎺").as_deref(), Some("🎺"));
        assert!(clean_sound_emoji("👨‍👩‍👧‍👦").is_some(), "a ZWJ family is 7 code points");
        assert_eq!(clean_sound_emoji("a b"), None);
        assert_eq!(clean_sound_emoji(&"x".repeat(13)), None);
    }

    #[test]
    fn wire_shapes() {
        let s = Sound {
            id: 3,
            name: "bruh".into(),
            emoji: "🎺".into(),
            uploader: User::new(7, "Ana"),
            created_at_ms: 5,
            duration_ms: 1200,
            size: 4096,
            hash: "0011223344556677".into(),
        };
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(
            json,
            r#"{"id":3,"name":"bruh","emoji":"🎺","uploader":{"id":7,"username":"Ana"},"created_at_ms":5,"duration_ms":1200,"size":4096,"hash":"0011223344556677"}"#
        );
        assert_eq!(serde_json::from_str::<Sound>(&json).unwrap(), s);
        // A sound without an emoji (the field is optional on the wire).
        let no_emoji = r#"{"id":3,"name":"x","uploader":{"id":7,"username":"Ana"},"created_at_ms":5,"duration_ms":1200,"size":4096,"hash":"ab"}"#;
        assert_eq!(serde_json::from_str::<Sound>(no_emoji).unwrap().emoji, "");
        assert_eq!(serde_json::to_string(&SoundRenameRequest { name: "n".into(), emoji: None }).unwrap(), r#"{"name":"n"}"#);
        assert_eq!(serde_json::from_str::<SoundRenameRequest>(r#"{"name":"n"}"#).unwrap().emoji, None);
        assert_eq!(serde_json::from_str::<SoundRenameRequest>(r#"{"name":"n","emoji":""}"#).unwrap().emoji.as_deref(), Some(""));
    }

    #[test]
    fn who_may_edit() {
        use Role::*;
        assert!(may_edit_sound(Member, 3, 3), "the uploader");
        assert!(!may_edit_sound(Member, 3, 4));
        assert!(may_edit_sound(Moderator, 3, 4) && may_edit_sound(Admin, 3, 4) && may_edit_sound(Owner, 3, 4));
    }
}
