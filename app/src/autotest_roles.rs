//! Item 12 autotest roles (a child module of `autotest`, so it drives the same `Auto` steps).
//!
//! - `mod-a`: the server owner: the hub's `owner` from `config.toml`, or, on a hub with no owner
//!   configured ("quem hospeda vira dono"), the first login from 127.0.0.1, which must therefore
//!   happen before B's (the launcher starts A first and waits for its "logged in" line). It
//!   logs in, sets its personal password the way
//!   the prompt does, opens "Configurações do servidor", then server-mutes member B, moves B to
//!   another voice channel and unmutes B, each time waiting for B's confirmation in the chat; at
//!   the end it checks the moderation log.
//! - `mod-b` (`--mode` of `mod-a` is B's name): a member in voice. It checks that its own UI
//!   follows: "Silenciado pelo servidor" (mic button disabled, red icon, the notice, and the voice
//!   session holding the mic back), the move into the other channel, and the unmute.
//!
//! Silent: no sound is played (`VOX_NO_PLAYBACK=1`); B's microphone is only captured.

use std::time::Duration;

use vox_proto::{ModAction, Role, UserId, AUDIT_MOVE, AUDIT_SERVER_MUTE, AUDIT_SERVER_UNMUTE};

use super::Auto;
use crate::app::App;
use crate::roles::AdminCmd;

/// The owner's personal password in these runs (fresh dev data dir each time).
const OWNER_PW: &str = "autotest-dono-12";

#[derive(Default)]
pub struct State {
    target: Option<UserId>,
    from: Option<i64>,
    to: Option<i64>,
    mic_checked: bool,
}

fn log(what: &str) {
    tracing::info!(target: "autotest", "AUTOTEST item12: {what}");
}

impl Auto {
    fn tag(&self, what: &str) -> String {
        format!("modtest {what} {}", self.args.run_id)
    }

    fn login_step(&mut self, app: &mut App) -> Option<Option<i32>> {
        if self.logged_in(app) {
            return Some(None);
        }
        if self.waited(30) || (app.form.error.is_some() && !app.form.busy) {
            return Some(self.fail(&format!("could not log in: {}", app.form.error.clone().unwrap_or_default())));
        }
        None
    }

    pub(super) fn run_mod_a(&mut self, app: &mut App) -> Option<i32> {
        let target_name = self.args.mode.clone().unwrap_or_else(|| "modb".into());
        match self.step {
            0 => {
                // The personal password is sent along: ignored while the name is still open.
                // (`--show login`: first without it, to see the form ask for it.)
                if self.args.show.as_deref() != Some("login") {
                    app.form.personal = OWNER_PW.into();
                }
                self.connect(app);
            }
            1 if self.args.show.as_deref() == Some("login") && app.form.need_personal => {
                if self.waited(6) {
                    log(&format!("login form asked for the personal password: {:?}", app.form.error));
                    app.form.personal = OWNER_PW.into();
                    app.connect();
                    self.args.show = None;
                    self.goto(1, "logging in with the personal password");
                }
            }
            1 if self.args.show.as_deref() == Some("prompt") && app.roles.account.is_some_and(|a| a.must_set_password) && !self.waited(6) => {}
            1 => match self.login_step(app) {
                Some(Some(code)) => return Some(code),
                Some(None) => {
                    if !app.roles.supported {
                        return self.fail("the hub does not know roles");
                    }
                    let me = app.me.clone().unwrap_or_else(|| vox_proto::User::new(0, ""));
                    log(&format!("logged in as {} ({:?}), account {:?}", me.username, me.role, app.roles.account));
                    if me.role != Role::Owner {
                        return self.fail("this name is not the hub's owner");
                    }
                    if app.roles.account.is_some_and(|a| a.must_set_password) {
                        if app.my_role() != Role::Member {
                            return self.fail("owner powers must stay off until the personal password is set");
                        }
                        // What the prompt's "Salvar senha" does.
                        app.set_personal_password(None, Some(OWNER_PW.into()));
                        self.goto(2, "setting the owner's personal password");
                    } else {
                        self.goto(3, "owner already protected");
                    }
                }
                None => {}
            },
            2 => {
                if let Some((false, e)) = &app.roles.pw_msg {
                    return self.fail(&format!("setting the personal password failed: {e}"));
                }
                if app.roles.account.is_some_and(|a| a.has_password && !a.must_set_password) {
                    self.goto(3, "personal password set");
                } else if self.waited(15) {
                    return self.fail("the personal password was not confirmed");
                }
            }
            3 => {
                if app.my_role() != Role::Owner {
                    return self.fail(&format!("expected owner powers, have {:?}", app.my_role()));
                }
                app.open_server_settings();
                self.goto(4, "server settings open");
            }
            4 => {
                if app.roles.members_loaded {
                    log(&format!("server settings: {} members, {} log lines", app.roles.members.len(), app.roles.audit.len()));
                    app.roles.server_open = false;
                    self.goto(5, "waiting for B in voice");
                } else if self.waited(10) {
                    return self.fail("members list did not load");
                }
            }
            5 => {
                let b = app.voice_members.values().find(|m| m.user.username.eq_ignore_ascii_case(&target_name) && m.channel_id.is_some()).cloned();
                if let Some(b) = b.as_ref().filter(|b| b.server_muted) {
                    // Still muted from an earlier run: start clean.
                    if self.item12.target.is_none() {
                        log("B is still server-muted from an earlier run: unmuting first");
                        self.item12.target = Some(b.user.id);
                        app.admin(AdminCmd::Mute { user: b.user.id, muted: false });
                    }
                } else if let Some(b) = b.filter(|_| Self::saw_message(app, &self.tag("ready"))) {
                    // Only once B says it is in voice and ready: a mute that lands while B's
                    // client is still joining would look like one left over from an earlier run.
                    let from = b.channel_id.unwrap_or_default();
                    let Some(to) = app.channels.iter().find(|c| c.kind == vox_proto::ChannelKind::Voice && c.id != from).map(|c| c.id) else {
                        return self.fail("needs two voice channels");
                    };
                    self.item12 = State { target: Some(b.user.id), from: Some(from), to: Some(to), mic_checked: false };
                    if !app.moderation_available(&b.user) {
                        return self.fail("no moderation menu for a member");
                    }
                    app.admin(AdminCmd::Mute { user: b.user.id, muted: true });
                    self.goto(6, "server-muted B");
                } else if self.waited(90) {
                    return self.fail(&format!("{target_name} never joined voice and said it was ready"));
                }
            }
            6 => {
                let id = self.item12.target.unwrap_or_default();
                let flagged = app.voice_members.get(&id).is_some_and(|m| m.server_muted);
                if flagged && Self::saw_message(app, &self.tag("muted-ok")) {
                    app.admin(AdminCmd::Move { user: id, channel: self.item12.to.unwrap_or_default() });
                    self.goto(7, "B confirmed the mute; moving B");
                } else if self.waited(40) {
                    return self.fail(&format!("B's mute not confirmed (flag seen: {flagged})"));
                }
            }
            7 => {
                let id = self.item12.target.unwrap_or_default();
                let there = app.voice_members.get(&id).is_some_and(|m| m.channel_id == self.item12.to && m.server_muted);
                if there && Self::saw_message(app, &self.tag("moved-ok")) {
                    app.admin(AdminCmd::Mute { user: id, muted: false });
                    self.goto(8, "B confirmed the move; unmuting B");
                } else if self.waited(60) {
                    return self.fail(&format!("B's move not confirmed (in the new channel, still muted: {there})"));
                }
            }
            8 => {
                let id = self.item12.target.unwrap_or_default();
                let free = app.voice_members.get(&id).is_some_and(|m| !m.server_muted);
                if free && Self::saw_message(app, &self.tag("unmuted-ok")) {
                    app.open_server_settings();
                    app.roles.tab = crate::roles::ServerTab::Audit;
                    self.goto(9, "B confirmed the unmute; reading the moderation log");
                } else if self.waited(40) {
                    return self.fail("B's unmute not confirmed");
                }
            }
            9 => {
                let me = app.my_id();
                let id = self.item12.target;
                let mine = |action: &str| {
                    app.roles.audit.iter().any(|e| e.action == action && e.actor.as_ref().map(|a| a.id) == me && e.target.as_ref().map(|t| t.id) == id)
                };
                if app.roles.audit_loaded && mine(AUDIT_SERVER_MUTE) && mine(AUDIT_MOVE) && mine(AUDIT_SERVER_UNMUTE) {
                    log(&format!("moderation log has the 3 actions ({} lines)", app.roles.audit.len()));
                    // A hub that does not pin the owner gave it to the first login from this PC,
                    // and said so in the log; one that pins it did not.
                    let pinned = app.roles.account.is_some_and(|a| a.owner_pinned);
                    let claimed = app.roles.audit.iter().any(|e| {
                        e.action == vox_proto::AUDIT_CLAIM_OWNER && e.detail == "pc-do-servidor" && e.actor.as_ref().map(|a| a.id) == me
                    });
                    if claimed == pinned {
                        return self.fail(&format!("owner claim: pinned by config = {pinned}, but the log says claimed = {claimed}"));
                    }
                    log(&format!("owner: {}", if pinned { "pinned in config.toml" } else { "claimed automatically from the server PC (audited)" }));
                    app.roles.server_open = false;
                    let done = self.tag("done");
                    Self::say(app, &done);
                    self.goto(10, "done; waiting a moment for B");
                } else if self.waited(10) {
                    return self.fail("the moderation log does not show the actions");
                } else if self.step_at.elapsed() > Duration::from_secs(2) && !app.roles.audit_loaded {
                    app.admin(AdminCmd::Audit { before: None });
                }
            }
            // `--show server`: leave "Configurações do servidor" up on each tab for a look.
            10 if self.waited(3) && self.args.show.as_deref() == Some("server") => {
                app.open_server_settings();
                app.roles.tab = crate::roles::ServerTab::Members;
                self.goto(11, "showing Membros");
            }
            10 if self.waited(3) => return self.pass(),
            11 if self.waited(6) => {
                app.roles.tab = crate::roles::ServerTab::Audit;
                self.goto(12, "showing Registro de moderação");
            }
            12 if self.waited(6) => {
                app.roles.tab = crate::roles::ServerTab::Channels;
                self.goto(13, "showing Canais");
            }
            13 if self.waited(6) => return self.pass(),
            _ => {}
        }
        None
    }

    pub(super) fn run_mod_b(&mut self, app: &mut App) -> Option<i32> {
        match self.step {
            0 => self.connect(app),
            1 => match self.login_step(app) {
                Some(Some(code)) => return Some(code),
                Some(None) => {
                    if app.me.as_ref().is_some_and(|u| u.role == Role::Owner) {
                        return self.fail("B became the owner: on a hub with no `owner`, A (mod-a) must log in from 127.0.0.1 first");
                    }
                    let Some(vc) = Self::first_voice(app) else { return self.fail("no voice channel") };
                    self.item12.from = Some(vc);
                    app.join_voice(vc);
                    self.goto(2, "joining voice");
                }
                None => {}
            },
            2 => {
                if app.voice.channel.is_some() {
                    if app.i_am_server_muted() {
                        // Left over from an earlier run (the mute is kept by the hub): A lifts it first.
                        if self.waited(60) {
                            return self.fail("still server-muted from before");
                        }
                        return None;
                    }
                    let ready = self.tag("ready");
                    Self::say(app, &ready);
                    self.goto(3, "in voice; told A I'm ready; waiting to be server-muted");
                } else if let Some(e) = app.voice.error.clone() {
                    return self.fail(&format!("voice join failed: {e}"));
                } else if self.waited(60) {
                    return self.fail("voice join took too long");
                }
            }
            3 => {
                let notice = app.notice.clone().unwrap_or_default();
                let told = app.roles.moderated.contains(&ModAction::ServerMute) && notice.contains("silenciou você");
                if app.i_am_server_muted() && told {
                    // With a microphone, the voice session itself holds it back (LiveKit refused it).
                    if !app.voice.no_mic && !app.roles.session_server_muted {
                        if self.waited(70) {
                            return self.fail("the voice session never reported the server mute");
                        }
                        return None;
                    }
                    log(&format!("UI shows 'Silenciado pelo servidor'; session mic held back: {} (no_mic={})", app.roles.session_server_muted, app.voice.no_mic));
                    let me = app.my_id().unwrap_or_default();
                    if !app.voice_members.get(&me).is_some_and(|m| m.server_muted) {
                        return self.fail("my member row is not marked server-muted");
                    }
                    if self.args.show.as_deref() == Some("muted") {
                        self.goto(30, "server mute seen; holding it on screen");
                        return None;
                    }
                    let tag = self.tag("muted-ok");
                    Self::say(app, &tag);
                    self.goto(4, "server mute seen; waiting to be moved");
                } else if self.waited(90) {
                    return self.fail(&format!("server mute not shown (flag={}, told={told})", app.i_am_server_muted()));
                }
            }
            4 => {
                let moved = app.voice.channel.is_some_and(|c| Some(c) != self.item12.from) && app.voice.joining.is_none();
                if moved && app.roles.moderated.iter().any(|a| matches!(a, ModAction::Move { .. })) {
                    // The hub re-announces me in the new channel a moment after the room is joined.
                    if !app.i_am_server_muted() {
                        if self.waited(70) {
                            return self.fail("the server mute did not follow me into the new channel");
                        }
                        return None;
                    }
                    log(&format!("moved to channel {:?}, notice: {:?}", app.voice.channel, app.notice));
                    let tag = self.tag("moved-ok");
                    Self::say(app, &tag);
                    self.goto(5, "moved; waiting for the unmute");
                } else if let Some(e) = app.voice.error.clone().filter(|_| self.waited(20)) {
                    return self.fail(&format!("voice error after the move: {e}"));
                } else if self.waited(60) {
                    return self.fail(&format!("not moved (channel {:?}, joining {:?})", app.voice.channel, app.voice.joining));
                }
            }
            5 => {
                if !app.i_am_server_muted() && app.roles.moderated.contains(&ModAction::ServerUnmute) {
                    if !self.item12.mic_checked {
                        self.item12.mic_checked = true;
                        log(&format!("unmuted; mic button enabled again (no_mic={})", app.voice.no_mic));
                    }
                    let tag = self.tag("unmuted-ok");
                    Self::say(app, &tag);
                    self.goto(6, "unmute seen; waiting for A");
                } else if self.waited(60) {
                    return self.fail("unmute not shown");
                }
            }
            6 => {
                if Self::saw_message(app, &self.tag("done")) {
                    app.leave_voice();
                    self.goto(7, "A is done");
                } else if self.waited(60) {
                    return self.fail("A never finished");
                }
            }
            7 if self.waited(2) => return self.pass(),
            // `--show muted`: keep the muted state on screen for a while, then go on.
            30 if self.waited(8) => {
                let tag = self.tag("muted-ok");
                Self::say(app, &tag);
                self.goto(4, "server mute seen; waiting to be moved");
            }
            _ => {}
        }
        None
    }
}
