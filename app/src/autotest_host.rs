//! Host mode and invites, end to end in real apps (v0.8; a child module of `autotest`).
//!
//! - `host-a`: creates a server in host mode ("Criar servidor"), which runs inside this app on the
//!   test ports (`VOX_HOST_FIRST_BLOCK=1`: 7990 / 7980-7982) with the network part off
//!   (`VOX_HOST_OFFLINE=1`: the router is never touched), logs into it through 127.0.0.1 (and so
//!   becomes its Dono), sets its personal password, makes an invite and writes the code to
//!   `<--mode dir>/invite.txt`. Once B has chatted and joined voice, A joins too, then revokes the
//!   invite (`revoked.txt`), waits for C's verdict (`c-done.txt`) and stops the server.
//! - `host-b`: has only the code: joins with it, chats, joins voice, sees A there.
//! - `host-c`: after the revocation, tries the same code and must be refused.
//!
//! The launcher sets `VOX_HOST_DIR` (a fresh folder), `VOX_HOST_BIND=127.0.0.1`,
//! `VOX_HOST_LAN=127.0.0.1/8` (everyone is on this PC) and `VOX_HOST_LIVEKIT_BIN` (the checked-out
//! `bin\livekit-server.exe`, used in place and still checked against the pinned SHA-256).

use std::path::PathBuf;

use vox_proto::{InviteState, Role};

use super::Auto;
use crate::app::{App, Screen};
use crate::host::{HomeTab, InviteCmd};

const OWNER_PW: &str = "autotest-host-dono";

#[derive(Default)]
pub struct State {
    invite: Option<i64>,
}

fn log(what: &str) {
    tracing::info!(target: "autotest", "AUTOTEST host: {what}");
}

impl Auto {
    fn host_dir(&self) -> PathBuf {
        PathBuf::from(self.args.mode.clone().unwrap_or_else(|| ".".into()))
    }

    fn host_tag(&self, what: &str) -> String {
        format!("hosttest {what} {}", self.args.run_id)
    }

    pub(super) fn run_host_a(&mut self, app: &mut App) -> Option<i32> {
        match self.step {
            0 => {
                app.form.user = self.args.name.clone();
                app.host.tab = HomeTab::Host;
                app.host_create(&format!("Servidor de teste {}", self.args.run_id));
                if let Some(e) = &app.host.error {
                    return self.fail(&format!("could not create the server: {e}"));
                }
                self.goto(1, "server created, opening it");
            }
            1 => {
                if let Some(e) = &app.host.error {
                    return self.fail(&format!("the server did not start: {e}"));
                }
                if self.logged_in(app) {
                    let st = app.host.status();
                    let me = app.me.clone().map(|u| (u.username, u.role));
                    log(&format!("running {:?} on {:?}, logged in as {me:?}", st.as_ref().map(|s| s.ports), st.as_ref().and_then(|s| s.hub_addr)));
                    if me.as_ref().map(|m| m.1) != Some(Role::Owner) {
                        return self.fail("the host is not the Dono of its own server");
                    }
                    if app.roles.account.is_some_and(|a| a.must_set_password) {
                        app.set_personal_password(None, Some(OWNER_PW.into()));
                        self.goto(2, "setting the Dono's personal password");
                    } else {
                        self.goto(3, "Dono already protected");
                    }
                } else if self.waited(90) || (app.form.error.is_some() && !app.form.busy && app.screen == Screen::Connect && !app.host.hosting()) {
                    return self.fail(&format!("the hosted server did not come up: {:?} / {:?}", app.form.error, app.host.status().map(|s| s.phase)));
                }
            }
            2 => {
                if let Some((false, e)) = &app.roles.pw_msg {
                    return self.fail(&format!("personal password failed: {e}"));
                }
                if app.roles.account.is_some_and(|a| a.has_password && !a.must_set_password) {
                    self.goto(3, "personal password set");
                } else if self.waited(15) {
                    return self.fail("the personal password was not confirmed");
                }
            }
            3 => {
                app.host.panel_open = true;
                app.host.inv.expiry = 1; // 24 horas
                app.host.inv.uses = 1; // 5 pessoas
                app.create_invite();
                self.goto(4, "creating an invite");
            }
            4 => {
                if let Some(e) = &app.host.inv.error {
                    return self.fail(&format!("invite failed: {e}"));
                }
                if let Some((id, code)) = app.host.inv.last.clone() {
                    let dir = self.host_dir();
                    if let Err(e) = std::fs::write(dir.join("invite.txt"), &code) {
                        return self.fail(&format!("cannot write the code: {e}"));
                    }
                    self.host.invite = Some(id);
                    log(&format!("invite #{id}: {} chars", code.len()));
                    self.goto(5, "waiting for B (chat and voice)");
                } else if self.waited(15) {
                    return self.fail("no invite came back");
                }
            }
            5 => {
                let me = app.my_id();
                let b = app.voice_members.values().find(|m| Some(m.user.id) != me && m.channel_id.is_some()).cloned();
                if let Some(b) = b.filter(|_| Self::saw_message(app, &self.host_tag("hello"))) {
                    log(&format!("{} chatted and is in voice; online here: {}", b.user.username, app.online.len()));
                    app.join_voice(b.channel_id.unwrap_or_default());
                    self.goto(6, "joining B in voice");
                } else if self.waited(150) {
                    return self.fail("B never chatted and joined voice");
                }
            }
            6 => {
                if app.voice.channel.is_some() {
                    let tag = self.host_tag("ok");
                    Self::say(app, &tag);
                    self.goto(7, "both in voice");
                } else if self.waited(45) {
                    return self.fail(&format!("could not join voice: {:?}", app.voice.error));
                }
            }
            7 if self.waited(4) => {
                let Some(id) = self.host.invite else { return self.fail("no invite id") };
                app.invite_cmd(InviteCmd::Revoke(id));
                self.goto(8, "revoking the invite");
            }
            8 => {
                let revoked = app.host.inv.list.iter().any(|i| Some(i.id) == self.host.invite && i.state(crate::app::now_ms()) == InviteState::Revoked);
                if revoked {
                    let _ = std::fs::write(self.host_dir().join("revoked.txt"), "revoked");
                    self.goto(9, "revoked; waiting for C's attempt");
                } else if let Some(e) = &app.host.inv.error {
                    return self.fail(&format!("revoke failed: {e}"));
                } else if self.waited(15) {
                    return self.fail("the invite did not show as revoked");
                }
            }
            9 => {
                let done = std::fs::read_to_string(self.host_dir().join("c-done.txt")).ok();
                if let Some(verdict) = done {
                    log(&format!("C says: {verdict}"));
                    if !verdict.starts_with("refused") {
                        return self.fail(&format!("C was not refused: {verdict}"));
                    }
                    app.leave_voice();
                    app.host_stop();
                    self.goto(10, "stopping the server");
                } else if self.waited(150) {
                    return self.fail("C never reported");
                }
            }
            10 => {
                if app.host.runner.is_none() && !app.host.hosting() {
                    log("server stopped");
                    return self.pass();
                } else if self.waited(20) {
                    return self.fail(&format!("the server did not stop: {:?}", app.host.status().map(|s| s.phase)));
                }
            }
            _ => {}
        }
        None
    }

    pub(super) fn run_host_b(&mut self, app: &mut App) -> Option<i32> {
        match self.step {
            0 => {
                let Ok(code) = std::fs::read_to_string(self.host_dir().join("invite.txt")) else {
                    if self.waited(180) {
                        return self.fail("no invite code from A");
                    }
                    return None;
                };
                log(&format!("got a {}-char code", code.trim().len()));
                app.form.url = code.trim().to_string();
                app.form.user = self.args.name.clone();
                app.form.password.clear();
                app.connect();
                self.goto(1, "joining with only the invite");
            }
            1 => {
                if self.logged_in(app) {
                    let me = app.me.clone().map(|u| (u.username, u.role));
                    log(&format!("joined {} at {} as {me:?}", app.server_name, app.form.url));
                    if vox_proto::looks_like_invite(&app.form.url) {
                        return self.fail("the address field still shows the code");
                    }
                    if app.settings.device_token(&crate::backend::normalize_url(&app.session_url), &self.args.name).is_none() {
                        return self.fail("no device token kept for next time");
                    }
                    let tag = self.host_tag("hello");
                    Self::say(app, &tag);
                    let Some(vc) = Self::first_voice(app) else { return self.fail("no voice channel") };
                    app.join_voice(vc);
                    self.goto(2, "chatted; joining voice");
                } else if self.waited(40) || (app.form.error.is_some() && !app.form.busy) {
                    return self.fail(&format!("could not join with the invite: {:?}", app.form.error));
                }
            }
            2 => {
                if app.voice.channel.is_some() && Self::saw_message(app, &self.host_tag("ok")) {
                    let a_here = app.voice_members.values().filter(|m| m.channel_id == app.voice.channel).count();
                    log(&format!("in voice with {a_here} people"));
                    if a_here < 2 {
                        return self.fail("A is not in the voice channel");
                    }
                    self.goto(3, "A joined");
                } else if self.waited(120) {
                    return self.fail(&format!("no voice together with A: {:?}", app.voice.error));
                }
            }
            3 if self.waited(2) => {
                app.leave_voice();
                return self.pass();
            }
            _ => {}
        }
        None
    }

    pub(super) fn run_host_c(&mut self, app: &mut App) -> Option<i32> {
        match self.step {
            0 => {
                if !self.host_dir().join("revoked.txt").exists() {
                    if self.waited(300) {
                        return self.fail("A never revoked");
                    }
                    return None;
                }
                let code = std::fs::read_to_string(self.host_dir().join("invite.txt")).unwrap_or_default();
                app.form.url = code.trim().to_string();
                app.form.user = self.args.name.clone();
                app.connect();
                self.goto(1, "trying the revoked invite");
            }
            1 => {
                let report = |v: &str| {
                    let _ = std::fs::write(self.host_dir().join("c-done.txt"), v);
                };
                if self.logged_in(app) {
                    report("joined (wrong)");
                    return self.fail("joined with a revoked invite");
                }
                if let Some(e) = app.form.error.clone().filter(|_| !app.form.busy) {
                    log(&format!("refused: {e}"));
                    if !e.contains("cancelado") {
                        report(&format!("wrong message: {e}"));
                        return self.fail(&format!("unexpected message: {e}"));
                    }
                    report(&format!("refused: {e}"));
                    return self.pass();
                }
                if self.waited(40) {
                    report("timeout");
                    return self.fail("no answer");
                }
            }
            _ => {}
        }
        None
    }
}
