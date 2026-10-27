//! Item 12 in the app: roles, the personal password, moderation and "Configurações do servidor".
//!
//! The hub enforces every permission; this module only shows what the user's role allows (the
//! matrix lives in `vox_proto::Role`). On a hub without roles (`Ready` has no `account`), none of
//! it is shown: no badges, no moderation menus, no message deletion, no server dialog.

use eframe::egui::{self, Color32, RichText, Sense, Vec2};
use vox_client_core::hub::HubClient;
use vox_proto::{
    Account, AuditEntry, ChannelId, ChannelKind, ClaimOwnerResponse, CustomRole, CustomRoleRequest, MemberInfo, MessageId, ModAction, PrivateAccess, ResetPasswordResponse, Role,
    ServerEvent, SetPasswordRequest, SetPasswordResponse, SetRoleRequest, SetUserCustomRolesRequest, TransferOwnerRequest, User, UserId, perm,
    PERSONAL_PASSWORD_MAX, PERSONAL_PASSWORD_MIN,
};

use crate::app::{App, Center};
use crate::backend::Cmd;
use crate::theme::{self, ph, radius, size, space};
use crate::ui_kit::{self, IconState, Kind, Size};

// v1.0: the role colours stay away from the lamp (gold = speaking / main action / mentions) and
// from "ao vivo" red: lavender, rose and teal, soft enough for the night-room palette.
const OWNER_COLOR: Color32 = Color32::from_rgb(0xC3, 0xA6, 0xF5);
const ADMIN_COLOR: Color32 = Color32::from_rgb(0xE9, 0x9A, 0xC0);
const MOD_COLOR: Color32 = Color32::from_rgb(0x62, 0xC6, 0xD0);
/// Audit log page size.
const AUDIT_PAGE: u32 = 100;

pub fn role_label(role: Role) -> &'static str {
    match role {
        Role::Owner => "Dono",
        Role::Admin => "Admin",
        Role::Moderator => "Moderador",
        Role::Member => "Membro",
    }
}

/// Staff names are drawn in their role's color (members keep their usual color).
pub fn role_color(role: Role) -> Option<Color32> {
    match role {
        Role::Owner => Some(OWNER_COLOR),
        Role::Admin => Some(ADMIN_COLOR),
        Role::Moderator => Some(MOD_COLOR),
        Role::Member => None,
    }
}

pub(crate) fn role_icon(role: Role) -> Option<&'static str> {
    match role {
        Role::Owner => Some(crate::theme::ph::CROWN),
        Role::Admin => Some(crate::theme::ph::SHIELD),
        Role::Moderator => Some(crate::theme::ph::GAVEL),
        Role::Member => None,
    }
}

/// A small icon after a staff member's name (nothing for members).
pub fn role_badge(ui: &mut egui::Ui, role: Role) {
    if let (Some(icon), Some(color)) = (role_icon(role), role_color(role)) {
        ui.label(RichText::new(icon).small().color(color)).on_hover_text(role_label(role));
    }
}

/// `name` in its role's color, or `fallback` for members.
pub fn role_name(name: &str, role: Role, fallback: Option<Color32>) -> RichText {
    let t = RichText::new(name);
    match role_color(role).or(fallback) {
        Some(c) => t.color(c),
        None => t,
    }
}

fn audit_action_pt(action: &str) -> String {
    match action {
        vox_proto::AUDIT_SERVER_MUTE => "silenciou no servidor".into(),
        vox_proto::AUDIT_SERVER_UNMUTE => "liberou o microfone de".into(),
        vox_proto::AUDIT_DISCONNECT => "desconectou da voz".into(),
        vox_proto::AUDIT_MOVE => "moveu".into(),
        vox_proto::AUDIT_STOP_STREAM => "encerrou a transmissão de".into(),
        vox_proto::AUDIT_DELETE_MESSAGE => "apagou uma mensagem de".into(),
        vox_proto::AUDIT_SET_ROLE => "mudou o cargo de".into(),
        vox_proto::AUDIT_RESET_PASSWORD => "redefiniu a senha pessoal de".into(),
        vox_proto::AUDIT_CHANNEL_CREATE => "criou o canal".into(),
        vox_proto::AUDIT_CHANNEL_RENAME => "renomeou o canal".into(),
        vox_proto::AUDIT_CHANNEL_DELETE => "apagou o canal".into(),
        vox_proto::AUDIT_CHANNEL_REORDER => "reordenou os canais".into(),
        vox_proto::AUDIT_CHANNEL_ACCESS => "mudou o acesso do canal".into(),
        vox_proto::AUDIT_REMOVE_AVATAR => "removeu a foto de perfil de".into(),
        vox_proto::AUDIT_CLEAR_PROFILE => "limpou o perfil de".into(),
        vox_proto::AUDIT_CLAIM_OWNER => "virou o Dono do servidor".into(),
        vox_proto::AUDIT_TRANSFER_OWNER => "transferiu a posse do servidor para".into(),
        vox_proto::AUDIT_RESET_OWNER => "tirou o dono do servidor (linha de comando), que era".into(),
        vox_proto::AUDIT_SOUND_DELETE => "apagou um som do soundboard de".into(),
        vox_proto::AUDIT_SOUND_RENAME => "renomeou um som do soundboard de".into(),
        vox_proto::AUDIT_INVITE_CREATE => "criou o convite".into(),
        vox_proto::AUDIT_INVITE_REVOKE => "revogou o convite".into(),
        vox_proto::AUDIT_INVITE_JOIN => "entrou com o convite".into(),
        vox_proto::AUDIT_SET_BADGES => "mudou as badges de".into(),
        vox_proto::AUDIT_BAN => "baniu".into(),
        vox_proto::AUDIT_UNBAN => "removeu o banimento de".into(),
        vox_proto::AUDIT_ROLE_CREATE => "criou o cargo".into(),
        vox_proto::AUDIT_ROLE_UPDATE => "editou o cargo".into(),
        vox_proto::AUDIT_ROLE_DELETE => "apagou o cargo".into(),
        vox_proto::AUDIT_ROLE_ASSIGN => "mudou os cargos de".into(),
        other => other.to_string(),
    }
}

/// The hub's "#3 86400s 5x" ("sem prazo" / "ilimitado" when there is no limit) -> "#3 · vale 24
/// horas · até 5 pessoas".
fn invite_detail_pt(detail: &str) -> String {
    let detail = detail.replace("sem prazo", "sem-prazo");
    let mut out = Vec::new();
    for p in detail.split_whitespace() {
        let secs = p.strip_suffix('s').and_then(|n| n.parse::<u64>().ok());
        let uses = p.strip_suffix('x').and_then(|n| n.parse::<u32>().ok());
        out.push(match (secs, uses, p) {
            (Some(secs), _, _) => match crate::host::EXPIRY.iter().find(|(e, _)| *e == Some(secs)) {
                Some((_, label)) => format!("vale {label}"),
                None if secs % 3600 == 0 => format!("vale {} h", secs / 3600),
                None => format!("vale {secs} s"),
            },
            (_, Some(1), _) => "1 pessoa".to_string(),
            (_, Some(n), _) => format!("até {n} pessoas"),
            (_, _, "sem-prazo") => "sem prazo".to_string(),
            (_, _, "ilimitado") => "sem limite de pessoas".to_string(),
            (_, _, other) => other.to_string(),
        });
    }
    out.join(" · ")
}

/// "member -> moderator" -> "de Membro para Moderador" (the font has no arrow glyph).
fn audit_detail_pt(action: &str, detail: &str) -> String {
    if action == vox_proto::AUDIT_CLAIM_OWNER {
        return match detail {
            "pc-do-servidor" => "entrando pelo próprio PC do servidor".into(),
            "codigo" => "com o código do servidor".into(),
            other => other.to_string(),
        };
    }
    if action == vox_proto::AUDIT_INVITE_CREATE {
        return invite_detail_pt(detail);
    }
    if action == vox_proto::AUDIT_SET_ROLE {
        if let Some((a, b)) = detail.split_once(" -> ") {
            return format!("de {} para {}", role_label(Role::from_wire(a)), role_label(Role::from_wire(b)));
        }
    }
    match detail.split_once(" -> ") {
        Some((a, b)) => format!("de {a} para {b}"),
        None => detail.to_string(),
    }
}

// ---------------------------------------------------------------- backend side

/// The item-12 HTTP calls, run by the backend ([`Cmd::Admin`]); the answer comes back as
/// `Ev::Admin` with the same command.
#[derive(Debug, Clone)]
pub enum AdminCmd {
    SetPassword(SetPasswordRequest),
    Mute { user: UserId, muted: bool },
    Disconnect { user: UserId },
    Move { user: UserId, channel: ChannelId },
    StopStream { user: UserId },
    DeleteMessage { id: MessageId },
    ListUsers,
    SetRole { user: UserId, role: Role },
    /// v1.1: replace the member's manual badge grants (the whole mask at once).
    SetBadges { user: UserId, flags: u64 },
    /// v1.1: ban with a reason (what the login refusal and the audit row tell).
    Ban { user: User, reason: String },
    /// v1.1: remove the ban.
    Unban { user: UserId },
    /// v1.1: create a role (the reply carries it with the id).
    CreateCustomRole { name: String, color: String, position: i64, allow: u64 },
    /// v1.1: edit everything about one role.
    UpdateCustomRole { role: CustomRole },
    /// v1.1: delete a role (the memberships go with it).
    DeleteCustomRole { id: i64 },
    /// v1.1: a member's whole set of custom roles at once.
    SetMemberCustomRoles { user: UserId, role_ids: Vec<i64> },
    ResetPassword { user: User },
    Audit { before: Option<i64> },
    /// v0.9: `private` makes it private from the start.
    CreateChannel { name: String, kind: ChannelKind, private: Option<PrivateAccess> },
    /// "Canal privado" (v0.9): private with this list, or public again (`None`).
    SetChannelAccess { id: ChannelId, private: Option<PrivateAccess> },
    RenameChannel { id: ChannelId, name: String },
    DeleteChannel { id: ChannelId },
    ReorderChannels { ids: Vec<ChannelId> },
    /// "Reivindicar servidor": take a server with no owner with the code its console shows.
    ClaimOwner { code: String },
    /// "Transferir posse do servidor" (owner only).
    TransferOwner { user: User },
}

#[derive(Debug)]
pub enum AdminReply {
    Done,
    Password(SetPasswordResponse),
    Users(Vec<MemberInfo>),
    Audit(Vec<AuditEntry>),
    ResetCode(ResetPasswordResponse),
    Claimed(ClaimOwnerResponse),
    /// v1.1: the freshly created custom role (with its id).
    CustomRole(CustomRole),
}

pub async fn run_admin(hub: &HubClient, cmd: &AdminCmd) -> Result<AdminReply, String> {
    use AdminCmd as C;
    let r = match cmd.clone() {
        C::SetPassword(req) => hub.set_password(&req).await.map(AdminReply::Password),
        C::Mute { user, muted } => hub.mod_mute(user, muted).await.map(|_| AdminReply::Done),
        C::Disconnect { user } => hub.mod_disconnect(user).await.map(|_| AdminReply::Done),
        C::Move { user, channel } => hub.mod_move(user, channel).await.map(|_| AdminReply::Done),
        C::StopStream { user } => hub.mod_stop_stream(user).await.map(|_| AdminReply::Done),
        C::DeleteMessage { id } => hub.delete_message(id).await.map(|_| AdminReply::Done),
        C::ListUsers => hub.admin_users().await.map(AdminReply::Users),
        C::SetRole { user, role } => hub.set_role(user, role).await.map(|_| AdminReply::Done),
        C::SetBadges { user, flags } => hub.set_badges(user, flags).await.map(|_| AdminReply::Done),
        C::Ban { user, reason } => hub.ban_user(user.id, &reason).await.map(|_| AdminReply::Done),
        C::Unban { user } => hub.unban_user(user).await.map(|_| AdminReply::Done),
        C::CreateCustomRole { name, color, position, allow } => hub.create_custom_role(&name, &color, position, allow).await.map(AdminReply::CustomRole),
        C::UpdateCustomRole { role } => hub.update_custom_role(&role).await.map(|_| AdminReply::Done),
        C::DeleteCustomRole { id } => hub.delete_custom_role(id).await.map(|_| AdminReply::Done),
        C::SetMemberCustomRoles { user, role_ids } => hub.set_custom_roles(user, &role_ids).await.map(|_| AdminReply::Done),
        C::ResetPassword { user } => hub.reset_password(user.id).await.map(AdminReply::ResetCode),
        C::Audit { before } => hub.audit_log(before, AUDIT_PAGE).await.map(AdminReply::Audit),
        C::CreateChannel { name, kind, private } => hub.create_channel(&name, kind, private).await.map(|_| AdminReply::Done),
        C::SetChannelAccess { id, private } => hub.set_channel_access(id, private).await.map(|_| AdminReply::Done),
        C::RenameChannel { id, name } => hub.rename_channel(id, &name).await.map(|_| AdminReply::Done),
        C::DeleteChannel { id } => hub.delete_channel(id).await.map(|_| AdminReply::Done),
        C::ReorderChannels { ids } => hub.reorder_channels(ids).await.map(|_| AdminReply::Done),
        C::ClaimOwner { code } => hub.claim_owner(&code).await.map(AdminReply::Claimed),
        C::TransferOwner { user } => hub.transfer_owner(user.id).await.map(|_| AdminReply::Done),
    };
    r.map_err(|e| {
        // Passwords and codes never go to the log: only which call failed.
        let what = match cmd {
            C::SetPassword(_) => "SetPassword".to_string(),
            C::ClaimOwner { .. } => "ClaimOwner".to_string(),
            other => format!("{other:?}"),
        };
        tracing::warn!(error = %format!("{e:#}"), cmd = %what, "admin action failed");
        admin_error(&e)
    })
}

/// Hub answers in Portuguese, for the people.
pub fn admin_error(e: &anyhow::Error) -> String {
    let full = format!("{e:#}");
    let l = full.to_lowercase();
    if l.contains("owner proof required") {
        "Por segurança, a primeira senha pessoal do dono só pode ser definida no app do próprio PC do servidor \
         (endereço 127.0.0.1) ou com o código que a janela do servidor mostra. Digite esse código no campo \
         \"Código do servidor\" e salve de novo."
            .into()
    } else if l.contains("wrong claim code") {
        "Código incorreto. Confira o que a janela do servidor mostra.".into()
    } else if l.contains("already has an owner") {
        "Este servidor já tem dono.".into()
    } else if l.contains("config.toml") {
        "O dono deste servidor está fixado no config.toml (owner = ...): para trocar, quem hospeda edita esse arquivo e reinicia o servidor.".into()
    } else if l.contains("ownership changed meanwhile") {
        "A posse do servidor mudou enquanto você pedia. Abra a lista de novo.".into()
    } else if l.contains("you are already the owner") {
        "Você já é o dono.".into()
    } else if l.contains("personal password first") {
        "A pessoa precisa definir uma senha pessoal antes (em Configurações > Senha pessoal).".into()
    } else if l.contains(vox_proto::ERR_WRONG_PERSONAL_PASSWORD) {
        "A senha atual está incorreta.".into()
    } else if l.contains(vox_proto::ERR_TOO_MANY_ATTEMPTS) {
        "Muitas tentativas seguidas. Aguarde alguns segundos e tente de novo.".into()
    } else if l.contains("personal password must be") {
        format!("A senha pessoal precisa ter de {PERSONAL_PASSWORD_MIN} a {PERSONAL_PASSWORD_MAX} caracteres.")
    } else if l.contains("your role needs a personal password") {
        "Seu cargo exige uma senha pessoal: ela não pode ser removida.".into()
    } else if l.contains("not in voice") {
        "A pessoa não está em um canal de voz.".into()
    } else if l.contains("not streaming") {
        "A pessoa não está transmitindo.".into()
    } else if l.contains("no access to that private channel") {
        "A pessoa não tem acesso a esse canal privado.".into()
    } else if l.contains("already in that channel") {
        "A pessoa já está nesse canal.".into()
    } else if l.contains("last text channel") {
        "O último canal de texto não pode ser apagado.".into()
    } else if l.contains("channel name must be") {
        format!("O nome do canal precisa ter de 1 a {} caracteres.", vox_proto::CHANNEL_NAME_MAX)
    } else if l.contains("http 403") {
        "Seu cargo não permite isso.".into()
    } else if l.contains("http 404") || l.contains("http 405") {
        "Não encontrado: talvez já tenha sido apagado, ou o servidor é de uma versão antiga.".into()
    } else {
        crate::backend::friendly(e)
    }
}

// ---------------------------------------------------------------- UI state

/// The pages of the server window ("Servidor").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ServerTab {
    /// "Membros e cargos".
    #[default]
    Members,
    /// v1.1: the custom roles editor ("Cargos").
    Roles,
    Channels,
    /// v0.8 (`host.rs`).
    Invites,
    /// "Registro de moderação".
    Audit,
    /// "Configurações do servidor": about the server, its ownership, my password.
    Settings,
}

#[derive(Default)]
pub struct RolesUi {
    /// The hub knows roles (its `Ready` carried an account).
    pub supported: bool,
    pub account: Option<Account>,
    /// Our voice session says LiveKit does not accept our microphone.
    pub session_server_muted: bool,
    /// The server window ("Servidor").
    pub server_open: bool,
    pub tab: ServerTab,
    pub members: Vec<MemberInfo>,
    /// "Membros e cargos": the search box.
    pub members_search: String,
    /// "Configurações do servidor": who "Transferir posse" would go to.
    pub transfer_pick: Option<UserId>,
    pub members_loaded: bool,
    pub audit: Vec<AuditEntry>,
    pub audit_loaded: bool,
    pub audit_more: bool,
    pub new_channel: String,
    pub new_voice: bool,
    pub renaming: Option<(ChannelId, String)>,
    /// "Canal privado" of the new channel (v0.9).
    pub new_access: AccessDraft,
    /// The access editor open under one channel (v0.9).
    pub editing_access: Option<(ChannelId, AccessDraft)>,
    /// v1.1: the badge editor open for one member — the draft manual mask. It commits
    /// on "Salvar" (the popup stays open so several can be flipped before saving).
    pub badge_draft: Option<(UserId, u64)>,
    /// v1.1: the server's custom roles (`Ready` and `CustomRolesChanged` keep it fresh).
    pub custom: Vec<CustomRole>,
    /// v1.1: the member whose cargo assignment editor is open (the draft id set).
    pub cargo_draft: Option<(UserId, Vec<i64>)>,
    /// v1.1: the role being created/edited (a new one starts with id = 0).
    pub role_edit: Option<CustomRole>,
    pub confirm_delete: Option<ChannelId>,
    pub confirm_reset: Option<User>,
    /// v1.1: "Banir..." of this person, open as a dialog (the reason is typed inside).
    pub confirm_ban: Option<User>,
    /// "Transferir posse do servidor": waiting for the owner's confirmation for this person.
    pub confirm_transfer: Option<User>,
    // "Reivindicar servidor" (a server with no owner).
    pub claim_open: bool,
    pub claim_code: String,
    pub claim_busy: bool,
    pub claim_msg: Option<String>,
    /// The last reset code to pass on: (username, code).
    pub reset_code: Option<(String, ResetPasswordResponse)>,
    pub server_error: Option<String>,
    // The personal password form (Settings, and the prompt after login).
    pub pw_current: String,
    pub pw_new: String,
    pub pw_confirm: String,
    pub pw_busy: bool,
    pub pw_msg: Option<(bool, String)>,
    /// The code the server's window shows, for the owner's first password from another PC.
    pub pw_owner_code: String,
    /// "Agora não" on the prompt (it comes back at the next login).
    pub prompt_later: bool,
    /// Moderation notices received (for the autotest), newest last.
    pub moderated: Vec<ModAction>,
}

/// What the "Canal privado" fields show (v0.9): the new channel's, or one channel's being edited.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccessDraft {
    pub private: bool,
    pub moderator: bool,
    pub member: bool,
    pub users: Vec<UserId>,
    /// The member picker's search box.
    pub search: String,
}

impl AccessDraft {
    pub fn of(p: Option<&PrivateAccess>) -> Self {
        match p {
            None => AccessDraft::default(),
            Some(p) => AccessDraft {
                private: true,
                moderator: p.roles.contains(&Role::Moderator),
                member: p.roles.contains(&Role::Member),
                users: p.users.clone(),
                search: String::new(),
            },
        }
    }

    /// What to send to the hub (`None`: public).
    pub fn to_access(&self) -> Option<PrivateAccess> {
        if !self.private {
            return None;
        }
        let roles = [(self.moderator, Role::Moderator), (self.member, Role::Member)].into_iter().filter(|(on, _)| *on).map(|(_, r)| r).collect();
        Some(PrivateAccess { roles, users: self.users.clone() }.normalized())
    }
}

/// People the picker offers for `query`: Moderador and Membro (Dono and Admin always have access),
/// not already chosen, by username or display name, at most eight.
fn pick_candidates<'a>(members: &'a [MemberInfo], chosen: &[UserId], query: &str) -> Vec<&'a MemberInfo> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    members
        .iter()
        .filter(|m| m.user.role < Role::Admin && !chosen.contains(&m.user.id))
        .filter(|m| m.user.username.to_lowercase().contains(&q) || m.user.display_name.as_deref().is_some_and(|d| d.to_lowercase().contains(&q)))
        .take(8)
        .collect()
}

/// The "Canal privado" fields (v0.9): the switch, "Cargos com acesso", "Pessoas com acesso" (a
/// member picker with search) and the reminder that Dono and Admin always have access.
fn access_fields(ui: &mut egui::Ui, d: &mut AccessDraft, members: &[MemberInfo], loaded: bool, salt: (&str, ChannelId)) {
    ui.checkbox(&mut d.private, format!("{}  Canal privado", crate::app::ICON_PRIVATE))
        .on_hover_text("Só quem tem acesso vê o canal, o histórico e as imagens dele, e entra na voz. Quem perde o acesso sai na hora.");
    if !d.private {
        return;
    }
    ui.indent(salt, |ui| {
        ui.spacing_mut().item_spacing.y = 6.0;
        ui_kit::hint(ui, "Dono e Admin sempre têm acesso.");
        ui_kit::field_label(ui, "Cargos com acesso");
        ui.horizontal(|ui| {
            ui.checkbox(&mut d.moderator, "Moderador");
            ui.checkbox(&mut d.member, "Membro");
        });
        ui_kit::field_label(ui, "Pessoas com acesso");
        let name_of = |id: UserId| members.iter().find(|m| m.user.id == id).map(|m| m.user.shown_name().to_string()).unwrap_or_else(|| format!("#{id}"));
        let mut remove = None;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            if d.users.is_empty() {
                ui_kit::hint(ui, "Ninguém escolhido.");
            }
            for &id in &d.users {
                let chip = ui_kit::button_sized(ui, Kind::Secondary, Size::Small, Some(ph::X), &name_of(id));
                if chip.on_hover_text("Tirar o acesso desta pessoa").clicked() {
                    remove = Some(id);
                }
            }
        });
        if let Some(id) = remove {
            d.users.retain(|u| *u != id);
        }
        ui.add(ui_kit::field(&mut d.search).hint_text("Buscar pessoa para dar acesso").desired_width(240.0).id_salt(salt));
        if !loaded {
            ui_kit::hint(ui, "Carregando membros...");
            return;
        }
        let found = pick_candidates(members, &d.users, &d.search);
        if found.is_empty() && !d.search.trim().is_empty() {
            ui_kit::hint(ui, "Ninguém encontrado (Dono e Admin já têm acesso).");
        }
        let mut add = None;
        for m in found {
            ui.horizontal(|ui| {
                ui.label(role_name(m.user.shown_name(), m.user.role, Some(theme::GIZ)));
                if ui_kit::button_sized(ui, Kind::Secondary, Size::Small, Some(ph::PLUS), "Adicionar").clicked() {
                    add = Some(m.user.id);
                }
            });
        }
        if let Some(id) = add {
            d.users.push(id);
            d.search.clear();
        }
    });
}

impl App {
    /// The role permissions are checked against: Membro on a hub without roles, and while a
    /// needed personal password is not set.
    pub fn my_role(&self) -> Role {
        if !self.roles.supported || self.roles.account.is_some_and(|a| a.must_set_password) {
            return Role::Member;
        }
        self.me.as_ref().map(|u| u.role).unwrap_or_default()
    }

    /// Someone's current role, as far as we know.
    pub fn role_of(&self, id: UserId) -> Role {
        if let Some(me) = self.me.as_ref().filter(|m| m.id == id) {
            return me.role;
        }
        self.online
            .iter()
            .find(|u| u.id == id)
            .map(|u| u.role)
            .or_else(|| self.voice_members.get(&id).map(|m| m.user.role))
            .unwrap_or_default()
    }

    /// A moderator server-muted us: the hub says so, or our own voice session noticed.
    pub fn i_am_server_muted(&self) -> bool {
        let hub = self.my_id().and_then(|id| self.voice_members.get(&id)).is_some_and(|m| m.server_muted);
        hub || (self.voice.channel.is_some() && self.roles.session_server_muted)
    }

    pub fn admin(&mut self, cmd: AdminCmd) {
        self.backend.send(Cmd::Admin(cmd));
    }

    /// The hub's snapshot: whether it knows roles, and my account.
    pub fn on_roles_ready(&mut self, account: Option<Account>) {
        self.roles.supported = account.is_some();
        self.roles.account = account;
        if !self.roles.supported || !self.my_role().can_admin() {
            self.roles.server_open = false;
        }
        tracing::info!(supported = self.roles.supported, ?account, role = ?self.me.as_ref().map(|u| u.role), "roles");
    }

    pub fn reset_roles(&mut self) {
        let later = self.roles.prompt_later;
        self.roles = RolesUi { prompt_later: later, ..Default::default() };
    }

    fn update_user(&mut self, user: &User) {
        if let Some(me) = self.me.as_mut().filter(|m| m.id == user.id) {
            *me = user.clone();
        }
        for u in self.online.iter_mut().filter(|u| u.id == user.id) {
            *u = user.clone();
        }
        if let Some(m) = self.voice_members.get_mut(&user.id) {
            m.user = user.clone();
        }
        for log in self.logs.values_mut() {
            for m in log.msgs.iter_mut().filter(|m| m.author.id == user.id) {
                m.author = user.clone();
            }
        }
        for m in self.roles.members.iter_mut().filter(|m| m.user.id == user.id) {
            m.user = user.clone();
        }
        if user.role == Role::Owner {
            // The server has an owner now: nobody needs to claim it any more.
            if let Some(a) = self.roles.account.as_mut() {
                a.no_owner = false;
            }
            self.roles.claim_open = false;
        }
        if Some(user.id) == self.my_id() && user.role > Role::Member {
            // A role above Membro needs a personal password; the powers stay off until it is set.
            if let Some(a) = self.roles.account.as_mut().filter(|a| !a.has_password) {
                a.must_set_password = true;
            }
        }
        if Some(user.id) == self.my_id() && !self.my_role().can_admin() {
            self.roles.server_open = false;
        }
    }

    /// The item-12 events from the hub.
    pub fn on_roles_event(&mut self, ev: ServerEvent) {
        match ev {
            ServerEvent::MessageDeleted { channel_id, message_id } => {
                let gone = self.logs.get_mut(&channel_id).and_then(|log| log.msgs.iter().position(|m| m.id == message_id).map(|i| log.msgs.remove(i)));
                tracing::info!(channel = channel_id, message = message_id, had_it = gone.is_some(), "message deleted");
                // Its images, chips, unread count and a reply being written to it go; replies to
                // it now quote "mensagem apagada" (v0.8).
                let atts = gone.map(|m| m.attachments).unwrap_or_default();
                self.forget_message(crate::chat_images::Target::Channel(channel_id), message_id, &atts);
            }
            ServerEvent::ChannelsChanged { mut channels } => {
                channels.sort_by_key(|c| (c.position, c.id));
                self.channels = channels;
                // A channel that went takes all its client state with it (v0.8.2).
                self.prune_channels();
                let chans = self.channels.clone();
                let alive = |id: ChannelId, kind: ChannelKind| chans.iter().any(|c| c.id == id && c.kind == kind);
                if self.selected.is_some_and(|s| !alive(s, ChannelKind::Text)) {
                    self.selected = None;
                    if let Some(first) = chans.iter().find(|c| c.kind == ChannelKind::Text).map(|c| c.id) {
                        self.select_text(first);
                    }
                }
                if self.voice.channel.is_some_and(|v| !alive(v, ChannelKind::Voice)) {
                    self.leave_voice();
                    self.notice = Some("O canal de voz em que você estava foi apagado, ou você não tem mais acesso a ele.".into());
                }
            }
            ServerEvent::UserUpdated { user } => {
                self.chatx.note_user(&user);
                self.update_user(&user);
            }
            ServerEvent::CustomRolesChanged { roles } => {
                // v1.1: the whole list swaps at once (created, edited, deleted, reordered).
                self.roles.custom = roles;
            }
            ServerEvent::Moderated { action, by } => self.on_moderated(action, &by),
            _ => {}
        }
    }

    /// Forget every text channel the hub no longer lists (messages, counters, drafts, replies,
    /// scroll), so nothing of it can show up again under the same id.
    pub(crate) fn prune_channels(&mut self) {
        let alive: Vec<ChannelId> = self.channels.iter().map(|c| c.id).collect();
        let mut gone: Vec<ChannelId> = self
            .logs
            .keys()
            .chain(self.unread.keys())
            .chain(self.chatx.mention_unread.keys())
            .chain(self.chatx.unread_ids.keys())
            .copied()
            .chain(self.chatx.replying.keys().filter_map(|t| match t {
                crate::chat_images::Target::Channel(c) => Some(*c),
                crate::chat_images::Target::Dm(_) => None,
            }))
            .chain(self.selected)
            .filter(|id| !alive.contains(id))
            .collect();
        gone.sort_unstable();
        gone.dedup();
        for ch in gone {
            self.forget_channel(ch);
        }
    }

    fn on_moderated(&mut self, action: ModAction, by: &User) {
        tracing::info!(?action, by = %by.username, "moderated");
        self.roles.moderated.push(action.clone());
        let who = by.shown_name();
        match action {
            ModAction::ServerMute => {
                self.notice = Some(format!("{who} silenciou você no servidor: os outros não ouvem seu microfone até alguém liberar."));
            }
            ModAction::ServerUnmute => {
                self.notice = Some(format!("{who} liberou seu microfone."));
            }
            ModAction::Disconnect => {
                if self.voice.channel.is_some() || self.voice.joining.is_some() {
                    self.leave_voice();
                }
                self.voice.error = None;
                self.notice = Some(format!("{who} desconectou você da voz."));
            }
            ModAction::Move { channel_id } => {
                let name = self.channels.iter().find(|c| c.id == channel_id).map(|c| c.name.clone()).unwrap_or_default();
                if self.voice.channel.is_some() || self.voice.joining.is_some() {
                    // Keep whatever the center shows (chat or call).
                    let center: Center = self.center;
                    self.join_voice(channel_id);
                    self.center = center;
                }
                self.notice = Some(format!("{who} moveu você para {name}."));
            }
            ModAction::StopStream => {
                if self.my_share_live() {
                    self.stop_share();
                }
                self.notice = Some(format!("{who} encerrou sua transmissão."));
            }
            ModAction::Banned { ref reason } => {
                // The hub cuts the socket right after this. Drop the voice at once and
                // say why — the login screen tells the rest when they try to return.
                if self.voice.channel.is_some() || self.voice.joining.is_some() {
                    self.leave_voice();
                }
                let why = if reason.trim().is_empty() { String::new() } else { format!(" Motivo: {}.", reason.trim()) };
                self.notice = Some(format!("{who} baniu você deste servidor.{why}"));
            }
        }
    }

    /// The answer to an [`AdminCmd`].
    pub fn on_admin_reply(&mut self, cmd: AdminCmd, result: Result<AdminReply, String>) {
        let err = match result {
            Ok(reply) => {
                self.on_admin_ok(cmd, reply);
                return;
            }
            Err(e) => e,
        };
        match cmd {
            AdminCmd::SetPassword(_) => {
                self.roles.pw_busy = false;
                self.roles.pw_msg = Some((false, err));
            }
            AdminCmd::ClaimOwner { .. } => {
                self.roles.claim_busy = false;
                self.roles.claim_msg = Some(err);
            }
            _ if self.roles.server_open => self.roles.server_error = Some(err),
            _ => self.notice = Some(err),
        }
    }

    fn on_admin_ok(&mut self, cmd: AdminCmd, reply: AdminReply) {
        self.roles.server_error = None;
        match (cmd, reply) {
            (AdminCmd::SetPassword(req), AdminReply::Password(resp)) => {
                self.roles.account = Some(resp.account);
                self.roles.pw_busy = false;
                self.roles.pw_current.clear();
                self.roles.pw_new.clear();
                self.roles.pw_confirm.clear();
                self.roles.pw_owner_code.clear();
                let text = if req.new.is_some() { "Senha pessoal salva." } else { "Senha pessoal removida: seu nome não está mais protegido." };
                self.roles.pw_msg = Some((true, text.into()));
                if let Some(me) = self.me.clone() {
                    let server = self.session_url.clone();
                    self.settings.set_device_token(&server, &me.username, resp.device_token.as_deref());
                    // v1.0.1: the new token belongs to this server.
                    let id = self.session_server_id.clone();
                    self.settings.set_device_token_server(&server, &me.username, id.as_deref());
                    // v0.9.1: the saved entry knows whether the name is protected.
                    self.settings.set_server_personal(&server, &me.username, req.new.is_some());
                    self.save_settings();
                }
            }
            (_, AdminReply::Users(mut list)) => {
                list.sort_by(|a, b| b.user.role.cmp(&a.user.role).then_with(|| a.user.username.to_lowercase().cmp(&b.user.username.to_lowercase())));
                self.roles.members = list;
                self.roles.members_loaded = true;
            }
            (AdminCmd::ClaimOwner { .. }, AdminReply::Claimed(resp)) => {
                // The server is mine: the hub wants a personal password right away (the prompt).
                self.roles.account = Some(resp.account);
                if let Some(me) = self.me.as_mut() {
                    *me = resp.user.clone();
                }
                self.roles.claim_busy = false;
                self.roles.claim_open = false;
                self.roles.claim_code.clear();
                self.roles.claim_msg = None;
                self.roles.prompt_later = false;
                self.notice = Some("Agora você é o Dono deste servidor. Defina sua senha pessoal para ligar as permissões.".into());
            }
            (AdminCmd::TransferOwner { user }, _) => {
                self.roles.confirm_transfer = None;
                self.notice = Some(format!("Posse do servidor transferida para {}. Você agora é Admin.", user.username));
                self.admin(AdminCmd::ListUsers);
            }
            (AdminCmd::Audit { before }, AdminReply::Audit(entries)) => {
                self.roles.audit_more = entries.len() as u32 >= AUDIT_PAGE;
                if before.is_none() {
                    self.roles.audit = entries;
                } else {
                    self.roles.audit.extend(entries);
                }
                self.roles.audit_loaded = true;
            }
            (AdminCmd::ResetPassword { user }, AdminReply::ResetCode(code)) => {
                self.roles.reset_code = Some((user.username, code));
                self.admin(AdminCmd::ListUsers);
            }
            (AdminCmd::SetRole { .. } | AdminCmd::Mute { .. }, _) if self.roles.server_open => self.admin(AdminCmd::ListUsers),
            (AdminCmd::CreateChannel { .. }, _) => {
                self.roles.new_channel.clear();
                self.roles.new_access = AccessDraft::default();
            }
            (AdminCmd::SetChannelAccess { id, .. }, _) if self.roles.editing_access.as_ref().is_some_and(|(e, _)| *e == id) => self.roles.editing_access = None,
            (AdminCmd::RenameChannel { .. }, _) => self.roles.renaming = None,
            _ => {}
        }
        if self.roles.server_open && self.roles.tab == ServerTab::Audit {
            self.admin(AdminCmd::Audit { before: None });
        }
    }

    // ------------------------------------------------------------ menus

    /// Would the person menu offer me a moderation action on this person (enabled)?
    pub fn moderation_available(&self, user: &User) -> bool {
        use crate::context_menu::{enabled_ids, Id, Lead, Origin};
        let rows = self.menu_rows(user, Origin::Sidebar, Lead::None);
        enabled_ids(&rows).iter().any(|i| {
            matches!(i, Id::ModServerMute | Id::ModServerUnmute | Id::ModDisconnect | Id::ModMove | Id::ModStopStream | Id::ModSetRole | Id::ModRemovePhoto | Id::ModClearProfile)
        })
    }

    // ------------------------------------------------------------ personal password

    /// Set (or change) the personal password; `new = None` removes it (members only).
    pub fn set_personal_password(&mut self, current: Option<String>, new: Option<String>) {
        self.roles.pw_busy = true;
        self.roles.pw_msg = None;
        let code = self.roles.pw_owner_code.trim();
        let owner_code = (self.owner_first_password() && !code.is_empty()).then(|| code.to_string());
        self.admin(AdminCmd::SetPassword(SetPasswordRequest { current, new, remember_device: true, owner_code }));
    }

    /// I am the owner and my name has no personal password yet: the first one needs proof of
    /// being the host (this PC is the server, or the code its window shows).
    fn owner_first_password(&self) -> bool {
        self.my_role() == Role::Owner && self.roles.account.is_some_and(|a| !a.has_password)
    }

    /// The "Código do servidor" field, for the owner's first password from another PC.
    fn owner_code_field(&mut self, ui: &mut egui::Ui) {
        if !self.owner_first_password() {
            return;
        }
        ui_kit::hint(
            ui,
            "Dono: se você não está no PC do servidor, digite também o código que a janela do servidor mostra \
             (protege o cargo de dono de quem entrar com o seu nome antes de você).",
        );
        ui_kit::field_label(ui, "Código do servidor");
        ui.add(ui_kit::field(&mut self.roles.pw_owner_code).hint_text("XXXX-XXXX").font(egui::TextStyle::Monospace).desired_width(140.0));
        ui.add_space(space::S);
    }

    /// Checks the form; `Some(error)` when it cannot be sent.
    fn pw_form_error(&self) -> Option<String> {
        let n = self.roles.pw_new.chars().count();
        if !(PERSONAL_PASSWORD_MIN..=PERSONAL_PASSWORD_MAX).contains(&n) {
            return Some(format!("A senha pessoal precisa ter de {PERSONAL_PASSWORD_MIN} a {PERSONAL_PASSWORD_MAX} caracteres."));
        }
        if self.roles.pw_new != self.roles.pw_confirm {
            return Some("As duas senhas não são iguais.".into());
        }
        None
    }

    /// The fields; returns true when "Salvar senha" was pressed with a valid form.
    fn pw_fields(&mut self, ui: &mut egui::Ui, has_password: bool) -> bool {
        let mut submit = false;
        let w = 260.0;
        if has_password {
            ui_kit::field_label(ui, "Senha atual");
            ui.add(ui_kit::field(&mut self.roles.pw_current).password(true).desired_width(w));
        }
        ui_kit::field_label(ui, if has_password { "Nova senha" } else { "Senha pessoal" });
        ui.add(ui_kit::field(&mut self.roles.pw_new).password(true).desired_width(w));
        ui_kit::field_label(ui, "Repita a senha");
        let r = ui.add(ui_kit::field(&mut self.roles.pw_confirm).password(true).desired_width(w));
        submit |= r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        ui.add_space(space::S);
        let label = if self.roles.pw_busy { "Salvando..." } else { "Salvar senha" };
        submit |= ui.add_enabled_ui(!self.roles.pw_busy, |ui| ui_kit::button(ui, Kind::Primary, None, label)).inner.clicked();
        if submit {
            if let Some(e) = self.pw_form_error() {
                self.roles.pw_msg = Some((false, e));
                return false;
            }
        }
        submit
    }

    fn pw_message(&self, ui: &mut egui::Ui) {
        if let Some((ok, msg)) = &self.roles.pw_msg {
            ui_kit::result_text(ui, *ok, msg);
        }
    }

    /// "Senha pessoal" in Settings.
    pub fn personal_password_section(&mut self, ui: &mut egui::Ui) {
        if !self.roles.supported {
            return;
        }
        self.claim_owner_section(ui);
        ui.add_space(space::M);
        ui_kit::section(ui, "Senha pessoal");
        let account = self.roles.account.unwrap_or_default();
        let role = self.me.as_ref().map(|u| u.role).unwrap_or_default();
        ui_kit::hint(
            ui,
            if account.has_password {
                "Seu nome está protegido: entrar com ele exige esta senha (este computador fica lembrado)."
            } else if role > Role::Member {
                "Seu cargo exige uma senha pessoal. Defina uma para usar as permissões do cargo."
            } else {
                "Opcional: protege seu nome, para ninguém entrar como você."
            },
        );
        ui.add_space(space::S);
        self.owner_code_field(ui);
        if self.pw_fields(ui, account.has_password) {
            let current = account.has_password.then(|| self.roles.pw_current.clone());
            let new = self.roles.pw_new.clone();
            self.set_personal_password(current, Some(new));
        }
        if account.has_password && role == Role::Member && !self.roles.pw_busy {
            ui.add_space(space::S);
            ui.horizontal(|ui| {
                ui_kit::hint(ui, "Para remover, digite a senha atual:");
                if ui_kit::button_sized(ui, Kind::Secondary, Size::Small, None, "Remover senha").clicked() {
                    let current = Some(self.roles.pw_current.clone());
                    self.set_personal_password(current, None);
                }
            });
        }
        self.pw_message(ui);
    }

    /// Right after logging in: the owner's first login after the update (or a login with a
    /// reset code, or a promoted role without a password) must choose a personal password.
    pub fn password_prompt_window(&mut self, ctx: &egui::Context) {
        let must = self.roles.supported && self.roles.account.is_some_and(|a| a.must_set_password);
        if !must || self.roles.prompt_later || self.me.is_none() {
            return;
        }
        let role = self.me.as_ref().map(|u| u.role).unwrap_or_default();
        let mut later = false;
        egui::Window::new("Defina sua senha pessoal")
            .collapsible(false)
            .resizable(false)
            .title_bar(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .frame(ui_kit::dialog_frame(&ctx.global_style()))
            .show(ctx, |ui| {
                ui.set_width(ui_kit::DIALOG_W);
                ui.spacing_mut().item_spacing.y = 6.0;
                ui_kit::dialog_title(ui, "Defina sua senha pessoal");
                let why = if role > Role::Member {
                    format!(
                        "Você é {} neste servidor. Para usar as permissões do cargo, proteja seu nome com uma senha pessoal: \
                         depois dela, ninguém mais consegue entrar como você. Este computador fica lembrado.",
                        role_label(role)
                    )
                } else {
                    "Sua senha pessoal foi redefinida. Escolha uma nova para continuar protegendo seu nome.".to_string()
                };
                ui_kit::body_text(ui, why);
                ui.add_space(space::S);
                self.owner_code_field(ui);
                if self.pw_fields(ui, false) {
                    let new = self.roles.pw_new.clone();
                    self.set_personal_password(None, Some(new));
                }
                self.pw_message(ui);
                ui.add_space(space::S);
                if ui_kit::button_sized(ui, Kind::Ghost, Size::Small, None, "Agora não")
                    .on_hover_text("As permissões do cargo ficam desligadas até a senha ser definida (em Configurações)")
                    .clicked()
                {
                    later = true;
                }
            });
        if later {
            self.roles.prompt_later = true;
        }
    }

    // ------------------------------------------------------------ claiming a server with no owner

    /// "Reivindicar servidor": the claim code the server's console shows, for a server with no owner.
    pub fn claim_owner_window(&mut self, ctx: &egui::Context) {
        if !self.roles.claim_open {
            return;
        }
        if !self.roles.supported || !self.roles.account.is_some_and(|a| a.no_owner) {
            self.roles.claim_open = false;
            return;
        }
        let mut open = !ctx.input(|i| i.key_pressed(egui::Key::Escape));
        let mut submit = false;
        egui::Window::new("Reivindicar servidor")
            .collapsible(false)
            .resizable(false)
            .title_bar(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .frame(ui_kit::dialog_frame(&ctx.global_style()))
            .show(ctx, |ui| {
                ui.set_width(ui_kit::DIALOG_W + 20.0);
                ui.spacing_mut().item_spacing.y = 6.0;
                ui_kit::dialog_title(ui, "Reivindicar servidor");
                ui_kit::body_text(
                    ui,
                    "Este servidor ainda não tem dono. Quem hospeda vira dono sozinho ao entrar no app pelo próprio PC do servidor \
                     (endereço 127.0.0.1). Se você não está nele, digite o código que a janela do servidor mostra:",
                );
                ui_kit::field_label(ui, "Código do servidor");
                let r = ui.add(ui_kit::field(&mut self.roles.claim_code).hint_text("XXXX-XXXX").font(egui::TextStyle::Monospace).desired_width(160.0));
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if let Some(msg) = &self.roles.claim_msg {
                    ui_kit::result_text(ui, false, msg);
                }
                ui_kit::hint(ui, "Depois você define uma senha pessoal, que protege o seu nome e liga as permissões de Dono.");
                ui.add_space(space::L);
                ui.allocate_ui_with_layout(Vec2::new(ui.available_width(), 32.0), egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let can = !self.roles.claim_busy && !self.roles.claim_code.trim().is_empty();
                    let label = if self.roles.claim_busy { "Reivindicando..." } else { "Reivindicar servidor" };
                    submit = ui.add_enabled_ui(can, |ui| ui_kit::button(ui, Kind::Primary, None, label)).inner.clicked() || (enter && can);
                    if ui_kit::button(ui, Kind::Secondary, None, "Cancelar").clicked() {
                        open = false;
                    }
                });
            });
        if submit {
            self.roles.claim_busy = true;
            self.roles.claim_msg = None;
            let code = self.roles.claim_code.trim().to_string();
            self.admin(AdminCmd::ClaimOwner { code });
        }
        if !open {
            self.roles.claim_open = false;
        }
    }

    /// Settings: a server with no owner can be claimed.
    fn claim_owner_section(&mut self, ui: &mut egui::Ui) {
        if !self.roles.account.is_some_and(|a| a.no_owner) {
            return;
        }
        ui.add_space(space::M);
        ui_kit::section(ui, "Servidor sem dono");
        ui_kit::hint(ui, "Quem hospeda vira dono entrando pelo próprio PC do servidor, ou com o código que a janela do servidor mostra.");
        if ui_kit::button(ui, Kind::Secondary, Some(ph::CROWN), "Reivindicar servidor...").clicked() {
            self.roles.claim_open = true;
            self.roles.claim_msg = None;
        }
    }

    // ------------------------------------------------------------ the server window

    pub fn open_server_settings(&mut self) {
        self.roles.server_open = true;
        self.roles.server_error = None;
        self.roles.members_loaded = false;
        self.roles.audit_loaded = false;
        self.admin(AdminCmd::ListUsers);
        self.admin(AdminCmd::Audit { before: None });
    }

    /// Any of the server window's confirmations is up.
    fn server_confirming(&self) -> bool {
        self.roles.confirm_delete.is_some() || self.roles.confirm_reset.is_some() || self.roles.confirm_transfer.is_some() || self.host_confirming()
    }

    /// "Servidor": the Discord-like settings of the server (Admin and Dono). A page list on the
    /// left ("Membros e cargos", "Canais", "Convites", "Registro de moderação", "Configurações do
    /// servidor"), the open page on the right; its confirmations float over it.
    pub fn server_settings_window(&mut self, ctx: &egui::Context) {
        if !self.roles.server_open {
            return;
        }
        if !self.roles.supported || !self.my_role().can_admin() {
            self.roles.server_open = false;
            return;
        }
        let tab = self.roles.tab;
        let name = self.server_name.clone();
        let mut picked = None;
        let esc = !self.server_confirming() && !egui::Popup::is_any_open(ctx) && self.cmenu.open.is_none() && self.cmenu.confirm.is_none();
        let open = ui_kit::settings_window(
            ctx,
            "Servidor",
            Vec2::new(860.0, 580.0),
            esc,
            |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.horizontal(|ui| {
                    ui.add_space(space::M);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        ui.label(RichText::new(format!("{}  Servidor", ph::WRENCH)).font(theme::body(12.5)).color(theme::NEVOA));
                        ui.add(egui::Label::new(RichText::new(&name).font(theme::display(size::TITLE)).color(theme::GIZ)).truncate().selectable(false));
                    });
                });
                ui.add_space(space::L);
                let pages: Vec<(&str, &str)> = SERVER_PAGES.iter().map(|(_, icon, label)| (*icon, *label)).collect();
                let at = SERVER_PAGES.iter().position(|(t, _, _)| *t == tab).unwrap_or(0);
                if let Some(i) = ui_kit::nav_list(ui, &pages, at) {
                    picked = Some(SERVER_PAGES[i].0);
                }
            },
            |ui| {
                if let Some(e) = self.roles.server_error.clone() {
                    ui_kit::notice_frame(ui_kit::Notice::Error).show(ui, |ui| {
                        ui.horizontal_top(|ui| {
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                                if ui_kit::icon_button(ui, ph::X, "Fechar o aviso", IconState::Normal).clicked() {
                                    self.roles.server_error = None;
                                }
                                ui.with_layout(egui::Layout::left_to_right(egui::Align::Min), |ui| ui_kit::notice_text(ui, ui_kit::Notice::Error, &e));
                            });
                        });
                    });
                    ui.add_space(space::L);
                }
                match tab {
                    ServerTab::Members => self.members_tab(ui),
                    ServerTab::Roles => self.roles_tab(ui),
                    ServerTab::Channels => self.channels_tab(ui),
                    ServerTab::Invites => {
                        ui_kit::page_title(ui, "Convites", Some(crate::host::INVITE_HINT));
                        self.invite_tools(ui);
                    }
                    ServerTab::Audit => self.audit_tab(ui),
                    ServerTab::Settings => self.server_tab(ui),
                }
            },
        );
        if let Some(t) = picked.filter(|t| *t != tab) {
            self.roles.tab = t;
            if t == ServerTab::Audit {
                self.admin(AdminCmd::Audit { before: None });
            }
        }
        self.server_confirmations(ctx);
        if !open {
            self.roles.server_open = false;
            self.roles.reset_code = None;
            self.roles.confirm_delete = None;
            self.roles.confirm_reset = None;
            self.roles.confirm_transfer = None;
            self.roles.editing_access = None;
            self.roles.renaming = None;
        }
    }

    /// "Apagar canal", "Redefinir senha pessoal" and "Transferir posse do servidor": the kit's
    /// confirmation, over the server window.
    fn server_confirmations(&mut self, ctx: &egui::Context) {
        if let Some(id) = self.roles.confirm_delete {
            let Some(c) = self.channels.iter().find(|c| c.id == id).cloned() else {
                self.roles.confirm_delete = None;
                return;
            };
            let (what, body) = if c.kind == ChannelKind::Text {
                (format!("Apagar #{}?", c.name), "O canal some para todo mundo, com todas as mensagens e imagens dele. Isso não pode ser desfeito.")
            } else {
                (format!("Apagar a sala {}?", c.name), "Quem estiver nela sai da voz. Isso não pode ser desfeito.")
            };
            match ui_kit::confirm(ctx, "server_delete_channel", "Apagar canal", "Apagar canal", Kind::Danger, true, |ui| {
                ui_kit::body_text(ui, what);
                ui_kit::hint(ui, body);
            }) {
                Some(true) => {
                    self.admin(AdminCmd::DeleteChannel { id });
                    self.roles.confirm_delete = None;
                }
                Some(false) => self.roles.confirm_delete = None,
                None => {}
            }
        } else if let Some(u) = self.roles.confirm_reset.clone() {
            match ui_kit::confirm(ctx, "server_reset_password", "Redefinir senha pessoal", "Redefinir senha", Kind::Danger, true, |ui| {
                ui_kit::body_text(ui, format!("Redefinir a senha pessoal de {}?", u.shown_name()));
                ui_kit::hint(ui, "A senha atual da pessoa é apagada e um código de uso único é gerado para ela entrar e escolher outra. Fica no registro de moderação.");
            }) {
                Some(true) => {
                    self.admin(AdminCmd::ResetPassword { user: u });
                    self.roles.confirm_reset = None;
                }
                Some(false) => self.roles.confirm_reset = None,
                None => {}
            }
        } else if let Some(u) = self.roles.confirm_transfer.clone() {
            match ui_kit::confirm(ctx, "server_transfer_owner", "Transferir posse do servidor", "Transferir posse", Kind::Danger, true, |ui| {
                ui_kit::body_text(ui, format!("Passar o servidor para {}?", u.shown_name()));
                ui_kit::hint(ui, "A pessoa vira a dona do servidor e você passa a Admin. Só ela pode devolver a posse.");
            }) {
                Some(true) => {
                    self.admin(AdminCmd::TransferOwner { user: u });
                    self.roles.confirm_transfer = None;
                }
                Some(false) => self.roles.confirm_transfer = None,
                None => {}
            }
        } else if let Some(u) = self.roles.confirm_ban.clone() {
            // v1.1: the ban dialog — with the reason, kept in egui's memory so the
            // confirm closure borrows nothing from the app.
            let reason_id = egui::Id::new("ban_reason_field");
            match ui_kit::confirm(ctx, "server_ban", "Banir", "Banir", Kind::Danger, true, |ui| {
                ui_kit::body_text(ui, format!("Banir {} deste servidor?", u.shown_name()));
                ui_kit::hint(ui, "A pessoa sai na hora e não consegue mais entrar — nem com senha, nem com convite. Dá para desfazer com \"Remover banimento\".");
                ui.add_space(space::S);
                let mut reason = ui.ctx().data_mut(|d| d.get_persisted::<String>(reason_id).unwrap_or_default());
                ui.add(ui_kit::field(&mut reason).hint_text("Motivo (opcional: aparece pra pessoa e no registro)"));
                ui.ctx().data_mut(move |d| d.insert_persisted(reason_id, reason));
            }) {
                Some(true) => {
                    let reason = ctx.data_mut(|d| d.get_persisted::<String>(reason_id).unwrap_or_default());
                    ctx.data_mut(|d| d.remove::<String>(reason_id));
                    self.admin(AdminCmd::Ban { user: u, reason: reason.trim().to_owned() });
                    self.roles.confirm_ban = None;
                }
                Some(false) => {
                    let _ = ctx.data_mut(|d| d.remove::<String>(reason_id));
                    self.roles.confirm_ban = None;
                }
                None => {}
            }
        }
    }

    // ------------------------------------------------------------ "Membros e cargos"

    fn members_tab(&mut self, ui: &mut egui::Ui) {
        ui_kit::page_title(ui, "Membros e cargos", Some("Quem já entrou neste servidor. O cargo decide o que cada um pode fazer; o registro de moderação guarda cada mudança."));
        if let Some((name, code)) = self.roles.reset_code.clone() {
            ui_kit::notice_frame(ui_kit::Notice::Warning).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui_kit::body_text(ui, format!("Senha pessoal de {name} redefinida. Passe este código para a pessoa (vale até ela escolher a senha nova, por no máximo 3 dias):"));
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&code.code).monospace().strong().size(18.0).color(theme::GIZ));
                    if ui_kit::button_sized(ui, Kind::Secondary, Size::Small, Some(ph::COPY), "Copiar").clicked() {
                        ui.ctx().copy_text(code.code.clone());
                    }
                    if ui_kit::button_sized(ui, Kind::Ghost, Size::Small, None, "OK").clicked() {
                        self.roles.reset_code = None;
                    }
                });
                ui_kit::hint(ui, "Ela entra com o nome e o código no campo \"Senha pessoal\", e escolhe uma senha nova.");
            });
            ui.add_space(space::L);
        }
        if !self.roles.members_loaded {
            ui_kit::hint(ui, "Carregando...");
            return;
        }
        ui.add(ui_kit::field(&mut self.roles.members_search).hint_text(format!("{}  Buscar pelo nome", ph::MAGNIFYING_GLASS_PLUS)).desired_width(280.0));
        ui.add_space(space::M);
        let q = self.roles.members_search.trim().to_lowercase();
        let members: Vec<MemberInfo> = self
            .roles
            .members
            .iter()
            .filter(|m| q.is_empty() || m.user.username.to_lowercase().contains(&q) || m.user.display_name.as_deref().is_some_and(|d| d.to_lowercase().contains(&q)))
            .cloned()
            .collect();
        ui_kit::section(ui, &format!("Membros — {}", members.len()));
        ui.add_space(space::S);
        if members.is_empty() {
            ui_kit::hint(ui, "Ninguém encontrado.");
            return;
        }
        ui_kit::group_frame().inner_margin(egui::Margin::symmetric(6, 6)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            for m in &members {
                self.server_member_row(ui, m);
                // v1.1: the badge editor open under this member, like the access editor
                // under a channel ("Editar badges" in the row).
                let editing = self.roles.badge_draft.as_ref().is_some_and(|(id, _)| *id == m.user.id);
                if let Some((id, mut draft)) = self.roles.badge_draft.take_if(|_| editing) {
                    let mut keep = true;
                    let flags = m.user.flags;
                    let granted = m.granted;
                    let name = m.user.shown_name().to_owned();
                    egui::Frame::new().fill(theme::NOITE).corner_radius(radius::ROW).inner_margin(egui::Margin::symmetric(14, 10)).outer_margin(egui::Margin { left: 28, right: 4, top: 2, bottom: 6 }).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.spacing_mut().item_spacing.y = 6.0;
                        ui.label(egui::RichText::new(format!("Badges de {name}")).font(theme::semibold(13.0)).color(theme::GIZ));
                        ui_kit::hint(ui, "Concedidas por você ficam salvas; as automáticas (tempo de casa, primeiros membros, staff) vêm por cima.");
                        // The automatic ones, shown but never editable here.
                        let auto = flags & !granted;
                        if auto != 0 {
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new("Automáticas:").font(theme::body(11.5)).color(theme::NEVOA));
                                self.badges.row(ui, auto, 16.0, 8);
                            });
                            ui.add_space(space::S);
                        }
                        // The catalog: one toggle per badge, art + name + story.
                        for d in crate::badges::CATALOG {
                            let on = draft & d.bit != 0;
                            let (r, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 26.0), egui::Sense::click());
                            if resp.clicked() {
                                draft ^= d.bit;
                            }
                            let mut row = ui.new_child(egui::UiBuilder::new().max_rect(r.shrink2(Vec2::new(8.0, 0.0))).layout(egui::Layout::left_to_right(egui::Align::Center)));
                            row.spacing_mut().item_spacing.x = 8.0;
                            if let Some(tex) = self.badges.get(row.ctx(), d.bit) {
                                let (ir, _) = row.allocate_exact_size(Vec2::splat(18.0), egui::Sense::hover());
                                row.painter().image(tex, ir, egui::Rect::from_min_max(egui::Pos2::ZERO, egui::Pos2::new(1.0, 1.0)), if on { egui::Color32::WHITE } else { egui::Color32::WHITE.gamma_multiply(0.45) });
                            }
                            row.label(egui::RichText::new(d.title).font(theme::body(12.5)).color(if on { theme::GIZ } else { theme::NEVOA }));
                            let mark = if on { ph::CHECK } else { "" };
                            row.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.label(egui::RichText::new(mark).font(theme::body(13.0)).color(theme::ABAJUR));
                            });
                            resp.on_hover_text(format!("{}\n{}", d.title, d.desc));
                        }
                        ui.add_space(space::S);
                        ui.horizontal(|ui| {
                            if ui_kit::button_sized(ui, Kind::Primary, Size::Small, None, "Salvar badges").clicked() {
                                self.admin(AdminCmd::SetBadges { user: id, flags: draft });
                                keep = false;
                            }
                            if ui_kit::button_sized(ui, Kind::Ghost, Size::Small, None, "Cancelar").clicked() {
                                keep = false;
                            }
                        });
                    });
                    if keep && self.roles.badge_draft.is_none() {
                        self.roles.badge_draft = Some((id, draft));
                    }
                }
                // v1.1: the cargo assignment editor, the same way right under the row.
                let editing_cargo = self.roles.cargo_draft.as_ref().is_some_and(|(uid, _)| *uid == m.user.id);
                if let Some((uid, mut draft)) = self.roles.cargo_draft.take_if(|_| editing_cargo) {
                    let mut keep = true;
                    let customs = self.roles.custom.clone();
                    let name = m.user.shown_name().to_owned();
                    egui::Frame::new().fill(theme::NOITE).corner_radius(radius::ROW).inner_margin(egui::Margin::symmetric(14, 10)).outer_margin(egui::Margin { left: 28, right: 4, top: 2, bottom: 6 }).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.spacing_mut().item_spacing.y = 6.0;
                        ui.label(RichText::new(format!("Cargos de {name}")).font(theme::semibold(13.0)).color(theme::GIZ));
                        if customs.is_empty() {
                            ui_kit::hint(ui, "Ainda não há cargos: crie um na aba \"Cargos\".");
                        } else {
                            ui_kit::hint(ui, "Marque os cargos dela; o mais alto pinta o nome.");
                        }
                        for r in &customs {
                            let on = draft.contains(&r.id);
                            let (rr, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 26.0), egui::Sense::click());
                            if resp.clicked() {
                                if on {
                                    draft.retain(|x| *x != r.id);
                                } else {
                                    draft.push(r.id);
                                }
                            }
                            if resp.hovered() {
                                ui.painter().rect_filled(rr, radius::BUTTON, theme::ROW_HOVER);
                            }
                            let color = Color32::from_hex(r.color.get(1..).unwrap_or("99A7B5")).unwrap_or(theme::NEVOA);
                            ui.painter().circle_filled(egui::pos2(rr.left() + 18.0, rr.center().y), 6.0, color);
                            ui.painter().text(egui::pos2(rr.left() + 32.0, rr.center().y), egui::Align2::LEFT_CENTER, &r.name, theme::body(12.5), if on { theme::GIZ } else { theme::NEVOA });
                            let mark = if on { ph::CHECK } else { " " };
                            ui.painter().text(egui::pos2(rr.right() - 14.0, rr.center().y), egui::Align2::RIGHT_CENTER, mark, theme::body(12.5), if on { theme::ABAJUR } else { theme::NEVOA });
                            let n = Self::PERMS.iter().filter(|(b, _, _)| r.allow & b != 0).count();
                            resp.on_hover_text(format!("{} permissões", n));
                        }
                        ui.add_space(space::S);
                        ui.horizontal(|ui| {
                            if ui_kit::button_sized(ui, Kind::Primary, Size::Small, None, "Salvar cargos").clicked() {
                                self.admin(AdminCmd::SetMemberCustomRoles { user: uid, role_ids: draft.clone() });
                                keep = false;
                            }
                            if ui_kit::button_sized(ui, Kind::Ghost, Size::Small, None, "Cancelar").clicked() {
                                keep = false;
                            }
                        });
                    });
                    if keep && self.roles.cargo_draft.is_none() {
                        self.roles.cargo_draft = Some((uid, draft));
                    }
                }
            }
        });
    }

    /// One person of "Membros e cargos": picture, name (in the role's colour), badges, then the
    /// role (a menu when I may change it), "Liberar microfone" when muted, and the "⋯" menu
    /// (reset the personal password, transfer the server, and the person's usual menu).
    fn server_member_row(&mut self, ui: &mut egui::Ui, m: &MemberInfo) {
        let mine = self.my_role();
        let my_id = self.my_id();
        let theirs = m.user.role;
        let me = Some(m.user.id) == my_id;
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 46.0), Sense::hover());
        if resp.hovered() {
            ui.painter().rect_filled(rect, radius::ROW, theme::ROW_HOVER);
        }
        let mut row = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink2(Vec2::new(8.0, 0.0))).layout(egui::Layout::left_to_right(egui::Align::Center)));
        let ui = &mut row;
        ui.spacing_mut().item_spacing.x = space::M;
        let user = self.known_user(m.user.id).unwrap_or_else(|| m.user.clone());
        crate::avatars::avatar_widget(&mut self.avatars.store, ui, &user, 32.0, crate::avatars::Surface::Online);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                ui.label(role_name(user.shown_name(), theirs, Some(theme::GIZ)).font(theme::semibold(13.5)));
                role_badge(ui, theirs);
                if user.flags != 0 {
                    // v1.1: their badges, tiny, next to the name — at most three, so the
                    // row never gets wide enough to wrap (the card tells the stories).
                    self.badges.row(ui, user.flags, 14.0, 3);
                }
                // v1.1: a member (no fixed colour) may paint with their highest custom role.
                if crate::roles::role_color(theirs).is_none() {
                    if let Some(c) = self.user_custom_color(&user) {
                        ui.label(RichText::new(ph::CIRCLE).font(theme::body(9.0)).color(c)).on_hover_text("A cor é de um cargo");
                    }
                }
                if m.has_password {
                    ui.label(RichText::new(ph::LOCK_SIMPLE).font(theme::body(13.0)).color(theme::NEVOA)).on_hover_text("Nome protegido por senha pessoal");
                }
                if me {
                    ui.label(RichText::new("(você)").font(theme::body(12.0)).color(theme::NEVOA));
                }
            });
            ui.label(RichText::new(format!("@{}", user.username)).font(theme::body(12.0)).color(theme::NEVOA));
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            // "⋯": the admin's own items, then the person's usual menu (moderation included).
            let more = ui_kit::icon_button(ui, ph::DOTS_THREE, "Mais opções", IconState::Normal);
            egui::Popup::menu(&more).frame(ui_kit::popup_frame(ui.style())).style(ui_kit::menu_style).show(|ui| {
                ui.set_min_width(250.0);
                let mut any = false;
                if mine.can_reset_passwords() && !me {
                    any = true;
                    if ui_kit::menu_item(ui, ph::LOCK_SIMPLE, "Redefinir senha pessoal...", false, None) {
                        self.roles.confirm_reset = Some(m.user.clone());
                        ui.close();
                    }
                }
                // v1.1: the badge editor, in the menu (an inline button used to wrap below
                // the row and vanish under the next one's hover).
                if mine.can_admin() {
                    any = true;
                    if ui_kit::menu_item(ui, ph::MEDAL, "Badges...", false, None) {
                        self.roles.badge_draft = Some((m.user.id, m.granted));
                        ui.close();
                    }
                }
                // v1.1: this person's custom roles.
                if self.i_allow(perm::MANAGE_ROLES) {
                    any = true;
                    if ui_kit::menu_item(ui, ph::GAVEL, "Cargos...", false, None) {
                        self.roles.cargo_draft = Some((m.user.id, m.user.role_ids.clone()));
                        ui.close();
                    }
                }
                // v1.1: the ban (Discord keeps it in the member's menu too).
                if mine.can_admin() && !me && theirs != Role::Owner && mine.outranks(theirs) && m.banned.is_none() {
                    any = true;
                    if ui_kit::menu_item(ui, ph::PROHIBIT, "Banir...", true, None) {
                        self.roles.confirm_ban = Some(m.user.clone());
                        ui.close();
                    }
                }
                if mine == Role::Owner && !me {
                    any = true;
                    let pinned = self.roles.account.is_some_and(|a| a.owner_pinned);
                    let why = if pinned {
                        Some("O dono está fixado no config.toml do servidor (owner = ...): para trocar, quem hospeda edita o arquivo e reinicia.")
                    } else if !m.has_password {
                        Some("A pessoa precisa definir uma senha pessoal antes.")
                    } else {
                        None
                    };
                    if ui_kit::menu_item(ui, ph::CROWN, "Transferir posse do servidor...", true, why) {
                        self.roles.confirm_transfer = Some(m.user.clone());
                        ui.close();
                    }
                }
                if any {
                    ui_kit::menu_separator(ui);
                }
                let pos = ui.min_rect().left_top();
                self.person_menu_ui(ui, &user, crate::context_menu::Origin::Online, crate::context_menu::Lead::None, pos);
            });
            if m.server_muted && mine.outranks(theirs) {
                if ui_kit::button_sized(ui, Kind::Secondary, Size::Small, Some(ph::MICROPHONE), "Liberar microfone").on_hover_text("Está silenciada no servidor").clicked() {
                    self.admin(AdminCmd::Mute { user: m.user.id, muted: false });
                }
            } else if m.server_muted {
                ui_kit::pill(ui, &format!("{} silenciada", crate::app::ICON_MUTED), theme::PERIGO_FUNDO, theme::AO_VIVO);
            }
            let editable = !me && theirs != Role::Owner && (mine == Role::Owner || theirs < mine) && mine.can_admin();
            if let Some(b) = &m.banned {
                // v1.1: banned — the pill with the reason on hover, and the way back in.
                let why = if b.reason.trim().is_empty() { "Banido".to_string() } else { format!("Banido: {}", b.reason.trim()) };
                ui_kit::pill(ui, &why, theme::PERIGO_FUNDO, theme::AO_VIVO);
                if mine.can_admin() && ui_kit::button_sized(ui, Kind::Secondary, Size::Small, None, "Remover banimento").clicked() {
                    self.admin(AdminCmd::Unban { user: m.user.id });
                }
            } else if editable {
                let mut pick = theirs;
                egui::ComboBox::from_id_salt(("role", m.user.id)).width(120.0).selected_text(role_label(theirs)).show_ui(ui, |ui| {
                    for r in mine.assignable() {
                        let enabled = r == Role::Member || m.has_password;
                        ui.add_enabled_ui(enabled, |ui| {
                            ui.selectable_value(&mut pick, r, role_label(r)).on_disabled_hover_text("A pessoa precisa definir uma senha pessoal antes");
                        });
                    }
                });
                if pick != theirs {
                    self.admin(AdminCmd::SetRole { user: m.user.id, role: pick });
                }
            } else {
                ui_kit::pill(ui, role_label(theirs), theme::MOVEL, role_color(theirs).unwrap_or(theme::NEVOA));
            }
        });
    }

    // ------------------------------------------------------------ "Cargos" (v1.1)

    /// One permission of the editor matrix: its bit, its PT name and the story.
    const PERMS: &[(u64, &str, &str)] = &[
        (perm::VIEW_CHANNEL, "Ver canais", "Sem isto, os canais somem pra quem tem o cargo."),
        (perm::SEND_MESSAGES, "Enviar mensagens", "Escrever nos canais de texto."),
        (perm::ATTACH, "Enviar anexos", "Imagens e arquivos junto com a mensagem."),
        (perm::REACT, "Reagir", "Os emojis embaixo das mensagens."),
        (perm::CONNECT, "Entrar na voz", "Entrar nas salas."),
        (perm::SPEAK, "Falar", "O microfone na voz."),
        (perm::VIDEO, "Câmera", "Ligar a webcam."),
        (perm::STREAM, "Go Live", "Compartilhar a tela."),
        (perm::DELETE_MESSAGES, "Apagar mensagens", "As dos outros também (moderação)."),
        (perm::MUTE_MEMBERS, "Silenciar no servidor", "O muting que vale pra todos."),
        (perm::MOVE_MEMBERS, "Mover na voz", "Levar alguém pra outra sala."),
        (perm::STOP_STREAMS, "Encerrar Go Live", "Parar a transmissão de alguém."),
        (perm::DISCONNECT, "Desconectar da voz", "Tirar alguém da sala."),
        (perm::BAN_MEMBERS, "Banir", "E também remover banimentos."),
        (perm::MANAGE_CHANNELS, "Gerenciar canais", "Criar, renomear, reordenar, apagar."),
        (perm::MANAGE_INVITES, "Gerenciar convites", "Criar e revogar convites."),
        (perm::MANAGE_ROLES, "Gerenciar cargos", "Criar, editar e dar cargos."),
        (perm::MANAGE_BADGES, "Gerenciar badges", "Conceder badges na mão."),
        (perm::VIEW_AUDIT, "Registro de moderação", "Ler o registro."),
        (perm::RESET_PASSWORDS, "Redefinir senhas", "Gerar os códigos de uso único."),
        (perm::ADMINISTRATOR, "Administrador", "Tudo acima; não dá pra tirar de alguém com cargo menor."),
    ];

    /// The colour swatches of the editor (Discord's role palette, roughly).
    const ROLE_SWATCHES: [Color32; 10] = [
        Color32::from_rgb(0x99, 0xA7, 0xB5), Color32::from_rgb(0x1E, 0x21, 0x44),
        Color32::from_rgb(0xE9, 0x1B, 0x62), Color32::from_rgb(0xD3, 0x83, 0x0C),
        Color32::from_rgb(0xE6, 0x71, 0x0B), Color32::from_rgb(0x24, 0x80, 0x4A),
        Color32::from_rgb(0x28, 0x9B, 0x11), Color32::from_rgb(0x1F, 0x8A, 0x71),
        Color32::from_rgb(0x29, 0xA3, 0xA6), Color32::from_rgb(0x1D, 0x6F, 0xC9),
    ];

    fn roles_tab(&mut self, ui: &mut egui::Ui) {
        let may_edit = self.i_allow(perm::MANAGE_ROLES);
        ui_kit::page_title(ui, "Cargos", Some("Cargos seus, com cor, ordem e permissões — eles somam poder em cima dos cargos fixos (Membro, Moderador, Admin, Dono)."));
        ui.add_space(space::S);
        // The editor: creating (id = 0) or editing an existing one.
        if let Some(mut role) = self.roles.role_edit.take() {
            let creating = role.id == 0;
            let mut keep = true;
            egui::Frame::new().fill(theme::NOITE).corner_radius(radius::ROW).inner_margin(egui::Margin::symmetric(14, 10)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().item_spacing.y = 6.0;
                ui.label(RichText::new(if creating { "Novo cargo" } else { "Editando cargo" }).font(theme::semibold(13.5)).color(theme::GIZ));
                ui.add(ui_kit::field(&mut role.name).hint_text("Nome do cargo (1 a 24)"));
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Cor").font(theme::body(12.5)).color(theme::NEVOA));
                    for sw in Self::ROLE_SWATCHES {
                        let (r, resp) = ui.allocate_exact_size(Vec2::splat(18.0), egui::Sense::click());
                        ui.painter().rect_filled(r, 4, sw);
                        let mine = format!("#{:02X}{:02X}{:02X}", sw.r(), sw.g(), sw.b());
                        if role.color != mine {
                            ui.painter().rect_stroke(r, 4, egui::Stroke::new(1.0, theme::PAREDE), egui::StrokeKind::Inside);
                        }
                        if resp.clicked() {
                            role.color = mine;
                        }
                        resp.on_hover_text(role.color.clone());
                    }
                });
                egui::ComboBox::from_id_salt("role_position").width(120.0).selected_text(format!("Ordem: {}", role.position)).show_ui(ui, |ui| {
                    for p in -3..=3i64 {
                        ui.selectable_value(&mut role.position, p, p.to_string());
                    }
                });
                ui.add_space(space::S);
                ui.label(RichText::new("Permissões").font(theme::semibold(12.5)).color(theme::NEVOA));
                egui::Grid::new("perm_matrix").spacing(egui::vec2(10.0, 2.0)).show(ui, |ui| {
                    for (i, (bit, title, desc)) in Self::PERMS.iter().enumerate() {
                        let on = role.allow & bit != 0;
                        let (r, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 24.0), egui::Sense::click());
                        if resp.clicked() {
                            role.allow ^= bit;
                        }
                        ui.painter().rect_filled(r, radius::BUTTON, if on { theme::ABAJUR.gamma_multiply(0.22) } else { theme::MOVEL.gamma_multiply(0.5) });
                        let mark = if on { ph::CHECK } else { " " };
                        ui.painter().text(egui::pos2(r.left() + 12.0, r.center().y), egui::Align2::LEFT_CENTER, mark, theme::body(12.0), if on { theme::ABAJUR } else { theme::NEVOA });
                        ui.painter().text(egui::pos2(r.left() + 30.0, r.center().y), egui::Align2::LEFT_CENTER, *title, theme::body(12.0), if on { theme::GIZ } else { theme::NEVOA });
                        resp.on_hover_text(*desc);
                        if i % 2 == 1 {
                            ui.end_row();
                        }
                    }
                });
                ui.add_space(space::S);
                if may_edit {
                    ui.horizontal(|ui| {
                        if ui_kit::button(ui, Kind::Primary, None, if creating { "Criar cargo" } else { "Salvar" }).clicked() {
                            if creating {
                                self.admin(AdminCmd::CreateCustomRole { name: role.name.clone(), color: role.color.clone(), position: role.position, allow: role.allow });
                            } else {
                                self.admin(AdminCmd::UpdateCustomRole { role: role.clone() });
                            }
                            keep = false;
                        }
                        if ui_kit::button(ui, Kind::Ghost, None, "Cancelar").clicked() {
                            keep = false;
                        }
                    });
                }
            });
            if keep {
                self.roles.role_edit = Some(role);
            }
            ui.add_space(space::M);
        }
        // The list.
        ui_kit::section(ui, &format!("Cargos do servidor — {}", self.roles.custom.len()));
        ui.add_space(space::S);
        if self.roles.custom.is_empty() {
            ui_kit::hint(ui, "Nenhum cargo seu ainda. \"Criar cargo\" monta um com cor e permissões.");
        }
        if may_edit && self.roles.role_edit.is_none() && ui_kit::button(ui, Kind::Secondary, Some(ph::PLUS), "Criar cargo").clicked() {
            self.roles.role_edit = Some(CustomRole { id: 0, name: String::new(), color: "#99A7B5".into(), position: 0, allow: 0 });
        }
        ui.add_space(space::S);
        let customs = self.roles.custom.clone();
        egui::Grid::new("custom_roles").spacing(egui::vec2(8.0, 4.0)).show(ui, |ui| {
            for r in &customs {
                let color = Color32::from_hex(r.color.get(1..).unwrap_or("99A7B5")).unwrap_or(theme::NEVOA);
                let (dot, _) = ui.allocate_exact_size(Vec2::splat(14.0), egui::Sense::hover());
                ui.painter().circle_filled(dot.center(), 6.0, color);
                ui.label(RichText::new(&r.name).font(theme::semibold(13.0)).color(theme::GIZ));
                let n = Self::PERMS.iter().filter(|(b, _, _)| r.allow & b != 0).count();
                ui.label(RichText::new(format!("{n} permissões")).font(theme::body(11.5)).color(theme::NEVOA));
                if may_edit {
                    if ui_kit::button_sized(ui, Kind::Secondary, Size::Small, None, "Editar").clicked() {
                        self.roles.role_edit = Some(r.clone());
                    }
                    if ui_kit::button_sized(ui, Kind::Danger, Size::Small, None, "Apagar").on_hover_text("Os membros que o têm ficam sem ele (e sem o que ele dava).").clicked() {
                        self.admin(AdminCmd::DeleteCustomRole { id: r.id });
                    }
                }
                ui.end_row();
            }
        });
        ui.add_space(space::L);
        ui_kit::hint(ui, "A ordem pinta: o cargo mais alto de alguém decide a cor do nome. Dar cargos a alguém é no \"⋯\" da pessoa, em \"Membros e cargos\".");
    }

    /// Whether I have a permission (my fixed role first, then what custom roles gave me).
    pub fn i_allow(&self, bit: u64) -> bool {
        if self.my_role().can_admin() {
            return true;
        }
        let Some(me) = &self.me else { return false };
        me.role_ids.iter().filter_map(|id| self.roles.custom.iter().find(|r| r.id == *id)).any(|r| r.allow & bit != 0)
    }

    /// Someone's custom-role colour: the paint of their highest role, if any (the fixed
    /// role colour wins over it in the UI, matching how Discord paints).
    pub fn user_custom_color(&self, user: &User) -> Option<Color32> {
        user.role_ids
            .iter()
            .filter_map(|id| self.roles.custom.iter().find(|r| r.id == *id))
            .max_by_key(|r| (r.position, r.id))
            .and_then(|r| Color32::from_hex(r.color.get(1..).unwrap_or("99A7B5")).ok())
    }

    // ------------------------------------------------------------ "Canais"

    fn channels_tab(&mut self, ui: &mut egui::Ui) {
        ui_kit::page_title(ui, "Canais", Some("A ordem daqui é a da lateral. Apagar um canal de texto leva as mensagens dele junto."));
        let mut chans = self.channels.clone();
        chans.sort_by_key(|c| (c.position, c.id));
        let ids: Vec<ChannelId> = chans.iter().map(|c| c.id).collect();
        let mut reorder: Option<Vec<ChannelId>> = None;
        for (kind, title) in [(ChannelKind::Text, "Canais de texto"), (ChannelKind::Voice, "Canais de voz")] {
            let of_kind: Vec<&vox_proto::Channel> = chans.iter().filter(|c| c.kind == kind).collect();
            if of_kind.is_empty() {
                continue;
            }
            ui_kit::section(ui, title);
            ui.add_space(space::S);
            ui_kit::group_frame().inner_margin(egui::Margin::symmetric(6, 6)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().item_spacing.y = 0.0;
                for (i, c) in of_kind.iter().enumerate() {
                    // Up / down swap with the neighbour of the same kind (what the sidebar shows).
                    let swap = |other: ChannelId| {
                        let mut v = ids.clone();
                        let (a, b) = (v.iter().position(|x| *x == c.id), v.iter().position(|x| *x == other));
                        if let (Some(a), Some(b)) = (a, b) {
                            v.swap(a, b);
                        }
                        v
                    };
                    let up = (i > 0).then(|| of_kind[i - 1].id);
                    let down = of_kind.get(i + 1).map(|n| n.id);
                    if let Some(v) = self.channel_row(ui, c, up.map(swap), down.map(swap)) {
                        reorder = Some(v);
                    }
                }
            });
            ui.add_space(space::L);
        }
        if let Some(ids) = reorder {
            self.admin(AdminCmd::ReorderChannels { ids });
        }
        self.new_channel_group(ui);
    }

    /// One channel: its icon and name (or the rename field), then up, down, rename, "Acesso"
    /// (v0.9) and delete; the access editor opens right under it. Returns a new order.
    fn channel_row(&mut self, ui: &mut egui::Ui, c: &vox_proto::Channel, up: Option<Vec<ChannelId>>, down: Option<Vec<ChannelId>>) -> Option<Vec<ChannelId>> {
        let mut reorder = None;
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 38.0), Sense::hover());
        if resp.hovered() {
            ui.painter().rect_filled(rect, radius::ROW, theme::ROW_HOVER);
        }
        let mut row = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink2(Vec2::new(8.0, 0.0))).layout(egui::Layout::left_to_right(egui::Align::Center)));
        if self.channel_row_line(&mut row, c, up, down, &mut reorder) {
            return None;
        }
        // "Canal privado" of this channel (v0.9), right under its row.
        let mine = self.roles.editing_access.as_ref().is_some_and(|(id, _)| *id == c.id);
        if let Some((id, mut draft)) = self.roles.editing_access.take_if(|_| mine) {
            let mut keep = true;
            egui::Frame::new().fill(theme::NOITE).corner_radius(radius::ROW).inner_margin(egui::Margin::symmetric(14, 10)).outer_margin(egui::Margin { left: 28, right: 4, top: 2, bottom: 6 }).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().item_spacing.y = 6.0;
                access_fields(ui, &mut draft, &self.roles.members, self.roles.members_loaded, ("access", id));
                ui.add_space(space::S);
                // (One lamp button per view: a rename open at the same time keeps it.)
                let kind = if self.roles.renaming.is_some() { Kind::Secondary } else { Kind::Primary };
                ui.horizontal(|ui| {
                    if ui_kit::button_sized(ui, kind, Size::Small, None, "Salvar acesso").clicked() {
                        self.admin(AdminCmd::SetChannelAccess { id, private: draft.to_access() });
                    }
                    if ui_kit::button_sized(ui, Kind::Ghost, Size::Small, None, "Cancelar").clicked() {
                        keep = false;
                    }
                });
            });
            if keep && self.roles.editing_access.is_none() {
                self.roles.editing_access = Some((id, draft));
            }
        }
        reorder
    }

    /// The line of [`App::channel_row`]; true while it shows the rename field.
    fn channel_row_line(&mut self, ui: &mut egui::Ui, c: &vox_proto::Channel, up: Option<Vec<ChannelId>>, down: Option<Vec<ChannelId>>, reorder: &mut Option<Vec<ChannelId>>) -> bool {
        let private_ok = self.chatx.features.private_channels;
        ui.spacing_mut().item_spacing.x = 4.0;
        let icon = match (c.kind, c.private.is_some()) {
            (_, true) => crate::app::ICON_PRIVATE,
            (ChannelKind::Text, false) => ph::HASH,
            (ChannelKind::Voice, false) => ph::SPEAKER_HIGH,
        };
        ui.label(RichText::new(icon).font(theme::body(size::ICON)).color(theme::NEVOA));
        let renaming = matches!(&self.roles.renaming, Some((id, _)) if *id == c.id);
        if renaming {
            if let Some((id, name)) = self.roles.renaming.as_mut() {
                let r = ui.add(ui_kit::field(name).desired_width(200.0));
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                let (id, text) = (*id, name.trim().to_string());
                if ui_kit::button_sized(ui, Kind::Primary, Size::Small, None, "Salvar").clicked() || enter {
                    self.admin(AdminCmd::RenameChannel { id, name: text });
                }
                if ui_kit::button_sized(ui, Kind::Ghost, Size::Small, None, "Cancelar").clicked() {
                    self.roles.renaming = None;
                }
            }
            return true;
        }
        ui.add(egui::Label::new(RichText::new(&c.name).color(theme::GIZ)).truncate().selectable(false));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            let hover = if c.kind == ChannelKind::Text { "Apagar canal (e todas as mensagens dele)" } else { "Apagar sala (quem estiver nela sai da voz)" };
            if ui_kit::icon_button(ui, ph::TRASH, hover, IconState::Normal).clicked() {
                self.roles.confirm_delete = Some(c.id);
            }
            let editing = self.roles.editing_access.as_ref().is_some_and(|(id, _)| *id == c.id);
            if private_ok {
                let state = if editing { IconState::On } else { IconState::Normal };
                if ui_kit::icon_button(ui, ph::LOCK_SIMPLE, "Acesso: quem vê e entra neste canal (canal privado)", state).clicked() {
                    self.roles.editing_access = if editing { None } else { Some((c.id, AccessDraft::of(c.private.as_ref()))) };
                }
            }
            if ui_kit::icon_button(ui, ph::PENCIL_SIMPLE, "Renomear", IconState::Normal).clicked() {
                self.roles.renaming = Some((c.id, c.name.clone()));
            }
            if ui.add_enabled_ui(down.is_some(), |ui| ui_kit::icon_button(ui, ph::CARET_DOWN, "Descer na lista", IconState::Normal)).inner.clicked() {
                *reorder = down.clone();
            }
            if ui.add_enabled_ui(up.is_some(), |ui| ui_kit::icon_button(ui, ph::CARET_UP, "Subir na lista", IconState::Normal)).inner.clicked() {
                *reorder = up.clone();
            }
            if c.private.is_some() {
                ui_kit::pill(ui, "privado", theme::MOVEL, theme::NEVOA);
            }
        });
        false
    }

    /// "Novo canal": text or voice, the name and (v0.9) private from the start.
    fn new_channel_group(&mut self, ui: &mut egui::Ui) {
        let private_ok = self.chatx.features.private_channels;
        ui_kit::section(ui, "Novo canal");
        ui.add_space(space::S);
        let mut create = false;
        ui_kit::group_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 6.0;
            ui.horizontal(|ui| {
                ui.radio_value(&mut self.roles.new_voice, false, format!("{}  Texto", ph::HASH));
                ui.radio_value(&mut self.roles.new_voice, true, format!("{}  Voz", ph::SPEAKER_HIGH));
            });
            ui_kit::field_label(ui, "Nome do canal");
            let r = ui.add(ui_kit::field(&mut self.roles.new_channel).hint_text(if self.roles.new_voice { "ex.: Sala 3" } else { "ex.: jogatina" }).desired_width(280.0));
            let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if private_ok {
                let mut draft = std::mem::take(&mut self.roles.new_access);
                access_fields(ui, &mut draft, &self.roles.members, self.roles.members_loaded, ("access", -1));
                self.roles.new_access = draft;
            }
            ui.add_space(space::S);
            let name = self.roles.new_channel.trim().to_string();
            // (One lamp button per view: while a channel's rename or access editor is open, its
            // "Salvar" is the main action.)
            let kind = if self.roles.renaming.is_some() || self.roles.editing_access.is_some() { Kind::Secondary } else { Kind::Primary };
            create = (ui.add_enabled_ui(!name.is_empty(), |ui| ui_kit::button(ui, kind, Some(ph::PLUS), "Criar canal")).inner.clicked() || enter) && !name.is_empty();
        });
        if create {
            let name = self.roles.new_channel.trim().to_string();
            let kind = if self.roles.new_voice { ChannelKind::Voice } else { ChannelKind::Text };
            let private = if private_ok { self.roles.new_access.to_access() } else { None };
            self.admin(AdminCmd::CreateChannel { name, kind, private });
        }
    }

    // ------------------------------------------------------------ "Registro de moderação"

    fn audit_tab(&mut self, ui: &mut egui::Ui) {
        ui_kit::page_title(ui, "Registro de moderação", Some("Cada ação de moderação e de cargo, da mais recente para a mais antiga."));
        if !self.roles.audit_loaded {
            ui_kit::hint(ui, "Carregando...");
            return;
        }
        if self.roles.audit.is_empty() {
            ui_kit::hint(ui, "Nenhuma ação de moderação ainda.");
            return;
        }
        let now = crate::app::now_ms();
        let offset = crate::app::local_offset_min();
        ui_kit::group_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 8.0;
            for e in &self.roles.audit {
                ui.horizontal_top(|ui| {
                    let (r, _) = ui.allocate_exact_size(Vec2::new(92.0, 18.0), Sense::hover());
                    ui.painter().text(r.left_center(), egui::Align2::LEFT_CENTER, crate::app::format_time(e.at_ms, now, offset), theme::body(12.0), theme::NEVOA);
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        match &e.actor {
                            Some(a) => ui.label(role_name(&a.username, a.role, Some(theme::GIZ)).font(theme::semibold(13.5))),
                            None => ui.label(RichText::new("Host (linha de comando)").font(theme::semibold(13.5)).color(theme::GIZ)),
                        };
                        ui.label(RichText::new(audit_action_pt(&e.action)).color(theme::TEXTO));
                        if let Some(t) = &e.target {
                            ui.label(role_name(&t.username, t.role, Some(theme::GIZ)).font(theme::semibold(13.5)));
                        }
                        let d = audit_detail_pt(&e.action, &e.detail);
                        if !d.is_empty() {
                            ui.label(RichText::new(d).color(theme::NEVOA));
                        }
                    });
                });
            }
        });
        if self.roles.audit_more {
            ui.add_space(space::M);
            if ui_kit::button(ui, Kind::Secondary, None, "Mais antigos").clicked() {
                let before = self.roles.audit.last().map(|e| e.id);
                self.admin(AdminCmd::Audit { before });
            }
        }
    }

    // ------------------------------------------------------------ "Configurações do servidor"

    fn server_tab(&mut self, ui: &mut egui::Ui) {
        ui_kit::page_title(ui, "Configurações do servidor", None);
        let mine = self.my_role();
        let account = self.roles.account.unwrap_or_default();
        let label_w = 150.0;
        ui_kit::section(ui, "Sobre este servidor");
        ui.add_space(space::S);
        ui_kit::group_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui_kit::info_row(ui, label_w, "Nome", |ui| ui.label(RichText::new(&self.server_name).color(theme::GIZ)));
            let address = crate::host::url_host_port(&self.session_url).map(|(h, p)| p.map_or(h.clone(), |p| format!("{h}:{p}"))).unwrap_or_default();
            ui_kit::info_row(ui, label_w, "Endereço", |ui| ui.label(RichText::new(address).monospace().color(theme::GIZ)));
            ui_kit::info_row(ui, label_w, "Seu cargo", |ui| ui_kit::pill(ui, role_label(mine), theme::MOVEL, role_color(mine).unwrap_or(theme::GIZ)));
            if self.roles.members_loaded {
                let n = self.roles.members.len();
                ui_kit::info_row(ui, label_w, "Membros", |ui| ui.label(RichText::new(n.to_string()).color(theme::GIZ)));
            }
            ui_kit::info_row(ui, label_w, "Online agora", |ui| ui.label(RichText::new(self.online.len().to_string()).color(theme::GIZ)));
        });
        if self.host.runner.is_some() {
            ui.add_space(space::S);
            if ui_kit::button(ui, Kind::Secondary, Some(ph::HARD_DRIVES), "Meu servidor (portas, roteador, desligar)").clicked() {
                self.host.panel_open = true;
            }
        }
        ui.add_space(space::L);
        ui_kit::section(ui, "Posse do servidor");
        ui.add_space(space::S);
        ui_kit::group_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 6.0;
            let owner = self.roles.members.iter().find(|m| m.user.role == Role::Owner).map(|m| m.user.shown_name().to_string());
            if account.no_owner {
                ui_kit::body_text(ui, "Este servidor ainda não tem dono.");
                ui_kit::hint(ui, "Quem hospeda vira dono entrando pelo próprio PC do servidor, ou com o código que a janela do servidor mostra.");
                if ui_kit::button(ui, Kind::Secondary, Some(ph::CROWN), "Reivindicar servidor...").clicked() {
                    self.roles.claim_open = true;
                    self.roles.claim_msg = None;
                }
            } else if let Some(o) = &owner {
                ui_kit::info_row(ui, label_w, "Dono", |ui| ui.label(role_name(o, Role::Owner, None).font(theme::semibold(13.5))));
            }
            if account.owner_pinned {
                ui_kit::hint(ui, "O dono está fixado no config.toml do servidor (owner = ...): para trocar, quem hospeda edita esse arquivo e reinicia o servidor.");
            } else if mine == Role::Owner {
                ui_kit::field_label(ui, "Transferir posse do servidor");
                ui_kit::hint(ui, "A pessoa vira a dona e você passa a Admin. Só aparece quem já tem senha pessoal.");
                let my_id = self.my_id();
                let candidates: Vec<MemberInfo> = self.roles.members.iter().filter(|m| Some(m.user.id) != my_id && m.has_password).cloned().collect();
                ui.horizontal(|ui| {
                    let chosen = self.roles.transfer_pick.and_then(|id| candidates.iter().find(|m| m.user.id == id));
                    let text = chosen.map(|m| m.user.shown_name().to_string()).unwrap_or_else(|| "Escolha uma pessoa".into());
                    egui::ComboBox::from_id_salt("transfer_owner_pick").width(220.0).selected_text(text).show_ui(ui, |ui| {
                        for m in &candidates {
                            ui.selectable_value(&mut self.roles.transfer_pick, Some(m.user.id), format!("{} ({})", m.user.shown_name(), role_label(m.user.role)));
                        }
                        if candidates.is_empty() {
                            ui.label(RichText::new("Ninguém com senha pessoal ainda.").color(theme::NEVOA));
                        }
                    });
                    let pick = self.roles.transfer_pick.and_then(|id| candidates.iter().find(|m| m.user.id == id)).map(|m| m.user.clone());
                    if ui.add_enabled_ui(pick.is_some(), |ui| ui_kit::button(ui, Kind::Danger, Some(ph::CROWN), "Transferir posse...")).inner.clicked() {
                        self.roles.confirm_transfer = pick;
                    }
                });
            }
        });
        ui.add_space(space::L);
        ui_kit::section(ui, "Sua senha pessoal");
        ui.add_space(space::S);
        ui_kit::group_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui_kit::hint(
                ui,
                if account.has_password {
                    "Seu nome está protegido por uma senha pessoal. Para trocá-la, abra as Configurações."
                } else {
                    "Seu cargo exige uma senha pessoal. Defina uma nas Configurações para usar as permissões."
                },
            );
            ui.add_space(space::S);
            if ui_kit::button(ui, Kind::Secondary, Some(ph::GEAR_SIX), "Abrir configurações").clicked() {
                self.open_settings();
            }
        });
    }
}

/// The pages of the server window, in order: (tab, icon, name).
const SERVER_PAGES: [(ServerTab, &str, &str); 6] = [
    (ServerTab::Members, ph::USERS, "Membros e cargos"),
    (ServerTab::Roles, ph::GAVEL, "Cargos"),
    (ServerTab::Channels, ph::HASH, "Canais"),
    (ServerTab::Invites, ph::ENVELOPE_SIMPLE, "Convites"),
    (ServerTab::Audit, ph::CLIPBOARD_TEXT, "Registro de moderação"),
    (ServerTab::Settings, ph::GEAR_SIX, "Configurações do servidor"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hub_errors_read_in_portuguese() {
        let e = |s: &str| anyhow::anyhow!("{s}");
        assert!(admin_error(&e("set role failed: HTTP 409 Conflict: they must set a personal password first")).contains("senha pessoal antes"));
        assert_eq!(admin_error(&e("server mute failed: HTTP 403 Forbidden: your role does not allow this")), "Seu cargo não permite isso.");
        assert!(admin_error(&e("disconnect failed: HTTP 409 Conflict: not in voice")).contains("não está em um canal de voz"));
        assert!(admin_error(&e("set password failed: HTTP 401 Unauthorized: wrong personal password")).contains("senha atual"));
        assert!(admin_error(&e("delete channel failed: HTTP 409 Conflict: the last text channel cannot be deleted")).contains("último canal"));
        assert!(admin_error(&e("members failed: HTTP 404 Not Found: ")).contains("versão antiga"));
    }

    #[test]
    fn audit_lines_read_in_portuguese() {
        assert_eq!(audit_action_pt(vox_proto::AUDIT_MOVE), "moveu");
        assert_eq!(audit_action_pt("something_new"), "something_new");
        assert_eq!(audit_detail_pt(vox_proto::AUDIT_SET_ROLE, "member -> moderator"), "de Membro para Moderador");
        assert_eq!(audit_detail_pt(vox_proto::AUDIT_MOVE, "Sala 1 -> Sala 2"), "de Sala 1 para Sala 2");
        assert_eq!(audit_action_pt(vox_proto::AUDIT_INVITE_CREATE), "criou o convite");
        assert_eq!(audit_detail_pt(vox_proto::AUDIT_INVITE_CREATE, "#1 86400s 5x"), "#1 · vale 24 horas · até 5 pessoas");
        assert_eq!(audit_detail_pt(vox_proto::AUDIT_INVITE_CREATE, "#2 sem prazo ilimitado"), "#2 · sem prazo · sem limite de pessoas");
    }

    #[test]
    fn owner_messages_read_in_portuguese() {
        let e = |s: &str| anyhow::anyhow!("{s}");
        assert!(admin_error(&e("claim server failed: HTTP 403 Forbidden: wrong claim code")).starts_with("Código incorreto"));
        let proof = admin_error(&e(&format!("set password failed: HTTP 403 Forbidden: {}", vox_proto::ERR_OWNER_PROOF_REQUIRED)));
        assert!(proof.contains("127.0.0.1") && proof.contains("Código do servidor"), "{proof}");
        assert!(admin_error(&e("claim server failed: HTTP 409 Conflict: this server already has an owner")).contains("já tem dono"));
        assert!(admin_error(&e("claim server failed: HTTP 429 Too Many Requests: too many attempts, wait 2 s")).contains("Muitas tentativas"));
        assert!(admin_error(&e("transfer ownership failed: HTTP 409 Conflict: the owner is set in config.toml: change it there")).contains("config.toml"));
        assert!(admin_error(&e("transfer ownership failed: HTTP 409 Conflict: they must set a personal password first")).contains("senha pessoal antes"));
        assert_eq!(audit_action_pt(vox_proto::AUDIT_CLAIM_OWNER), "virou o Dono do servidor");
        assert_eq!(audit_detail_pt(vox_proto::AUDIT_CLAIM_OWNER, "pc-do-servidor"), "entrando pelo próprio PC do servidor");
        assert_eq!(audit_detail_pt(vox_proto::AUDIT_CLAIM_OWNER, "codigo"), "com o código do servidor");
        assert!(audit_action_pt(vox_proto::AUDIT_TRANSFER_OWNER).contains("transferiu a posse"));
    }

    #[test]
    fn private_channel_fields_round_trip() {
        assert_eq!(AccessDraft::default().to_access(), None, "public");
        let p = PrivateAccess { roles: vec![Role::Member], users: vec![9, 4] };
        let d = AccessDraft::of(Some(&p));
        assert!(d.private && d.member && !d.moderator);
        assert_eq!(d.to_access(), Some(PrivateAccess { roles: vec![Role::Member], users: vec![4, 9] }));
        let only_staff = AccessDraft { private: true, ..Default::default() };
        assert_eq!(only_staff.to_access(), Some(PrivateAccess::default()), "private with nobody: Dono and Admin only");
        assert_eq!(audit_action_pt(vox_proto::AUDIT_CHANNEL_ACCESS), "mudou o acesso do canal");
        let e = anyhow::anyhow!("move failed: HTTP 403 Forbidden: they have no access to that private channel");
        assert!(admin_error(&e).contains("canal privado"));
    }

    #[test]
    fn the_people_picker_offers_members_and_moderators_only() {
        let info = |id, name: &str, role| MemberInfo { user: User { id, username: name.into(), role, flags: 0, role_ids: Vec::new(), avatar: None, display_name: None }, has_password: false, server_muted: false, created_at_ms: 0, granted: 0, banned: None };
        let ms = vec![info(1, "dono", Role::Owner), info(2, "adm", Role::Admin), info(3, "ana", Role::Member), info(4, "anabela", Role::Moderator), info(5, "bia", Role::Member)];
        let names = |v: Vec<&MemberInfo>| v.iter().map(|m| m.user.username.clone()).collect::<Vec<_>>();
        assert_eq!(names(pick_candidates(&ms, &[], "an")), ["ana", "anabela"]);
        assert_eq!(names(pick_candidates(&ms, &[3], "AN")), ["anabela"], "already chosen");
        assert!(pick_candidates(&ms, &[], "d").is_empty(), "Dono and Admin always have access: not offered");
        assert!(pick_candidates(&ms, &[], " ").is_empty());
    }

    #[test]
    fn staff_names_get_a_color_members_keep_theirs() {
        assert_eq!(role_color(Role::Member), None);
        assert!(role_color(Role::Owner).is_some() && role_color(Role::Moderator) != role_color(Role::Admin));
        assert_eq!(role_label(Role::Moderator), "Moderador");
    }

    /// Every permission bit has a row in the editor matrix — a bit the proto learns can
    /// never be invisible in the UI.
    #[test]
    fn the_permission_matrix_covers_every_grantable_bit() {
        let mut covered = 0u64;
        for (bit, title, desc) in App::PERMS {
            assert_eq!(bit.count_ones(), 1, "{title} é um bit único");
            assert!(!title.is_empty() && !desc.is_empty());
            covered |= bit;
        }
        assert_eq!(covered, perm::GRANTABLE, "o editor cobre exatamente as permissões concedíveis");
        assert_eq!(App::PERMS.len(), 21);
    }
}
