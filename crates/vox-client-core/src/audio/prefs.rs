//! Local listening preferences: per-person microphone volume / mute, per-person screenshare
//! audio volume / mute, and the "mute everyone" switch. They only change what *this* client
//! plays; nothing is sent to the server or to the other participants.
//!
//! Settings are keyed by participant **identity**, not by track SID, and live as long as the
//! [`AudioPrefs`] does, so they survive a participant leaving and rejoining and are already in
//! place when their track subscribes. Every control is a [`Gain`] (atomics) shared with the
//! mixer, so changing one is lock-free for the audio thread.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;

use super::mixer::{Gain, SourceCtl, MAX_GAIN};

/// A snapshot of one person's local audio settings (what [`AudioPrefs::user_audio`] returns and
/// what the app persists). `volume` is a gain: `1.0` = 100 %, at most `2.0`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UserAudio {
    /// Their microphone.
    pub volume: f32,
    pub muted: bool,
    /// The audio of their screenshare, when watching.
    pub stream_volume: f32,
    pub stream_muted: bool,
}

impl Default for UserAudio {
    fn default() -> Self {
        Self { volume: 1.0, muted: false, stream_volume: 1.0, stream_muted: false }
    }
}

impl UserAudio {
    /// Everything at its default (100 %, not muted).
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Same values with the volumes forced into `0.0..=MAX_GAIN` (NaN becomes 100 %).
    pub fn sanitized(self) -> Self {
        let fix = |v: f32| if v.is_nan() { 1.0 } else { v.clamp(0.0, MAX_GAIN) };
        Self { volume: fix(self.volume), stream_volume: fix(self.stream_volume), ..self }
    }
}

#[derive(Default)]
struct UserGains {
    mic: Arc<Gain>,
    stream: Arc<Gain>,
    /// Their stream is silenced because another watched stream is in focus ("som só da
    /// transmissão em foco", item 9). Not part of the saved settings.
    stream_unfocused: Arc<AtomicBool>,
}

/// All local listening preferences of one voice session.
#[derive(Default)]
pub struct AudioPrefs {
    users: Mutex<HashMap<String, Arc<UserGains>>>,
    others_muted: Arc<AtomicBool>,
}

impl AudioPrefs {
    pub fn new() -> Self {
        Self::default()
    }

    fn user(&self, identity: &str) -> Arc<UserGains> {
        self.users.lock().entry(identity.to_string()).or_default().clone()
    }

    pub fn set_user_volume(&self, identity: &str, gain: f32) {
        self.user(identity).mic.set_level(gain);
    }

    pub fn set_user_muted(&self, identity: &str, muted: bool) {
        self.user(identity).mic.set_muted(muted);
    }

    pub fn set_stream_volume(&self, identity: &str, gain: f32) {
        self.user(identity).stream.set_level(gain);
    }

    pub fn set_stream_muted(&self, identity: &str, muted: bool) {
        self.user(identity).stream.set_muted(muted);
    }

    /// Apply a whole snapshot (used to restore saved settings).
    pub fn set_user_audio(&self, identity: &str, a: UserAudio) {
        let u = self.user(identity);
        u.mic.set_level(a.volume);
        u.mic.set_muted(a.muted);
        u.stream.set_level(a.stream_volume);
        u.stream.set_muted(a.stream_muted);
    }

    pub fn user_audio(&self, identity: &str) -> UserAudio {
        match self.users.lock().get(identity) {
            Some(u) => UserAudio {
                volume: u.mic.level(),
                muted: u.mic.muted(),
                stream_volume: u.stream.level(),
                stream_muted: u.stream.muted(),
            },
            None => UserAudio::default(),
        }
    }

    /// Silence every other participant's microphone without touching their own settings (and
    /// without affecting our microphone). Turning it off restores each person as they were.
    /// Screenshare audio is not affected: it has its own volume / mute.
    pub fn set_others_muted(&self, muted: bool) {
        self.others_muted.store(muted, Ordering::Relaxed);
    }

    pub fn others_muted(&self) -> bool {
        self.others_muted.load(Ordering::Relaxed)
    }

    /// Mixer control for `identity`'s microphone track.
    pub fn mic_ctl(&self, identity: &str) -> SourceCtl {
        SourceCtl::new(self.user(identity).mic.clone(), Some(self.others_muted.clone()))
    }

    /// Silence `identity`'s screenshare audio while another stream is in focus, without touching
    /// its volume / mute (they come back as they were).
    pub fn set_stream_focus_muted(&self, identity: &str, muted: bool) {
        self.user(identity).stream_unfocused.store(muted, Ordering::Relaxed);
    }

    pub fn stream_focus_muted(&self, identity: &str) -> bool {
        self.users.lock().get(identity).is_some_and(|u| u.stream_unfocused.load(Ordering::Relaxed))
    }

    /// Mixer control for `identity`'s screenshare audio track.
    pub fn stream_ctl(&self, identity: &str) -> SourceCtl {
        let u = self.user(identity);
        SourceCtl::new(u.stream.clone(), Some(u.stream_unfocused.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::mixer::{Mixer, MIX_10MS};

    fn play(m: &Mixer, id: u64, v: i16) -> i16 {
        m.push(id, &vec![v; 4 * MIX_10MS]);
        let mut out = vec![0i16; MIX_10MS];
        // First block may ramp after a change; the third is settled. Leave nothing queued.
        for _ in 0..4 {
            m.mix(&mut out);
        }
        assert!(out.iter().all(|&s| s == out[0]));
        out[0]
    }

    #[test]
    fn defaults_are_unity() {
        let p = AudioPrefs::new();
        assert_eq!(p.user_audio("7"), UserAudio::default());
        assert!(p.user_audio("7").is_default());
        assert!(!p.others_muted());
    }

    #[test]
    fn settings_apply_to_a_track_that_subscribes_later_and_survive_rejoin() {
        let p = AudioPrefs::new();
        let m = Mixer::new();
        // Settings exist before any track does.
        p.set_user_volume("ana", 0.5);
        p.set_stream_volume("ana", 2.0);
        let mic = m.add_source_with(p.mic_ctl("ana"));
        let stream = m.add_source_with(p.stream_ctl("ana"));
        assert_eq!(play(&m, mic, 1000), 500);
        assert_eq!(play(&m, stream, 1000), 2000);
        // She leaves (sources dropped) and rejoins with a new track: same settings.
        m.remove_source(mic);
        m.remove_source(stream);
        let mic = m.add_source_with(p.mic_ctl("ana"));
        assert_eq!(play(&m, mic, 1000), 500);
        // Mic and stream are independent; other people are untouched.
        p.set_user_muted("ana", true);
        assert_eq!(play(&m, mic, 1000), 0);
        let bob = m.add_source_with(p.mic_ctl("bob"));
        assert_eq!(play(&m, bob, 1000), 1000);
        assert_eq!(
            p.user_audio("ana"),
            UserAudio { volume: 0.5, muted: true, stream_volume: 2.0, stream_muted: false }
        );
    }

    #[test]
    fn mute_all_hits_mics_only_and_restores_previous_state() {
        let p = AudioPrefs::new();
        let m = Mixer::new();
        p.set_user_volume("ana", 1.5);
        p.set_user_muted("bob", true); // already muted on its own
        let ana = m.add_source_with(p.mic_ctl("ana"));
        let bob = m.add_source_with(p.mic_ctl("bob"));
        let cat = m.add_source_with(p.mic_ctl("cat"));
        let share = m.add_source_with(p.stream_ctl("ana"));
        let heard = |m: &Mixer| (play(m, ana, 1000), play(m, bob, 1000), play(m, cat, 1000), play(m, share, 1000));
        assert_eq!(heard(&m), (1500, 0, 1000, 1000));
        p.set_others_muted(true);
        assert_eq!(heard(&m), (0, 0, 0, 1000), "stream audio has its own mute");
        // The per-person settings were not rewritten by the switch.
        assert_eq!(p.user_audio("ana").volume, 1.5);
        assert!(!p.user_audio("ana").muted);
        assert!(p.user_audio("bob").muted);
        p.set_others_muted(false);
        assert_eq!(heard(&m), (1500, 0, 1000, 1000));
        // A person who joins while "mute all" is on is silenced too.
        p.set_others_muted(true);
        let dan = m.add_source_with(p.mic_ctl("dan"));
        assert_eq!(play(&m, dan, 1000), 0);
        p.set_others_muted(false);
        assert_eq!(play(&m, dan, 1000), 1000);
    }

    #[test]
    fn several_streams_mix_with_their_own_gains_and_focus_mute() {
        let p = AudioPrefs::new();
        let m = Mixer::new();
        p.set_stream_volume("ana", 0.5);
        let ana = m.add_source_with(p.stream_ctl("ana"));
        let bob = m.add_source_with(p.stream_ctl("bob"));
        // Both audible at their own volume by default.
        assert_eq!((play(&m, ana, 1000), play(&m, bob, 1000)), (500, 1000));
        // Ana in focus: Bob's stream is silenced without touching his settings.
        p.set_stream_focus_muted("bob", true);
        assert!(p.stream_focus_muted("bob"));
        assert_eq!((play(&m, ana, 1000), play(&m, bob, 1000)), (500, 0));
        assert_eq!(p.user_audio("bob"), UserAudio::default());
        // A later subscription (Bob restarted his live) is still silenced; focus off restores.
        m.remove_source(bob);
        let bob = m.add_source_with(p.stream_ctl("bob"));
        assert_eq!(play(&m, bob, 1000), 0);
        p.set_stream_focus_muted("bob", false);
        assert_eq!(play(&m, bob, 1000), 1000);
        // Nobody's microphone is affected by it.
        p.set_stream_focus_muted("ana", true);
        let ana_mic = m.add_source_with(p.mic_ctl("ana"));
        assert_eq!(play(&m, ana_mic, 1000), 1000);
    }

    #[test]
    fn snapshots_roundtrip_and_sanitize() {
        let p = AudioPrefs::new();
        let a = UserAudio { volume: 0.25, muted: true, stream_volume: 1.75, stream_muted: true };
        p.set_user_audio("x", a);
        assert_eq!(p.user_audio("x"), a);
        let wild = UserAudio { volume: 9.0, muted: false, stream_volume: f32::NAN, stream_muted: false }.sanitized();
        assert_eq!(wild.volume, MAX_GAIN);
        assert_eq!(wild.stream_volume, 1.0);
        p.set_user_audio("y", UserAudio { volume: -3.0, ..Default::default() });
        assert_eq!(p.user_audio("y").volume, 0.0);
    }
}
