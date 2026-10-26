//! v1.0 navigation and the center modes (phase 2a, docs/PLANO-v1.0.md "Navegação como no
//! Discord"), a child module of `autotest`: `nav-a` (the owner, "dono") checks, `nav-b` is the
//! other person in the voice room (camera on) and answers on cue. With `VOX_AUTOTEST_SHOT_DIR`,
//! `nav-a` also saves the screens for design review (`tools\shots.ps1`).
//!
//! 1. A makes two more text channels (and a private one), says hello in #geral; B reacts, replies,
//!    mentions A and says it is ready. Only "# geral" is highlighted (shot `sidebar-chat`).
//! 2. A joins the voice room: the stage is open, only the room is highlighted, the text channel
//!    loses it; the remembered mode (cards with the chat docked) shows (shot `mode-cards-chat`).
//! 3. The three modes from the header buttons: only chat (the channel highlighted again, shot
//!    `mode-chat`), only cards (no chat on screen, shot `mode-cards`), cards with the chat (the
//!    docked channel is never highlighted). In only chat, the idle check (phase 3b): 8 s with
//!    nothing changing draw at most a few frames, and the meter's repaint thread is off.
//! 4. Docked chat read marking: with #geral docked and the window in front, B's message there
//!    arrives read (no counter); with #memes docked instead, the next one counts (the control);
//!    docking #geral again reads it.
//! 5. text → voice → text from the sidebar: each time only what the center shows is highlighted.
//! 6. The hover bar over B's message with reactions (shot `hover-reactions`).
//! 7. A DM with B: its row is highlighted while open, not once the stage is in focus (shot `dm`).
//!
//! Cues go through #memes (never on screen in the shots). Every step lingers a moment (the hub's
//! message rate limit, and the sidebar rows are the ones drawn in the last frame).

use std::time::{Duration, Instant};

use vox_proto::{ChannelKind, Message, PrivateAccess};

use super::Auto;
use crate::app::chat_extras::Target;
use crate::app::{channel_label, App};
use crate::layout::{CenterMode, Open};
use crate::roles::AdminCmd;

const JOGATINA: &str = "jogatina-de-sexta";
const MEMES: &str = "memes";
const ADM: &str = "administração";
/// A's opening lines in #geral, and B's answers.
const HELLO: &str = "alguém topa a ranqueada depois do jantar?";
const A_2: &str = "vou abrir a sala de voz daqui a pouco";
const B_REPLY: &str = "topo! só termino essa build aqui";
const B_MENTION: &str = "abre a sala do foco pra gente depois?";
const B_PONG1: &str = "chegando aí em 5 min";
const B_PONG2: &str = "alguém viu o placar de ontem?";
const DM_A: &str = "oi! você vem hoje?";
const DM_B: &str = "vou sim, já já entro na sala";
/// The idle check (phase 3b): how long, and the frames allowed in it (the check's own wake-up, a
/// clock line now and then; a repaint loop draws hundreds).
const QUIET_SECS: u64 = 8;
const QUIET_MAX_FRAMES: u64 = 16;
/// The owner's personal password (test value; the hub is a throwaway one).
const NAV_PW: &str = "nav-dono-123";

#[derive(Default)]
pub struct State {
    geral: i64,
    jogatina: i64,
    memes: i64,
    voice: i64,
    peer: i64,
    dm: i64,
    created: bool,
    sent: u32,
    /// When the current step started lingering (a `Cell`: read in match guards).
    at: std::cell::Cell<Option<Instant>>,
}

fn log(what: &str) {
    tracing::info!(target: "autotest", "AUTOTEST nav: {what}");
}

fn channel_id(app: &App, name: &str, kind: ChannelKind) -> Option<i64> {
    app.channels.iter().find(|c| c.name == name && c.kind == kind).map(|c| c.id)
}

fn message<'a>(app: &'a App, ch: i64, text: &str) -> Option<&'a Message> {
    // (B's mention starts with "@dono ".)
    app.logs.get(&ch).and_then(|l| l.msgs.iter().find(|m| m.content.ends_with(text)))
}

impl Auto {
    fn nav_shot(&self, app: &App, name: &str) {
        super::shots::save(app, name);
    }

    /// Wait `secs` in the current step (true once over).
    fn nav_lingered(&self, secs: f32) -> bool {
        let at = self.nav.at.get().unwrap_or_else(Instant::now);
        self.nav.at.set(Some(at));
        at.elapsed() >= Duration::from_secs_f32(secs)
    }

    fn nav_next(&mut self, step: u32, what: &str) {
        self.nav.at.set(None);
        self.goto(step, what);
    }

    /// The label of a channel's sidebar row.
    fn nav_label(app: &App, id: i64) -> String {
        app.channels.iter().find(|c| c.id == id).map(|c| channel_label(c, "")).unwrap_or_default()
    }

    /// Exactly one sidebar row drawn highlighted, and it is `label`.
    fn nav_only_highlighted(app: &App, label: &str) -> Result<(), String> {
        let on: Vec<&str> = app.sidebar_rows.iter().filter(|d| d.selected).map(|d| d.label.as_str()).collect();
        if on == [label] {
            Ok(())
        } else {
            Err(format!("highlighted rows {on:?}, expected only {label:?} (open: {:?}, mode: {:?})", app.open_in_center(), app.effective_mode()))
        }
    }

    fn nav_cue(&mut self, app: &mut App, text: &str) {
        app.backend.send(crate::backend::Cmd::SendMessage { channel: self.nav.memes, content: text.to_string(), extras: Default::default() });
    }

    // ------------------------------------------------------------ A

    pub(super) fn run_nav_a(&mut self, app: &mut App) -> Option<i32> {
        let shots = std::env::var_os("VOX_AUTOTEST_SHOT_DIR").is_some();
        match self.step {
            0 => {
                if self.nav_lingered(if shots { 3.0 } else { 0.5 }) {
                    self.nav_shot(app, "home");
                    app.form.personal = NAV_PW.into();
                    self.connect(app);
                    self.nav.at.set(None);
                }
            }
            1 => {
                if !self.logged_in(app) {
                    return if self.waited(30) { self.fail("could not log in") } else { None };
                }
                // The owner sets its personal password first (the hub asks it before any admin action).
                if app.roles.account.is_some_and(|a| a.must_set_password) {
                    if !self.nav.created {
                        self.nav.created = true;
                        app.set_personal_password(None, Some(NAV_PW.into()));
                    }
                    return if self.waited(20) { self.fail(&format!("the personal password was not set: {:?}", app.roles.pw_msg)) } else { None };
                }
                let (Some(geral), Some(voice)) = (Self::first_text(app), Self::first_voice(app)) else { return self.fail("no channels") };
                (self.nav.geral, self.nav.voice) = (geral, voice);
                if self.nav.sent == 0 {
                    self.nav.sent = 1;
                    for (name, private) in [(JOGATINA, None), (MEMES, None), (ADM, Some(PrivateAccess::default()))] {
                        if channel_id(app, name, ChannelKind::Text).is_none() {
                            app.admin(AdminCmd::CreateChannel { name: name.into(), kind: ChannelKind::Text, private });
                        }
                    }
                }
                match (channel_id(app, JOGATINA, ChannelKind::Text), channel_id(app, MEMES, ChannelKind::Text), channel_id(app, ADM, ChannelKind::Text)) {
                    (Some(j), Some(m), Some(_)) => {
                        (self.nav.jogatina, self.nav.memes) = (j, m);
                        self.nav.sent = 0;
                        app.select_text(geral);
                        app.notify.focus_override = Some(true);
                        self.nav_next(2, "channels ready; saying hello in #geral");
                    }
                    _ if self.waited(20) => return self.fail("could not create the channels"),
                    _ => {}
                }
            }
            2 => {
                if self.nav_lingered(1.0) {
                    Self::say(app, HELLO);
                    self.nav_next(3, "waiting for B's answers");
                }
            }
            3 => {
                let ready = channel_id(app, MEMES, ChannelKind::Text).and_then(|m| app.logs.get(&m)).is_some_and(|l| l.msgs.iter().any(|m| m.content == "navready"));
                let answered = message(app, self.nav.geral, B_MENTION).is_some();
                if !(ready && answered) {
                    return if self.waited(90) { self.fail("B never answered") } else { None };
                }
                if !self.nav_lingered(1.5) {
                    return None;
                }
                if let Some(peer) = message(app, self.nav.geral, B_REPLY).map(|m| m.author.id) {
                    self.nav.peer = peer;
                }
                Self::say(app, A_2);
                self.nav_next(4, "#geral open");
            }
            4 if self.nav_lingered(2.0) => {
                let label = Self::nav_label(app, self.nav.geral);
                if app.open_in_center() != Open::Text(self.nav.geral) {
                    return self.fail(&format!("#geral is not open: {:?}", app.open_in_center()));
                }
                if let Err(e) = Self::nav_only_highlighted(app, &label) {
                    return self.fail(&e);
                }
                if app.unread.get(&self.nav.geral).copied().unwrap_or(0) != 0 {
                    return self.fail("#geral is open but counts unread messages");
                }
                self.nav_shot(app, "sidebar-chat");
                log("text channel open: only its row is highlighted");
                app.join_voice(self.nav.voice);
                self.nav_next(5, "joining the voice room");
            }
            5 => {
                if app.voice.channel != Some(self.nav.voice) || app.members_here().len() < 2 {
                    return if self.waited(60) { self.fail("could not join the voice room with B") } else { None };
                }
                if !self.nav_lingered(4.0) {
                    return None;
                }
                let room = Self::nav_label(app, self.nav.voice);
                if app.open_in_center() != Open::Stage(self.nav.voice) {
                    return self.fail(&format!("joining did not open the stage: {:?}", app.open_in_center()));
                }
                if let Err(e) = Self::nav_only_highlighted(app, &room) {
                    return self.fail(&format!("stage in focus: {e}"));
                }
                if app.effective_mode() != app.settings.layout.center_mode && app.settings.layout.center_mode != CenterMode::Chat {
                    return self.fail(&format!("the remembered mode {:?} is not the one shown ({:?})", app.settings.layout.center_mode, app.effective_mode()));
                }
                // Start from the docked chat (the mockup's main screen).
                app.set_center_mode(CenterMode::CardsWithChat);
                self.nav_next(6, "stage open; cards with the chat docked");
            }
            6 if self.nav_lingered(3.0) => {
                let room = Self::nav_label(app, self.nav.voice);
                if app.docked_channel() != Some(self.nav.geral) || app.chat_on_screen() != Some(self.nav.geral) {
                    return self.fail(&format!("the docked chat is not #geral: {:?} / {:?}", app.docked_channel(), app.chat_on_screen()));
                }
                if let Err(e) = Self::nav_only_highlighted(app, &room) {
                    return self.fail(&format!("docked chat: {e}"));
                }
                self.nav_shot(app, "mode-cards-chat");
                app.set_center_mode(CenterMode::Chat);
                self.nav_next(7, "mode: only the chat");
            }
            7 if self.nav_lingered(2.0) => {
                if app.effective_mode() != CenterMode::Chat || app.open_in_center() != Open::Text(self.nav.geral) {
                    return self.fail(&format!("only chat: mode {:?}, open {:?}", app.effective_mode(), app.open_in_center()));
                }
                if let Err(e) = Self::nav_only_highlighted(app, &Self::nav_label(app, self.nav.geral)) {
                    return self.fail(&format!("only chat: {e}"));
                }
                if app.settings.layout.center_mode != CenterMode::Chat {
                    return self.fail("the mode was not remembered");
                }
                self.nav_shot(app, "mode-chat");
                // Phase 3b: the chat alone in the center during a call, nothing changing: the app
                // must stay at rest (a repaint loop once cost 22% of a core here, docs/perf-v1.md).
                self.quiet_start(app, QUIET_SECS);
                self.nav_next(70, "idle check: only the chat in a call, nothing changing");
            }
            70 => {
                let frames = self.quiet_done(app)?;
                let watch = crate::repaint_watch::snapshot();
                log(&format!("idle check: {frames} frames in {QUIET_SECS} s (busy windows so far {}, last causes {:?}), meter thread {}", watch.busy_windows, watch.last_causes, app.meter_watch_running()));
                if frames > QUIET_MAX_FRAMES {
                    return self.fail(&format!("the chat-only center repaints without a reason: {frames} frames in {QUIET_SECS} s (causes {:?})", watch.last_causes));
                }
                if app.meter_watch_running() {
                    return self.fail("the microphone meter's repaint thread runs while no meter is on screen (only the chat)");
                }
                app.set_center_mode(CenterMode::Cards);
                self.nav_next(8, "mode: only the cards");
            }
            8 if self.nav_lingered(2.0) => {
                if app.effective_mode() != CenterMode::Cards || app.chat_on_screen().is_some() {
                    return self.fail(&format!("only cards: mode {:?}, chat on screen {:?}", app.effective_mode(), app.chat_on_screen()));
                }
                if let Err(e) = Self::nav_only_highlighted(app, &Self::nav_label(app, self.nav.voice)) {
                    return self.fail(&format!("only cards: {e}"));
                }
                self.nav_shot(app, "mode-cards");
                app.set_center_mode(CenterMode::CardsWithChat);
                self.nav_next(9, "cards with the chat again; B writes in the docked #geral");
            }
            9 if self.nav_lingered(1.5) => {
                if app.chat_on_screen() != Some(self.nav.geral) {
                    return self.fail("#geral is not docked");
                }
                self.nav_cue(app, "navping1");
                self.nav_next(10, "waiting for B's line in the docked chat");
            }
            10 => {
                if message(app, self.nav.geral, B_PONG1).is_none() {
                    return if self.waited(30) { self.fail("B's line never came") } else { None };
                }
                if !self.nav_lingered(1.0) {
                    return None;
                }
                let n = app.unread.get(&self.nav.geral).copied().unwrap_or(0);
                if n != 0 {
                    return self.fail(&format!("a message in the visible docked chat counted as unread ({n})"));
                }
                log("a message arriving in the visible docked chat counts as read");
                app.dock_channel(self.nav.memes);
                self.nav_next(11, "#memes docked instead: the control");
            }
            11 if self.nav_lingered(1.5) => {
                if app.chat_on_screen() != Some(self.nav.memes) {
                    return self.fail("#memes did not take the dock");
                }
                self.nav_cue(app, "navping2");
                self.nav_next(12, "waiting for B's second line (#geral not on screen)");
            }
            12 => {
                if message(app, self.nav.geral, B_PONG2).is_none() {
                    return if self.waited(30) { self.fail("B's second line never came") } else { None };
                }
                if !self.nav_lingered(1.0) {
                    return None;
                }
                let n = app.unread.get(&self.nav.geral).copied().unwrap_or(0);
                if n == 0 {
                    return self.fail("a message in a channel nobody sees did not count as unread");
                }
                app.dock_channel(self.nav.geral);
                self.nav_next(13, "#geral docked again");
            }
            13 if self.nav_lingered(1.5) => {
                if app.unread.get(&self.nav.geral).copied().unwrap_or(0) != 0 {
                    return self.fail("docking #geral again did not read it");
                }
                log("docked chat read marking: visible = read, hidden = unread, shown again = read");
                // text → voice → text from the sidebar.
                app.select_text(self.nav.jogatina);
                self.nav_next(14, "sidebar: a text channel");
            }
            14 if self.nav_lingered(1.0) => {
                if app.open_in_center() != Open::Text(self.nav.jogatina) {
                    return self.fail(&format!("text: {:?}", app.open_in_center()));
                }
                if let Err(e) = Self::nav_only_highlighted(app, &Self::nav_label(app, self.nav.jogatina)) {
                    return self.fail(&format!("text: {e}"));
                }
                app.join_voice(self.nav.voice);
                self.nav_next(15, "sidebar: the voice room I am in");
            }
            15 if self.nav_lingered(1.0) => {
                if app.open_in_center() != Open::Stage(self.nav.voice) {
                    return self.fail(&format!("voice: {:?}", app.open_in_center()));
                }
                if let Err(e) = Self::nav_only_highlighted(app, &Self::nav_label(app, self.nav.voice)) {
                    return self.fail(&format!("voice: {e}"));
                }
                app.select_text(self.nav.geral);
                self.nav_next(16, "sidebar: the text channel again");
            }
            16 if self.nav_lingered(1.0) => {
                if app.open_in_center() != Open::Text(self.nav.geral) {
                    return self.fail(&format!("text again: {:?}", app.open_in_center()));
                }
                if let Err(e) = Self::nav_only_highlighted(app, &Self::nav_label(app, self.nav.geral)) {
                    return self.fail(&format!("text again: {e}"));
                }
                log("text → voice → text: only what the center shows is highlighted");
                // Reactions on B's reply, and its hover bar.
                let Some(reply) = message(app, self.nav.geral, B_REPLY).map(|m| m.id) else { return self.fail("B's reply is gone") };
                app.react(Target::Channel(self.nav.geral), reply, "👍", true);
                app.chatx.force_hover = Some(reply);
                self.nav_next(17, "reacting to B's reply");
            }
            17 => {
                let Some(m) = message(app, self.nav.geral, B_REPLY) else { return self.fail("B's reply is gone") };
                let me = app.my_id().unwrap_or_default();
                let mine = m.reactions.iter().any(|r| r.emoji == "👍" && r.users.contains(&me));
                let theirs = m.reactions.iter().any(|r| r.emoji == "👍" && r.users.contains(&self.nav.peer));
                if !(mine && theirs) {
                    return if self.waited(20) { self.fail(&format!("the reactions did not arrive: {:?}", m.reactions)) } else { None };
                }
                if !self.nav_lingered(2.0) {
                    return None;
                }
                self.nav_shot(app, "hover-reactions");
                app.chatx.force_hover = None;
                app.open_dm(self.nav.peer);
                self.nav_next(18, "a DM with B");
            }
            18 => {
                let Some(dm) = app.dms.selected else {
                    return if self.waited(20) { self.fail("the DM did not open") } else { None };
                };
                self.nav.dm = dm;
                if self.nav.sent == 0 && self.nav_lingered(1.0) {
                    self.nav.sent = 1;
                    app.dms.input = DM_A.into();
                    app.send_dm();
                }
                let answered = app.dms.logs.get(&dm).is_some_and(|l| l.msgs.iter().any(|m| m.content == DM_B));
                if !answered {
                    return if self.waited(30) { self.fail("B never answered the DM") } else { None };
                }
                self.nav_next(19, "DM answered");
            }
            19 if self.nav_lingered(2.0) => {
                let peer = app.dms.conv(self.nav.dm).map(|c| c.peer.shown_name().to_string()).unwrap_or_default();
                if app.open_in_center() != Open::Dm(self.nav.dm) {
                    return self.fail(&format!("DM: {:?}", app.open_in_center()));
                }
                if let Err(e) = Self::nav_only_highlighted(app, &peer) {
                    return self.fail(&format!("DM: {e}"));
                }
                self.nav_shot(app, "dm");
                app.show_call();
                self.nav_next(20, "the stage over the DM");
            }
            20 if self.nav_lingered(1.0) => {
                if let Err(e) = Self::nav_only_highlighted(app, &Self::nav_label(app, self.nav.voice)) {
                    return self.fail(&format!("stage after a DM: {e}"));
                }
                log("DMs follow the same rule");
                self.nav_cue(app, "navdone");
                self.nav_next(21, "done");
            }
            21 if self.nav_lingered(1.0) => {
                if !shots {
                    return self.pass();
                }
                // (Screenshots only, v1.0 phase 4: the home screen with saved servers. Two made-up
                // entries next to this hub's own; their addresses are closed local ports, so the
                // online check finds them offline without touching anything.)
                app.switch_server();
                let now = crate::app::now_ms();
                for (id, name, user, port, ago_h) in [(9001, "Jogatina de sexta", "dono", 7995, 26), (9002, "Banda do bairro", "lua", 7996, 24 * 9)] {
                    app.settings.servers.servers.push(crate::servers::SavedServer {
                        id,
                        url: format!("http://127.0.0.1:{port}"),
                        username: user.into(),
                        server_name: name.into(),
                        last_used_ms: now - ago_h * 3_600_000,
                        ..Default::default()
                    });
                }
                self.nav_next(22, "the saved servers");
            }
            22 if self.nav_lingered(4.0) => {
                self.nav_shot(app, "home-servidores");
                return self.pass();
            }
            _ => {}
        }
        None
    }

    // ------------------------------------------------------------ B

    pub(super) fn run_nav_b(&mut self, app: &mut App) -> Option<i32> {
        let cue = |app: &App, memes: i64, text: &str| app.logs.get(&memes).is_some_and(|l| l.msgs.iter().any(|m| m.content == text));
        match self.step {
            0 => self.connect(app),
            1 => {
                if !self.logged_in(app) {
                    return if self.waited(30) { self.fail("could not log in") } else { None };
                }
                let owner = app.online.iter().find(|u| u.username == "dono").map(|u| u.id);
                let (Some(geral), Some(voice), Some(memes), Some(owner)) = (Self::first_text(app), Self::first_voice(app), channel_id(app, MEMES, ChannelKind::Text), owner) else {
                    return if self.waited(90) { self.fail("A or its channels never showed up") } else { None };
                };
                (self.nav.geral, self.nav.voice, self.nav.memes, self.nav.peer) = (geral, voice, memes, owner);
                app.select_text(memes);
                app.select_text(geral);
                app.join_voice(voice);
                self.nav_next(2, "in the room; waiting for A's hello");
            }
            2 => {
                if app.voice.channel == Some(self.nav.voice) && app.voice.camera == crate::app::CameraUi::Off && self.nav.sent == 0 {
                    app.start_camera();
                    self.nav.sent = 1;
                }
                let Some(hello) = message(app, self.nav.geral, HELLO).map(|m| m.id) else {
                    return if self.waited(90) { self.fail("A's hello never came") } else { None };
                };
                if !app.chatx.people.contains_key(&self.nav.peer) || !self.nav_lingered(1.0) {
                    return None;
                }
                let target = Target::Channel(self.nav.geral);
                app.react(target, hello, "🔥", true);
                app.start_reply(target, hello);
                if let Some(r) = app.chatx.replying.get_mut(&target) {
                    r.ping = false;
                }
                Self::say(app, B_REPLY);
                self.nav_next(3, "replied; now the mention");
            }
            3 if self.nav_lingered(1.2) => {
                let target = Target::Channel(self.nav.geral);
                app.input = "@dono".into();
                let cursor = app.input.len();
                let Some((at, list)) = app.mention_list(target, &app.input, cursor) else { return self.fail("no member list for @dono") };
                let Some(c) = list.into_iter().find(|c| c.id == Some(self.nav.peer)) else { return self.fail("A is not in the member list") };
                app.pick_mention(target, at, cursor, &c);
                app.input.push_str(B_MENTION);
                app.send_message_to(self.nav.geral);
                self.nav_next(4, "mentioned A");
            }
            4 if self.nav_lingered(1.2) => {
                // A's reaction comes back on my reply; I add mine (two people on 👍).
                if let Some(r) = message(app, self.nav.geral, B_REPLY).map(|m| m.id) {
                    app.react(Target::Channel(self.nav.geral), r, "👍", true);
                }
                self.nav_cue(app, "navready");
                self.nav_next(5, "ready; answering cues");
            }
            5 => {
                if cue(app, self.nav.memes, "navping1") && self.nav.sent < 2 {
                    self.nav.sent = 2;
                    app.backend.send(crate::backend::Cmd::SendMessage { channel: self.nav.geral, content: B_PONG1.into(), extras: Default::default() });
                }
                if cue(app, self.nav.memes, "navping2") && self.nav.sent < 3 {
                    self.nav.sent = 3;
                    app.backend.send(crate::backend::Cmd::SendMessage { channel: self.nav.geral, content: B_PONG2.into(), extras: Default::default() });
                }
                // A's DM: answer it once.
                if self.nav.dm == 0 {
                    if let Some(c) = app.dms.convs.iter().find(|c| c.peer.id == self.nav.peer).map(|c| c.id) {
                        self.nav.dm = c;
                        app.select_dm(c);
                    }
                } else if self.nav.sent < 4 && app.dms.logs.get(&self.nav.dm).is_some_and(|l| l.msgs.iter().any(|m| m.content == DM_A)) {
                    self.nav.sent = 4;
                    app.dms.input = DM_B.into();
                    app.send_dm();
                }
                if cue(app, self.nav.memes, "navdone") {
                    return self.pass();
                }
                if self.waited(240) {
                    return self.fail("A never finished");
                }
            }
            _ => {}
        }
        None
    }
}
