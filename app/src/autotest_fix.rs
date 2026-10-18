//! v0.8.2 fixes autotest (a child module of `autotest`): `fix-a` / `fix-b`. A is the hub's owner
//! (`dono`), B a member (`fxB<n>`). Every step waits a second first (the hub's message rate limit).
//!
//! 1. **Deleting my own message with an image.** A sends a picture; its menu offers "Apagar
//!    mensagem" (B, a member, is offered nothing on A's message); A deletes it through the
//!    confirmation ("Apagar esta mensagem?"). It vanishes for both, and both drop its cached
//!    thumbnail. B, a member, sends its own picture and deletes it with Shift (no confirmation);
//!    then the same in a DM with A.
//! 2. **A new image after the newest one was deleted.** A sends another picture: its message and
//!    image ids are fresh (above the deleted ones) and B draws the NEW thumbnail (its bytes differ
//!    from the deleted one's, and so does the content hash it is cached under).
//! 3. **Channel deleted and recreated.** A creates a channel and writes in it; B opens it, starts a
//!    reply and a draft. A deletes the channel: both drop everything of it (messages, counters,
//!    reply, draft, scroll). A creates a channel again: a fresh id, and it opens empty for both.
//! 4. **The sidebar badges.** A writes twice in that channel (a long name), once mentioning B. B,
//!    reading another channel, gets the counter and the "@" on it; every row B drew keeps its
//!    badges clear of the name, the name cut with "…" before them.
//!
//! Generated pictures only; silent (`VOX_NO_PLAYBACK=1`, `VOX_NO_MIC=1`).

use std::hash::{Hash, Hasher};

use vox_proto::{Attachment, ChannelKind, Message, UserId};

use super::Auto;
use crate::app::App;
use crate::chat_images::{ImgKey, Target};
use crate::roles::AdminCmd;

/// The owner's personal password in these runs (fresh dev data dir each time).
const FX_PW: &str = "autotest-dono-fix";
/// The channel made and deleted in part 3, and the one made again (as long as a name may be).
const TEMP_NAME: &str = "fx-temp";
const LONG_NAME: &str = "canal-de-nome-bem-comprido-teste";

#[derive(Default)]
pub struct State {
    geral: i64,
    peer: Option<UserId>,
    m1: Option<Message>,
    m2: Option<Message>,
    m3: Option<i64>,
    thumb1: Option<u64>,
    dm: Option<i64>,
    dm_line: Option<i64>,
    c1: Option<i64>,
    c2: Option<i64>,
    epoch_before: u32,
}

fn log(what: &str) {
    tracing::info!(target: "autotest", "AUTOTEST fix: {what}");
}

/// A generated picture (different for every `seed`).
fn picture(seed: u8) -> image::DynamicImage {
    let img = image::RgbImage::from_fn(900, 600, |x, y| {
        let band = ((x / 60 + y / 60 + u32::from(seed)) % 2) as u8;
        image::Rgb([seed.wrapping_mul(70).wrapping_add(band * 90), (x % 256) as u8, (y % 256) as u8 ^ seed])
    });
    image::DynamicImage::ImageRgb8(img)
}

fn digest(bytes: &[u8]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
}

impl Auto {
    fn fx_tag(&self, what: &str) -> String {
        format!("fxtest {what} {}", self.args.run_id)
    }

    fn fx_msg(app: &App, ch: i64, caption: &str) -> Option<Message> {
        app.logs.get(&ch)?.msgs.iter().find(|m| m.caption() == caption).cloned()
    }

    fn fx_has(app: &App, ch: i64, id: i64) -> bool {
        app.logs.get(&ch).is_some_and(|l| l.msgs.iter().any(|m| m.id == id))
    }

    fn fx_channel(app: &App, name: &str) -> Option<i64> {
        app.channels.iter().find(|c| c.name == name && c.kind == ChannelKind::Text).map(|c| c.id)
    }

    /// Attach a generated picture to `target`'s next message.
    fn fx_attach(&mut self, app: &mut App, target: Target, seed: u8) -> Option<i32> {
        let Some(ctx) = app.repaint.ctx() else { return self.fail("no egui context") };
        let img = picture(seed);
        let bytes = u64::from(img.width()) * u64::from(img.height()) * 4;
        app.attach_picture(target, format!("fx{seed}.png"), img, bytes, &ctx);
        None
    }

    /// The compressed picture is ready: send it with `caption`. `Some(Err)`: failed.
    fn fx_send_when_ready(&mut self, app: &mut App, target: Target, caption: &str) -> Option<Result<(), i32>> {
        if !app.images.ready(target) {
            return self.waited(60).then(|| Err(self.fail("the picture was not compressed in 60 s").unwrap_or(1)));
        }
        match target {
            Target::Channel(_) => {
                app.input = caption.to_string();
                app.send_message();
            }
            Target::Dm(_) => {
                app.dms.input = caption.to_string();
                app.send_dm();
            }
        }
        if !app.images.sending_to(target) {
            return Some(Err(self.fail("the send did not start").unwrap_or(1)));
        }
        Some(Ok(()))
    }

    /// Nothing of the deleted image is cached any more.
    fn fx_forgotten(app: &App, a: &Attachment) -> bool {
        !app.images.store.knows(ImgKey::of(a))
    }

    /// Every sidebar row drawn keeps its badges clear of the name, and the name inside its space.
    fn fx_rows_clear(app: &App) -> Result<(), String> {
        for d in &app.sidebar_rows {
            if !d.layout.name_is_clear() || d.text_right > d.layout.name.right() + 0.5 {
                return Err(format!("row {:?}: name {:?} (drawn to {}), badges {:?}", d.label, d.layout.name, d.text_right, d.layout.badges().collect::<Vec<_>>()));
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------ A (the owner)

    pub(super) fn run_fix_a(&mut self, app: &mut App) -> Option<i32> {
        if self.step >= 4 && !self.waited(1) {
            return None;
        }
        let geral = self.fx.geral;
        let target = Target::Channel(geral);
        match self.step {
            0 => {
                app.form.personal = FX_PW.into();
                self.connect(app);
            }
            1 => {
                if !self.logged_in(app) {
                    return if self.waited(30) { self.fail(&format!("could not log in: {:?}", app.form.error)) } else { None };
                }
                if app.me.as_ref().map(|u| u.role) != Some(vox_proto::Role::Owner) {
                    return self.fail("this name is not the hub's owner");
                }
                if app.roles.account.is_some_and(|a| a.must_set_password) {
                    app.set_personal_password(None, Some(FX_PW.into()));
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
                let peer = app.online.iter().find(|u| u.username.starts_with("fxB")).map(|u| u.id);
                match (Self::first_text(app), peer) {
                    (Some(ch), Some(p)) => {
                        (self.fx.geral, self.fx.peer) = (ch, Some(p));
                        app.select_text(ch);
                        self.goto(4, "B is online; waiting for it to be ready");
                    }
                    _ if self.waited(60) => return self.fail("B never came online"),
                    _ => {}
                }
            }
            4 => {
                if !Self::saw_message(app, &self.fx_tag("ready")) {
                    return if self.waited(60) { self.fail("B never got ready") } else { None };
                }
                if let Some(code) = self.fx_attach(app, target, 1) {
                    return Some(code);
                }
                self.goto(5, "part 1: a picture attached");
            }
            5 => match self.fx_send_when_ready(app, target, &self.fx_tag("img1"))? {
                Err(code) => return Some(code),
                Ok(()) => self.goto(6, "sending the picture"),
            },
            6 => {
                let Some(m) = Self::fx_msg(app, geral, &self.fx_tag("img1")) else {
                    return if self.waited(60) { self.fail("my picture did not come back") } else { None };
                };
                if m.attachments.len() != 1 {
                    return self.fail(&format!("the message came back with {} images", m.attachments.len()));
                }
                // The UI offers "Apagar mensagem" on my own message with an image (drawn this frame).
                if !app.chatx.delete_offers.contains(&(target, m.id)) {
                    return if self.waited(20) { self.fail("no \"Apagar mensagem\" on my own message with an image") } else { None };
                }
                log(&format!("own picture message {} (image {} hash {}) offers \"Apagar mensagem\"", m.id, m.attachments[0].id, m.attachments[0].hash));
                self.fx.m1 = Some(m);
                self.goto(7, "waiting for B to see it");
            }
            7 => {
                if !Self::saw_message(app, &self.fx_tag("saw1")) {
                    return if self.waited(90) { self.fail("B never saw the picture") } else { None };
                }
                let id = self.fx.m1.as_ref().map(|m| m.id).unwrap_or_default();
                // "Apagar mensagem" without Shift asks first.
                app.ask_delete(target, id, false);
                if !app.chatx.confirm_delete.as_ref().is_some_and(|c| c.id == id) {
                    return self.fail("\"Apagar mensagem\" did not ask \"Apagar esta mensagem?\"");
                }
                // "Apagar" in the question.
                app.delete_message_now(target, id);
                self.goto(8, "deleted my picture message (confirmed)");
            }
            8 => {
                let Some(m1) = self.fx.m1.clone() else { return self.fail("lost the first message") };
                if Self::fx_has(app, geral, m1.id) || !Self::fx_forgotten(app, &m1.attachments[0]) {
                    return if self.waited(30) { self.fail("my deleted message (or its cached image) is still here") } else { None };
                }
                self.goto(9, "gone here, image cache dropped; waiting for B");
            }
            9 => {
                if !Self::saw_message(app, &self.fx_tag("gone1")) {
                    return if self.waited(60) { self.fail("B never saw the message go") } else { None };
                }
                if let Some(code) = self.fx_attach(app, target, 2) {
                    return Some(code);
                }
                self.goto(10, "part 2: another picture, after the newest message was deleted");
            }
            10 => match self.fx_send_when_ready(app, target, &self.fx_tag("img2"))? {
                Err(code) => return Some(code),
                Ok(()) => self.goto(11, "sending the second picture"),
            },
            11 => {
                let Some(m2) = Self::fx_msg(app, geral, &self.fx_tag("img2")) else {
                    return if self.waited(60) { self.fail("the second picture did not come back") } else { None };
                };
                let Some(m1) = self.fx.m1.clone() else { return self.fail("lost the first message") };
                let (a1, a2) = (&m1.attachments[0], &m2.attachments[0]);
                log(&format!("deleted: message {} image {} hash {}; new: message {} image {} hash {}", m1.id, a1.id, a1.hash, m2.id, a2.id, a2.hash));
                if m2.id <= m1.id || a2.id <= a1.id {
                    return self.fail("the hub handed a deleted message's or image's id out again");
                }
                if a2.hash == a1.hash {
                    return self.fail("the new picture has the deleted one's content hash");
                }
                self.fx.m2 = Some(m2);
                self.goto(12, "fresh ids; waiting for B's thumbnail check");
            }
            12 => {
                if !Self::saw_message(app, &self.fx_tag("saw2")) {
                    return if self.waited(90) { self.fail("B never checked the new thumbnail") } else { None };
                }
                self.goto(13, "B drew the new thumbnail; B deletes its own picture and DM line");
            }
            13 => {
                // B's own picture appears here, then goes (B deleted it with Shift).
                if let Some(m) = Self::fx_msg(app, geral, &self.fx_tag("bimg")) {
                    if self.fx.m3.is_none() {
                        log(&format!("B's picture {} arrived; B (a member) deletes it", m.id));
                    }
                    self.fx.m3 = Some(m.id);
                    if app.chatx.delete_offers.contains(&(target, m.id)) {
                        log("the owner is offered \"Apagar mensagem (moderação)\" on B's message");
                    }
                }
                if !Self::saw_message(app, &self.fx_tag("gone4")) {
                    return if self.waited(120) { self.fail("B never finished deleting its own picture and DM line") } else { None };
                }
                if self.fx.m3.is_some_and(|id| Self::fx_has(app, geral, id)) {
                    return self.fail("B's deleted picture is still here");
                }
                app.admin(AdminCmd::CreateChannel { name: TEMP_NAME.into(), kind: ChannelKind::Text, private: None });
                self.goto(14, "part 3: creating a channel");
            }
            14 => {
                let Some(c1) = Self::fx_channel(app, TEMP_NAME) else {
                    return if self.waited(20) { self.fail("the new channel never showed") } else { None };
                };
                self.fx.c1 = Some(c1);
                app.select_text(c1);
                Self::say(app, &self.fx_tag("in-temp"));
                self.goto(15, "wrote in the new channel");
            }
            15 => {
                let c1 = self.fx.c1.unwrap_or_default();
                let Some(m) = Self::fx_msg(app, c1, &self.fx_tag("in-temp")) else {
                    return if self.waited(30) { self.fail("my message in the new channel did not come back") } else { None };
                };
                if !Self::saw_message(app, &self.fx_tag("temp")) {
                    return if self.waited(60) { self.fail("B never opened the new channel") } else { None };
                }
                // State to be dropped with the channel: a reply being written, a draft.
                app.start_reply(Target::Channel(c1), m.id);
                app.input = "rascunho".into();
                self.fx.epoch_before = app.channel_epochs.get(&c1).copied().unwrap_or(0);
                app.admin(AdminCmd::DeleteChannel { id: c1 });
                self.goto(16, "deleting the channel");
            }
            16 => {
                let c1 = self.fx.c1.unwrap_or_default();
                if app.channels.iter().any(|c| c.id == c1) {
                    return if self.waited(20) { self.fail("the deleted channel is still listed") } else { None };
                }
                if let Err(e) = Self::fx_channel_forgotten(app, c1, self.fx.epoch_before) {
                    return self.fail(&e);
                }
                app.admin(AdminCmd::CreateChannel { name: LONG_NAME.into(), kind: ChannelKind::Text, private: None });
                self.goto(17, "channel gone with all its state; creating one again");
            }
            17 => {
                let Some(c2) = Self::fx_channel(app, LONG_NAME) else {
                    return if self.waited(20) { self.fail("the recreated channel never showed") } else { None };
                };
                let c1 = self.fx.c1.unwrap_or_default();
                if c2 <= c1 {
                    return self.fail(&format!("the new channel got id {c2}, the deleted one had {c1}: an id was reused"));
                }
                self.fx.c2 = Some(c2);
                app.select_text(c2);
                self.goto(18, "the new channel has a fresh id; opening it");
            }
            18 => {
                let c2 = self.fx.c2.unwrap_or_default();
                match app.logs.get(&c2) {
                    Some(l) if l.requested && l.no_more && l.msgs.is_empty() && !l.loading_older => {}
                    Some(l) if !l.msgs.is_empty() => return self.fail(&format!("the new channel shows {} old messages", l.msgs.len())),
                    _ => return if self.waited(20) { self.fail("the new channel never loaded") } else { None },
                }
                app.select_text(geral);
                Self::say(app, &self.fx_tag("new"));
                self.goto(19, "the new channel opened empty; waiting for B");
            }
            19 => {
                if !Self::saw_message(app, &self.fx_tag("empty")) {
                    return if self.waited(60) { self.fail("B never checked the new channel") } else { None };
                }
                let c2 = self.fx.c2.unwrap_or_default();
                let peer = app.online.iter().find(|u| Some(u.id) == self.fx.peer).map(|u| u.username.clone()).unwrap_or_default();
                app.select_text(c2);
                Self::say(app, &format!("{} @{peer}", self.fx_tag("u1")));
                self.goto(20, "part 4: two messages in the long-named channel (one mentions B)");
            }
            20 => {
                Self::say(app, &self.fx_tag("u2"));
                app.select_text(geral);
                self.goto(21, "waiting for B's sidebar check");
            }
            21 => {
                if Self::saw_message(app, &self.fx_tag("bdone")) {
                    if let Err(e) = Self::fx_rows_clear(app) {
                        return self.fail(&e);
                    }
                    return self.pass();
                }
                if self.waited(90) {
                    return self.fail("B never finished");
                }
            }
            _ => {}
        }
        None
    }

    /// Nothing of the deleted channel `ch` is left in the app.
    fn fx_channel_forgotten(app: &App, ch: i64, epoch_before: u32) -> Result<(), String> {
        let target = Target::Channel(ch);
        let mut left = Vec::new();
        if app.logs.contains_key(&ch) {
            left.push("messages");
        }
        if app.unread.contains_key(&ch) || app.chatx.unread_ids.contains_key(&ch) || app.chatx.mention_unread.contains_key(&ch) {
            left.push("unread counters");
        }
        if app.chatx.replying.contains_key(&target) {
            left.push("the reply being written");
        }
        if app.selected == Some(ch) {
            left.push("the selection");
        }
        if app.input == "rascunho" {
            left.push("the draft");
        }
        if app.images.pending_for(target) != 0 {
            left.push("pictures waiting to be sent");
        }
        if app.channel_epochs.get(&ch).copied().unwrap_or(0) <= epoch_before {
            left.push("the scroll state");
        }
        if left.is_empty() {
            log(&format!("channel {ch} deleted: messages, counters, reply, draft and scroll state dropped"));
            Ok(())
        } else {
            Err(format!("the deleted channel {ch} left behind: {}", left.join(", ")))
        }
    }

    // ------------------------------------------------------------ B (a member)

    pub(super) fn run_fix_b(&mut self, app: &mut App) -> Option<i32> {
        if self.step >= 3 && !self.waited(1) {
            return None;
        }
        let geral = self.fx.geral;
        let target = Target::Channel(geral);
        match self.step {
            0 => self.connect(app),
            1 => {
                if !self.logged_in(app) {
                    return if self.waited(30) { self.fail(&format!("could not log in: {:?}", app.form.error)) } else { None };
                }
                let peer = app.online.iter().find(|u| u.username == "dono").map(|u| u.id);
                match (Self::first_text(app), peer) {
                    (Some(ch), Some(p)) if self.waited(2) => {
                        (self.fx.geral, self.fx.peer) = (ch, Some(p));
                        app.select_text(ch);
                        self.goto(2, "A is online");
                    }
                    _ if self.waited(60) => return self.fail("A never came online"),
                    _ => {}
                }
            }
            2 => {
                Self::say(app, &self.fx_tag("ready"));
                self.goto(3, "ready; waiting for A's picture");
            }
            3 => {
                let Some(m) = Self::fx_msg(app, geral, &self.fx_tag("img1")) else {
                    return if self.waited(90) { self.fail("A's picture never arrived") } else { None };
                };
                if app.chatx.delete_offers.contains(&(target, m.id)) {
                    return self.fail("a member is offered \"Apagar mensagem\" on somebody else's message");
                }
                let key = ImgKey::of(&m.attachments[0]);
                let Some(bytes) = app.images.store.thumb_bytes(key).filter(|_| app.images.store.thumb_shown(key)) else {
                    return if self.waited(30) { self.fail("A's thumbnail was not drawn") } else { None };
                };
                self.fx.thumb1 = Some(digest(bytes));
                log(&format!("A's picture {} drawn (image {} hash {})", m.id, key.id, m.attachments[0].hash));
                self.fx.m1 = Some(m);
                Self::say(app, &self.fx_tag("saw1"));
                self.goto(4, "saw A's picture; waiting for A to delete it");
            }
            4 => {
                let Some(m1) = self.fx.m1.clone() else { return self.fail("lost the first message") };
                if Self::fx_has(app, geral, m1.id) || !Self::fx_forgotten(app, &m1.attachments[0]) {
                    return if self.waited(60) { self.fail("A's deleted message (or its cached image) is still here") } else { None };
                }
                Self::say(app, &self.fx_tag("gone1"));
                self.goto(5, "A's message vanished live, its thumbnail dropped");
            }
            5 => {
                let Some(m2) = Self::fx_msg(app, geral, &self.fx_tag("img2")) else {
                    return if self.waited(90) { self.fail("A's second picture never arrived") } else { None };
                };
                let Some(m1) = self.fx.m1.clone() else { return self.fail("lost the first message") };
                let (a1, a2) = (&m1.attachments[0], &m2.attachments[0]);
                let key = ImgKey::of(a2);
                let Some(bytes) = app.images.store.thumb_bytes(key).filter(|_| app.images.store.thumb_shown(key)) else {
                    return if self.waited(30) { self.fail("the new thumbnail was not drawn") } else { None };
                };
                let d2 = digest(bytes);
                log(&format!("old image {} hash {} thumb {:016x}; new image {} hash {} thumb {d2:016x}", a1.id, a1.hash, self.fx.thumb1.unwrap_or_default(), a2.id, a2.hash));
                if Some(d2) == self.fx.thumb1 || a2.hash == a1.hash || ImgKey::of(a1) == key {
                    return self.fail("the new picture shows the deleted picture's thumbnail");
                }
                if m2.id <= m1.id || a2.id <= a1.id {
                    return self.fail("an id of the deleted message came back");
                }
                Self::say(app, &self.fx_tag("saw2"));
                if let Some(code) = self.fx_attach(app, target, 3) {
                    return Some(code);
                }
                self.goto(6, "the NEW thumbnail is drawn; now my own picture");
            }
            6 => match self.fx_send_when_ready(app, target, &self.fx_tag("bimg"))? {
                Err(code) => return Some(code),
                Ok(()) => self.goto(7, "sending my own picture"),
            },
            7 => {
                let Some(m) = Self::fx_msg(app, geral, &self.fx_tag("bimg")) else {
                    return if self.waited(60) { self.fail("my picture did not come back") } else { None };
                };
                if !app.chatx.delete_offers.contains(&(target, m.id)) {
                    return if self.waited(20) { self.fail("a member is not offered \"Apagar mensagem\" on their own picture") } else { None };
                }
                // Shift+click: no question.
                app.ask_delete(target, m.id, true);
                if app.chatx.confirm_delete.is_some() {
                    return self.fail("Shift+click still asked");
                }
                self.fx.m3 = Some(m.id);
                self.fx.m2 = Some(m);
                self.goto(8, "deleted my own picture (Shift)");
            }
            8 => {
                let Some(m) = self.fx.m2.clone() else { return self.fail("lost my picture message") };
                if Self::fx_has(app, geral, m.id) || !Self::fx_forgotten(app, &m.attachments[0]) {
                    return if self.waited(30) { self.fail("my deleted picture is still here") } else { None };
                }
                let Some(peer) = self.fx.peer else { return self.fail("no peer") };
                app.open_dm(peer);
                self.goto(9, "my picture is gone; now a DM picture");
            }
            9 => {
                let Some(dm) = app.dms.selected.filter(|d| app.dms.logs.get(d).is_some_and(|l| l.requested)) else {
                    return if self.waited(20) { self.fail("the DM with A did not open") } else { None };
                };
                self.fx.dm = Some(dm);
                if let Some(code) = self.fx_attach(app, Target::Dm(dm), 4) {
                    return Some(code);
                }
                self.goto(10, "DM opened; a picture attached");
            }
            10 => {
                let t = Target::Dm(self.fx.dm.unwrap_or_default());
                match self.fx_send_when_ready(app, t, &self.fx_tag("dmimg"))? {
                    Err(code) => return Some(code),
                    Ok(()) => self.goto(11, "sending the DM picture"),
                }
            }
            11 => {
                let dm = self.fx.dm.unwrap_or_default();
                let t = Target::Dm(dm);
                let caption = self.fx_tag("dmimg");
                let Some(line) = app.dms.logs.get(&dm).and_then(|l| l.msgs.iter().find(|m| m.caption() == caption).cloned()) else {
                    return if self.waited(60) { self.fail("my DM picture did not come back") } else { None };
                };
                if !app.chatx.delete_offers.contains(&(t, line.id)) {
                    return if self.waited(20) { self.fail("no \"Apagar mensagem\" on my own DM line") } else { None };
                }
                app.ask_delete(t, line.id, true);
                self.fx.dm_line = Some(line.id);
                self.goto(12, "deleting my DM picture");
            }
            12 => {
                let dm = self.fx.dm.unwrap_or_default();
                let id = self.fx.dm_line.unwrap_or_default();
                if app.dms.logs.get(&dm).is_some_and(|l| l.msgs.iter().any(|m| m.id == id)) {
                    return if self.waited(30) { self.fail("my deleted DM line is still here") } else { None };
                }
                app.select_text(geral);
                Self::say(app, &self.fx_tag("gone4"));
                self.goto(13, "DM line gone; waiting for A's new channel");
            }
            13 => {
                let Some(c1) = Self::fx_channel(app, TEMP_NAME) else {
                    return if self.waited(60) { self.fail("A's channel never showed") } else { None };
                };
                self.fx.c1 = Some(c1);
                app.select_text(c1);
                let Some(m) = Self::fx_msg(app, c1, &self.fx_tag("in-temp")) else {
                    return if self.waited(30) { self.fail("A's message in the new channel never showed") } else { None };
                };
                // Reading the channel with state that must go with it: a reply, a draft.
                app.start_reply(Target::Channel(c1), m.id);
                app.input = "rascunho".into();
                self.fx.epoch_before = app.channel_epochs.get(&c1).copied().unwrap_or(0);
                // Tell A without leaving the channel (straight to the hub, the input untouched).
                let tag = self.fx_tag("temp");
                app.backend.send(crate::backend::Cmd::SendMessage { channel: geral, content: tag, extras: Default::default() });
                self.goto(14, "reading the new channel with a reply and a draft; waiting for A to delete it");
            }
            14 => {
                let c1 = self.fx.c1.unwrap_or_default();
                if app.channels.iter().any(|c| c.id == c1) {
                    return if self.waited(60) { self.fail("A never deleted the channel") } else { None };
                }
                if let Err(e) = Self::fx_channel_forgotten(app, c1, self.fx.epoch_before) {
                    return self.fail(&e);
                }
                self.goto(16, "the channel went with all its state");
            }
            16 => {
                let Some(c2) = Self::fx_channel(app, LONG_NAME) else {
                    return if self.waited(60) { self.fail("the recreated channel never showed") } else { None };
                };
                if !Self::saw_message(app, &self.fx_tag("new")) {
                    return if self.waited(60) { self.fail("A never opened the recreated channel") } else { None };
                }
                if c2 <= self.fx.c1.unwrap_or_default() {
                    return self.fail("the recreated channel reused the deleted one's id");
                }
                self.fx.c2 = Some(c2);
                app.select_text(c2);
                self.goto(17, "opening the recreated channel");
            }
            17 => {
                let c2 = self.fx.c2.unwrap_or_default();
                match app.logs.get(&c2) {
                    Some(l) if l.requested && l.no_more && l.msgs.is_empty() => {}
                    Some(l) if !l.msgs.is_empty() => return self.fail(&format!("the recreated channel shows {} messages", l.msgs.len())),
                    _ => return if self.waited(20) { self.fail("the recreated channel never loaded") } else { None },
                }
                app.select_text(geral);
                Self::say(app, &self.fx_tag("empty"));
                self.goto(18, "it opened empty; waiting for A's messages there");
            }
            18 => {
                let c2 = self.fx.c2.unwrap_or_default();
                let (unread, mentions) = (app.unread.get(&c2).copied().unwrap_or(0), app.chatx.mention_unread.get(&c2).copied().unwrap_or(0));
                if unread < 2 || mentions < 1 {
                    return if self.waited(60) { self.fail(&format!("the long-named channel has {unread} unread / {mentions} mentions, expected 2 / 1")) } else { None };
                }
                let label = format!("# {LONG_NAME}");
                let Some(row) = app.sidebar_rows.iter().find(|d| d.label == label).cloned() else {
                    return if self.waited(10) { self.fail("the long-named channel's row was not drawn") } else { None };
                };
                if row.layout.count.is_none() || row.layout.mention.is_none() {
                    return self.fail(&format!("the row has no counter / \"@\": {row:?}"));
                }
                if let Err(e) = Self::fx_rows_clear(app) {
                    return self.fail(&e);
                }
                log(&format!(
                    "sidebar: \"{}\" name {:?} drawn to {:.1} (cut: {}), counter {:?}, @ {:?}; {} rows checked",
                    row.label,
                    row.layout.name,
                    row.text_right,
                    row.elided,
                    row.layout.count,
                    row.layout.mention,
                    app.sidebar_rows.len()
                ));
                Self::say(app, &self.fx_tag("bdone"));
                self.goto(19, "done");
            }
            19 if self.waited(2) => return self.pass(),
            _ => {}
        }
        None
    }
}
