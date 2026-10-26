//! Profile-picture autotest (a child module of `autotest`, so it drives the same `Auto` steps).
//!
//! - `avatar-a`: joins the first voice channel, "chooses a file" (a generated test image, never a
//!   personal picture) the way the settings do, saves it, tells B in the chat, then (when B has
//!   seen it) removes it again.
//! - `avatar-b`: in the same voice channel. Waits for A's `User` to carry a picture version, then
//!   checks through the app state that A's real picture (not the initial) was drawn in A's call
//!   tile, the sidebar and the online list, then in the chat next to A's message; later that all of
//!   it falls back to the initial after A removes the picture.
//!
//! Silent: no sound (`VOX_NO_PLAYBACK=1`), no microphone (`VOX_NO_MIC=1`).

use std::io::Cursor;

use vox_proto::UserId;

use super::Auto;
use crate::app::{App, Center, Screen};
use crate::avatars::Surface;

#[derive(Default)]
pub struct State {
    /// B: A's id and the picture version B is waiting to see disappear.
    other: Option<UserId>,
    version: Option<String>,
}

fn log(what: &str) {
    tracing::info!(target: "autotest", "AUTOTEST avatars: {what}");
}

/// A 640x360 test picture: red, green and blue thirds. The hub keeps the centre 360x360, so the
/// round picture is green with a little of the neighbours at the edges.
pub fn test_image() -> Vec<u8> {
    let img = image::RgbImage::from_fn(640, 360, |x, y| match x / 214 {
        0 => image::Rgb([220, 40, 40]),
        1 => image::Rgb([40, (150 + y / 4) as u8, 70]),
        _ => image::Rgb([50, 60, 220]),
    });
    let mut out = Vec::new();
    image::DynamicImage::ImageRgb8(img).write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png).expect("encode the test picture");
    out
}

impl Auto {
    fn av_tag(&self, what: &str) -> String {
        format!("avtest {what} {}", self.args.run_id)
    }

    /// Logged in and ready? `Some(code)` = failed.
    fn av_login(&mut self, app: &mut App) -> Option<Option<i32>> {
        if app.screen == Screen::Main && app.ready && app.hub_up {
            return Some(None);
        }
        if self.waited(30) || (app.form.error.is_some() && !app.form.busy) {
            return Some(self.fail(&format!("could not log in: {}", app.form.error.clone().unwrap_or_default())));
        }
        None
    }

    /// Both people in the voice channel (and so in each other's call view).
    /// v1.0: the tiles are on the stage and the member list beside the chat, never both at once.
    /// Show the stage until `who`'s tile was drawn, then the chat (the drawn marks add up).
    fn av_show_both(app: &mut App, who: UserId) {
        let st = &app.avatars.store;
        if !st.was_drawn(who, Surface::Tile) {
            app.center = Center::Call;
        } else if !st.was_drawn(who, Surface::Online) {
            if app.selected.is_none() {
                if let Some(ch) = Self::first_text(app) {
                    app.select_text(ch);
                }
            }
            app.center = Center::Chat;
            app.settings.layout.show_members = true;
        }
    }

    fn av_together(app: &App) -> bool {
        app.voice.channel.is_some() && app.voice.joining.is_none() && app.members_here().len() >= 2
    }

    pub(super) fn run_avatar_a(&mut self, app: &mut App) -> Option<i32> {
        match self.step {
            0 => self.connect(app),
            1 => match self.av_login(app) {
                Some(Some(code)) => return Some(code),
                Some(None) => {
                    if !app.roles.supported {
                        return self.fail("the hub does not know roles (it predates profile pictures)");
                    }
                    let Some(vc) = Self::first_voice(app) else { return self.fail("no voice channel") };
                    if app.me.as_ref().is_some_and(|m| m.avatar.is_some()) {
                        log("A already has a picture from an earlier run: removing it first");
                        app.remove_profile_picture();
                    }
                    app.join_voice(vc);
                    self.goto(2, "joining voice");
                }
                None => {}
            },
            2 => {
                if Self::av_together(app) && app.me.as_ref().is_some_and(|m| m.avatar.is_none()) {
                    self.goto(3, "B is here and I have no picture; choosing the test image");
                } else if let Some(e) = app.voice.error.clone() {
                    return self.fail(&format!("voice join failed: {e}"));
                } else if self.waited(90) {
                    return self.fail("B never joined the voice channel");
                }
            }
            3 => {
                let ctx = app.repaint.ctx()?;
                // The same path as the file dialog's result: decode, crop preview, ready to save.
                app.choose_profile_picture_bytes(&ctx, "teste.png", test_image());
                if app.avatars.profile.picked.is_none() {
                    return self.fail(&format!("the test image was refused: {:?}", app.avatars.profile.msg));
                }
                if self.args.show.as_deref() == Some("profile") {
                    // `--show profile`: the settings open on the chosen picture's preview, for a look.
                    app.open_settings();
                    self.goto(30, "settings open on the preview");
                    return None;
                }
                app.save_profile_picture();
                self.goto(4, "saving the picture");
            }
            30 if self.waited(self.args.hold_secs) => {
                app.save_profile_picture();
                self.goto(31, "saving the picture from the settings");
            }
            31 if self.waited(2) => {
                app.settings_ui.open = false;
                self.goto(4, "settings closed");
            }
            4 => {
                if let Some((false, e)) = &app.avatars.profile.msg {
                    return self.fail(&format!("the hub refused the picture: {e}"));
                }
                if let Some(v) = app.me.as_ref().and_then(|m| m.avatar.clone()) {
                    log(&format!("saved: version {v}; the settings say {:?}", app.avatars.profile.msg));
                    self.avatar.version = Some(v);
                    app.avatars.store.clear_drawn();
                    let tag = self.av_tag("set");
                    Self::say(app, &tag);
                    self.goto(5, "picture saved; told B");
                } else if self.waited(20) {
                    return self.fail("the picture was not confirmed");
                }
            }
            5 => {
                // My own screens show it too (tile, sidebar, online list).
                let me = app.my_id().unwrap_or_default();
                Self::av_show_both(app, me);
                let st = &app.avatars.store;
                let all = st.was_drawn(me, Surface::Tile) && st.was_drawn(me, Surface::Sidebar) && st.was_drawn(me, Surface::Online);
                if all && Self::saw_message(app, &self.av_tag("seen")) {
                    log(&format!("my own picture is on my tile, sidebar and online list; B saw it too ({:?})", st.stats()));
                    if self.args.show.as_deref() == Some("hold") {
                        // `--show hold`: keep the picture on screen for `--hold` seconds (screenshots).
                        self.goto(20, "holding the picture on screen");
                    } else {
                        app.remove_profile_picture();
                        self.goto(6, "removing the picture");
                    }
                } else if self.waited(40) {
                    return self.fail(&format!(
                        "own picture drawn: tile={} sidebar={} online={}; B confirmed={}",
                        st.was_drawn(me, Surface::Tile),
                        st.was_drawn(me, Surface::Sidebar),
                        st.was_drawn(me, Surface::Online),
                        Self::saw_message(app, &self.av_tag("seen"))
                    ));
                }
            }
            20 if self.waited(self.args.hold_secs) => {
                app.remove_profile_picture();
                self.goto(6, "removing the picture");
            }
            6 => {
                if app.me.as_ref().is_some_and(|m| m.avatar.is_none()) && app.avatars.profile.msg.as_ref().is_some_and(|(ok, _)| *ok) {
                    let tag = self.av_tag("removed");
                    Self::say(app, &tag);
                    self.goto(7, "removed; told B");
                } else if self.waited(20) {
                    return self.fail("the removal was not confirmed");
                }
            }
            7 => {
                if Self::saw_message(app, &self.av_tag("gone")) {
                    app.leave_voice();
                    self.goto(8, "B saw the picture go");
                } else if self.waited(40) {
                    return self.fail("B never confirmed the removal");
                }
            }
            8 if self.waited(2) => return self.pass(),
            _ => {}
        }
        None
    }

    pub(super) fn run_avatar_b(&mut self, app: &mut App) -> Option<i32> {
        match self.step {
            0 => self.connect(app),
            1 => match self.av_login(app) {
                Some(Some(code)) => return Some(code),
                Some(None) => {
                    let Some(vc) = Self::first_voice(app) else { return self.fail("no voice channel") };
                    app.join_voice(vc);
                    self.goto(2, "joining voice");
                }
                None => {}
            },
            2 => {
                if Self::av_together(app) {
                    let a = app.members_here().into_iter().find(|m| Some(m.user.id) != app.my_id()).map(|m| m.user.id);
                    self.avatar.other = a;
                    self.goto(3, "A is here; waiting for A's picture");
                } else if let Some(e) = app.voice.error.clone() {
                    return self.fail(&format!("voice join failed: {e}"));
                } else if self.waited(90) {
                    return self.fail("A never joined the voice channel");
                }
            }
            3 => {
                let a = self.avatar.other.unwrap_or_default();
                let version = app.voice_members.get(&a).and_then(|m| m.user.avatar.clone());
                if let Some(v) = version {
                    log(&format!("A's picture version arrived: {v}"));
                    self.avatar.version = Some(v);
                    app.center = Center::Call;
                    app.avatars.store.clear_drawn();
                    self.goto(4, "looking for A's picture on the tile, sidebar and online list");
                } else if self.waited(90) {
                    return self.fail("A's picture never arrived");
                }
            }
            4 => {
                let a = self.avatar.other.unwrap_or_default();
                Self::av_show_both(app, a);
                let st = &app.avatars.store;
                let (tile, side, online) = (st.was_drawn(a, Surface::Tile), st.was_drawn(a, Surface::Sidebar), st.was_drawn(a, Surface::Online));
                if tile && side && online {
                    log(&format!("A's real picture is drawn on the tile, the sidebar and the online list (store: {:?})", st.stats()));
                    // The chat next: A's message is there, its author carries the version.
                    if let Some(ch) = Self::first_text(app) {
                        app.select_text(ch);
                    }
                    app.center = Center::Chat;
                    app.avatars.store.clear_drawn();
                    self.goto(5, "showing the chat");
                } else if self.waited(20) {
                    return self.fail(&format!("A's picture drawn: tile={tile} sidebar={side} online={online}"));
                }
            }
            5 => {
                let a = self.avatar.other.unwrap_or_default();
                let told = Self::saw_message(app, &self.av_tag("set"));
                let author_ok = app.logs.values().flat_map(|l| &l.msgs).any(|m| m.author.id == a && m.author.avatar.is_some() && m.content.starts_with(&self.av_tag("set")));
                if told && author_ok && app.avatars.store.was_drawn(a, Surface::Chat) {
                    log("A's picture is drawn next to A's chat message");
                    let tag = self.av_tag("seen");
                    Self::say(app, &tag);
                    self.goto(6, "told A; waiting for the removal");
                } else if self.waited(25) {
                    return self.fail(&format!(
                        "chat: A's message seen={told}, its author carries the picture={author_ok}, drawn={}",
                        app.avatars.store.was_drawn(a, Surface::Chat)
                    ));
                }
            }
            6 => {
                let a = self.avatar.other.unwrap_or_default();
                let gone_online = app.online.iter().find(|u| u.id == a).is_some_and(|u| u.avatar.is_none());
                let gone_voice = app.voice_members.get(&a).is_some_and(|m| m.user.avatar.is_none());
                let gone_chat = app.logs.values().flat_map(|l| &l.msgs).filter(|m| m.author.id == a).all(|m| m.author.avatar.is_none());
                if gone_online && gone_voice && gone_chat && Self::saw_message(app, &self.av_tag("removed")) {
                    // Everything falls back to the initial: nothing of A's picture is drawn any more.
                    app.avatars.store.clear_drawn();
                    self.goto(7, "A's picture is gone everywhere; checking the fallback");
                } else if self.waited(40) {
                    return self.fail(&format!("removal not shown: online={gone_online} voice={gone_voice} chat={gone_chat}"));
                }
            }
            7 if self.waited(2) => {
                let a = self.avatar.other.unwrap_or_default();
                let st = &app.avatars.store;
                if [Surface::Tile, Surface::Sidebar, Surface::Online, Surface::Chat].iter().any(|s| st.was_drawn(a, *s)) {
                    return self.fail("A's picture is still drawn after the removal");
                }
                let tag = self.av_tag("gone");
                Self::say(app, &tag);
                self.goto(8, "fallback confirmed; told A");
            }
            8 if self.waited(3) => return self.pass(),
            _ => {}
        }
        None
    }
}
