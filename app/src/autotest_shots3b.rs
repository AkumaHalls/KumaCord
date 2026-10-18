//! Screenshots of the phase 3b windows (v1.0 design review), a child module of `autotest`.
//!
//! - `shots3b-a` (the owner, "dono"): makes a few channels (one private), an invite and a
//!   moderation line, then saves the server window's five pages (and the "Acesso" editor), a
//!   confirmation dialog, the person menu and a stage card's menu, the profile card and its
//!   editor, a DM's header (and blocked), "Nova mensagem", the incoming call, a toast and the
//!   "Apagar mensagem" dialog.
//! - `shots3b-b` ("lua"): fills in a profile (name, about, status, colour), sits in the voice room
//!   and, on cue, calls the owner (for the ringing window).
//! - `shots3b-host`: hosts a server in the app (the gate's host-mode test environment: test ports,
//!   no router), and saves "Meu servidor", the "Parar servidor" dialog and the home tab.
//!
//! With `VOX_AUTOTEST_SHOT_DIR=<dir>` each screen is `<dir>/<name>.png` (`tools\shots.ps1
//! -Phase3b`). Cues go through #bastidores. Every check has a time limit; the windows stay behind
//! the others the whole time (background test mode).

use std::time::{Duration, Instant};

use eframe::egui;
use vox_proto::{ChannelKind, PrivateAccess, User};

use super::{shots, Auto};
use crate::app::chat_extras::Target;
use crate::app::{App, Screen};
use crate::call::TileKey;
use crate::context_menu::{Lead, Origin};
use crate::host::HomeTab;
use crate::roles::{AccessDraft, AdminCmd, ServerTab};

/// Test values for this throwaway hub.
const OWNER_PW: &str = "shots-dono-123";
const PARTNER: &str = "lua";
const CUES: &str = "bastidores";
const JOGATINA: &str = "jogatina-de-sexta";
const ADM: &str = "administração";
const SALA2: &str = "Sala 2";
const BIO: &str = "Toco baixo nas sextas e organizo a jogatina. Pergunte sobre o placar de ontem!";
const HELLO: &str = "alguém viu o placar de ontem?";

#[derive(Default)]
pub struct State {
    at: std::cell::Cell<Option<Instant>>,
    created: bool,
    peer: Option<User>,
    cues: i64,
    geral: i64,
    jogatina: i64,
    adm: i64,
    sent: u32,
}

fn channel_id(app: &App, name: &str, kind: ChannelKind) -> Option<i64> {
    app.channels.iter().find(|c| c.name == name && c.kind == kind).map(|c| c.id)
}

fn cued(app: &App, ch: i64, text: &str) -> bool {
    app.logs.get(&ch).is_some_and(|l| l.msgs.iter().any(|m| m.content == text))
}

impl Auto {
    /// Wait `secs` in the current step (true once over).
    fn s3_lingered(&self, secs: f32) -> bool {
        let at = self.s3b.at.get().unwrap_or_else(Instant::now);
        self.s3b.at.set(Some(at));
        at.elapsed() >= Duration::from_secs_f32(secs)
    }

    fn s3_next(&mut self, step: u32, what: &str) {
        self.s3b.at.set(None);
        self.goto(step, what);
    }

    fn s3_shot(&mut self, app: &App, name: &str, next: u32) {
        shots::save(app, name);
        self.s3_next(next, name);
    }

    fn s3_cue(&mut self, app: &mut App, text: &str) {
        app.backend.send(crate::backend::Cmd::SendMessage { channel: self.s3b.cues, content: text.to_string(), extras: Default::default() });
    }

    /// Close whatever menu is open.
    fn s3_close_menus(app: &mut App) {
        app.cmenu.open = None;
        egui::Popup::close_all(&app.repaint_ctx());
    }

    // ------------------------------------------------------------ the owner

    pub(super) fn run_shots3b_a(&mut self, app: &mut App) -> Option<i32> {
        let ctx = app.repaint_ctx();
        match self.step {
            0 => {
                app.form.personal = OWNER_PW.into();
                self.connect(app);
            }
            1 => {
                if !self.logged_in(app) {
                    return if self.waited(30) { self.fail("could not log in") } else { None };
                }
                if app.roles.account.is_some_and(|a| a.must_set_password) {
                    if !self.s3b.created {
                        self.s3b.created = true;
                        app.set_personal_password(None, Some(OWNER_PW.into()));
                    }
                    return if self.waited(20) { self.fail("the personal password was not set") } else { None };
                }
                if self.s3b.sent == 0 {
                    self.s3b.sent = 1;
                    for (name, kind, private) in [
                        (JOGATINA, ChannelKind::Text, None),
                        (CUES, ChannelKind::Text, None),
                        (ADM, ChannelKind::Text, Some(PrivateAccess::default())),
                        (SALA2, ChannelKind::Voice, None),
                    ] {
                        if channel_id(app, name, kind).is_none() {
                            app.admin(AdminCmd::CreateChannel { name: name.into(), kind, private });
                        }
                    }
                }
                let ids = (Self::first_text(app), channel_id(app, JOGATINA, ChannelKind::Text), channel_id(app, CUES, ChannelKind::Text), channel_id(app, ADM, ChannelKind::Text), channel_id(app, SALA2, ChannelKind::Voice));
                match ids {
                    (Some(g), Some(j), Some(c), Some(a), Some(_)) => {
                        (self.s3b.geral, self.s3b.jogatina, self.s3b.cues, self.s3b.adm) = (g, j, c, a);
                        app.select_text(c);
                        app.select_text(g);
                        self.s3_next(2, "channels ready; waiting for lua");
                    }
                    _ if self.waited(30) => return self.fail("could not create the channels"),
                    _ => {}
                }
            }
            2 => {
                if !cued(app, self.s3b.cues, "pronto") {
                    return if self.waited(120) { self.fail("lua never got ready") } else { None };
                }
                let Some(peer) = app.online.iter().find(|u| u.username == PARTNER).cloned() else { return self.fail("lua is not online") };
                self.s3b.peer = Some(peer.clone());
                Self::say(app, HELLO);
                // The moderation log gets a line or two, and the invite page a code.
                app.admin(AdminCmd::Mute { user: peer.id, muted: true });
                app.admin(AdminCmd::Mute { user: peer.id, muted: false });
                app.create_invite();
                if let Some(vc) = Self::first_voice(app) {
                    app.join_voice(vc);
                }
                self.s3_next(3, "joining the room; opening the server window");
            }
            3 => {
                if app.voice.channel.is_none() || app.members_here().len() < 2 {
                    return if self.waited(60) { self.fail("could not join the room with lua") } else { None };
                }
                app.select_text(self.s3b.geral);
                app.set_center_mode(crate::layout::CenterMode::Chat);
                app.open_server_settings();
                app.roles.tab = ServerTab::Members;
                self.s3_next(4, "server window: Membros e cargos");
            }
            4 if app.roles.members_loaded && self.s3_lingered(2.0) => self.s3_shot(app, "admin-membros", 5),
            5 => {
                app.roles.tab = ServerTab::Channels;
                self.s3_next(6, "Canais");
            }
            6 if self.s3_lingered(1.5) => {
                shots::save(app, "admin-canais");
                let draft = AccessDraft::of(app.channels.iter().find(|c| c.id == self.s3b.adm).and_then(|c| c.private.as_ref()));
                app.roles.editing_access = Some((self.s3b.adm, draft));
                self.s3_next(7, "the Acesso editor of the private channel");
            }
            7 if self.s3_lingered(1.5) => {
                shots::save(app, "admin-canais-acesso");
                app.roles.editing_access = None;
                app.roles.tab = ServerTab::Invites;
                self.s3_next(8, "Convites");
            }
            8 if (app.host.inv.last.is_some() || self.waited(10)) && self.s3_lingered(1.5) => self.s3_shot(app, "admin-convites", 9),
            9 => {
                app.roles.tab = ServerTab::Audit;
                app.admin(AdminCmd::Audit { before: None });
                self.s3_next(10, "Registro de moderação");
            }
            10 if app.roles.audit_loaded && self.s3_lingered(1.5) => self.s3_shot(app, "admin-registro", 11),
            11 => {
                app.roles.tab = ServerTab::Settings;
                self.s3_next(12, "Configurações do servidor");
            }
            12 if self.s3_lingered(1.5) => {
                shots::save(app, "admin-config");
                app.roles.tab = ServerTab::Channels;
                app.roles.confirm_delete = Some(self.s3b.jogatina);
                self.s3_next(13, "a confirmation over the server window");
            }
            13 if self.s3_lingered(1.5) => {
                shots::save(app, "dialogo-apagar-canal");
                app.roles.confirm_delete = None;
                app.roles.server_open = false;
                let peer = self.s3b.peer.clone()?;
                app.open_person_menu(&ctx, &peer, Origin::Sidebar, Lead::None, egui::pos2(150.0, 330.0));
                self.s3_next(14, "lua's menu from the sidebar");
            }
            14 if self.s3_lingered(1.5) => {
                if app.cmenu.open.is_none() {
                    return self.fail("the person menu did not open");
                }
                shots::save(app, "menu-pessoa");
                Self::s3_close_menus(app);
                app.show_call();
                app.set_center_mode(crate::layout::CenterMode::Cards);
                self.s3_next(15, "the stage; lua's card menu");
            }
            15 if self.s3_lingered(2.0) => {
                let peer = self.s3b.peer.clone()?;
                let Some(card) = app.call.drawn.iter().find(|d| d.key == TileKey::Member(peer.id)).map(|d| d.rect) else {
                    return if self.waited(20) { self.fail("lua's card is not on the stage") } else { None };
                };
                let pos = egui::pos2(card.center().x, card.top() + 14.0);
                if !app.open_card_menu(&ctx, TileKey::Member(peer.id), pos) {
                    return self.fail("the card's menu did not open");
                }
                self.s3_next(16, "card menu open");
            }
            16 if self.s3_lingered(1.5) => {
                shots::save(app, "menu-card");
                Self::s3_close_menus(app);
                app.set_center_mode(crate::layout::CenterMode::Chat);
                let peer = self.s3b.peer.clone()?;
                app.open_profile_at(&peer, egui::pos2(300.0, 120.0));
                self.s3_next(17, "lua's profile card");
            }
            17 => {
                let peer = self.s3b.peer.clone()?;
                if app.profile.popout_drawn != Some((peer.id, true)) {
                    return if self.waited(20) { self.fail("lua's card never showed the profile") } else { None };
                }
                if self.s3_lingered(1.5) {
                    shots::save(app, "perfil-cartao");
                    app.close_profile();
                    app.open_settings_at(crate::app::SettingsPage::Account);
                    self.s3_next(18, "the profile editor (Configurações)");
                }
            }
            18 if self.s3_lingered(2.5) => {
                shots::save(app, "perfil-editor");
                app.settings_ui.open = false;
                let peer = self.s3b.peer.clone()?;
                app.open_dm(peer.id);
                self.s3_next(19, "a DM with lua");
            }
            19 => {
                if app.dms.selected.is_none() {
                    return if self.waited(20) { self.fail("the DM did not open") } else { None };
                }
                if self.s3b.sent < 2 && self.s3_lingered(1.0) {
                    self.s3b.sent = 2;
                    app.dms.input = "oi! você vem hoje?".into();
                    app.send_dm();
                }
                if self.s3_lingered(3.0) {
                    shots::save(app, "dm-cabecalho");
                    let peer = self.s3b.peer.clone()?;
                    app.block_user(&peer, true);
                    self.s3_next(20, "lua blocked (the banner)");
                }
            }
            20 => {
                let blocked = app.dms.selected.and_then(|d| app.dms.conv(d)).is_some_and(|c| c.blocked);
                if !blocked {
                    return if self.waited(20) { self.fail("the block did not show") } else { None };
                }
                if self.s3_lingered(1.5) {
                    shots::save(app, "dm-bloqueado");
                    let peer = self.s3b.peer.clone()?;
                    app.block_user(&peer, false);
                    app.open_dm_picker();
                    self.s3_next(21, "Nova mensagem");
                }
            }
            21 if !app.dms.picker.loading && self.s3_lingered(1.5) => {
                shots::save(app, "nova-mensagem");
                app.dms.picker.open = false;
                self.s3_cue(app, "ligar");
                self.s3_next(22, "lua calls me");
            }
            22 => {
                if app.dms.incoming(app.my_id()).is_none() {
                    return if self.waited(40) { self.fail("lua's call never rang") } else { None };
                }
                if self.s3_lingered(1.5) {
                    shots::save(app, "chamada-recebida");
                    app.decline_dm_call();
                    app.select_text(self.s3b.geral);
                    app.toast(format!("Copiado: {PARTNER}"));
                    self.s3_next(23, "a toast");
                }
            }
            23 if self.s3_lingered(1.0) => {
                shots::save(app, "aviso");
                app.call.toast = None;
                let mine = app.logs.get(&self.s3b.geral).and_then(|l| l.msgs.iter().rev().find(|m| m.content == HELLO).map(|m| m.id));
                let Some(id) = mine else { return self.fail("my message is gone") };
                app.ask_delete(Target::Channel(self.s3b.geral), id, false);
                self.s3_next(24, "the Apagar mensagem dialog");
            }
            24 if self.s3_lingered(1.5) => {
                shots::save(app, "dialogo-apagar-mensagem");
                app.chatx.confirm_delete = None;
                self.s3_cue(app, "fim");
                self.s3_next(25, "done");
            }
            25 if self.s3_lingered(1.0) => return self.pass(),
            _ => {}
        }
        if self.waited(90) && self.step > 2 {
            return self.fail(&format!("stuck at step {}", self.step));
        }
        None
    }

    // ------------------------------------------------------------ the partner

    pub(super) fn run_shots3b_b(&mut self, app: &mut App) -> Option<i32> {
        match self.step {
            0 => self.connect(app),
            1 => {
                if !self.logged_in(app) || app.profile.form.loaded.is_none() {
                    return if self.waited(30) { self.fail("could not log in") } else { None };
                }
                app.set_presence(vox_proto::Presence::Online);
                let f = &mut app.profile.form;
                f.display_name = "Lua".into();
                f.bio = BIO.into();
                f.accent = Some([0x8E, 0x6B, 0xD8]);
                app.save_profile();
                self.s3_next(2, "saving my profile");
            }
            2 => {
                if app.profile.form.msg.as_ref().is_some_and(|(ok, _)| *ok) {
                    let f = &mut app.profile.form;
                    f.status_emoji = "🎧".into();
                    f.status_text = "ouvindo o ensaio da banda".into();
                    app.save_status();
                    self.s3_next(3, "setting my status");
                } else if self.waited(20) {
                    return self.fail(&format!("the profile was not saved: {:?}", app.profile.form.msg));
                }
            }
            3 => {
                let ok = app.profile.form.status_msg.as_ref().is_some_and(|(ok, _)| *ok);
                let cues = channel_id(app, CUES, ChannelKind::Text);
                match (ok, cues, Self::first_voice(app)) {
                    (true, Some(c), Some(vc)) => {
                        self.s3b.cues = c;
                        app.select_text(c);
                        app.join_voice(vc);
                        self.s3_next(4, "joining the room");
                    }
                    _ if self.waited(60) => return self.fail("the status or the owner's channels never came"),
                    _ => {}
                }
            }
            4 => {
                if app.voice.channel.is_none() {
                    return if self.waited(60) { self.fail("could not join the room") } else { None };
                }
                if self.s3_lingered(1.0) {
                    self.s3_cue(app, "pronto");
                    self.s3_next(5, "ready; waiting for cues");
                }
            }
            5 => {
                if cued(app, self.s3b.cues, "ligar") && self.s3b.sent == 0 {
                    if let Some(owner) = app.online.iter().find(|u| u.username == "dono").map(|u| u.id) {
                        self.s3b.sent = 1;
                        app.call_user(owner);
                    }
                }
                if cued(app, self.s3b.cues, "fim") {
                    return self.pass();
                }
                if self.waited(300) {
                    return self.fail("the owner never finished");
                }
            }
            _ => {}
        }
        None
    }

    // ------------------------------------------------------------ hosting

    pub(super) fn run_shots3b_host(&mut self, app: &mut App) -> Option<i32> {
        match self.step {
            0 => {
                app.form.user = self.args.name.clone();
                app.host.tab = HomeTab::Host;
                app.host_create("Turma da Lua");
                if let Some(e) = &app.host.error {
                    return self.fail(&format!("could not create the server: {e}"));
                }
                self.s3_next(1, "server created, opening it");
            }
            1 => {
                if !self.logged_in(app) {
                    return if self.waited(90) { self.fail(&format!("the hosted server did not come up: {:?}", app.form.error)) } else { None };
                }
                if app.roles.account.is_some_and(|a| a.must_set_password) {
                    if !self.s3b.created {
                        self.s3b.created = true;
                        app.set_personal_password(None, Some(OWNER_PW.into()));
                    }
                    return if self.waited(20) { self.fail("the personal password was not set") } else { None };
                }
                app.host.panel_open = true;
                app.create_invite();
                self.s3_next(2, "Meu servidor, with an invite");
            }
            2 => {
                if app.host.inv.last.is_none() {
                    return if self.waited(20) { self.fail(&format!("no invite: {:?}", app.host.inv.error)) } else { None };
                }
                if self.s3_lingered(2.0) {
                    shots::save(app, "host-painel");
                    app.host.confirm_stop = true;
                    self.s3_next(3, "the Parar servidor dialog");
                }
            }
            3 if self.s3_lingered(1.5) => {
                shots::save(app, "dialogo-parar-servidor");
                app.host.confirm_stop = false;
                app.host.panel_open = false;
                app.host_stop();
                self.s3_next(4, "stopped; the home tab");
            }
            4 => {
                if app.screen != Screen::Connect || app.host.hosting() {
                    return if self.waited(30) { self.fail("the server did not stop") } else { None };
                }
                app.host.tab = HomeTab::Host;
                app.refresh_servers();
                if self.s3_lingered(2.0) {
                    shots::save(app, "host-inicio");
                    return self.pass();
                }
            }
            _ => {}
        }
        None
    }
}
