//! The chat (v1.0): one renderer for a text channel (in the center or docked beside the cards)
//! and a DM. Discord-like groups: the picture, the name and the time on a group's first message,
//! compact follow-ups (their time shows in the gutter under the pointer); the quote line of a
//! reply, mention chips, the "mentions me" highlight, images, link cards and reaction pills; the
//! hover bar (reply, react, delete). The input: the field with the attach and emoji buttons, the
//! member list for `@`.
//!
//! Light on the frame like before: egui caches the text layouts, the chip layout of a message is
//! cached in `chat_extras`, nothing is cloned per message unless it is clicked.
//! A child module of `app` (it uses the app's private helpers).

use std::time::Instant;

use eframe::egui::{self, Align2, RichText, Sense, Vec2};
use vox_proto::{Attachment, ChannelId, ChatFeatures, DmMessage, DmMessageKind, Message, Reaction, ReplyRef, User, UserId};

use super::chat_extras::{self, Act, Target};
use super::{format_time, local_offset_min, name_color, now_ms, App};
use crate::backend::Cmd;
use crate::context_menu::{Lead, Origin};
use crate::previews;
use crate::theme::{self, ph};
use crate::ui_kit;

/// The picture beside a group's first message.
pub const AVATAR: f32 = 32.0;
/// Between the picture column and the text.
const GUTTER_GAP: f32 = 12.0;
/// Space above a group.
const GROUP_GAP: f32 = 12.0;
/// A new group starts after this much time from the same author.
const GROUP_MS: i64 = 5 * 60_000;

/// What the renderer needs of a message (a channel's or a DM's).
pub trait ChatLine {
    fn id(&self) -> i64;
    fn author(&self) -> &User;
    fn at_ms(&self) -> i64;
    fn reply(&self) -> Option<&ReplyRef>;
    fn caption(&self) -> &str;
    fn attachments(&self) -> &[Attachment];
    fn reactions(&self) -> &[Reaction];
    /// Who the text mentions (chips); DMs use the two people instead.
    fn mention_ids(&self) -> &[UserId];
    fn everyone(&self) -> bool;
    fn mentions_me(&self, me: UserId) -> bool;
    /// A DM's call line ("Chamada perdida"...): its text and whether it ended normally.
    fn call_line(&self, _me: UserId) -> Option<(String, bool)> {
        None
    }
}

impl ChatLine for Message {
    fn id(&self) -> i64 {
        self.id
    }
    fn author(&self) -> &User {
        &self.author
    }
    fn at_ms(&self) -> i64 {
        self.created_at_ms
    }
    fn reply(&self) -> Option<&ReplyRef> {
        self.reply.as_ref()
    }
    fn caption(&self) -> &str {
        Message::caption(self)
    }
    fn attachments(&self) -> &[Attachment] {
        &self.attachments
    }
    fn reactions(&self) -> &[Reaction] {
        &self.reactions
    }
    fn mention_ids(&self) -> &[UserId] {
        &self.mentions
    }
    fn everyone(&self) -> bool {
        self.mention_everyone
    }
    fn mentions_me(&self, me: UserId) -> bool {
        Message::mentions_me(self, me)
    }
}

impl ChatLine for DmMessage {
    fn id(&self) -> i64 {
        self.id
    }
    fn author(&self) -> &User {
        &self.author
    }
    fn at_ms(&self) -> i64 {
        self.created_at_ms
    }
    fn reply(&self) -> Option<&ReplyRef> {
        self.reply.as_ref()
    }
    fn caption(&self) -> &str {
        DmMessage::caption(self)
    }
    fn attachments(&self) -> &[Attachment] {
        &self.attachments
    }
    fn reactions(&self) -> &[Reaction] {
        &self.reactions
    }
    fn mention_ids(&self) -> &[UserId] {
        &[]
    }
    fn everyone(&self) -> bool {
        false
    }
    fn mentions_me(&self, _me: UserId) -> bool {
        false
    }
    fn call_line(&self, me: UserId) -> Option<(String, bool)> {
        (self.kind != DmMessageKind::Text).then(|| (super::dms::call_line_text(self, me), self.kind == DmMessageKind::CallEnded))
    }
}

/// Where a list is and what is allowed in it.
pub struct ListSpec<'a> {
    pub target: Target,
    pub scroll_id: egui::Id,
    pub me: Option<UserId>,
    pub features: ChatFeatures,
    /// Replies and reactions allowed (not in a blocked DM).
    pub social: bool,
    /// Role colours and badges on names (channels on a hub with roles).
    pub roles_on: bool,
    /// May delete others' messages (channels, Moderador and above).
    pub can_mod: bool,
    /// The two people of a DM (their `@name` shows as a chip).
    pub dm_people: Option<[UserId; 2]>,
    /// What an empty, fully loaded list says.
    pub empty: &'a str,
    /// The history paging state.
    pub requested: bool,
    pub no_more: bool,
    pub loading_older: bool,
    /// `previews::tick_view` key of this chat.
    pub view_key: i64,
}

/// What the list asks for, done after it is drawn (outside its borrows).
#[derive(Default)]
pub struct ListOut {
    pub acts: Vec<Act>,
    pub offers: Vec<(Target, i64)>,
    pub open_profile: Option<User>,
    pub open_menu: Option<(User, egui::Pos2)>,
    pub open_image: Option<(Vec<Attachment>, usize)>,
    pub load_older: Option<i64>,
}

/// The borrowed parts of the app the renderer draws with.
pub struct Parts<'a> {
    pub avatars: &'a mut crate::avatars::AvatarStore,
    pub chatx: &'a mut chat_extras::ChatUi,
    pub images: &'a mut crate::chat_images::ImagesUi,
    pub previews: &'a mut crate::previews::PreviewStore,
    pub previews_on: bool,
    pub auto_images: bool,
}

/// The message list of one chat, scrolled to the bottom as messages arrive.
pub fn message_list<T: ChatLine>(ui: &mut egui::Ui, p: Parts<'_>, msgs: &[T], spec: &ListSpec<'_>) -> ListOut {
    let mut out = ListOut::default();
    let (now, offset, now_instant) = (now_ms(), local_offset_min(), Instant::now());
    let images_settled = p.previews.tick_view(spec.view_key);
    let may_ask = p.previews_on && images_settled;
    if p.previews_on {
        p.previews.maintain(now_instant);
    }
    let me = spec.me;
    egui::ScrollArea::vertical().id_salt(spec.scroll_id).auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.add_space(6.0);
        let quiet = |ui: &mut egui::Ui, text: &str| {
            ui.add_space(6.0);
            ui.label(RichText::new(text).color(theme::NEVOA));
        };
        if !spec.requested || (msgs.is_empty() && !spec.no_more) {
            quiet(ui, "Carregando mensagens...");
        } else if !spec.no_more {
            ui.add_enabled_ui(!spec.loading_older, |ui| {
                if ui_kit::button_sized(ui, ui_kit::Kind::Ghost, ui_kit::Size::Small, None, "Carregar mensagens anteriores").clicked() {
                    out.load_older = msgs.first().map(|m| m.id());
                }
            });
        } else if msgs.is_empty() {
            quiet(ui, spec.empty);
        }
        let mut prev: Option<(UserId, i64)> = None;
        for m in msgs {
            if let Some((text, ended)) = me.and_then(|me| m.call_line(me)) {
                call_line(ui, &text, ended, &format_time(m.at_ms(), now, offset));
                prev = None;
                continue;
            }
            let author = m.author();
            // A reply always starts a group: its quote sits above the header.
            let new_group = prev.is_none_or(|(a, t)| a != author.id || m.at_ms() - t > GROUP_MS) || m.reply().is_some();
            ui.add_space(if new_group { GROUP_GAP } else { 2.0 });
            let bg = chat_extras::row_bg_slot(ui);
            let row_top = ui.cursor().top();
            let menus_before = out.open_menu.is_some();
            let row = ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = GUTTER_GAP;
                if new_group {
                    let pic = crate::avatars::avatar_widget(p.avatars, ui, author, AVATAR, crate::avatars::Surface::Chat);
                    if pic.secondary_clicked() {
                        out.open_menu = Some((author.clone(), pic.ctx.pointer_interact_pos().unwrap_or(pic.rect.center())));
                    }
                    if crate::profile::hit(pic, author) {
                        out.open_profile = Some(author.clone());
                    }
                } else {
                    ui.allocate_exact_size(Vec2::new(AVATAR, 1.0), Sense::hover());
                }
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 3.0;
                    if let Some(r) = m.reply() {
                        if chat_extras::quote_line(ui, r) {
                            out.acts.push(Act::Jump { id: r.id, deleted: r.deleted });
                        }
                    }
                    if new_group {
                        header(ui, author, spec.roles_on, &format_time(m.at_ms(), now, offset), &mut out);
                    }
                    let top = ui.cursor().top();
                    let caption = m.caption();
                    if !caption.is_empty() || m.attachments().is_empty() {
                        let (ids, everyone) = match &spec.dm_people {
                            Some(us) => (&us[..], false),
                            None => (m.mention_ids(), m.everyone()),
                        };
                        chat_extras::message_text(ui, p.chatx, spec.target, m.id(), caption, ids, everyone, me);
                    }
                    if let Some(i) = crate::chat_images::show_attachments(ui, p.images, m.attachments(), p.auto_images, images_settled) {
                        out.open_image = Some((m.attachments().to_vec(), i));
                    }
                    if p.previews_on {
                        let band = egui::Rect::from_min_max(egui::pos2(ui.max_rect().left(), top), egui::pos2(ui.max_rect().right(), ui.cursor().top().max(top + 1.0)));
                        let on_screen = may_ask && ui.is_rect_visible(band);
                        for url in crate::links::urls(caption, previews::MAX_CARDS_PER_MESSAGE) {
                            if on_screen {
                                p.previews.want(&url, now_instant);
                            }
                            let ctx = ui.ctx().clone();
                            if let Some(card) = p.previews.card(&ctx, &url, now_instant) {
                                ui.add_space(2.0);
                                previews::show_card(ui, &url, &card);
                                ui.add_space(2.0);
                            }
                        }
                    }
                    chat_extras::reaction_chips(ui, p.chatx, m.id(), m.reactions(), me, &mut out.acts);
                });
            });
            // The row's background (mention, a clicked quote's flash, hover), its right-click menu
            // (anywhere on the row but the author's name and picture, which have the person's
            // menu), the time of a follow-up and the hover bar.
            let left = ui.max_rect().left() - 6.0;
            let row = egui::Rect::from_min_max(egui::pos2(left, row_top - 2.0), egui::pos2(ui.max_rect().right() + 6.0, row.response.rect.bottom() + 2.0));
            let hovered = ui.rect_contains_pointer(row) || p.chatx.force_hover == Some(m.id());
            let flash = chat_extras::jump_flash(ui, p.chatx, spec.target, m.id(), row);
            chat_extras::paint_row_bg(ui, bg, row, me.is_some_and(|me| m.mentions_me(me)), flash, hovered);
            if hovered && !new_group {
                let t = format_time(m.at_ms(), now, offset);
                let t = t.rsplit(' ').next().unwrap_or_default();
                ui.painter().text(egui::pos2(left + 6.0 + AVATAR / 2.0, row.top() + 12.0), Align2::CENTER_CENTER, t, theme::body(10.5), theme::NEVOA);
            }
            let own = me == Some(author.id);
            let (roles_for_delete, can_mod) = match spec.target {
                Target::Channel(_) => (spec.roles_on, spec.can_mod),
                Target::Dm(_) => (true, false),
            };
            let actions = chat_extras::MsgActions {
                features: spec.features,
                social: spec.social,
                delete: chat_extras::may_delete(spec.target, roles_for_delete, me, author.id, can_mod, true),
                own,
            };
            if actions.delete {
                out.offers.push((spec.target, m.id()));
            }
            let person_menu = !menus_before && out.open_menu.is_some();
            chat_extras::message_menu(ui, row, ui.id().with(("msg_menu", m.id())), actions, m.id(), person_menu, &mut out.acts);
            if hovered {
                chat_extras::hover_bar(ui, row, actions, m.id(), &mut out.acts);
            }
            prev = Some((author.id, m.at_ms()));
        }
        ui.add_space(10.0);
    });
    out
}

/// A group's header: the name (role colour, or the person's colour), the role badge, the time.
fn header(ui: &mut egui::Ui, author: &User, roles_on: bool, time: &str, out: &mut ListOut) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let role = if roles_on { author.role } else { Default::default() };
        let color = crate::roles::role_color(role).unwrap_or_else(|| name_color(author.id));
        let who = ui.add(egui::Label::new(RichText::new(author.shown_name()).font(theme::semibold(14.0)).color(color)).sense(Sense::click()));
        if who.secondary_clicked() {
            out.open_menu = Some((author.clone(), who.ctx.pointer_interact_pos().unwrap_or(who.rect.center())));
        }
        if crate::profile::hit(who, author) {
            out.open_profile = Some(author.clone());
        }
        crate::roles::role_badge(ui, role);
        ui.label(RichText::new(time).font(theme::body(11.0)).color(theme::NEVOA));
    });
}

/// A DM call line ("Chamada perdida", "Chamada de 3 min"): quiet, across the text column.
fn call_line(ui: &mut egui::Ui, text: &str, ended: bool, time: &str) {
    ui.add_space(GROUP_GAP);
    ui.horizontal(|ui| {
        ui.add_space(AVATAR + GUTTER_GAP - ui.spacing().item_spacing.x);
        let color = if ended { theme::ONLINE } else { theme::AVISO };
        ui.label(RichText::new(format!("{}  {text}", ph::PHONE)).color(color));
        ui.label(RichText::new(time).font(theme::body(11.0)).color(theme::NEVOA));
    });
}

impl App {
    /// Do what a list asked for.
    fn list_done(&mut self, ui: &egui::Ui, target: Target, out: ListOut, origin: Origin) {
        self.chatx.delete_offers = out.offers;
        self.chat_acts(target, out.acts);
        if let Some((items, i)) = out.open_image {
            self.open_viewer(items, i);
        }
        self.pump_images();
        if let Some(author) = out.open_profile {
            let ctx = ui.ctx().clone();
            self.open_profile(&ctx, &author);
        }
        if let Some((author, pos)) = out.open_menu {
            let ctx = ui.ctx().clone();
            self.open_person_menu(&ctx, &author, origin, Lead::None, pos);
        }
        if self.settings.show_link_previews && !self.previews.unsupported() {
            self.pump_previews();
        }
    }

    /// A text channel's chat (messages and input) filling `ui`: the center's, or the docked one.
    pub(crate) fn channel_chat(&mut self, ui: &mut egui::Ui, ch: ChannelId) {
        egui::Panel::bottom(egui::Id::new("chat_input_panel")).resizable(false)
            .frame(egui::Frame::new().inner_margin(egui::Margin { left: 12, right: 12, top: 4, bottom: 12 }))
            .show_separator_line(false)
            .show(ui, |ui| self.chat_input(ui, ch));
        egui::CentralPanel::no_frame().show(ui, |ui| {
            let ui = &mut ui.new_child(egui::UiBuilder::new().max_rect(ui.max_rect().shrink2(egui::vec2(12.0, 0.0))));
            self.channel_messages(ui, ch);
        });
    }

    fn channel_messages(&mut self, ui: &mut egui::Ui, ch: ChannelId) {
        let epoch = self.channel_epochs.get(&ch).copied().unwrap_or(0);
        let (me, can_mod) = (self.my_id(), self.my_role().can_moderate());
        let log = self.logs.entry(ch).or_default();
        let spec = ListSpec {
            target: Target::Channel(ch),
            scroll_id: egui::Id::new(("msgs", ch, epoch)),
            me,
            features: self.chatx.features,
            social: true,
            roles_on: self.roles.supported,
            can_mod,
            dm_people: None,
            empty: "Nenhuma mensagem ainda. Diga olá!",
            requested: log.requested,
            no_more: log.no_more,
            loading_older: log.loading_older,
            view_key: ch,
        };
        let previews_on = self.settings.show_link_previews && !self.previews.unsupported();
        let parts = Parts {
            avatars: &mut self.avatars.store,
            chatx: &mut self.chatx,
            images: &mut self.images,
            previews: &mut self.previews,
            previews_on,
            auto_images: self.settings.auto_show_images,
        };
        let out = message_list(ui, parts, &log.msgs, &spec);
        if let Some(before) = out.load_older {
            log.loading_older = true;
            self.backend.send(Cmd::LoadHistory { channel: ch, before: Some(before) });
        }
        self.list_done(ui, Target::Channel(ch), out, Origin::Chat);
    }

    /// The DM's lines (the header is the center header's).
    pub(super) fn dm_messages(&mut self, ui: &mut egui::Ui, dm: vox_proto::DmId, peer: &User, blocked: bool) {
        let me = self.my_id().unwrap_or_default();
        let log = self.dms.logs.entry(dm).or_default();
        let empty = format!("Este é o começo da sua conversa com {}.", peer.shown_name());
        let spec = ListSpec {
            target: Target::Dm(dm),
            scroll_id: egui::Id::new(("dm", dm)),
            me: Some(me),
            features: self.chatx.features,
            social: !blocked,
            roles_on: false,
            can_mod: false,
            dm_people: Some([me, peer.id]),
            empty: &empty,
            requested: log.requested,
            no_more: log.no_more,
            loading_older: log.loading_older,
            view_key: -1_000_000 - dm,
        };
        let previews_on = self.settings.show_link_previews && !self.previews.unsupported();
        let parts = Parts {
            avatars: &mut self.avatars.store,
            chatx: &mut self.chatx,
            images: &mut self.images,
            previews: &mut self.previews,
            previews_on,
            auto_images: self.settings.auto_show_images,
        };
        let out = message_list(ui, parts, &log.msgs, &spec);
        if let Some(before) = out.load_older {
            log.loading_older = true;
            self.backend.send(Cmd::Dm(super::dms::DmCmd::History { dm, before: Some(before) }));
        }
        self.list_done(ui, Target::Dm(dm), out, Origin::Dm);
    }

    /// The channel's input: pictures waiting, the reply bar, then the field with the attach and
    /// emoji buttons (Enter sends).
    fn chat_input(&mut self, ui: &mut egui::Ui, ch: ChannelId) {
        let name = self.channels.iter().find(|c| c.id == ch).map(|c| c.name.as_str()).unwrap_or_default();
        let hint = format!("Mensagem em #{name}");
        let target = Target::Channel(ch);
        self.attach_strip(ui, target);
        self.reply_bar(ui, target);
        let mut send = false;
        input_frame().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                self.attach_button(ui, target);
                let right = if self.input.trim().is_empty() { 30.0 } else { 64.0 };
                let width = (ui.available_width() - right).max(40.0);
                let resp = self.mention_input(ui, target, hint, width);
                send |= resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if emoji_button(ui, &mut self.input, &mut self.emoji, &mut self.emoji_textures, &mut self.settings.recent_emoji, crate::app::now_ms() as u64) {
                    resp.request_focus();
                }
                if !self.input.trim().is_empty() && ui_kit::icon_button(ui, ph::PAPER_PLANE_RIGHT, "Enviar (Enter)", ui_kit::IconState::Normal).clicked() {
                    send = true;
                }
                if send {
                    resp.request_focus();
                }
                if self.focus_input {
                    self.focus_input = false;
                    resp.request_focus();
                }
            });
        });
        if send {
            self.send_message_to(ch);
        }
    }

    /// The DM's input (same look; a blocked conversation cannot be written to).
    pub(super) fn dm_input_box(&mut self, ui: &mut egui::Ui, dm: vox_proto::DmId, name: &str, blocked: bool) {
        let target = Target::Dm(dm);
        if !blocked {
            self.attach_strip(ui, target);
            self.reply_bar(ui, target);
        }
        let mut send = false;
        input_frame().show(ui, |ui| {
            ui.add_enabled_ui(!blocked, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    if !blocked {
                        self.attach_button(ui, target);
                    }
                    let right = if self.dms.input.trim().is_empty() { 30.0 } else { 64.0 };
                    let hint = if blocked { format!("Você bloqueou @{name}") } else { format!("Mensagem para @{name}") };
                    let resp = ui.add(
                        field_edit(&mut self.dms.input, hint)
                            .char_limit(vox_proto::DM_MESSAGE_MAX_CHARS)
                            .desired_width((ui.available_width() - right).max(40.0))
                            .id(egui::Id::new("dm_input_box")),
                    );
                    send |= resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if resp.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                        self.cancel_reply(target);
                    }
                    if emoji_button(ui, &mut self.dms.input, &mut self.emoji, &mut self.emoji_textures, &mut self.settings.recent_emoji, crate::app::now_ms() as u64) {
                        resp.request_focus();
                    }
                    if !self.dms.input.trim().is_empty() && ui_kit::icon_button(ui, ph::PAPER_PLANE_RIGHT, "Enviar (Enter)", ui_kit::IconState::Normal).clicked() {
                        send = true;
                    }
                    if send || std::mem::take(&mut self.dms.focus_input) {
                        resp.request_focus();
                    }
                });
            });
        });
        if send {
            self.send_dm();
        }
    }
}

/// The input's frame: the field look of `ui_kit` (raised, radius 8), 44 high.
fn input_frame() -> egui::Frame {
    egui::Frame::new().fill(theme::MOVEL).corner_radius(theme::radius::BUTTON).inner_margin(egui::Margin::symmetric(6, 7))
}

/// A frameless text box for inside [`input_frame`].
pub(super) fn field_edit<'t>(text: &'t mut String, hint: String) -> egui::TextEdit<'t> {
    egui::TextEdit::singleline(text)
        .hint_text(RichText::new(hint).color(theme::NEVOA))
        .frame(egui::Frame::NONE)
        .margin(egui::Margin::symmetric(4, 4))
        .font(theme::body(14.0))
}

/// The smiley button: a small grid of emoji that go into the text. Returns true when one was
/// picked (the caller gives the focus back to the field).
fn emoji_button(ui: &mut egui::Ui, text: &mut String, picker: &mut crate::emoji::Picker, textures: &mut crate::emoji::Textures, uses: &mut Vec<crate::emoji::Use>, now_ms: u64) -> bool {
    let b = ui_kit::icon_button(ui, ph::SMILEY, "Emoji", ui_kit::IconState::Normal);
    let mut picked = false;
    if let Some(e) = crate::emoji::Picker::menu(ui, &b, picker, textures, uses, now_ms) {
        text.push_str(e);
        picked = true;
    }
    picked
}
