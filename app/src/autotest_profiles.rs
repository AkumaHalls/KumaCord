//! Profile autotest (a child module of `autotest`, so it drives the same `Auto` steps).
//!
//! - `profile-a`: edits its profile (display name, "Sobre mim", colour) and a custom status the way
//!   the settings do, tells B in the chat, then goes "Ausente" through idle detection (a faked idle
//!   time), "Invisível" (B must no longer see it online, though its chat keeps working) and back.
//! - `profile-b`: opens A's card (the popout) and checks, through the app state, that what A wrote
//!   is in it and was drawn; then follows A's presence: away, invisible (gone from the online list
//!   and offline in A's fresh profile), online again.
//!
//! Silent: no sound (`VOX_NO_PLAYBACK=1`), no microphone, and neither joins voice.

use eframe::egui::Pos2;
use vox_proto::{Presence, UserId};

use super::Auto;
use crate::app::{App, Screen};
use crate::profile::{set_fake_idle, ClearAfter};

#[derive(Default)]
pub struct State {
    /// B: A's id.
    other: Option<UserId>,
    name: String,
}

fn log(what: &str) {
    tracing::info!(target: "autotest", "AUTOTEST profiles: {what}");
}

const BIO: &str = "Sobre mim do autotest\nveja https://example.com/perfil";
const STATUS_TEXT: &str = "testando perfis";

impl Auto {
    fn pf_tag(&self, what: &str) -> String {
        format!("pftest {what} {}", self.args.run_id)
    }

    /// Logged in, and the hub has told us whether it knows profiles. `Some(code)` = failed.
    fn pf_login(&mut self, app: &mut App) -> Option<Option<i32>> {
        if app.screen == Screen::Main && app.ready && app.hub_up && app.profile.supported.is_some() {
            if app.profile.supported == Some(false) {
                return Some(self.fail("the hub does not know profiles (an older server)"));
            }
            return Some(None);
        }
        if self.waited(30) || (app.form.error.is_some() && !app.form.busy) {
            return Some(self.fail(&format!("could not log in: {}", app.form.error.clone().unwrap_or_default())));
        }
        None
    }

    pub(super) fn run_profile_a(&mut self, app: &mut App) -> Option<i32> {
        match self.step {
            0 => self.connect(app),
            1 => match self.pf_login(app) {
                Some(Some(code)) => return Some(code),
                Some(None) if app.profile.form.loaded.is_some() => {
                    set_fake_idle(Some(0));
                    // Start clean whatever an earlier run left behind.
                    app.set_presence(Presence::Online);
                    let name = format!("Ana Perfil {}", self.args.run_id.chars().rev().take(4).collect::<String>());
                    let f = &mut app.profile.form;
                    f.display_name = name.clone();
                    f.bio = BIO.to_string();
                    f.accent = Some([237, 66, 69]);
                    self.profile.name = name;
                    app.save_profile();
                    self.goto(2, "saving the profile");
                }
                Some(None) if self.waited(20) => return self.fail("my own profile never loaded"),
                Some(None) | None => {}
            },
            2 => {
                if let Some((false, e)) = &app.profile.form.msg {
                    return self.fail(&format!("the hub refused the profile: {e}"));
                }
                let saved = app.me.as_ref().is_some_and(|m| m.display_name.as_deref() == Some(self.profile.name.as_str()));
                if saved && app.profile.form.msg.as_ref().is_some_and(|(ok, _)| *ok) {
                    let f = &mut app.profile.form;
                    f.status_emoji = "🎮".into();
                    f.status_text = STATUS_TEXT.into();
                    f.clear_after = ClearAfter::Hour1;
                    app.save_status();
                    self.goto(3, "profile saved; setting the status");
                } else if self.waited(20) {
                    return self.fail("the profile was not confirmed");
                }
            }
            3 => {
                if let Some((false, e)) = &app.profile.form.status_msg {
                    return self.fail(&format!("the hub refused the status: {e}"));
                }
                let me = app.my_id().unwrap_or_default();
                let st = app.status_of(me);
                if st.as_ref().is_some_and(|s| s.text == STATUS_TEXT && s.expires_at_ms.is_some()) {
                    let tag = self.pf_tag("set");
                    Self::say(app, &tag);
                    if self.args.show.as_deref() == Some("settings") {
                        // `--show settings`: the settings open on the profile section for `--hold` seconds (a look).
                        app.open_settings();
                        self.goto(30, "status set; settings open on the profile section");
                        return None;
                    }
                    self.goto(4, "status set; told B");
                } else if self.waited(20) {
                    return self.fail(&format!("the status was not confirmed: {st:?}"));
                }
            }
            30 if self.waited(self.args.hold_secs) => {
                app.settings_ui.open = false;
                self.goto(4, "settings closed");
            }
            4 => {
                if Self::saw_message(app, &self.pf_tag("seen")) {
                    // Idle detection: the keyboard and mouse "have been idle for an hour".
                    set_fake_idle(Some(3600));
                    self.goto(5, "B saw my profile; going idle");
                } else if self.waited(60) {
                    return self.fail("B never confirmed my profile");
                }
            }
            5 => {
                if app.my_presence() == Presence::Away && app.profile.auto_away_sent {
                    let tag = self.pf_tag("away");
                    Self::say(app, &tag);
                    self.goto(6, "away through idle detection; told B");
                } else if self.waited(40) {
                    return self.fail(&format!("idle did not make me away (presence {:?})", app.my_presence()));
                }
            }
            6 => {
                if Self::saw_message(app, &self.pf_tag("awayseen")) {
                    set_fake_idle(Some(0));
                    self.goto(7, "B saw me away; back at the keyboard");
                } else if self.waited(40) {
                    return self.fail("B never confirmed my away");
                }
            }
            7 => {
                if app.my_presence() == Presence::Online && !app.profile.auto_away_sent {
                    app.set_presence(Presence::Invisible);
                    self.goto(8, "back online by activity; going invisible");
                } else if self.waited(40) {
                    return self.fail(&format!("activity did not bring me back (presence {:?})", app.my_presence()));
                }
            }
            8 => {
                if app.my_presence() == Presence::Invisible {
                    // Invisible still works: the message reaches B.
                    let tag = self.pf_tag("invisible");
                    Self::say(app, &tag);
                    self.goto(9, "invisible; told B");
                } else if self.waited(20) {
                    return self.fail(&format!("invisible was not confirmed (presence {:?})", app.my_presence()));
                }
            }
            9 => {
                if Self::saw_message(app, &self.pf_tag("gone")) {
                    app.set_presence(Presence::Online);
                    self.goto(10, "B no longer sees me; coming back");
                } else if self.waited(40) {
                    return self.fail("B never confirmed that I was gone");
                }
            }
            10 => {
                if app.my_presence() == Presence::Online {
                    let tag = self.pf_tag("back");
                    Self::say(app, &tag);
                    self.goto(11, "online again; told B");
                } else if self.waited(20) {
                    return self.fail("coming back online was not confirmed");
                }
            }
            11 => {
                if Self::saw_message(app, &self.pf_tag("backseen")) {
                    set_fake_idle(None);
                    return self.pass();
                } else if self.waited(40) {
                    return self.fail("B never confirmed that I was back");
                }
            }
            _ => {}
        }
        None
    }

    pub(super) fn run_profile_b(&mut self, app: &mut App) -> Option<i32> {
        match self.step {
            0 => self.connect(app),
            1 => match self.pf_login(app) {
                Some(Some(code)) => return Some(code),
                Some(None) => {
                    if let Some(ch) = Self::first_text(app) {
                        app.select_text(ch);
                    }
                    self.goto(2, "waiting for A's profile");
                }
                None => {}
            },
            2 => {
                if Self::saw_message(app, &self.pf_tag("set")) {
                    let me = app.my_id();
                    let a = app.logs.values().flat_map(|l| &l.msgs).find(|m| m.content.starts_with(&self.pf_tag("set"))).map(|m| m.author.clone());
                    let Some(a) = a.filter(|a| Some(a.id) != me) else { return self.fail("A's message has no author") };
                    // The author of A's chat message already carries the display name.
                    if !a.has_display_name() || !a.shown_name().starts_with("Ana Perfil") {
                        return self.fail(&format!("A's chat author is {:?}, not the display name", a.shown_name()));
                    }
                    log(&format!("A's chat author shows as {:?} (@{})", a.shown_name(), a.username));
                    self.profile.other = Some(a.id);
                    app.forget_profile(a.id);
                    app.open_profile_at(&a, Pos2::new(420.0, 160.0));
                    self.goto(3, "A's card is open; waiting for the profile to load and the card to draw");
                } else if self.waited(60) {
                    return self.fail("A's profile message never arrived");
                }
            }
            3 => {
                let a = self.profile.other.unwrap_or_default();
                let Some(p) = app.cached_profile(a).cloned() else {
                    if self.waited(20) {
                        return self.fail("A's profile never loaded");
                    }
                    return None;
                };
                let status = p.status.as_ref().map(|s| (s.emoji.as_str(), s.text.as_str(), s.expires_at_ms.is_some()));
                let ok = p.user.display_name.as_deref().is_some_and(|n| n.starts_with("Ana Perfil"))
                    && p.bio == BIO
                    && p.accent.as_deref() == Some("#ed4245")
                    && status == Some(("🎮", STATUS_TEXT, true))
                    && p.member_since_ms > 1_600_000_000_000
                    && p.online;
                if !ok {
                    return self.fail(&format!("A's profile is not what A wrote: {p:?}"));
                }
                let drawn = app.profile.popout_drawn == Some((a, true));
                let open = app.profile.popout.as_ref().is_some_and(|c| c.user.id == a);
                if drawn && open {
                    log(&format!("A's card shows {:?}, the bio, colour {:?}, status {:?}, member since {}", p.user.shown_name(), p.accent, status, p.member_since_ms));
                    let tag = self.pf_tag("seen");
                    Self::say(app, &tag);
                    if self.args.show.as_deref() == Some("card") {
                        // `--show card`: keep the card on screen for `--hold` seconds (a look).
                        self.goto(30, "card checked; holding it on screen");
                        return None;
                    }
                    self.goto(4, "card checked; told A");
                } else if self.waited(20) {
                    return self.fail(&format!("the card was not drawn with the profile (drawn={:?}, open={open})", app.profile.popout_drawn));
                }
            }
            30 if self.waited(self.args.hold_secs) => self.goto(4, "hold over"),
            4 => {
                let a = self.profile.other.unwrap_or_default();
                if Self::saw_message(app, &self.pf_tag("away")) {
                    if app.presence_of(a) == Presence::Away && app.online.iter().any(|u| u.id == a) {
                        log("A is shown as away");
                        let tag = self.pf_tag("awayseen");
                        Self::say(app, &tag);
                        self.goto(5, "A is away; told A");
                    } else if self.waited(30) {
                        return self.fail(&format!("A said away but I see {:?}", app.presence_of(a)));
                    }
                } else if self.waited(60) {
                    return self.fail("A never went away");
                }
            }
            5 => {
                let a = self.profile.other.unwrap_or_default();
                if Self::saw_message(app, &self.pf_tag("invisible")) {
                    let listed = app.online.iter().any(|u| u.id == a);
                    if !listed {
                        // A fresh profile says offline too, and the card follows.
                        app.forget_profile(a);
                        if let Some(u) = app.known_user(a) {
                            app.open_profile_at(&u, Pos2::new(420.0, 160.0));
                        }
                        self.goto(6, "A left my online list; checking the fresh profile");
                    } else if self.waited(20) {
                        return self.fail("A is invisible but still in my online list");
                    }
                } else if self.waited(90) {
                    return self.fail("A never went invisible");
                }
            }
            6 => {
                let a = self.profile.other.unwrap_or_default();
                if let Some(p) = app.cached_profile(a) {
                    if p.online || app.online.iter().any(|u| u.id == a) || app.profile.presence.contains_key(&a) {
                        return self.fail("A is invisible but I can still tell they are online");
                    }
                    let tag = self.pf_tag("gone");
                    Self::say(app, &tag);
                    self.goto(7, "A is offline to me (profile and list); told A");
                } else if self.waited(20) {
                    return self.fail("the profile of invisible A never loaded");
                }
            }
            7 => {
                let a = self.profile.other.unwrap_or_default();
                if Self::saw_message(app, &self.pf_tag("back")) {
                    if app.online.iter().any(|u| u.id == a) && app.presence_of(a) == Presence::Online {
                        let tag = self.pf_tag("backseen");
                        Self::say(app, &tag);
                        self.goto(8, "A is back; told A");
                    } else if self.waited(30) {
                        return self.fail("A came back but is not in my online list");
                    }
                } else if self.waited(60) {
                    return self.fail("A never came back");
                }
            }
            8 if self.waited(3) => return self.pass(),
            _ => {}
        }
        None
    }
}
