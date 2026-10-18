//! Call stage autotest (v1.0, a child module of `autotest`): role `stage-b`, with two `multi-a`
//! partners (each shares a test window) and a `cam-a` partner (synthetic camera,
//! `VOX_FAKE_CAMERA=1`) in the same voice room.
//!
//! B watches both streams, then:
//! 1. the normal grid: every card on the stage, every video window exactly over its card's picture;
//! 2. pins the two streams (they share the big area, the four people's cards go to the strip),
//!    then pins the camera with a real click on its card (three pinned);
//! 3. opens the menu of a card without video in the strip as a right click does (a right click
//!    posted to a window kept behind the others never reaches egui as a secondary click), checks
//!    that it draws the card's own items, hides
//!    it ("Ocultar este card"), then hides every card without video (only the three pinned video
//!    cards are left, no strip), shows them again and hides one card again;
//! 4. double-clicks the stage's empty space: the stage fullscreen (the window covers its monitor),
//!    the video windows move with their cards and keep presenting frames; Esc leaves it and the
//!    window comes back where it was, videos again over their cards and still playing.
//!
//! 5. once the partners are done, puts the stage fullscreen again and leaves the call: the stage
//!    fullscreen must end and the window come back where it was (any end of the call - leave, kick,
//!    disconnect, room deleted, access revoked - goes through the same reset in `sync_video`).
//!
//! At every step the native video windows must sit exactly where the cards' pictures are (in
//! physical pixels: the DPI is applied the same way). With `VOX_AUTOTEST_SHOT_DIR` it also saves
//! `stage-normal`, `stage-pinned`, `stage-menu`, `stage-fullscreen` and `stage-dock` (the dock's
//! "⋯" menu open). Deafening at the end is the partners' signal to stop. Silent: `VOX_NO_PLAYBACK=1`,
//! `VOX_NO_MIC=1`.

use eframe::egui::{self, Pos2};
use vox_client_core::golive::SELF_SHARE_KEY;
use vox_client_core::voice::{camera_key, SELF_CAMERA_KEY};
use vox_proto::UserId;

use super::{shots, win, Auto};
use crate::app::{App, Center};
use crate::call::{to_px, DrawnCard, TileKey};

#[derive(Default)]
pub struct State {
    /// The two streamers (by username) and the person whose camera is on.
    streams: Vec<UserId>,
    cam: Option<UserId>,
    /// The card without video that gets hidden.
    hidden: Option<UserId>,
    /// Frames presented per video (streams, camera) at the last baseline.
    presented: Vec<u64>,
    /// The window before the stage fullscreen.
    win_rect: Option<win::Rect>,
    /// The stage's area before the fullscreen (points).
    stage_w: f32,
    /// Strip scrolls done looking for the camera card.
    scrolls: u32,
}

fn log(what: &str) {
    tracing::info!(target: "autotest", "AUTOTEST stage: {what}");
}

/// The viewer key of a card's video.
fn video_key(app: &App, key: TileKey) -> String {
    let me = app.my_id();
    match key {
        TileKey::Stream(u) if Some(u) == me => SELF_SHARE_KEY.to_string(),
        TileKey::Stream(u) => u.to_string(),
        TileKey::Member(u) if Some(u) == me => SELF_CAMERA_KEY.to_string(),
        TileKey::Member(u) => camera_key(&u.to_string()),
    }
}

fn card(app: &App, key: TileKey) -> Option<DrawnCard> {
    app.call.drawn.iter().find(|c| c.key == key).copied()
}

/// A point of the stage (points) in client pixels.
fn px(app: &App, p: Pos2) -> (i32, i32) {
    let ppp = app.repaint_ctx().pixels_per_point();
    ((p.x * ppp).round() as i32, (p.y * ppp).round() as i32)
}

/// Every card with a video: its window sits exactly over the card's picture. Returns how many.
fn hosts_match(app: &App) -> Result<usize, String> {
    let ppp = app.repaint_ctx().pixels_per_point();
    let mut n = 0;
    for c in &app.call.drawn {
        let Some(v) = c.video else { continue };
        let key = video_key(app, c.key);
        let want = to_px(v, ppp);
        let placed = app.video.placed.get(&key).copied();
        if placed != Some(want) {
            return Err(format!("{key}: the video window is at {placed:?}, the card's picture at {want:?} (ppp {ppp})"));
        }
        n += 1;
    }
    Ok(n)
}

/// A point of the stage that no card covers (for the double-click on the empty space).
fn empty_point(app: &App) -> Option<Pos2> {
    let area = app.call.stage_rect?;
    let covered = |p: Pos2| app.call.drawn.iter().any(|c| c.rect.expand(2.0).contains(p));
    let mut y = area.top() + 4.0;
    while y < area.bottom() - 4.0 {
        let mut x = area.left() + 4.0;
        while x < area.right() - 4.0 {
            let p = Pos2::new(x, y);
            if !covered(p) {
                return Some(p);
            }
            x += 6.0;
        }
        y += 6.0;
    }
    None
}

impl Auto {
    fn sg_keys(&self, app: &App) -> Vec<String> {
        let mut keys: Vec<String> = self.stage.streams.iter().map(|u| u.to_string()).collect();
        if let Some(c) = self.stage.cam {
            keys.push(video_key(app, TileKey::Member(c)));
        }
        keys
    }

    fn sg_presented(&self, app: &App) -> Vec<u64> {
        self.sg_keys(app).iter().map(|k| Self::tile_state(k).map(|s| s.presented).unwrap_or(0)).collect()
    }

    /// Frames presented on the first `need` videos (streams, then the camera) since the baseline
    /// (and a new baseline).
    fn sg_flowing(&mut self, app: &App, what: &str, need: usize) -> Result<(), String> {
        let now = self.sg_presented(app);
        let d: Vec<u64> = now.iter().zip(&self.stage.presented).map(|(n, b)| n.saturating_sub(*b)).collect();
        log(&format!("{what}: frames presented +{d:?} (streams, camera)"));
        self.stage.presented = now;
        if d.len() < 3 || d.iter().take(need).any(|&x| x < 5) {
            return Err(format!("{what}: the videos are not all playing (+{d:?})"));
        }
        Ok(())
    }

    pub(super) fn run_stage_b(&mut self, app: &mut App) -> Option<i32> {
        let parent = app.video.parent.unwrap_or_default();
        let res: Result<(), String> = match self.step {
            0 => {
                self.connect(app);
                Ok(())
            }
            1 => {
                if self.logged_in(app) {
                    let Some(vc) = Self::first_voice(app) else { return self.fail("no voice channel") };
                    app.join_voice(vc);
                    self.goto(2, "joining voice");
                } else if self.waited(30) || app.form.error.is_some() {
                    return self.fail(&format!("could not log in: {}", app.form.error.clone().unwrap_or_default()));
                }
                Ok(())
            }
            2 => {
                if app.voice.channel.is_some() {
                    self.goto(3, "in voice; waiting for two lives and a camera");
                } else if self.waited(60) {
                    return self.fail("voice join timed out");
                }
                Ok(())
            }
            3 => {
                let me = app.my_id();
                let mut live: Vec<(String, UserId)> = app.members_here().iter().filter(|m| m.streaming && Some(m.user.id) != me).map(|m| (m.user.username.clone(), m.user.id)).collect();
                live.sort();
                let cam = app.voice.cameras.iter().filter_map(|i| i.parse::<UserId>().ok()).find(|u| !live.iter().any(|(_, l)| l == u));
                if live.len() >= 2 && cam.is_some() && app.members_here().len() >= 4 && self.waited(3) {
                    self.stage.streams = live.iter().take(2).map(|(_, u)| *u).collect();
                    self.stage.cam = cam;
                    app.show_call();
                    for u in self.stage.streams.clone() {
                        if !app.watch(&u.to_string()) {
                            return self.fail("watch refused");
                        }
                    }
                    self.goto(4, &format!("lives {live:?}, camera of {cam:?}; watching both"));
                } else if self.waited(150) {
                    return self.fail("the two lives and the camera never showed up");
                }
                Ok(())
            }
            4 => {
                let watching = self.stage.streams.iter().all(|u| app.is_watching(&u.to_string()));
                if watching && self.waited(5) {
                    let n = hosts_match(app);
                    log(&format!("grid: {} cards, pinned {}, hidden {}, videos over their cards: {n:?}, layouts so far {}", app.call.card_count, app.call.pinned_count, app.call.hidden_count, app.call.layouts));
                    let n = match n {
                        Ok(n) => n,
                        Err(e) => return self.fail(&e),
                    };
                    if app.center != Center::Call || app.call.card_count != 6 || app.call.pinned_count != 0 || n != 3 {
                        return self.fail(&format!("the grid should show 6 cards (3 with video), none pinned: {:?}", app.call.drawn));
                    }
                    shots::save(app, "stage-normal");
                    self.stage.presented = self.sg_presented(app);
                    for u in self.stage.streams.clone() {
                        app.pin(TileKey::Stream(u));
                    }
                    self.goto(5, "pinned the two streams");
                } else if !watching && self.waited(30) {
                    return self.fail(&format!("watch did not start for both: {:?}", app.voice.watching));
                }
                Ok(())
            }
            5 if self.waited(2) => (|| {
                let n = hosts_match(app)?;
                let pinned: Vec<DrawnCard> = app.call.drawn.iter().filter(|c| c.pinned).copied().collect();
                let strip: Vec<DrawnCard> = app.call.drawn.iter().filter(|c| !c.pinned).copied().collect();
                log(&format!("two pinned: {pinned:?}; strip {strip:?}; videos over their cards: {n}"));
                // (The four people's cards are in the strip; in a small window some are scrolled out.)
                if app.call.pinned_count != 2 || app.call.card_count != 6 || pinned.len() != 2 || !pinned.iter().all(|c| matches!(c.key, TileKey::Stream(_))) || strip.is_empty() {
                    return Err("the two streams are not the pinned cards with the four people in the strip".into());
                }
                let lowest = pinned.iter().map(|c| c.rect.bottom()).fold(f32::MIN, f32::max);
                if !strip.iter().all(|s| s.rect.top() > lowest && pinned.iter().all(|p| p.rect.area() > 2.0 * s.rect.area())) {
                    return Err("the strip is not under the (bigger) pinned cards".into());
                }
                let cam_key = TileKey::Member(self.stage.cam.ok_or("no camera")?);
                let Some(cam) = card(app, cam_key) else {
                    // Scrolled out of the strip: scroll one card on (what an arrow does) and look again.
                    if self.stage.scrolls >= 8 {
                        return Err("the camera card is not in the strip, even scrolled".into());
                    }
                    self.stage.scrolls += 1;
                    log(&format!("the camera card is scrolled out of the strip: scrolling (first {})", app.call.strip_first));
                    app.call.strip_first += 1;
                    return Ok(());
                };
                // (The camera may have been scrolled out of the strip: only the streams.)
                self.sg_flowing(app, "two pinned", 2)?;
                shots::save(app, "stage-pinned");
                // Pin the camera like a person: a click on its card.
                let (x, y) = px(app, Pos2::new(cam.rect.center().x, cam.rect.top() + 14.0));
                win::click(parent, x, y);
                self.goto(6, "clicked the camera card");
                Ok(())
            })(),
            6 if self.waited(2) => (|| {
                let cam = self.stage.cam.ok_or("no camera")?;
                let n = hosts_match(app)?;
                log(&format!("after the click: pins {:?}, pinned {}, cards {}, videos over their cards {n}", app.call.pins, app.call.pinned_count, app.call.card_count));
                if !app.is_pinned(TileKey::Member(cam)) || app.call.pinned_count != 3 {
                    return Err("a click on the camera card did not pin it".into());
                }
                // A card without video in the strip (somebody else's if one is shown, else mine):
                // its menu, opened with a right click.
                let me = app.my_id();
                let no_video = |c: &&DrawnCard| !c.pinned && c.video.is_none() && matches!(c.key, TileKey::Member(_));
                let target = app.call.drawn.iter().filter(no_video).find(|c| Some(c.key.user()) != me).or_else(|| app.call.drawn.iter().find(no_video)).copied();
                let Some(target) = target else {
                    log(&format!("no card without video shown in the strip (from {}): scrolling", app.call.strip_first));
                    app.call.strip_first = if app.call.strip_first > 0 { 0 } else { 99 };
                    return Ok(());
                };
                self.stage.hidden = Some(target.key.user());
                // (A right click posted to a window kept behind the others does not reach egui as a
                // secondary click; the menu opens through the same path a right click takes.)
                let pos = Pos2::new(target.rect.center().x, target.rect.top() + 14.0);
                if !app.open_card_menu(&app.repaint_ctx(), target.key, pos) {
                    return Err("the card's menu did not open".into());
                }
                self.goto(7, &format!("opened the menu of the card of {}", target.key.user()));
                Ok(())
            })(),
            7 if self.waited(2) => (|| {
                let who = self.stage.hidden.ok_or("no card")?;
                log(&format!("menu open={} card={:?} (menus opened so far {})", app.cmenu.open.is_some(), app.call.menu_card, app.cmenu.opened));
                if app.cmenu.open.is_none() || app.call.menu_card != Some((TileKey::Member(who), false)) || app.call.card_menu_drawn != Some(TileKey::Member(who)) {
                    return Err(format!("the card's menu is not open with its own items ({:?})", app.call.card_menu_drawn));
                }
                shots::save(app, "stage-menu");
                app.cmenu.open = None;
                egui::Popup::close_all(&app.repaint_ctx());
                // "Ocultar este card".
                app.hide_card(who);
                self.goto(8, "Ocultar este card");
                Ok(())
            })(),
            8 if self.waited(2) => (|| {
                let who = self.stage.hidden.ok_or("no card")?;
                let n = hosts_match(app)?;
                log(&format!("one hidden: hidden {}, cards {}, drawn {:?}", app.call.hidden_count, app.call.card_count, app.call.drawn.iter().map(|c| c.key).collect::<Vec<_>>()));
                if app.call.hidden_count != 1 || app.call.card_count != 5 || card(app, TileKey::Member(who)).is_some() || n != 3 {
                    return Err("the hidden card still takes space".into());
                }
                app.hide_cards_without_video();
                self.goto(9, "Ocultar todos sem vídeo");
                Ok(())
            })(),
            9 if self.waited(2) => (|| {
                let n = hosts_match(app)?;
                log(&format!("all without video hidden: hidden {}, cards {}, drawn {:?}", app.call.hidden_count, app.call.card_count, app.call.drawn));
                if app.call.hidden_count != 3 || app.call.card_count != 3 || app.call.drawn.len() != 3 || !app.call.drawn.iter().all(|c| c.pinned && c.video.is_some()) || n != 3 {
                    return Err("hiding every card without video should leave the three pinned video cards alone".into());
                }
                app.show_hidden_cards();
                app.hide_card(self.stage.hidden.ok_or("no card")?);
                self.goto(10, "shown again; one card hidden again");
                Ok(())
            })(),
            10 if self.waited(2) => (|| {
                if app.call.hidden_count != 1 || app.call.card_count != 5 {
                    return Err(format!("mostrar ocultos / ocultar again: hidden {} cards {}", app.call.hidden_count, app.call.card_count));
                }
                let p = empty_point(app).ok_or("no empty space on the stage")?;
                self.stage.win_rect = win::window_rect(parent);
                self.stage.stage_w = app.call.stage_rect.map(|r| r.width()).unwrap_or(0.0);
                self.stage.presented = self.sg_presented(app);
                let (x, y) = px(app, p);
                win::double_click(parent, x, y);
                self.goto(11, &format!("double-clicked the stage's empty space at {p:?} (window {:?})", self.stage.win_rect));
                Ok(())
            })(),
            11 if self.waited(3) => (|| {
                let (wr, mr) = (win::window_rect(parent), win::monitor_rect(parent));
                let n = hosts_match(app)?;
                let w = app.call.stage_rect.map(|r| r.width()).unwrap_or(0.0);
                log(&format!("stage fullscreen: on={} window={wr:?} monitor={mr:?} stage width {w} (was {}) videos over their cards {n}", app.stage_fullscreen, self.stage.stage_w));
                if !app.stage_fullscreen || wr.is_none() || wr != mr || w <= self.stage.stage_w || n != 3 || app.call.pinned_count != 3 {
                    return Err("the double-click did not put the stage fullscreen over the monitor with its cards".into());
                }
                self.sg_flowing(app, "stage fullscreen", 3)?;
                shots::save(app, "stage-fullscreen");
                win::key(parent, win::VK_ESCAPE);
                self.goto(12, "pressed Esc");
                Ok(())
            })(),
            12 if self.waited(3) => (|| {
                let wr = win::window_rect(parent);
                let n = hosts_match(app)?;
                log(&format!("after Esc: on={} window={wr:?} (was {:?}) videos over their cards {n}", app.stage_fullscreen, self.stage.win_rect));
                if app.stage_fullscreen || wr != self.stage.win_rect || n != 3 {
                    return Err("Esc did not bring the window and the stage back".into());
                }
                self.sg_flowing(app, "back from the fullscreen", 3)?;
                egui::Popup::open_id(&app.repaint_ctx(), egui::Id::new(crate::call::dock::MORE_MENU));
                self.goto(13, "opened the dock's ⋯ menu (screenshot)");
                Ok(())
            })(),
            13 if self.waited(2) => {
                shots::save(app, "stage-dock");
                egui::Popup::close_all(&app.repaint_ctx());
                app.show_hidden_cards();
                app.pin_only(None);
                app.toggle_deafen();
                self.goto(14, "checks done (deafened: the partners' signal)");
                Ok(())
            }
            14 => {
                let me = app.my_id();
                let busy = app.members_here().iter().any(|m| Some(m.user.id) != me && m.streaming) || !app.voice.cameras.is_empty();
                if !busy || self.waited(90) {
                    // The call ends while the stage is fullscreen: the window must come back.
                    let ctx = app.repaint_ctx();
                    app.set_stage_fullscreen(&ctx, true);
                    self.goto(15, "partners done; the stage fullscreen again before leaving");
                }
                Ok(())
            }
            15 if self.waited(2) => {
                let (wr, mr) = (win::window_rect(parent), win::monitor_rect(parent));
                log(&format!("stage fullscreen before leaving: on={} window={wr:?} monitor={mr:?}", app.stage_fullscreen));
                if !app.stage_fullscreen || wr != mr {
                    return self.fail("the stage did not go fullscreen again");
                }
                app.leave_voice();
                self.goto(16, "left the voice room while the stage was fullscreen");
                Ok(())
            }
            16 if self.waited(2) => {
                let wr = win::window_rect(parent);
                log(&format!("after leaving: stage fullscreen={} stream fullscreen={} window={wr:?} (was {:?})", app.stage_fullscreen, app.call.fullscreen, self.stage.win_rect));
                if app.stage_fullscreen || app.call.fullscreen || wr != self.stage.win_rect {
                    return self.fail("leaving the call did not end the stage fullscreen and give the window back");
                }
                return self.pass();
            }
            _ => Ok(()),
        };
        match res {
            Ok(()) => None,
            Err(e) => self.fail(&e),
        }
    }
}
