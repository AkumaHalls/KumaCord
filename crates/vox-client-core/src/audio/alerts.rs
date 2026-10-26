//! Alert sounds ("ui sounds"): short synthesized chimes that only the local user hears. Nothing
//! is shipped as an audio file; each sound is a couple of decaying sine/triangle notes computed
//! once into a 48 kHz stereo buffer ([`AlertKind::samples`]) and mixed by the playback mixer
//! (see [`super::mixer::Mixer::play_ui`]) or by the standalone [`super::ui_player::UiPlayer`].
//!
//! Every sound is 80..=250 ms, peaks well below full scale (so mixing a few of them with voices
//! never clips) and starts and ends at exactly zero with smooth fades, so there is no click at
//! the edges.

use std::f32::consts::TAU;
use std::sync::{Arc, OnceLock};

use super::mixer::{MIX_CHANNELS, MIX_RATE};

/// Every alert the client can play.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AlertKind {
    /// Someone entered my voice channel (also: I entered one). Two rising bell notes.
    Join,
    /// Someone left my voice channel (also: I left). Two falling bell notes.
    Leave,
    /// Someone came online on the server. One soft high tick, quiet.
    Online,
    /// Someone went offline. One soft lower tick, quiet.
    Offline,
    /// I muted my microphone. Two falling blips.
    Mute,
    /// I unmuted. Two rising blips.
    Unmute,
    /// I deafened myself. Three falling blips.
    Deafen,
    /// I undeafened. Three rising blips.
    Undeafen,
    /// Someone in my channel went live. Three rising bell notes.
    StreamStart,
    /// Someone in my channel stopped their live. Three falling bell notes.
    StreamStop,
    /// Someone started watching *my* live. Two identical bell pings ("knock knock").
    Viewer,
    /// A direct message arrived ("Mensagem direta"). A rising two-note pop, higher than a join.
    DirectMessage,
    /// Somebody is calling me from a DM: one short trill, repeated by the app while it rings
    /// (at most `CALL_RING_SECS`); stopping is simply not repeating it.
    Ring,
    /// A message in a text channel I do not have open. One soft, low bell note: the quietest of
    /// the chat sounds, since a busy channel plays it all day.
    ChannelMessage,
    /// Somebody mentioned me (v0.8). Three rising bright notes, higher than a direct message.
    Mention,
}

impl AlertKind {
    pub const ALL: [AlertKind; 15] = [
        AlertKind::Join,
        AlertKind::Leave,
        AlertKind::Online,
        AlertKind::Offline,
        AlertKind::Mute,
        AlertKind::Unmute,
        AlertKind::Deafen,
        AlertKind::Undeafen,
        AlertKind::StreamStart,
        AlertKind::StreamStop,
        AlertKind::Viewer,
        AlertKind::DirectMessage,
        AlertKind::Ring,
        AlertKind::ChannelMessage,
        AlertKind::Mention,
    ];

    fn index(self) -> usize {
        Self::ALL.iter().position(|k| *k == self).expect("listed in ALL")
    }

    /// Short stable name for logs.
    pub fn name(self) -> &'static str {
        match self {
            AlertKind::Join => "join",
            AlertKind::Leave => "leave",
            AlertKind::Online => "online",
            AlertKind::Offline => "offline",
            AlertKind::Mute => "mute",
            AlertKind::Unmute => "unmute",
            AlertKind::Deafen => "deafen",
            AlertKind::Undeafen => "undeafen",
            AlertKind::StreamStart => "stream_start",
            AlertKind::StreamStop => "stream_stop",
            AlertKind::Viewer => "viewer",
            AlertKind::DirectMessage => "direct_message",
            AlertKind::Ring => "ring",
            AlertKind::ChannelMessage => "channel_message",
            AlertKind::Mention => "mention",
        }
    }

    /// The precomputed 48 kHz interleaved stereo buffer (built once, shared).
    pub fn samples(self) -> Arc<[i16]> {
        static BANK: OnceLock<Vec<Arc<[i16]>>> = OnceLock::new();
        BANK.get_or_init(|| AlertKind::ALL.iter().map(|k| Arc::from(synth(&k.spec()))).collect())[self.index()].clone()
    }

    fn spec(self) -> Spec {
        // Pitches: E5, A5, B5, E6 and friends from one pentatonic-ish family, so the chimes sit
        // together pleasantly.
        const E5: f32 = 659.25;
        const B5: f32 = 987.77;
        const A5: f32 = 880.0;
        const D5: f32 = 587.33;
        const C5: f32 = 523.25;
        const G5: f32 = 783.99;
        const E6: f32 = 1318.5;
        const FS5: f32 = 739.99;
        const B4: f32 = 493.88;
        const A4: f32 = 440.0;
        const G6: f32 = 1568.0;
        let n = |freq: f32, start_ms: f32, dur_ms: f32| Note { freq, start_ms, dur_ms };
        match self {
            AlertKind::Join => Spec::bell(0.38, vec![n(E5, 0.0, 130.0), n(B5, 80.0, 150.0)]),
            AlertKind::Leave => Spec::bell(0.34, vec![n(A5, 0.0, 120.0), n(D5, 80.0, 150.0)]),
            AlertKind::Online => Spec::bell(0.14, vec![n(E6, 0.0, 100.0)]),
            AlertKind::Offline => Spec::bell(0.12, vec![n(A5, 0.0, 100.0)]),
            AlertKind::Mute => Spec::blip(0.32, vec![n(FS5, 0.0, 70.0), n(B4, 55.0, 85.0)]),
            AlertKind::Unmute => Spec::blip(0.32, vec![n(B4, 0.0, 70.0), n(FS5, 55.0, 85.0)]),
            AlertKind::Deafen => Spec::blip(0.32, vec![n(A5, 0.0, 70.0), n(E5, 65.0, 70.0), n(A4, 130.0, 100.0)]),
            AlertKind::Undeafen => Spec::blip(0.32, vec![n(A4, 0.0, 70.0), n(E5, 65.0, 70.0), n(A5, 130.0, 100.0)]),
            AlertKind::StreamStart => Spec::bell(0.34, vec![n(C5, 0.0, 100.0), n(E5, 60.0, 100.0), n(G5, 120.0, 120.0)]),
            AlertKind::StreamStop => Spec::bell(0.30, vec![n(G5, 0.0, 100.0), n(E5, 60.0, 100.0), n(C5, 120.0, 120.0)]),
            AlertKind::Viewer => Spec::bell(0.30, vec![n(G5, 0.0, 80.0), n(G5, 105.0, 105.0)]),
            AlertKind::DirectMessage => Spec::bell(0.32, vec![n(B5, 0.0, 90.0), n(E6, 70.0, 140.0)]),
            AlertKind::Ring => Spec::blip(0.30, vec![n(E5, 0.0, 60.0), n(B5, 60.0, 60.0), n(E5, 120.0, 60.0), n(B5, 180.0, 60.0)]),
            AlertKind::ChannelMessage => Spec::bell(0.16, vec![n(G5, 0.0, 110.0)]),
            AlertKind::Mention => Spec::bell(0.34, vec![n(B5, 0.0, 80.0), n(E6, 55.0, 80.0), n(G6, 110.0, 130.0)]),
        }
    }
}

struct Note {
    freq: f32,
    start_ms: f32,
    dur_ms: f32,
}

#[derive(Clone, Copy, PartialEq)]
enum Timbre {
    /// Sine plus a little second partial: a soft bell that rings out.
    Bell,
    /// Triangle-ish (odd partials): a rounder, shorter "pop".
    Blip,
}

struct Spec {
    timbre: Timbre,
    /// Peak of the whole sound as a fraction of full scale.
    peak: f32,
    notes: Vec<Note>,
}

impl Spec {
    fn bell(peak: f32, notes: Vec<Note>) -> Self {
        Self { timbre: Timbre::Bell, peak, notes }
    }
    fn blip(peak: f32, notes: Vec<Note>) -> Self {
        Self { timbre: Timbre::Blip, peak, notes }
    }
}

const SR: f32 = MIX_RATE as f32;
/// Note attack (raised cosine) and release (raised cosine to zero).
const ATTACK_MS: f32 = 5.0;
const RELEASE_MS: f32 = 12.0;
/// Edge fades over the whole mixed sound (belt and braces on top of the per-note ones).
const EDGE_IN_MS: f32 = 2.0;
const EDGE_OUT_MS: f32 = 8.0;

fn ms_to_samples(ms: f32) -> usize {
    (ms * SR / 1000.0).round() as usize
}

fn waveform(timbre: Timbre, phase: f32) -> f32 {
    match timbre {
        Timbre::Bell => phase.sin() + 0.20 * (2.0 * phase).sin() + 0.05 * (3.0 * phase).sin(),
        Timbre::Blip => phase.sin() - (3.0 * phase).sin() / 9.0 + (5.0 * phase).sin() / 25.0,
    }
}

fn synth(spec: &Spec) -> Vec<i16> {
    let total = spec.notes.iter().map(|n| ms_to_samples(n.start_ms + n.dur_ms)).max().unwrap_or(0);
    let mut mono = vec![0f32; total];
    // How far a note decays by its end: bells ring out (-36 dB), blips are punchier (-14 dB).
    let decay_db = if spec.timbre == Timbre::Bell { 36.0f32 } else { 14.0 };
    for note in &spec.notes {
        let start = ms_to_samples(note.start_ms);
        let len = ms_to_samples(note.dur_ms);
        let (att, rel) = (ms_to_samples(ATTACK_MS).min(len / 2), ms_to_samples(RELEASE_MS).min(len / 2));
        let k = decay_db / 20.0 * std::f32::consts::LN_10 / len as f32; // exp(-k*len) = -decay_db
        for i in 0..len {
            let mut env = (-k * i as f32).exp();
            if i < att {
                env *= (std::f32::consts::FRAC_PI_2 * i as f32 / att as f32).sin().powi(2);
            }
            if i >= len - rel {
                env *= (std::f32::consts::FRAC_PI_2 * (len - 1 - i) as f32 / rel as f32).sin().powi(2);
            }
            let phase = TAU * note.freq * i as f32 / SR;
            mono[start + i] += env * waveform(spec.timbre, phase);
        }
    }
    // Edge fades over the whole sound; the very first and last sample are exactly 0.
    let (fin, fout) = (ms_to_samples(EDGE_IN_MS), ms_to_samples(EDGE_OUT_MS));
    for (i, v) in mono.iter_mut().enumerate().take(fin) {
        *v *= (std::f32::consts::FRAC_PI_2 * i as f32 / fin as f32).sin().powi(2);
    }
    for i in 0..fout.min(total) {
        mono[total - 1 - i] *= (std::f32::consts::FRAC_PI_2 * i as f32 / fout as f32).sin().powi(2);
    }
    let max = mono.iter().fold(0f32, |m, s| m.max(s.abs())).max(1e-6);
    let scale = spec.peak * i16::MAX as f32 / max;
    let mut out = Vec::with_capacity(total * MIX_CHANNELS);
    for s in mono {
        let v = (s * scale).round() as i16;
        out.extend(std::iter::repeat_n(v, MIX_CHANNELS));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(k: AlertKind) -> Vec<i16> {
        k.samples().as_chunks::<MIX_CHANNELS>().0.iter().map(|f| f[0]).collect()
    }

    fn peak(k: AlertKind) -> i16 {
        frames(k).iter().map(|s| s.saturating_abs()).max().unwrap()
    }

    #[test]
    fn lengths_are_short_and_stereo() {
        for k in AlertKind::ALL {
            let s = k.samples();
            assert_eq!(s.len() % MIX_CHANNELS, 0, "{k:?}");
            let ms = frames(k).len() as f32 * 1000.0 / SR;
            assert!((80.0..=250.0).contains(&ms), "{k:?} lasts {ms} ms");
            // Mono content duplicated into both channels.
            assert!(s.as_chunks::<2>().0.iter().all(|f| f[0] == f[1]), "{k:?} L != R");
        }
    }

    #[test]
    fn peaks_are_audible_but_never_near_clipping() {
        for k in AlertKind::ALL {
            let p = peak(k);
            assert!(p > 3_000, "{k:?} peak {p} too quiet to hear");
            assert!(p < (i16::MAX as f32 * 0.5) as i16, "{k:?} peak {p} too loud");
        }
        // Even every alert stacked on top of each other stays inside i16 when summed in i32 and
        // the mixer saturates; a realistic pair never exceeds full scale.
        assert!((peak(AlertKind::Join) as i32 + peak(AlertKind::Unmute) as i32) < i16::MAX as i32);
    }

    #[test]
    fn online_and_offline_are_quieter_than_the_voice_alerts() {
        for quiet in [AlertKind::Online, AlertKind::Offline] {
            for loud in [AlertKind::Join, AlertKind::Leave, AlertKind::Mute, AlertKind::Unmute, AlertKind::Deafen, AlertKind::Undeafen] {
                assert!(peak(quiet) * 2 < peak(loud), "{quiet:?} vs {loud:?}");
            }
        }
    }

    #[test]
    fn chat_sounds_are_ordered_soft_to_bright() {
        // A channel message is the softest, a direct message louder, a mention the brightest.
        let (c, d, m) = (peak(AlertKind::ChannelMessage), peak(AlertKind::DirectMessage), peak(AlertKind::Mention));
        assert!(c * 3 / 2 < d && c < m, "channel {c}, dm {d}, mention {m}");
        let top = |k: AlertKind| pitch(k, 100.0, 180.0);
        assert!(top(AlertKind::Mention) > top(AlertKind::DirectMessage), "the mention is brighter than a DM");
        assert!(frames(AlertKind::Mention).len() > frames(AlertKind::ChannelMessage).len());
    }

    #[test]
    fn edges_fade_from_and_to_silence_without_clicks() {
        let half_ms = ms_to_samples(0.5);
        for k in AlertKind::ALL {
            let f = frames(k);
            let p = peak(k) as f32;
            assert_eq!(f[0], 0, "{k:?} starts at zero");
            assert_eq!(*f.last().unwrap(), 0, "{k:?} ends at zero");
            // The first and last half millisecond are still nearly silent.
            assert!(f[..half_ms].iter().all(|s| (s.abs() as f32) < p * 0.05), "{k:?} fade-in too abrupt");
            assert!(f[f.len() - half_ms..].iter().all(|s| (s.abs() as f32) < p * 0.05), "{k:?} fade-out too abrupt");
            // No jump between neighbouring samples anywhere (a click would show as one).
            let max_step = f.windows(2).map(|w| (w[1] as i32 - w[0] as i32).abs()).max().unwrap() as f32;
            assert!(max_step < p * 0.3, "{k:?} has a {max_step} step at peak {p}");
        }
    }

    /// Zero crossings per second in `from_ms..to_ms` (a fine pitch tracker for steady tones).
    fn pitch(k: AlertKind, from_ms: f32, to_ms: f32) -> f32 {
        let f = frames(k);
        let (a, b) = (ms_to_samples(from_ms), ms_to_samples(to_ms).min(f.len()));
        let crossings = f[a..b].windows(2).filter(|w| (w[0] < 0) != (w[1] < 0)).count();
        crossings as f32 * 1000.0 / (to_ms - from_ms) / 2.0
    }

    #[test]
    fn rising_and_falling_shapes_are_recognisable() {
        let ms = |k: AlertKind| frames(k).len() as f32 * 1000.0 / SR;
        let ratio = |k| pitch(k, 5.0, 35.0) / pitch(k, ms(k) - 45.0, ms(k) - 15.0);
        for k in [AlertKind::Join, AlertKind::Unmute, AlertKind::Undeafen, AlertKind::StreamStart, AlertKind::Mention] {
            assert!(ratio(k) < 0.8, "{k:?} should rise (start/end pitch = {})", ratio(k));
        }
        for k in [AlertKind::Leave, AlertKind::Mute, AlertKind::Deafen, AlertKind::StreamStop] {
            assert!(ratio(k) > 1.25, "{k:?} should fall (start/end pitch = {})", ratio(k));
        }
    }

    #[test]
    fn every_alert_sounds_different() {
        for (i, a) in AlertKind::ALL.iter().enumerate() {
            for b in &AlertKind::ALL[i + 1..] {
                assert_ne!(a.samples(), b.samples(), "{a:?} and {b:?} are identical");
            }
        }
        // Mute/unmute and deafen/undeafen differ in note count so they are never confused.
        assert!(frames(AlertKind::Deafen).len() > frames(AlertKind::Mute).len());
    }

    #[test]
    fn buffers_are_computed_once_and_shared() {
        let a = AlertKind::Join.samples();
        let b = AlertKind::Join.samples();
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn names_are_unique() {
        let mut names: Vec<_> = AlertKind::ALL.iter().map(|k| k.name()).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), AlertKind::ALL.len());
    }
}
