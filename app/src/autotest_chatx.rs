//! Mentions, replies and reactions autotest (a child module of `autotest`): `chatx-a` /
//! `chatx-b` (v0.8). Names `cxA<n>` / `cxB<n>`; both behind another window (so what notifies is
//! counted), with the default "Só mensagens diretas e menções".
//!
//! 1. B says it is ready ("cxready"). A types `@cxB` in the channel input: the member list offers
//!    B (and not `@todos`: A is a Membro), A picks it and sends "cx1"; then "cx2" with a member's
//!    `@todos`, and "cx5" in the v0.7.0 shape (no mention ids, `@cxB1` only as text).
//! 2. B: cx1 mentions it by id (a chip over `@cxB1`), it counts as a mention (the mention sound, the
//!    channel's `@`, a Windows notification "… mencionou você em #…", the badge); cx2 and cx5 do not.
//! 3. B answers cx1 with "Responder" ("cx3", "@ ligado") and again with it off ("cx4"). A sees the
//!    quote of its own cx1, is mentioned by cx3 only, and a click on the quote finds cx1.
//! 4. A reacts 👍 on cx3: B sees it live ("cxok1"); A takes it off: B sees that ("cxok2"); B
//!    reacts 🎉 on cx1 and A sees it with B's id.
//! 5. DM: A writes "cxdm1", B replies to it ("cxdm2", the quote), A reacts ❤ on the reply, B sees it
//!    live ("cxok3" in the DM).
//! 6. A deletes cx1: B's cx3 now quotes "mensagem apagada" ("cxbdone"). A writes "ntdone <run>" for
//!    a v0.7.0 app's `notify-c` running alongside (the old-app check of the release gate).
//!
//! Every step waits a second first (the hub's message rate limit). Silent: `VOX_NO_PLAYBACK=1`;
//! the notifications are only recorded.

use super::Auto;
use crate::app::chat_extras::{quote_parts, Chip, Target};
use crate::app::App;
use crate::backend::Cmd;

#[derive(Default)]
pub struct State {
    ch: i64,
    peer: i64,
    sounds: usize,
    balloons: u32,
    cx1: i64,
    cx3: i64,
    dm: i64,
    dm1: i64,
}

fn log(what: &str) {
    tracing::info!(target: "autotest", "AUTOTEST chatx: {what}");
}

impl Auto {
    fn cx_tag(&self, what: &str) -> String {
        format!("{what} {}", self.args.run_id)
    }

    /// `cxA1` <-> `cxB1`.
    fn cx_peer_name(&self) -> String {
        let n = &self.args.name;
        match (n.strip_prefix("cxA"), n.strip_prefix("cxB")) {
            (Some(rest), _) => format!("cxB{rest}"),
            (_, Some(rest)) => format!("cxA{rest}"),
            _ => String::new(),
        }
    }

    fn cx_msg(app: &App, ch: i64, prefix: &str) -> Option<vox_proto::Message> {
        app.logs.get(&ch)?.msgs.iter().find(|m| m.content.starts_with(prefix)).cloned()
    }

    fn cx_dm_line(app: &App, dm: i64, prefix: &str) -> Option<vox_proto::DmMessage> {
        app.dms.logs.get(&dm)?.msgs.iter().find(|m| m.content.starts_with(prefix)).cloned()
    }

    /// Logged in, the hub has the v0.8 chat, the other one online: (channel, peer id).
    fn cx_ready(&mut self, app: &mut App) -> Option<Result<(i64, i64), i32>> {
        if !self.logged_in(app) {
            return self.waited(30).then(|| Err(self.fail("could not log in").unwrap_or(1)));
        }
        let f = app.chatx.features;
        if !(f.mentions && f.replies && f.reactions) {
            return Some(Err(self.fail(&format!("the hub does not announce the v0.8 chat: {f:?}")).unwrap_or(1)));
        }
        let name = self.cx_peer_name();
        match (app.online.iter().find(|u| u.username == name), Self::first_text(app)) {
            (Some(u), Some(ch)) if self.waited(3) => Some(Ok((ch, u.id))),
            _ if self.waited(60) => Some(Err(self.fail(&format!("{name} never came online")).unwrap_or(1))),
            _ => None,
        }
    }

    fn cx_react_of(list: &[vox_proto::Reaction], emoji: &str, user: i64) -> bool {
        list.iter().any(|r| r.emoji == emoji && r.users.contains(&user))
    }

    // ------------------------------------------------------------ A

    pub(super) fn run_chatx_a(&mut self, app: &mut App) -> Option<i32> {
        // Every step lingers a second: the hub allows ~5 messages a second per person.
        if self.step >= 2 && !self.waited(1) {
            return None;
        }
        let me = app.my_id().unwrap_or_default();
        let ch = self.cx.ch;
        let target = Target::Channel(ch);
        match self.step {
            0 => {
                self.connect(app);
                app.notify.focus_override = Some(false);
            }
            1 => match self.cx_ready(app)? {
                Err(code) => return Some(code),
                Ok((ch, peer)) => {
                    (self.cx.ch, self.cx.peer) = (ch, peer);
                    app.select_text(ch);
                    self.goto(2, "ready; waiting for B");
                }
            },
            2 => {
                if !Self::saw_message(app, &self.cx_tag("cxready")) {
                    return if self.waited(60) { self.fail("B never got ready") } else { None };
                }
                // Typing "@cxB" opens the member list with B in it.
                app.input = format!("{} oi @cxB", self.cx_tag("cx1"));
                let cursor = app.input.len();
                let Some((at, list)) = app.mention_list(target, &app.input, cursor) else {
                    return self.fail("typing @cxB opened no member list");
                };
                let Some(i) = list.iter().position(|c| c.id == Some(self.cx.peer)) else {
                    return self.fail(&format!("B is not in the member list: {list:?}"));
                };
                if app.mention_list(target, "@to", 3).is_some_and(|(_, l)| l.iter().any(|c| c.id.is_none())) {
                    return self.fail("a member was offered @todos");
                }
                let c = list[i].clone();
                app.pick_mention(target, at, cursor, &c);
                log(&format!("picked {:?} from the list: {:?}", c.name, app.input));
                if !app.input.ends_with("@cxB1 ") {
                    return self.fail(&format!("the completion is wrong: {:?}", app.input));
                }
                app.send_message();
                Self::say(app, &format!("{} @todos atenção", self.cx_tag("cx2")));
                // The v0.7.0 shape: no mention ids, just the text.
                app.backend.send(Cmd::SendMessage { channel: ch, content: format!("{} formato antigo @cxB1", self.cx_tag("cx5")), extras: Default::default() });
                self.goto(3, "sent cx1 (mention), cx2 (@todos as a member), cx5 (old shape)");
            }
            3 => {
                let (Some(cx1), Some(cx3), Some(cx4)) = (Self::cx_msg(app, ch, &self.cx_tag("cx1")), Self::cx_msg(app, ch, &self.cx_tag("cx3")), Self::cx_msg(app, ch, &self.cx_tag("cx4"))) else {
                    return if self.waited(60) { self.fail("B's replies never came") } else { None };
                };
                let q = cx3.reply.clone()?;
                if q.id != cx1.id || q.author.as_ref().map(|u| u.id) != Some(me) || q.deleted || !q.excerpt.starts_with(&self.cx_tag("cx1")) {
                    return self.fail(&format!("cx3's quote is wrong: {q:?}"));
                }
                if !cx3.mentions_me(me) || cx4.mentions_me(me) || cx4.reply.as_ref().map(|r| r.id) != Some(cx1.id) {
                    return self.fail(&format!("reply mentions: cx3 {:?}, cx4 {:?}", cx3.mentions, cx4.mentions));
                }
                let mentions = app.chatx.mention_unread.get(&ch).copied().unwrap_or(0);
                if mentions != 1 || !app.notify.stats.sounds.contains(&"mention") {
                    return self.fail(&format!("the reply did not notify me as a mention: @{mentions}, sounds {:?}", app.notify.stats.sounds));
                }
                // A click on the quote finds the original.
                app.jump_to(target, q.id, q.deleted);
                if app.chatx.jump.map(|j| j.id) != Some(cx1.id) || app.chatx.jumps_found != 1 {
                    return self.fail("the quote's jump did not find cx1");
                }
                (self.cx.cx1, self.cx.cx3) = (cx1.id, cx3.id);
                app.react(target, cx3.id, "👍", true);
                log(&format!("B's reply quotes cx1 ({:?}) and mentions me (@{mentions}); reacting 👍", quote_parts(&q)));
                self.goto(4, "reacted 👍 on cx3");
            }
            4 => {
                if Self::saw_message(app, &self.cx_tag("cxok1")) {
                    app.react(target, self.cx.cx3, "👍", false);
                    self.goto(5, "B saw it; took 👍 off");
                } else if self.waited(30) {
                    return self.fail("B never saw my reaction");
                }
            }
            5 => {
                let cx1 = Self::cx_msg(app, ch, &self.cx_tag("cx1"))?;
                if Self::saw_message(app, &self.cx_tag("cxok2")) && Self::cx_react_of(&cx1.reactions, "🎉", self.cx.peer) {
                    log(&format!("B saw the removal; B's 🎉 on cx1 arrived live: {:?}", cx1.reactions));
                    app.open_dm(self.cx.peer);
                    self.goto(6, "opening the DM");
                } else if self.waited(30) {
                    return self.fail(&format!("no cxok2 / no 🎉 from B: {:?}", cx1.reactions));
                }
            }
            6 => {
                if let Some(dm) = app.dms.selected.filter(|d| app.dms.conv(*d).is_some_and(|c| c.peer.id == self.cx.peer)) {
                    self.cx.dm = dm;
                    app.dms.input = self.cx_tag("cxdm1");
                    app.send_dm();
                    self.goto(7, "wrote cxdm1");
                } else if self.waited(20) {
                    return self.fail("the DM did not open");
                }
            }
            7 => {
                let Some(reply) = Self::cx_dm_line(app, self.cx.dm, &self.cx_tag("cxdm2")) else {
                    return if self.waited(40) { self.fail("B's DM reply never came") } else { None };
                };
                let dm1 = Self::cx_dm_line(app, self.cx.dm, &self.cx_tag("cxdm1"))?;
                if reply.reply.as_ref().map(|r| r.id) != Some(dm1.id) {
                    return self.fail(&format!("the DM reply does not quote cxdm1: {:?}", reply.reply));
                }
                app.react(Target::Dm(self.cx.dm), reply.id, "❤", true);
                self.goto(8, "B's DM reply quotes mine; reacted ❤");
            }
            8 => {
                if Self::cx_dm_line(app, self.cx.dm, &self.cx_tag("cxok3")).is_some() {
                    app.select_text(ch);
                    app.admin(crate::roles::AdminCmd::DeleteMessage { id: self.cx.cx1 });
                    self.goto(9, "B saw the DM reaction; deleting cx1");
                } else if self.waited(30) {
                    return self.fail("B never saw the DM reaction");
                }
            }
            9 => {
                if Self::saw_message(app, &self.cx_tag("cxbdone")) {
                    // For a v0.7.0 app's notify-c alongside (it waits for this line).
                    Self::say(app, &self.cx_tag("ntdone"));
                    self.goto(10, "B is done; told the old app");
                } else if self.waited(40) {
                    return self.fail("B never finished");
                }
            }
            10 if self.waited(3) => return self.pass(),
            _ => {}
        }
        None
    }

    // ------------------------------------------------------------ B

    pub(super) fn run_chatx_b(&mut self, app: &mut App) -> Option<i32> {
        // Every step lingers a second: the hub allows ~5 messages a second per person.
        if self.step >= 2 && !self.waited(1) {
            return None;
        }
        let me = app.my_id().unwrap_or_default();
        let ch = self.cx.ch;
        let target = Target::Channel(ch);
        match self.step {
            0 => {
                self.connect(app);
                app.notify.focus_override = Some(false);
            }
            1 => match self.cx_ready(app)? {
                Err(code) => return Some(code),
                Ok((ch, peer)) => {
                    (self.cx.ch, self.cx.peer) = (ch, peer);
                    app.select_text(ch);
                    self.cx.sounds = app.notify.stats.sounds.len();
                    self.cx.balloons = app.notify.stats.balloons;
                    Self::say(app, &self.cx_tag("cxready"));
                    self.goto(2, "ready; waiting for A's mention");
                }
            },
            2 => {
                let (Some(cx1), Some(cx2), Some(cx5)) = (Self::cx_msg(app, ch, &self.cx_tag("cx1")), Self::cx_msg(app, ch, &self.cx_tag("cx2")), Self::cx_msg(app, ch, &self.cx_tag("cx5"))) else {
                    return if self.waited(40) { self.fail("A's messages never came") } else { None };
                };
                if cx1.mentions != vec![me] || !cx1.mentions_me(me) {
                    return self.fail(&format!("cx1 does not mention me by id: {:?}", cx1.mentions));
                }
                if cx2.mention_everyone || cx2.mentions_me(me) || cx5.mentions_me(me) {
                    return self.fail("a member's @todos or an old-shape @name counted as a mention");
                }
                let chips = app.chatx.chips(target, cx1.id, cx1.caption(), &cx1.mentions, false);
                if chips.len() != 1 || chips[0].chip != Chip::User(me) || &cx1.caption()[chips[0].range.clone()] != "@cxB1" {
                    return self.fail(&format!("the chip is wrong: {chips:?}"));
                }
                let s = app.notify.stats.clone();
                let mentions = app.chatx.mention_unread.get(&ch).copied().unwrap_or(0);
                let unread = app.unread.get(&ch).copied().unwrap_or(0);
                let title = format!("{} mencionou você em #{}", self.cx_peer_name(), app.channels.iter().find(|c| c.id == ch).map(|c| c.name.as_str()).unwrap_or("?"));
                if mentions != 1 || unread != 3 || !s.sounds[self.cx.sounds..].contains(&"mention") {
                    return self.fail(&format!("the mention did not count: @{mentions}, unread {unread}, sounds {:?}", s.sounds));
                }
                if s.balloons != self.cx.balloons + 1 || s.last_balloon.as_ref().map(|b| b.0.as_str()) != Some(title.as_str()) || s.badge != 1 {
                    return self.fail(&format!("the mention's notification is wrong (wanted {title:?}, badge 1): {s:?}"));
                }
                log(&format!("mentioned by id, chip over @cxB1, @{mentions} of {unread} unread, notification {:?}, badge {}", s.last_balloon, s.badge));
                // "Responder", with "@ ligado" (the default), then with it off.
                app.start_reply(target, cx1.id);
                if !app.chatx.replying.get(&target).is_some_and(|r| r.ping && r.id == cx1.id) {
                    return self.fail("Responder did not start a reply with @ ligado");
                }
                Self::say(app, &format!("{} concordo", self.cx_tag("cx3")));
                app.start_reply(target, cx1.id);
                if let Some(r) = app.chatx.replying.get_mut(&target) {
                    r.ping = false;
                }
                Self::say(app, &format!("{} sem ping", self.cx_tag("cx4")));
                if !app.chatx.replying.is_empty() {
                    return self.fail("sending did not use up the reply");
                }
                self.cx.cx1 = cx1.id;
                self.goto(3, "replied twice");
            }
            3 => {
                let Some(cx3) = Self::cx_msg(app, ch, &self.cx_tag("cx3")) else {
                    return if self.waited(20) { self.fail("my reply never came back") } else { None };
                };
                self.cx.cx3 = cx3.id;
                if Self::cx_react_of(&cx3.reactions, "👍", self.cx.peer) {
                    Self::say(app, &self.cx_tag("cxok1"));
                    self.goto(4, "A's 👍 arrived live");
                } else if self.waited(40) {
                    return self.fail(&format!("A's reaction never arrived: {:?}", cx3.reactions));
                }
            }
            4 => {
                let cx3 = Self::cx_msg(app, ch, &self.cx_tag("cx3"))?;
                if cx3.reactions.is_empty() {
                    app.react(target, self.cx.cx1, "🎉", true);
                    Self::say(app, &self.cx_tag("cxok2"));
                    self.goto(5, "A's 👍 went away live; reacted 🎉 on cx1");
                } else if self.waited(30) {
                    return self.fail(&format!("A's reaction was never taken off: {:?}", cx3.reactions));
                }
            }
            5 => {
                let Some(conv) = app.dms.conv_with(self.cx.peer).cloned() else {
                    return if self.waited(40) { self.fail("A's DM never came") } else { None };
                };
                let Some(dm1) = Self::cx_dm_line(app, conv.id, &self.cx_tag("cxdm1")) else {
                    app.select_dm(conv.id);
                    return if self.waited(40) { self.fail("cxdm1 never came") } else { None };
                };
                (self.cx.dm, self.cx.dm1) = (conv.id, dm1.id);
                app.start_reply(Target::Dm(conv.id), dm1.id);
                app.dms.input = self.cx_tag("cxdm2");
                app.send_dm();
                self.goto(6, "replied in the DM");
            }
            6 => {
                let Some(reply) = Self::cx_dm_line(app, self.cx.dm, &self.cx_tag("cxdm2")) else {
                    return if self.waited(20) { self.fail("my DM reply never came back") } else { None };
                };
                if reply.reply.as_ref().map(|r| r.id) != Some(self.cx.dm1) {
                    return self.fail(&format!("my DM reply has no quote: {:?}", reply.reply));
                }
                if Self::cx_react_of(&reply.reactions, "❤", self.cx.peer) {
                    app.dms.input = self.cx_tag("cxok3");
                    app.send_dm();
                    app.select_text(ch);
                    self.goto(7, "A's ❤ arrived live in the DM");
                } else if self.waited(30) {
                    return self.fail("A's DM reaction never arrived");
                }
            }
            7 => {
                let gone = Self::cx_msg(app, ch, &self.cx_tag("cx1")).is_none();
                let cx3 = Self::cx_msg(app, ch, &self.cx_tag("cx3"))?;
                let quote = cx3.reply.as_ref().map(quote_parts);
                if gone && quote.as_ref().is_some_and(|(who, body)| who.is_none() && body == "mensagem apagada") {
                    log("A deleted cx1: my reply now quotes \"mensagem apagada\"");
                    Self::say(app, &self.cx_tag("cxbdone"));
                    self.goto(8, "done");
                } else if self.waited(30) {
                    return self.fail(&format!("cx1 deleted {gone}, quote {quote:?}"));
                }
            }
            8 if self.waited(2) => return self.pass(),
            _ => {}
        }
        None
    }
}
