//! Alert sounds: which hub/voice event deserves which sound, the user's settings for them, and
//! the debounce rules. Everything here is pure (no audio, no I/O) so it can be unit tested;
//! the synthesis and playback live in `vox_client_core::audio`.
//!
//! Rules of thumb:
//! - nothing plays for the state snapshot (`Ready`, also sent again after a reconnect), nor for
//!   the few seconds after it while everybody re-announces their state to a restarted hub;
//! - nothing plays for people who were already in the room when we joined (the hub does not
//!   send events for them, and a changed mute flag is not a join);
//! - bursts collapse into one sound and a flapping connection cannot spam: see [`AlertPolicy`].

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use vox_client_core::AlertKind;
use vox_proto::{ChannelId, User, UserId, VoiceMember};

/// What the user can switch on and off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    /// Someone came online / went offline.
    Presence,
    /// Someone (or I) entered / left my voice channel.
    Room,
    /// Someone went live / stopped in my channel (and, with item 7, started watching mine).
    Streams,
    /// My own mute / unmute / deafen / undeafen.
    OwnState,
    /// A direct message arrived ("Mensagem direta").
    DirectMessages,
    /// A message in a channel I do not have open, or a mention ("Mensagem em outro canal").
    Chat,
    /// Somebody is calling me from a DM (the ringtone).
    Calls,
}

pub fn category(kind: AlertKind) -> Category {
    match kind {
        AlertKind::Online | AlertKind::Offline => Category::Presence,
        AlertKind::Join | AlertKind::Leave => Category::Room,
        AlertKind::StreamStart | AlertKind::StreamStop | AlertKind::Viewer => Category::Streams,
        AlertKind::Mute | AlertKind::Unmute | AlertKind::Deafen | AlertKind::Undeafen => Category::OwnState,
        AlertKind::DirectMessage => Category::DirectMessages,
        AlertKind::Ring => Category::Calls,
        AlertKind::ChannelMessage | AlertKind::Mention => Category::Chat,
    }
}

/// `settings.json` -> `alerts`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AlertSettings {
    /// Master switch.
    pub enabled: bool,
    /// Alert volume, 0..=100.
    pub volume_pct: u32,
    /// Online / offline (off by default: it is the chattiest category).
    pub presence: bool,
    /// Enter / leave the voice channel.
    pub room: bool,
    /// Someone going live or watching.
    pub streams: bool,
    /// My own mute / deafen.
    pub own_state: bool,
    /// "Mensagem direta": a DM arrived (on by default; older settings files get the default).
    pub dms: bool,
    /// The ringtone of an incoming DM call (on by default).
    pub calls: bool,
    /// "Mensagem nova em outro canal": the soft sound of a channel message (and, from v0.8, the
    /// brighter one of a mention). On by default; older settings files get the default.
    pub chat: bool,
}

impl Default for AlertSettings {
    fn default() -> Self {
        Self { enabled: true, volume_pct: 60, presence: false, room: true, streams: true, own_state: true, dms: true, calls: true, chat: true }
    }
}

impl AlertSettings {
    /// Whether `kind` may play at all with these settings.
    pub fn allows(&self, kind: AlertKind) -> bool {
        self.enabled
            && self.volume_pct > 0
            && match category(kind) {
                Category::Presence => self.presence,
                Category::Room => self.room,
                Category::Streams => self.streams,
                Category::OwnState => self.own_state,
                Category::DirectMessages => self.dms,
                Category::Chat => self.chat,
                Category::Calls => self.calls,
            }
    }

    /// Playback gain for the mixer.
    pub fn gain(&self) -> f32 {
        self.volume_pct.min(100) as f32 / 100.0
    }
}

// ---------------------------------------------------------------- event -> sound

/// Sound for someone coming online. `already_online`: we already listed them (a repeat event,
/// e.g. a second session) and `me` is our own id.
pub fn online_alert(me: Option<UserId>, user: &User, already_online: bool) -> Option<AlertKind> {
    (!already_online && Some(user.id) != me).then_some(AlertKind::Online)
}

/// Sound for `VoiceStateChanged`: `prev` is what we knew of that person, `new` the update.
/// Only people in *our* voice channel matter; our own changes are covered by the local actions.
pub fn voice_alert(me: Option<UserId>, my_channel: Option<ChannelId>, prev: Option<&VoiceMember>, new: &VoiceMember) -> Option<AlertKind> {
    if Some(new.user.id) == me {
        return None;
    }
    let ch = my_channel?;
    let was_here = prev.is_some_and(|p| p.channel_id == Some(ch));
    let is_here = new.channel_id == Some(ch);
    match (was_here, is_here) {
        (false, true) => Some(AlertKind::Join),
        (true, false) => Some(AlertKind::Leave),
        (true, true) => {
            let was_live = prev.is_some_and(|p| p.streaming);
            match (was_live, new.streaming) {
                (false, true) => Some(AlertKind::StreamStart),
                (true, false) => Some(AlertKind::StreamStop),
                _ => None,
            }
        }
        (false, false) => None,
    }
}

/// Sound for someone starting to watch *my* live: `new` is the update about a member, `prev` what
/// we knew of them. Only fires on the transition (not listed -> listed as watching me), for people
/// in my voice channel, while I am live. A viewer leaving is silent.
pub fn viewer_alert(me: Option<UserId>, my_channel: Option<ChannelId>, i_am_live: bool, prev: Option<&VoiceMember>, new: &VoiceMember) -> Option<AlertKind> {
    let me = me?;
    let ch = my_channel?;
    if new.user.id == me || !i_am_live || new.channel_id != Some(ch) {
        return None;
    }
    let was = prev.is_some_and(|p| p.channel_id == Some(ch) && p.watching.contains(&me));
    (!was && new.watching.contains(&me)).then_some(AlertKind::Viewer)
}

/// Sound for `UserOffline`. Someone who drops out of our voice channel by going offline is a
/// "leave", not an "offline" (one sound, not two).
pub fn offline_alert(me: Option<UserId>, my_channel: Option<ChannelId>, user_id: UserId, was_online: bool, prev_voice: Option<&VoiceMember>) -> Option<AlertKind> {
    if Some(user_id) == me {
        return None;
    }
    if my_channel.is_some() && prev_voice.is_some_and(|p| p.channel_id == my_channel) {
        return Some(AlertKind::Leave);
    }
    was_online.then_some(AlertKind::Offline)
}

// ---------------------------------------------------------------- debounce

/// After a hub snapshot, remote events are ignored for this long (everyone re-announces).
pub const SNAPSHOT_QUIET: Duration = Duration::from_secs(3);
/// Two alerts of the same kind closer than this are one (a burst of joins plays one chime).
pub const KIND_GAP: Duration = Duration::from_millis(400);
/// The same person doing the same thing again this soon is a flap, not news.
pub const PERSON_GAP: Duration = Duration::from_secs(2);
/// At most [`WINDOW_MAX`] remote alerts per [`WINDOW`], whatever they are.
pub const WINDOW: Duration = Duration::from_secs(5);
pub const WINDOW_MAX: usize = 5;

/// Debounce for alerts caused by *other people*. Alerts for the user's own actions (mute,
/// deafen, own join/leave) bypass it: that is direct feedback for a click.
#[derive(Default)]
pub struct AlertPolicy {
    quiet_until: Option<Instant>,
    recent: VecDeque<Instant>,
    last_kind: HashMap<AlertKind, Instant>,
    last_person: HashMap<(UserId, AlertKind), Instant>,
}

impl AlertPolicy {
    /// The hub sent a full snapshot (first connect or reconnect).
    pub fn on_snapshot(&mut self, now: Instant) {
        self.quiet_until = Some(now + SNAPSHOT_QUIET);
    }

    /// Should the alert `kind` caused by `who` play now? Records it when it does.
    pub fn admit(&mut self, now: Instant, kind: AlertKind, who: UserId) -> bool {
        if self.quiet_until.is_some_and(|t| now < t) {
            return false;
        }
        let within = |last: Option<&Instant>, gap: Duration| last.is_some_and(|t| now.saturating_duration_since(*t) < gap);
        if within(self.last_person.get(&(who, kind)), PERSON_GAP) || within(self.last_kind.get(&kind), KIND_GAP) {
            return false;
        }
        // Closing the app sends "left the room" and then "offline" back to back: one sound.
        if kind == AlertKind::Offline && within(self.last_person.get(&(who, AlertKind::Leave)), PERSON_GAP) {
            return false;
        }
        while self.recent.front().is_some_and(|t| now.saturating_duration_since(*t) >= WINDOW) {
            self.recent.pop_front();
        }
        if self.recent.len() >= WINDOW_MAX {
            return false;
        }
        self.recent.push_back(now);
        self.last_kind.insert(kind, now);
        if self.last_person.len() > 64 {
            self.last_person.retain(|_, t| now.saturating_duration_since(*t) < PERSON_GAP);
        }
        self.last_person.insert((who, kind), now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(id: i64) -> User {
        User::new(id, format!("u{id}"))
    }

    fn member(id: i64, channel: Option<i64>, streaming: bool) -> VoiceMember {
        VoiceMember { user: user(id), channel_id: channel, muted: false, deafened: false, streaming, watching: vec![], camera: false, server_muted: false, audio_only: false }
    }

    const ME: Option<UserId> = Some(1);

    #[test]
    fn someone_joining_or_leaving_my_channel() {
        let here = Some(10);
        // Not in voice: nothing.
        assert_eq!(voice_alert(ME, None, None, &member(2, Some(10), false)), None);
        // Joins my channel (new person, or coming from another channel).
        assert_eq!(voice_alert(ME, here, None, &member(2, Some(10), false)), Some(AlertKind::Join));
        assert_eq!(voice_alert(ME, here, Some(&member(2, Some(11), false)), &member(2, Some(10), false)), Some(AlertKind::Join));
        // Leaves (to nothing, or to another channel).
        assert_eq!(voice_alert(ME, here, Some(&member(2, Some(10), false)), &member(2, None, false)), Some(AlertKind::Leave));
        assert_eq!(voice_alert(ME, here, Some(&member(2, Some(10), false)), &member(2, Some(11), false)), Some(AlertKind::Leave));
        // Other channels are none of my business.
        assert_eq!(voice_alert(ME, here, Some(&member(2, Some(11), false)), &member(2, Some(12), false)), None);
        assert_eq!(voice_alert(ME, here, None, &member(2, Some(11), false)), None);
        // A person already in the room changing mute / deafen is not a join.
        let mut muted = member(2, Some(10), false);
        muted.muted = true;
        assert_eq!(voice_alert(ME, here, Some(&member(2, Some(10), false)), &muted), None);
        // My own state is handled by my own actions.
        assert_eq!(voice_alert(ME, here, None, &member(1, Some(10), false)), None);
    }

    #[test]
    fn live_starting_and_stopping_in_my_channel() {
        let here = Some(10);
        let off = member(2, Some(10), false);
        let on = member(2, Some(10), true);
        assert_eq!(voice_alert(ME, here, Some(&off), &on), Some(AlertKind::StreamStart));
        assert_eq!(voice_alert(ME, here, Some(&on), &off), Some(AlertKind::StreamStop));
        assert_eq!(voice_alert(ME, here, Some(&on), &on), None);
        // Going live in some other room: silence. Joining while live: just the join.
        assert_eq!(voice_alert(ME, here, Some(&member(2, Some(11), false)), &member(2, Some(11), true)), None);
        assert_eq!(voice_alert(ME, here, None, &on), Some(AlertKind::Join));
        // Leaving while live: just the leave.
        assert_eq!(voice_alert(ME, here, Some(&on), &member(2, None, false)), Some(AlertKind::Leave));
    }

    #[test]
    fn someone_starting_to_watch_my_live() {
        let here = Some(10);
        let watching = |id: i64, ch: i64, w: &[i64]| VoiceMember { watching: w.to_vec(), ..member(id, Some(ch), false) };
        let none = watching(2, 10, &[]);
        let mine = watching(2, 10, &[1]);
        // Starts watching me: one sound; already watching (mute flip): none; stops: none.
        assert_eq!(viewer_alert(ME, here, true, Some(&none), &mine), Some(AlertKind::Viewer));
        assert_eq!(viewer_alert(ME, here, true, None, &mine), Some(AlertKind::Viewer));
        assert_eq!(viewer_alert(ME, here, true, Some(&mine), &mine), None);
        assert_eq!(viewer_alert(ME, here, true, Some(&mine), &none), None);
        // Watching somebody else's stream is not my business; neither is it when I am not live.
        assert_eq!(viewer_alert(ME, here, true, Some(&none), &watching(2, 10, &[3])), None);
        assert_eq!(viewer_alert(ME, here, false, Some(&none), &mine), None);
        assert_eq!(viewer_alert(ME, None, true, Some(&none), &mine), None);
        // Other rooms (stale data) and myself never count.
        assert_eq!(viewer_alert(ME, here, true, None, &watching(2, 11, &[1])), None);
        assert_eq!(viewer_alert(ME, here, true, None, &watching(1, 10, &[1])), None);
        assert_eq!(viewer_alert(None, here, true, None, &mine), None);
        // The viewer list changing around me (another viewer arrives) does not re-trigger for me.
        assert_eq!(viewer_alert(ME, here, true, Some(&watching(2, 10, &[1])), &watching(2, 10, &[1, 3])), None);
    }

    #[test]
    fn viewers_follow_the_snapshot_and_person_debounce() {
        let t0 = Instant::now();
        let mut p = AlertPolicy::default();
        // After a (re)connect snapshot everybody's watch re-announces: silent.
        p.on_snapshot(t0);
        assert!(!p.admit(t0 + Duration::from_millis(500), AlertKind::Viewer, 2));
        let t = t0 + SNAPSHOT_QUIET;
        assert!(p.admit(t, AlertKind::Viewer, 2));
        // Two people joining my stream at once: one knock. A flapping viewer: once per gap.
        assert!(!p.admit(t + Duration::from_millis(50), AlertKind::Viewer, 3));
        assert!(!p.admit(t + Duration::from_millis(1000), AlertKind::Viewer, 2));
        assert!(p.admit(t + PERSON_GAP, AlertKind::Viewer, 2));
    }

    #[test]
    fn online_and_offline() {
        assert_eq!(online_alert(ME, &user(2), false), Some(AlertKind::Online));
        assert_eq!(online_alert(ME, &user(2), true), None, "repeat event");
        assert_eq!(online_alert(ME, &user(1), false), None, "myself");
        assert_eq!(offline_alert(ME, None, 2, true, None), Some(AlertKind::Offline));
        assert_eq!(offline_alert(ME, None, 2, false, None), None, "never listed as online");
        assert_eq!(offline_alert(ME, None, 1, true, None), None, "myself");
        // Dropping out of my voice channel is a leave, not an offline.
        let in_room = member(2, Some(10), false);
        assert_eq!(offline_alert(ME, Some(10), 2, true, Some(&in_room)), Some(AlertKind::Leave));
        // In some other room, or while I am not in voice: plain offline.
        assert_eq!(offline_alert(ME, Some(11), 2, true, Some(&in_room)), Some(AlertKind::Offline));
        assert_eq!(offline_alert(ME, None, 2, true, Some(&in_room)), Some(AlertKind::Offline));
    }

    #[test]
    fn every_alert_belongs_to_one_category_and_the_switches_work() {
        let all_off = AlertSettings { enabled: true, volume_pct: 60, presence: false, room: false, streams: false, own_state: false, dms: false, calls: false, chat: false };
        for k in AlertKind::ALL {
            assert!(!all_off.allows(k), "{k:?}");
        }
        for (field, kinds) in [
            ("presence", vec![AlertKind::Online, AlertKind::Offline]),
            ("room", vec![AlertKind::Join, AlertKind::Leave]),
            ("streams", vec![AlertKind::StreamStart, AlertKind::StreamStop, AlertKind::Viewer]),
            ("own_state", vec![AlertKind::Mute, AlertKind::Unmute, AlertKind::Deafen, AlertKind::Undeafen]),
            ("dms", vec![AlertKind::DirectMessage]),
            ("calls", vec![AlertKind::Ring]),
            ("chat", vec![AlertKind::ChannelMessage, AlertKind::Mention]),
        ] {
            let mut s = all_off;
            match field {
                "presence" => s.presence = true,
                "room" => s.room = true,
                "streams" => s.streams = true,
                "dms" => s.dms = true,
                "calls" => s.calls = true,
                "chat" => s.chat = true,
                _ => s.own_state = true,
            }
            let allowed: Vec<_> = AlertKind::ALL.iter().copied().filter(|k| s.allows(*k)).collect();
            assert_eq!(allowed, kinds, "{field}");
        }
    }

    #[test]
    fn defaults_master_switch_and_volume() {
        let d = AlertSettings::default();
        assert!(d.enabled && d.room && d.streams && d.own_state);
        assert!(!d.presence, "online/offline sounds are opt-in");
        assert!(d.allows(AlertKind::Join) && !d.allows(AlertKind::Online));
        let off = AlertSettings { enabled: false, presence: true, ..d };
        assert!(AlertKind::ALL.iter().all(|k| !off.allows(*k)), "master switch");
        let silent = AlertSettings { volume_pct: 0, ..d };
        assert!(AlertKind::ALL.iter().all(|k| !silent.allows(*k)), "volume 0 plays nothing");
        assert_eq!(AlertSettings { volume_pct: 50, ..d }.gain(), 0.5);
        assert_eq!(AlertSettings { volume_pct: 900, ..d }.gain(), 1.0);
    }

    #[test]
    fn settings_load_from_partial_json() {
        let s: AlertSettings = serde_json::from_str(r#"{"presence":true}"#).unwrap();
        assert!(s.presence && s.enabled && s.volume_pct == 60);
        assert!(s.dms && s.calls, "a settings file from before DMs gets the DM sounds on");
        assert!(s.chat, "a settings file from before the chat sounds gets them on");
        let back: AlertSettings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn snapshot_silences_the_next_few_seconds() {
        let t0 = Instant::now();
        let mut p = AlertPolicy::default();
        assert!(p.admit(t0, AlertKind::Join, 2));
        p.on_snapshot(t0 + Duration::from_secs(10));
        let t = t0 + Duration::from_secs(10);
        assert!(!p.admit(t + Duration::from_millis(10), AlertKind::Join, 3));
        assert!(!p.admit(t + SNAPSHOT_QUIET - Duration::from_millis(1), AlertKind::Leave, 4));
        assert!(p.admit(t + SNAPSHOT_QUIET, AlertKind::Join, 5));
    }

    #[test]
    fn a_burst_of_the_same_kind_plays_once_but_different_kinds_each_play() {
        let t0 = Instant::now();
        let mut p = AlertPolicy::default();
        // Joining a full channel's worth of re-announcements within a moment: one chime.
        assert!(p.admit(t0, AlertKind::Join, 2));
        for (i, who) in (3..10).enumerate() {
            assert!(!p.admit(t0 + Duration::from_millis(10 * i as u64), AlertKind::Join, who));
        }
        // A different kind is independent; the same kind plays again after the gap.
        assert!(p.admit(t0 + Duration::from_millis(50), AlertKind::Leave, 3));
        assert!(p.admit(t0 + KIND_GAP, AlertKind::Join, 4));
    }

    #[test]
    fn a_flapping_person_is_heard_once_per_gap() {
        let t0 = Instant::now();
        let mut p = AlertPolicy::default();
        assert!(p.admit(t0, AlertKind::Online, 2));
        // 2 goes offline/online every second: online repeats within PERSON_GAP are dropped.
        assert!(!p.admit(t0 + Duration::from_millis(1000), AlertKind::Online, 2));
        assert!(p.admit(t0 + Duration::from_millis(1000), AlertKind::Offline, 2));
        assert!(p.admit(t0 + PERSON_GAP, AlertKind::Online, 2));
    }

    #[test]
    fn leaving_the_room_by_closing_the_app_is_one_sound() {
        let t0 = Instant::now();
        let mut p = AlertPolicy::default();
        // The hub sends the voice update, then the offline event, a few ms apart.
        assert!(p.admit(t0, AlertKind::Leave, 2));
        assert!(!p.admit(t0 + Duration::from_millis(3), AlertKind::Offline, 2));
        // Somebody else going offline is unaffected; so is the same person much later.
        assert!(p.admit(t0 + Duration::from_millis(500), AlertKind::Offline, 3));
        assert!(p.admit(t0 + Duration::from_secs(30), AlertKind::Offline, 2));
    }

    #[test]
    fn a_global_cap_stops_a_storm_and_recovers() {
        let t0 = Instant::now();
        let mut p = AlertPolicy::default();
        let kinds = [AlertKind::Join, AlertKind::Leave, AlertKind::Online, AlertKind::Offline, AlertKind::StreamStart, AlertKind::StreamStop];
        let mut played = 0;
        // Many different people doing different things in 2.4 s, each spaced past KIND_GAP.
        for i in 0..6u64 {
            if p.admit(t0 + Duration::from_millis(400 * i), kinds[i as usize], 100 + i as i64) {
                played += 1;
            }
        }
        assert_eq!(played, WINDOW_MAX);
        // Once the window has passed it plays again.
        assert!(p.admit(t0 + Duration::from_millis(400 * 5) + WINDOW, AlertKind::Join, 200));
    }
}
