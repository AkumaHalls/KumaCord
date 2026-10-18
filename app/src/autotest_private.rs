//! Private channels autotest (v0.9, a child module of `autotest`): `priv-a` / `priv-b` / `priv-c`.
//! A is the hub's owner (`dono`), B a member who is given access (`pvB<n>`), C a member who is not
//! (`pvC<n>`). Every step waits a second first (the hub's message rate limit). The three talk in
//! the first (public) text channel.
//!
//! 1. A creates a private text channel and a private voice channel for B (by name) from "🛠
//!    Servidor" (`AdminCmd::CreateChannel` with an access list): both show for A and B with the 🔒
//!    in the sidebar, and not at all for C.
//! 2. B writes in the private text channel and joins the private voice channel (LiveKit).
//! 3. C sees nothing: neither channel, none of B's message, B is not in voice for C; joining the
//!    voice channel by its id (A says the ids in the public channel) is refused by the hub.
//! 4. A takes B off both lists: the channels vanish for B live, B is taken out of the voice room
//!    at once (the hub removes B from LiveKit), and B drops everything it had of the text channel.
//!
//! Silent (`VOX_NO_PLAYBACK=1`, `VOX_NO_MIC=1`).

use vox_proto::{ChannelId, ChannelKind, PrivateAccess, UserId};

use super::Auto;
use crate::app::App;
use crate::roles::AdminCmd;

/// The owner's personal password in these runs (fresh dev data dir each time).
const PV_PW: &str = "autotest-dono-priv";
const TEXT_NAME: &str = "pv-cofre";
const VOICE_NAME: &str = "pv-voz";

#[derive(Default)]
pub struct State {
    geral: i64,
    b: Option<UserId>,
    text: Option<ChannelId>,
    voice: Option<ChannelId>,
}

fn log(what: &str) {
    tracing::info!(target: "autotest", "AUTOTEST priv: {what}");
}

impl Auto {
    fn pv_tag(&self, what: &str) -> String {
        format!("pvtest {what} {}", self.args.run_id)
    }

    fn pv_channel(app: &App, name: &str, kind: ChannelKind) -> Option<vox_proto::Channel> {
        app.channels.iter().find(|c| c.name == name && c.kind == kind).cloned()
    }

    /// The ids A wrote in the public channel ("pvtest ids <run> <text> <voice>").
    fn pv_ids(&self, app: &App) -> Option<(ChannelId, ChannelId)> {
        let prefix = format!("{} ", self.pv_tag("ids"));
        let m = app.logs.values().flat_map(|l| l.msgs.iter()).find(|m| m.content.starts_with(&prefix))?;
        let mut it = m.content[prefix.len()..].split_whitespace().map(|s| s.parse::<ChannelId>());
        Some((it.next()?.ok()?, it.next()?.ok()?))
    }

    /// The sidebar row of this channel was drawn with the lock.
    fn pv_locked_row(app: &App, c: &vox_proto::Channel) -> bool {
        let label = crate::app::channel_label(c, "");
        label.contains(crate::app::ICON_PRIVATE) && app.sidebar_rows.iter().any(|d| d.label == label)
    }

    // ------------------------------------------------------------ A (the owner)

    pub(super) fn run_priv_a(&mut self, app: &mut App) -> Option<i32> {
        if self.step >= 4 && !self.waited(1) {
            return None;
        }
        match self.step {
            0 => {
                app.form.personal = PV_PW.into();
                self.connect(app);
            }
            1 => {
                if !self.logged_in(app) {
                    return if self.waited(30) { self.fail(&format!("could not log in: {:?}", app.form.error)) } else { None };
                }
                if app.me.as_ref().map(|u| u.role) != Some(vox_proto::Role::Owner) {
                    return self.fail("this name is not the hub's owner");
                }
                if !app.chatx.features.private_channels {
                    return self.fail("the hub does not announce private channels");
                }
                if app.roles.account.is_some_and(|a| a.must_set_password) {
                    app.set_personal_password(None, Some(PV_PW.into()));
                    self.goto(2, "setting the owner's personal password");
                } else {
                    self.goto(3, "owner already protected");
                }
            }
            2 => {
                if app.roles.account.is_some_and(|a| a.has_password && !a.must_set_password) {
                    self.goto(3, "personal password set");
                } else if self.waited(15) {
                    return self.fail(&format!("the personal password was not set: {:?}", app.roles.pw_msg));
                }
            }
            3 => {
                let b = app.online.iter().find(|u| u.username.starts_with("pvB")).map(|u| u.id);
                let c = app.online.iter().any(|u| u.username.starts_with("pvC"));
                match (Self::first_text(app), b, c) {
                    (Some(ch), Some(b), true) => {
                        (self.pv.geral, self.pv.b) = (ch, Some(b));
                        app.select_text(ch);
                        self.goto(4, "B and C are online; waiting for them to be ready");
                    }
                    _ if self.waited(60) => return self.fail("B or C never came online"),
                    _ => {}
                }
            }
            4 => {
                if !Self::saw_message(app, &self.pv_tag("ready-b")) || !Self::saw_message(app, &self.pv_tag("ready-c")) {
                    return if self.waited(60) { self.fail("B or C never got ready") } else { None };
                }
                let access = Some(PrivateAccess { roles: vec![], users: self.pv.b.into_iter().collect() });
                app.admin(AdminCmd::CreateChannel { name: TEXT_NAME.into(), kind: ChannelKind::Text, private: access.clone() });
                app.admin(AdminCmd::CreateChannel { name: VOICE_NAME.into(), kind: ChannelKind::Voice, private: access });
                self.goto(5, "part 1: creating a private text and a private voice channel for B");
            }
            5 => {
                let (Some(t), Some(v)) = (Self::pv_channel(app, TEXT_NAME, ChannelKind::Text), Self::pv_channel(app, VOICE_NAME, ChannelKind::Voice)) else {
                    return if self.waited(20) { self.fail("the private channels never showed") } else { None };
                };
                if t.private.is_none() || v.private.is_none() {
                    return self.fail("the new channels are not private");
                }
                if !Self::pv_locked_row(app, &t) || !Self::pv_locked_row(app, &v) {
                    return if self.waited(10) { self.fail("the sidebar shows no lock on the private channels") } else { None };
                }
                (self.pv.text, self.pv.voice) = (Some(t.id), Some(v.id));
                log(&format!("private channels {} (text) and {} (voice) for B; the sidebar shows the lock", t.id, v.id));
                Self::say(app, &format!("{} {} {}", self.pv_tag("ids"), t.id, v.id));
                self.goto(6, "waiting for B to chat and join the private voice channel");
            }
            6 => {
                let (t, v) = (self.pv.text.unwrap_or_default(), self.pv.voice.unwrap_or_default());
                if !Self::saw_message(app, &self.pv_tag("b-in")) {
                    return if self.waited(120) { self.fail("B never got into the private channels") } else { None };
                }
                // B's message in the private channel reaches me (staff always has access).
                app.select_text(t);
                let b = self.pv.b;
                if !app.logs.get(&t).is_some_and(|l| l.msgs.iter().any(|m| m.content.starts_with(&self.pv_tag("segredo")))) {
                    return if self.waited(20) { self.fail("B's message in the private channel did not reach the owner") } else { None };
                }
                if !app.voice_members.values().any(|m| Some(m.user.id) == b && m.channel_id == Some(v)) {
                    return if self.waited(20) { self.fail("B is not listed in the private voice channel") } else { None };
                }
                app.select_text(self.pv.geral);
                self.goto(7, "B chats in the private channel and is in its voice room; waiting for C's checks");
            }
            7 => {
                if !Self::saw_message(app, &self.pv_tag("c-ok")) {
                    return if self.waited(90) { self.fail("C never finished its checks") } else { None };
                }
                // Part 4: nobody named any more (only Dono and Admin).
                let (t, v) = (self.pv.text.unwrap_or_default(), self.pv.voice.unwrap_or_default());
                app.admin(AdminCmd::SetChannelAccess { id: v, private: Some(PrivateAccess::default()) });
                app.admin(AdminCmd::SetChannelAccess { id: t, private: Some(PrivateAccess::default()) });
                self.goto(8, "part 4: B taken off both lists");
            }
            8 => {
                let (b, v) = (self.pv.b, self.pv.voice.unwrap_or_default());
                if app.voice_members.values().any(|m| Some(m.user.id) == b && m.channel_id == Some(v)) {
                    return if self.waited(15) { self.fail("B is still listed in the private voice channel") } else { None };
                }
                if !Self::saw_message(app, &self.pv_tag("b-out")) {
                    return if self.waited(60) { self.fail("B never confirmed it lost access") } else { None };
                }
                log("B left the private voice room live and lost the channels");
                return self.pass();
            }
            _ => {}
        }
        None
    }

    // ------------------------------------------------------------ B (given access)

    pub(super) fn run_priv_b(&mut self, app: &mut App) -> Option<i32> {
        if self.step >= 3 && !self.waited(1) {
            return None;
        }
        match self.step {
            0 => self.connect(app),
            1 => {
                if !self.logged_in(app) {
                    return if self.waited(30) { self.fail(&format!("could not log in: {:?}", app.form.error)) } else { None };
                }
                let owner = app.online.iter().any(|u| u.username == "dono");
                match Self::first_text(app) {
                    Some(ch) if owner && self.waited(2) => {
                        self.pv.geral = ch;
                        app.select_text(ch);
                        self.goto(2, "A is online");
                    }
                    _ if self.waited(60) => return self.fail("A never came online"),
                    _ => {}
                }
            }
            2 => {
                Self::say(app, &self.pv_tag("ready-b"));
                self.goto(3, "ready; waiting for the private channels");
            }
            3 => {
                let (Some(t), Some(v)) = (Self::pv_channel(app, TEXT_NAME, ChannelKind::Text), Self::pv_channel(app, VOICE_NAME, ChannelKind::Voice)) else {
                    return if self.waited(60) { self.fail("the private channels never showed for me") } else { None };
                };
                if !Self::pv_locked_row(app, &t) || !Self::pv_locked_row(app, &v) {
                    return if self.waited(10) { self.fail("my sidebar shows no lock on the private channels") } else { None };
                }
                (self.pv.text, self.pv.voice) = (Some(t.id), Some(v.id));
                app.select_text(t.id);
                Self::say(app, &self.pv_tag("segredo"));
                app.join_voice(v.id);
                self.goto(4, "part 2: wrote in the private channel, joining its voice room");
            }
            4 => {
                let (t, v) = (self.pv.text.unwrap_or_default(), self.pv.voice.unwrap_or_default());
                if let Some(e) = app.voice.error.clone() {
                    return self.fail(&format!("joining the private voice channel failed: {e}"));
                }
                if app.voice.channel != Some(v) {
                    return if self.waited(90) { self.fail("never got into the private voice room") } else { None };
                }
                if !app.logs.get(&t).is_some_and(|l| l.msgs.iter().any(|m| m.content.starts_with(&self.pv_tag("segredo")))) {
                    return if self.waited(20) { self.fail("my message in the private channel did not come back") } else { None };
                }
                app.select_text(self.pv.geral);
                Self::say(app, &self.pv_tag("b-in"));
                self.goto(5, "in the private voice room; waiting to lose access");
            }
            5 => {
                let (t, v) = (self.pv.text.unwrap_or_default(), self.pv.voice.unwrap_or_default());
                let listed = app.channels.iter().any(|c| c.id == t || c.id == v);
                if listed || app.voice.channel.is_some() || app.voice.joining.is_some() {
                    return if self.waited(120) { self.fail("still have the private channels (or their voice room) after A took me off") } else { None };
                }
                if app.logs.contains_key(&t) || app.unread.contains_key(&t) {
                    return self.fail("the private channel's messages are still cached");
                }
                if app.voice_members.values().any(|m| m.channel_id == Some(v)) {
                    return self.fail("I still see people in the private voice channel");
                }
                log(&format!("lost access live: channels gone, out of the voice room, notice {:?}", app.notice));
                app.select_text(self.pv.geral);
                Self::say(app, &self.pv_tag("b-out"));
                self.goto(6, "done");
            }
            6 if self.waited(2) => return self.pass(),
            _ => {}
        }
        None
    }

    // ------------------------------------------------------------ C (no access)

    pub(super) fn run_priv_c(&mut self, app: &mut App) -> Option<i32> {
        if self.step >= 3 && !self.waited(1) {
            return None;
        }
        match self.step {
            0 => self.connect(app),
            1 => {
                if !self.logged_in(app) {
                    return if self.waited(30) { self.fail(&format!("could not log in: {:?}", app.form.error)) } else { None };
                }
                let owner = app.online.iter().any(|u| u.username == "dono");
                match Self::first_text(app) {
                    Some(ch) if owner && self.waited(2) => {
                        self.pv.geral = ch;
                        app.select_text(ch);
                        self.goto(2, "A is online");
                    }
                    _ if self.waited(60) => return self.fail("A never came online"),
                    _ => {}
                }
            }
            2 => {
                Self::say(app, &self.pv_tag("ready-c"));
                self.goto(3, "ready; waiting for B to be inside");
            }
            3 => {
                let Some((t, v)) = self.pv_ids(app) else {
                    return if self.waited(90) { self.fail("A never said the channel ids") } else { None };
                };
                if !Self::saw_message(app, &self.pv_tag("b-in")) {
                    return if self.waited(120) { self.fail("B never got in") } else { None };
                }
                (self.pv.text, self.pv.voice) = (Some(t), Some(v));
                // Part 3: nothing of it here.
                let mut leaks = Vec::new();
                if app.channels.iter().any(|c| c.id == t || c.id == v || c.name.starts_with("pv-")) {
                    leaks.push("the channels are listed");
                }
                if app.logs.contains_key(&t) || app.unread.contains_key(&t) || app.chatx.mention_unread.contains_key(&t) {
                    leaks.push("messages or counters of the private channel");
                }
                if Self::saw_message(app, &self.pv_tag("segredo")) {
                    leaks.push("B's private message");
                }
                if app.voice_members.values().any(|m| m.channel_id == Some(v) || (m.user.username.starts_with("pvB") && m.channel_id.is_some())) {
                    leaks.push("B is in voice somewhere");
                }
                if !leaks.is_empty() {
                    return self.fail(&format!("a member without access sees: {}", leaks.join(", ")));
                }
                log("nothing of the private channels here; trying to join the voice channel by its id");
                app.join_voice(v);
                self.goto(4, "joining the private voice channel by id (must be refused)");
            }
            4 => {
                let v = self.pv.voice.unwrap_or_default();
                if app.voice.channel == Some(v) {
                    return self.fail("joined a private voice channel without access");
                }
                let Some(e) = app.voice.error.clone() else {
                    return if self.waited(60) { self.fail("the join was neither refused nor accepted") } else { None };
                };
                log(&format!("voice join refused: {e}"));
                app.select_text(self.pv.geral);
                Self::say(app, &self.pv_tag("c-ok"));
                self.goto(5, "refused; waiting for the end");
            }
            5 => {
                if !Self::saw_message(app, &self.pv_tag("b-out")) {
                    return if self.waited(120) { self.fail("B never finished") } else { None };
                }
                // Still nothing private here, after all of it.
                let (t, v) = (self.pv.text.unwrap_or_default(), self.pv.voice.unwrap_or_default());
                if app.channels.iter().any(|c| c.id == t || c.id == v) || app.logs.contains_key(&t) {
                    return self.fail("the private channels showed up at the end");
                }
                return self.pass();
            }
            _ => {}
        }
        None
    }
}
