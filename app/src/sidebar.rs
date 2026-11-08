//! The v1.0 sidebar (docs/PLANO-v1.0.md, the mockup in dist/design/voxhub-v1-proposta.html):
//!
//! - the server header (its name in the display font, "N online · M na voz"); a click opens the
//!   server menu: server settings, invites, my hosted server, claiming an ownerless server,
//!   "Configurações" and "Trocar de servidor";
//! - DMs, text channels (🔒 when private, the unread counter and the "@"), voice rooms with the
//!   people inside (the lamp ring on whoever speaks, "ao vivo" (a click watches), the camera);
//! - the voice panel while in (or joining) a call: the state, the room (a click opens the stage),
//!   camera, screen share and leave;
//! - the "você" panel: my picture, name and status (a click changes the presence), the
//!   microphone, deafen and settings buttons.
//!
//! Only what is open in the center is highlighted (`layout::open_in_center`). Rows are painted
//! (no nested layouts per row); the member list on the right of the chat is here too.
//! A child module of `app`.

use std::collections::HashMap;

use eframe::egui::{self, Align2, Color32, Margin, Pos2, Rect, RichText, Sense, Vec2};
use vox_proto::{ChannelCategory, ChannelId, ChannelKind, User, VoiceMember};

use super::{channel_label, App, CameraUi, Share, ICON_PRIVATE};
use crate::context_menu::{Lead, Origin};
use crate::layout::Open;
use crate::theme::{self, ph, radius};
use crate::ui_kit::{self, IconState, Kind, Size};

/// The bottom panels' background (a shade under the sidebar).
const PANEL_BG: Color32 = Color32::from_rgb(0x17, 0x22, 0x2D);
/// A person in a voice room.
const MEMBER_H: f32 = 28.0;
const MEMBER_INDENT: f32 = 30.0;
const MEMBER_AVATAR: f32 = 22.0;
/// A row of the member list.
const ONLINE_H: f32 = 40.0;
const ONLINE_AVATAR: f32 = 30.0;

/// The thickness of the insertion marker of a drag & drop.
const DROP_MARK: f32 = 2.0;

/// v1.2: what the sidebar is dragging. Only channels and categories are grabbable, and only
/// for those who may reorder them (`Role::can_manage_channels`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidebarDrag {
    /// A channel: reordered inside its kind, or moved into another group (category or loose).
    Channel(ChannelId),
    /// A whole category, moved among the other categories.
    Category(ChannelId),
}

/// v1.2: where the dragged row would land: the row under the pointer, which half of it, and
/// its rect (the marker is painted on it).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SidebarDrop {
    /// The hovered row (a channel while a channel is dragged, a category while a category is).
    pub at: ChannelId,
    /// `true`: past the row's middle (insert after it).
    pub after: bool,
    /// The hovered row's rect, so the marker knows where to sit.
    pub rect: Rect,
}

/// v1.2: a channel that moved: the whole new order of every channel (what
/// [`vox_proto::ChannelReorderRequest`] takes) and the category it now belongs to — only
/// different from before when it crossed from one group to another.
struct ChannelMove {
    ids: Vec<ChannelId>,
    category: Option<ChannelId>,
}

/// v1.2: the order the sidebar is drawn in — the single place that decides it, so a drag &
/// drop rebuilds exactly what the screen shows and sends that back. Text always comes before
/// voice rooms, loose channels before categories (like Discord).
struct SidebarPlan {
    /// Channels in no category, in the order drawn.
    loose: Vec<vox_proto::Channel>,
    /// Categories in the order drawn, each with its channels (text, then voice).
    categories: Vec<(ChannelCategory, Vec<vox_proto::Channel>, Vec<vox_proto::Channel>)>,
}

impl SidebarPlan {
    /// Every channel, in the order drawn: the flat truth behind `Channel::position`.
    fn flat(&self) -> Vec<vox_proto::Channel> {
        let mut all = self.loose.clone();
        for (_, text, voice) in &self.categories {
            all.extend(text.iter().cloned());
            all.extend(voice.iter().cloned());
        }
        all
    }
}

/// v1.2: `order` with `id` moved into `at`'s slot — the before/after arithmetic, once.
/// `None`: over itself, or one of the two is not in the list.
///
/// This is where the off-by-one lives: taking `id` out shifts every later slot left, so `at`
/// moves one step closer when it came after `id`; and "after" means one slot further still.
fn moved_slot(order: &[ChannelId], id: ChannelId, at: ChannelId, after: bool) -> Option<Vec<ChannelId>> {
    let from = order.iter().position(|x| *x == id)?;
    let to = order.iter().position(|x| *x == at)?;
    if from == to {
        return None;
    }
    let mut out = order.to_vec();
    out.remove(from);
    let mut idx = to;
    if from < to {
        idx -= 1;
    }
    if after {
        idx += 1;
    }
    out.insert(idx.clamp(0, out.len()), id);
    Some(out)
}

/// v1.2: `flat` with channel `id` moved to `at` — the whole new order of every channel (what
/// [`vox_proto::ChannelReorderRequest`] takes) and the category the moved channel now belongs
/// to (a different one when it crossed into another group).
///
/// `None` when the drop makes no sense: over itself, over the other kind (a text channel never
/// trades places with a voice room — the sidebar draws them in separate blocks), or over a
/// row that is gone. The move happens inside the same kind's list; the other kind keeps its
/// slots, so its rows never shift under the pointer.
fn moved_order(flat: &[vox_proto::Channel], id: ChannelId, at: ChannelId, after: bool) -> Option<(Vec<ChannelId>, Option<ChannelId>)> {
    let dragged = flat.iter().find(|c| c.id == id)?;
    let target = flat.iter().find(|c| c.id == at)?;
    if dragged.kind != target.kind {
        return None;
    }
    let sub: Vec<ChannelId> = flat.iter().filter(|c| c.kind == dragged.kind).map(|c| c.id).collect();
    let moved = moved_slot(&sub, id, at, after)?;
    // Rewrite only the moved kind's slots; the other kind's rows stay exactly where they were.
    let mut rest = moved.into_iter();
    let ids = flat
        .iter()
        .map(|c| if c.kind == dragged.kind { rest.next().unwrap_or(c.id) } else { c.id })
        .collect();
    Some((ids, target.category_id))
}

/// v1.2: the categories in their new order (same arithmetic, one list).
fn moved_category_order(cats: &[ChannelCategory], id: ChannelId, at: ChannelId, after: bool) -> Option<Vec<ChannelId>> {
    let order: Vec<ChannelId> = cats.iter().map(|c| c.id).collect();
    moved_slot(&order, id, at, after)
}

/// Paint a small icon at the right edge `x` (moving left), returning its rect.
fn icon_at(painter: &egui::Painter, x: &mut f32, cy: f32, icon: &str, color: Color32) -> Rect {
    let g = painter.layout_no_wrap(icon.to_owned(), theme::body(14.0), color);
    let r = Rect::from_min_size(Pos2::new(*x - g.size().x, cy - g.size().y / 2.0), g.size());
    painter.galley(r.min, g, color);
    *x = r.left() - 5.0;
    r
}

/// Lay out a one-line label (the category header's galley).
fn header_galley(ui: &egui::Ui, text: &str, font: egui::FontId, color: Color32) -> std::sync::Arc<egui::Galley> {
    ui.painter().layout_no_wrap(text.to_owned(), font, color)
}

impl App {
    pub(super) fn left_panel(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("server_head").resizable(false)
            .frame(egui::Frame::new().inner_margin(Margin { left: 8, right: 8, top: 8, bottom: 8 }))
            .show(ui, |ui| self.server_header(ui));
        egui::Panel::bottom("me_panel").resizable(false).frame(egui::Frame::new().fill(PANEL_BG).inner_margin(Margin::symmetric(8, 8))).show(ui, |ui| self.me_panel(ui));
        if self.voice.channel.is_some() || self.voice.joining.is_some() || self.voice.error.is_some() {
            egui::Panel::bottom("voice_panel").resizable(false)
                .frame(egui::Frame::new().fill(PANEL_BG).inner_margin(Margin { left: 12, right: 8, top: 10, bottom: 8 }))
                .show(ui, |ui| self.voice_panel(ui));
        }
        egui::CentralPanel::no_frame().show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                let ui = &mut ui.new_child(egui::UiBuilder::new().max_rect(ui.max_rect().shrink2(Vec2::new(8.0, 0.0))));
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.add_space(8.0);
                self.channel_lists(ui);
                ui.add_space(12.0);
            });
        });
    }

    /// A section header of the list, in sentence case.
    fn list_section(ui: &mut egui::Ui, text: &str) {
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            ui_kit::section(ui, text);
        });
        ui.add_space(2.0);
    }

    fn channel_lists(&mut self, ui: &mut egui::Ui) {
        self.sidebar_rows.clear();
        let open = self.open_in_center();
        self.dm_sidebar(ui);
        // v1.2: only those who may reorder get a grabbable row; a member cannot reorder anyway.
        let draggable = self.my_role().can_manage_channels();
        // The marker belongs to this frame: every row rewrites it while the pointer is over it.
        self.sidebar_drop = None;
        let plan = self.sidebar_plan();
        for c in &plan.loose {
            if c.kind == ChannelKind::Text {
                self.category_channel_row(ui, c, open, draggable);
            } else {
                self.voice_channel_row(ui, c, open, draggable);
            }
        }
        for (cat, text_channels, voice_channels) in &plan.categories {
            // An empty category is only drawn for those who can manage channels (a member has
            // nothing to do in it — Discord hides it too).
            if text_channels.is_empty() && voice_channels.is_empty() && !draggable {
                continue;
            }
            self.category_section(ui, cat, text_channels, voice_channels, open, draggable);
        }
        // The ghost under the pointer and the insertion marker are painted last, over the rows.
        self.sidebar_drag_paint(ui);
        // Released outside any row (or outside the list): the drag ends without moving anything.
        if self.sidebar_drag.is_some() && !ui.input(|i| i.pointer.primary_down()) {
            self.sidebar_drop_apply();
            self.sidebar_drag = None;
            self.sidebar_drop = None;
        }
    }

    /// v1.2: the order the sidebar is drawn in (see [`SidebarPlan`]). One place decides it, so
    /// the drag & drop and the screen can never disagree.
    fn sidebar_plan(&self) -> SidebarPlan {
        let mut by_category: HashMap<Option<ChannelId>, Vec<vox_proto::Channel>> = HashMap::new();
        for c in &self.channels {
            by_category.entry(c.category_id).or_default().push(c.clone());
        }
        // Loose channels (no category): text first, then voice rooms — always drawn.
        let mut loose: Vec<vox_proto::Channel> = by_category.remove(&None).unwrap_or_default();
        loose.sort_by_key(|c| (c.kind == ChannelKind::Voice, c.position, c.id));
        let mut cats: Vec<&ChannelCategory> = self.categories.iter().collect();
        cats.sort_by_key(|c| (c.position, c.id));
        let categories = cats
            .into_iter()
            .map(|cat| {
                let mut of: Vec<vox_proto::Channel> = by_category.remove(&Some(cat.id)).unwrap_or_default();
                of.sort_by_key(|c| (c.kind == ChannelKind::Voice, c.position, c.id));
                let voice: Vec<vox_proto::Channel> = of.iter().filter(|c| c.kind == ChannelKind::Voice).cloned().collect();
                let text: Vec<vox_proto::Channel> = of.iter().filter(|c| c.kind == ChannelKind::Text).cloned().collect();
                (cat.clone(), text, voice)
            })
            .collect();
        SidebarPlan { loose, categories }
    }

    /// v1.1 (Discord-style): one collapsible category — a thin header in CAPS with the caret on
    /// the left and the category's colour on the text; its channels (text, then voice) when open.
    /// v1.2: the header is also the category's drag handle.
    fn category_section(
        &mut self,
        ui: &mut egui::Ui,
        cat: &vox_proto::ChannelCategory,
        text_channels: &[vox_proto::Channel],
        voice_channels: &[vox_proto::Channel],
        open: Open,
        draggable: bool,
    ) {
        let is_open = self.category_open.get(&cat.id).copied().unwrap_or(true);
        let caret = if is_open { ph::CARET_DOWN } else { ph::CARET_RIGHT };

        // The header: caret + NAME IN CAPS, in the category's colour. Click toggles open/closed.
        let sense = if draggable { Sense::click_and_drag() } else { Sense::click() };
        let (header_rect, header_resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 22.0), sense);
        if header_resp.hovered() {
            ui.painter().rect_filled(header_rect, radius::ROW, theme::ROW_HOVER);
        }
        if header_resp.clicked() {
            self.category_open
                .entry(cat.id)
                .and_modify(|e| *e = !*e)
                .or_insert(false);
        }
        let color = Color32::from_hex(cat.color.get(1..).unwrap_or("5865F2")).unwrap_or(theme::NEVOA);
        let label = header_galley(ui, &format!("{caret} {}", cat.name.to_uppercase()), theme::semibold(11.5), color);
        ui.painter().galley(
            Pos2::new(header_rect.left() + 8.0, header_rect.center().y - label.size().y / 2.0),
            label,
            color,
        );
        if draggable {
            self.sidebar_category_dnd(ui, &header_resp, cat.id);
        }

        if is_open {
            ui.add_space(2.0);
            // Text channels first, then the voice rooms (Discord's order inside a category).
            for c in text_channels {
                self.category_channel_row(ui, c, open, draggable);
            }
            for c in voice_channels {
                self.voice_channel_row(ui, c, open, draggable);
            }
            ui.add_space(4.0);
        }
    }

    /// v1.1: render a single text channel row inside a category (reuses the row logic).
    /// v1.2: grabbable when the viewer may reorder.
    fn category_channel_row(&mut self, ui: &mut egui::Ui, c: &vox_proto::Channel, open: Open, draggable: bool) {
        let unread = self.unread.get(&c.id).copied().unwrap_or(0);
        let mentions = self.chatx.mention_unread.get(&c.id).copied().unwrap_or(0);
        let hover = c.private.is_some().then_some(super::PRIVATE_HOVER);
        let label = channel_label(&c, "");
        let icon = if c.private.is_some() { ICON_PRIVATE } else { ph::HASH };
        let row = ui_kit::Row {
            icon,
            name: &c.name,
            label: &label,
            selected: open == Open::Text(c.id),
            unread: unread > 0,
            count: unread,
            mentions,
            hover,
        };
        let resp = if draggable { ui_kit::sidebar_row_drag(ui, row) } else { ui_kit::sidebar_row(ui, row) };
        if resp.0.clicked() {
            self.select_text(c.id);
        }
        if draggable {
            self.sidebar_channel_dnd(ui, &resp.0, c.id);
        }
    }

    /// v1.1: render a single voice channel row inside a category.
    /// v1.2: grabbable when the viewer may reorder.
    fn voice_channel_row(&mut self, ui: &mut egui::Ui, c: &vox_proto::Channel, open: Open, draggable: bool) {
        let here = self.voice.channel == Some(c.id);
        let joining = self.voice.joining == Some(c.id);
        let suffix = if joining { "  (entrando...)" } else { "" };
        let hover = match (here, c.private.is_some()) {
            (true, _) => "Clique para abrir a sala",
            (false, true) => "Canal privado: só quem tem acesso vê e entra. Clique para entrar",
            (false, false) => "Clique para entrar",
        };
        let label = channel_label(&c, suffix);
        let name = if c.private.is_some() || !suffix.is_empty() {
            label.split_once(' ').map_or(label.as_str(), |(_, n)| n)
        } else {
            c.name.as_str()
        };
        let row = ui_kit::Row {
            icon: ph::SPEAKER_HIGH,
            name,
            label: &label,
            selected: open == Open::Stage(c.id),
            unread: false,
            count: 0,
            mentions: 0,
            hover: Some(hover),
        };
        let (resp, _drawn) = if draggable { ui_kit::sidebar_row_drag(ui, row) } else { ui_kit::sidebar_row(ui, row) };
        if resp.clicked() {
            self.join_voice(c.id);
        }
        if draggable {
            self.sidebar_channel_dnd(ui, &resp, c.id);
        }
        let members: Vec<VoiceMember> = self.voice_members.values().filter(|m| m.channel_id == Some(c.id)).cloned().collect();
        for m in members {
            self.member_row(ui, &m, here);
        }
        if !self.voice_members.values().any(|m| m.channel_id == Some(c.id)) {
            ui.add_space(2.0);
        }
    }

    // ------------------------------------------------------------ reorder by drag & drop (v1.2)

    /// v1.2: one channel row's part of the drag: start it, and mark where it would land while a
    /// channel is dragged. A row never accepts a category, and never a channel of the other
    /// kind — the sidebar always keeps text and voice apart.
    fn sidebar_channel_dnd(&mut self, ui: &egui::Ui, resp: &egui::Response, id: ChannelId) {
        if resp.drag_started() {
            self.sidebar_drag = Some(SidebarDrag::Channel(id));
        }
        if self.sidebar_drag == Some(SidebarDrag::Channel(id)) {
            return; // over itself: no marker, no move
        }
        self.sidebar_mark_drop(ui, SidebarDrag::Channel(id), resp.rect);
    }

    /// v1.2: a category header's part of the drag: start it, and mark where it would land
    /// (only while a category is dragged, and only over another header).
    fn sidebar_category_dnd(&mut self, ui: &egui::Ui, resp: &egui::Response, id: ChannelId) {
        if resp.drag_started() {
            self.sidebar_drag = Some(SidebarDrag::Category(id));
        }
        if self.sidebar_drag == Some(SidebarDrag::Category(id)) {
            return;
        }
        self.sidebar_mark_drop(ui, SidebarDrag::Category(id), resp.rect);
    }

    /// v1.2: the pointer is over `rect`, a row of the same kind as the drag: that is where it
    /// would land. The half of the row decides "before" or "after". The hit test is by pointer
    /// position rather than `hovered()`, because a widget that owns the drag reports itself
    /// hovered above everything else.
    fn sidebar_mark_drop(&mut self, ui: &egui::Ui, kind: SidebarDrag, rect: Rect) {
        let Some(dragged) = self.sidebar_drag else { return };
        // Only a row of the same kind accepts the drop (a category header takes only
        // categories; a channel row takes only channels).
        let same_kind = matches!(
            (dragged, kind),
            (SidebarDrag::Channel(_), SidebarDrag::Channel(_)) | (SidebarDrag::Category(_), SidebarDrag::Category(_))
        );
        if !same_kind {
            return;
        }
        let Some(pos) = ui.input(|i| i.pointer.hover_pos()) else { return };
        if !rect.contains(pos) {
            return;
        }
        let at = match kind {
            SidebarDrag::Channel(id) | SidebarDrag::Category(id) => id,
        };
        self.sidebar_drop = Some(SidebarDrop { at, after: pos.y > rect.center().y, rect });
    }

    /// v1.2: the dragged row's ghost under the pointer, plus the insertion marker where it
    /// would land. Painted last, over the rows (the marker must never hide under one).
    fn sidebar_drag_paint(&self, ui: &egui::Ui) {
        let Some(drag) = self.sidebar_drag else { return };
        let painter = ui.ctx().debug_painter();
        if let Some(drop) = self.sidebar_drop {
            // A 2 px line on the hovered row's near edge, full width of the sidebar.
            let y = if drop.after { drop.rect.bottom() } else { drop.rect.top() };
            let rect = Rect::from_min_size(Pos2::new(drop.rect.left(), y - DROP_MARK / 2.0), Vec2::new(drop.rect.width(), DROP_MARK));
            painter.rect_filled(rect, DROP_MARK / 2.0, theme::ABAJUR);
        }
        let Some(pos) = ui.input(|i| i.pointer.hover_pos()) else { return };
        let ghost = match drag {
            SidebarDrag::Channel(id) => self.channels.iter().find(|c| c.id == id).map(|c| c.name.clone()),
            SidebarDrag::Category(id) => self.categories.iter().find(|c| c.id == id).map(|c| c.name.clone()),
        };
        let Some(name) = ghost else { return };
        let text = painter.layout_no_wrap(name, theme::semibold(13.0), theme::GIZ);
        let pad = Vec2::new(10.0, 5.0);
        let rect = Rect::from_min_size(pos + Vec2::new(6.0, 4.0), text.size() + pad * 2.0);
        painter.rect_filled(rect, radius::ROW, theme::MOVEL);
        painter.galley(rect.min + pad, text, theme::GIZ);
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        // The ghost follows the pointer: it must keep up even when nothing else moves.
        ui.ctx().request_repaint();
    }

    /// v1.2: the drag was released — send the new order. A channel that changed groups needs
    /// its new category first (the hub broadcasts `ChannelsChanged` and the row moves), then
    /// the whole order; a category only needs its order.
    fn sidebar_drop_apply(&mut self) {
        let (Some(drag), Some(drop)) = (self.sidebar_drag, self.sidebar_drop) else { return };
        match drag {
            SidebarDrag::Channel(id) => {
                let Some(moved) = self.sidebar_channel_move(id, drop.at, drop.after) else { return };
                if self.channel_of(id).and_then(|c| c.category_id) != moved.category {
                    self.admin(crate::roles::AdminCmd::SetChannelCategory { id, category: moved.category });
                }
                self.admin(crate::roles::AdminCmd::ReorderChannels { ids: moved.ids });
            }
            SidebarDrag::Category(id) => {
                if let Some(ids) = self.sidebar_category_move(id, drop.at, drop.after) {
                    self.admin(crate::roles::AdminCmd::ReorderCategories { ids });
                }
            }
        }
    }

    /// v1.2: move channel `id` to `at` in the drawn order and hand back the whole new order.
    /// `None`: nothing to do — over itself, over the other kind (a text channel never trades
    /// places with a voice room), or over a row that is gone.
    fn sidebar_channel_move(&self, id: ChannelId, at: ChannelId, after: bool) -> Option<ChannelMove> {
        let (ids, category) = moved_order(&self.sidebar_plan().flat(), id, at, after)?;
        Some(ChannelMove { ids, category })
    }

    /// v1.2: move category `id` to `at` among the other categories (`None`: over itself).
    fn sidebar_category_move(&self, id: ChannelId, at: ChannelId, after: bool) -> Option<Vec<ChannelId>> {
        let mut cats: Vec<&ChannelCategory> = self.categories.iter().collect();
        cats.sort_by_key(|c| (c.position, c.id));
        moved_category_order(&cats.iter().map(|c| (*c).clone()).collect::<Vec<_>>(), id, at, after)
    }

    /// A channel we know of (`None`: it was deleted while the menu was open).
    fn channel_of(&self, id: ChannelId) -> Option<&vox_proto::Channel> {
        self.channels.iter().find(|c| c.id == id)
    }

    // ------------------------------------------------------------ server header and menu

    fn server_header(&mut self, ui: &mut egui::Ui) {
        let in_voice = self.voice_members.values().filter(|m| m.channel_id.is_some()).count();
        let mut sub = format!("{} online · {in_voice} na voz", self.online.len());
        let no_owner = self.roles.supported && self.roles.account.is_some_and(|a| a.no_owner);
        if no_owner {
            sub.push_str(" · sem dono");
        }
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 50.0), Sense::click());
        if ui.is_rect_visible(rect) {
            let p = ui.painter();
            if resp.hovered() {
                p.rect_filled(rect, radius::ROW, theme::ROW_HOVER);
            }
            let x = rect.left() + 8.0;
            let caret = p.layout_no_wrap(ph::CARET_DOWN.to_owned(), theme::body(15.0), theme::NEVOA);
            let name = crate::app::notify::fit_name(p, &self.server_name, theme::display(theme::size::HEADING), theme::GIZ, rect.width() - 24.0 - caret.size().x);
            let name_h = name.size().y;
            p.galley(Pos2::new(x, rect.top() + 6.0), name, theme::GIZ);
            p.galley(Pos2::new(rect.right() - 8.0 - caret.size().x, rect.top() + 6.0 + (name_h - caret.size().y) / 2.0), caret, theme::NEVOA);
            p.text(Pos2::new(x, rect.bottom() - 7.0), Align2::LEFT_BOTTOM, &sub, theme::body(12.0), if no_owner { theme::AVISO } else { theme::NEVOA });
        }
        let resp = resp.on_hover_text("Menu do servidor");
        egui::Popup::menu(&resp).frame(ui_kit::popup_frame(ui.style())).show(|ui| self.server_header_menu(ui));
    }

    /// Everything that is about the server (and leaving it): the old header buttons live here.
    fn server_header_menu(&mut self, ui: &mut egui::Ui) {
        ui.set_min_width(230.0);
        let admin = self.roles.supported && self.my_role().can_admin();
        let mut any = false;
        if admin {
            any = true;
            if ui_kit::menu_row(ui, ph::WRENCH, "Configurações do servidor", false).on_hover_text("Membros, cargos, convites e o registro de moderação").clicked() {
                self.open_server_settings();
                ui.close();
            }
        }
        if self.host.runner.is_some() {
            any = true;
            if ui_kit::menu_row(ui, ph::HARD_DRIVES, "Meu servidor", false).on_hover_text("Status, portas e convites do servidor que você hospeda").clicked() {
                self.host.panel_open = true;
                ui.close();
            }
        } else if admin {
            any = true;
            if ui_kit::menu_row(ui, ph::ENVELOPE_SIMPLE, "Convidar", false).on_hover_text("Criar um código de convite para este servidor").clicked() {
                self.open_server_settings();
                self.roles.tab = crate::roles::ServerTab::Invites;
                ui.close();
            }
        }
        if self.roles.supported && self.roles.account.is_some_and(|a| a.no_owner) {
            any = true;
            if ui_kit::menu_row(ui, ph::CROWN, "Reivindicar servidor (sem dono)", false).on_hover_text("Este servidor ainda não tem dono. Clique para reivindicar.").clicked() {
                self.roles.claim_open = true;
                self.roles.claim_msg = None;
                ui.close();
            }
        }
        if any {
            ui_kit::menu_separator(ui);
        }
        if ui_kit::menu_row(ui, ph::GEAR_SIX, "Configurações", false).clicked() {
            self.open_settings();
            ui.close();
        }
        let hover = if self.host.hosting() {
            "Sair deste servidor e voltar para a lista de servidores. Seu servidor continua ligado."
        } else {
            "Sair deste servidor e voltar para a lista de servidores"
        };
        if ui_kit::menu_row(ui, ph::ARROWS_LEFT_RIGHT, "Trocar de servidor", false).on_hover_text(hover).clicked() {
            ui.close();
            self.switch_server();
        }
    }

    // ------------------------------------------------------------ voice rooms

    /// One person in a voice room: the picture (with the lamp ring while they speak), the name,
    /// then on the right "ao vivo" (a click watches / stops), the camera, deafened, muted, my
    /// local volume / mute of them, and who watches their live.
    fn member_row(&mut self, ui: &mut egui::Ui, m: &VoiceMember, same_channel: bool) {
        let me = self.my_id() == Some(m.user.id);
        // My own ring follows what is actually sent (gate open), not the room's report.
        let speaking = if me { self.sending_voice() } else { self.voice.speaking.contains(&m.user.id.to_string()) } && !m.muted && !m.server_muted;
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), MEMBER_H), Sense::click());
        if !ui.is_rect_visible(rect) {
            return;
        }
        let key = m.user.id.to_string();
        let audio_only = m.streaming && self.stream_is_audio_only(m);
        let watching = m.streaming && !me && self.is_watching(&key);
        let ctx = ui.ctx().clone();
        let painter = ui.painter_at(rect);
        if resp.hovered() {
            painter.rect_filled(rect, radius::ROW, theme::ROW_HOVER);
        }
        let cy = rect.center().y;
        let center = Pos2::new(rect.left() + MEMBER_INDENT + MEMBER_AVATAR / 2.0, cy);
        crate::avatars::draw_avatar(&mut self.avatars.store, &painter, &ctx, center, MEMBER_AVATAR / 2.0, &m.user, crate::avatars::Surface::Sidebar);
        if speaking {
            ui_kit::paint_speaking_ring(&painter, center, MEMBER_AVATAR / 2.0, if resp.hovered() { theme::ROW_HOVER } else { theme::PAREDE });
        }
        // Right to left: the live tag, then the small icons.
        let mut x = rect.right() - 6.0;
        let mut live_rect = None;
        if m.streaming {
            let s = ui_kit::live_tag_size(&painter);
            let r = Rect::from_min_size(Pos2::new(x - s.x, cy - s.y / 2.0), s);
            ui_kit::paint_live_tag(&painter, r);
            live_rect = Some(r);
            x = r.left() - 5.0;
            if audio_only {
                icon_at(&painter, &mut x, cy, super::AUDIO_ONLY_ICON, theme::NEVOA);
            }
        }
        let mut viewers_rect = None;
        if m.streaming && self.viewers_supported {
            let n = self.viewers_of(m.user.id).len();
            let g = painter.layout_no_wrap(format!("{} {n}", ph::EYE), theme::body(12.0), theme::NEVOA);
            let r = Rect::from_min_size(Pos2::new(x - g.size().x, cy - g.size().y / 2.0), g.size());
            painter.galley(r.min, g, theme::NEVOA);
            viewers_rect = Some(r);
            x = r.left() - 6.0;
        }
        if self.camera_flag_of(m) {
            icon_at(&painter, &mut x, cy, ph::VIDEO_CAMERA, theme::NEVOA);
        }
        if m.deafened {
            icon_at(&painter, &mut x, cy, super::ICON_DEAF, theme::AO_VIVO);
        }
        if m.server_muted {
            icon_at(&painter, &mut x, cy, super::ICON_MUTED, theme::AO_VIVO);
        } else if m.muted {
            icon_at(&painter, &mut x, cy, super::ICON_MUTED, theme::NEVOA);
        }
        let mut local_rect = None;
        if !me && same_channel {
            let pref = self.settings.user_audio(&m.user.username);
            if pref.muted {
                local_rect = Some((icon_at(&painter, &mut x, cy, super::ICON_SPEAKER_OFF, theme::GIZ), "Silenciado só para você".to_string()));
            } else if pref.volume_pct != 100 {
                local_rect = Some((icon_at(&painter, &mut x, cy, super::ICON_VOL, theme::GIZ), format!("Volume ajustado só para você: {}%", pref.volume_pct)));
            }
        }
        let role = if self.roles.supported { m.user.role } else { Default::default() };
        let role_rect = match (crate::roles::role_icon(role), crate::roles::role_color(role)) {
            (Some(icon), Some(color)) => Some(icon_at(&painter, &mut x, cy, icon, color)),
            _ => None,
        };
        let name_left = center.x + MEMBER_AVATAR / 2.0 + 8.0;
        let color = match crate::roles::role_color(role) {
            Some(c) => c,
            None if speaking => theme::GIZ,
            None => theme::NEVOA,
        };
        let name = crate::app::notify::fit_name(&painter, m.user.shown_name(), theme::body(13.5), color, x - name_left - 2.0);
        painter.galley(Pos2::new(name_left, cy - name.size().y / 2.0), name, color);

        // The parts with their own hover / click, on top of the row's.
        if let Some(r) = local_rect.as_ref() {
            ui.interact(r.0, ui.id().with(("local_audio", m.user.id)), Sense::hover()).on_hover_text(&r.1);
        }
        if let Some(r) = role_rect {
            ui.interact(r, ui.id().with(("role", m.user.id)), Sense::hover()).on_hover_text(crate::roles::role_label(role));
        }
        if let Some(r) = viewers_rect {
            ui.interact(r, ui.id().with(("viewers", m.user.id)), Sense::hover()).on_hover_ui(|ui| {
                let names = self.viewers_of(m.user.id);
                ui.label(if names.is_empty() { App::viewers_text(&names) } else { names.join("\n") });
            });
        }
        if let Some(r) = live_rect {
            let live = ui.interact(r.expand(2.0), ui.id().with(("live", m.user.id)), Sense::click());
            let hover = match (me, watching, audio_only, same_channel) {
                (true, ..) => "Você está ao vivo",
                (false, true, true, _) => "Parar de ouvir",
                (false, true, false, _) => "Parar de assistir",
                (false, false, true, true) => "Ouvir",
                (false, false, false, true) => "Assistir",
                (false, false, true, false) => "Entre neste canal de voz para ouvir",
                (false, false, false, false) => "Entre neste canal de voz para assistir",
            };
            let live = live.on_hover_text(hover);
            if !me && live.clicked() {
                if watching {
                    self.unwatch(&key);
                } else if same_channel {
                    self.watch_focused(&key);
                }
            }
        }
        let resp = self.profile_hit(ui, resp, &m.user);
        self.menu_hit(&resp, &m.user, Origin::Sidebar, Lead::None);
    }

    // ------------------------------------------------------------ the voice panel

    fn voice_panel(&mut self, ui: &mut egui::Ui) {
        let room = |app: &App, id: Option<vox_proto::ChannelId>| -> String {
            if id == Some(super::dms::DM_CALL_CHANNEL) {
                return app.dm_call_title();
            }
            id.and_then(|i| app.channels.iter().find(|c| c.id == i)).map(|c| c.name.clone()).unwrap_or_default()
        };
        if let Some(err) = self.voice.error.clone() {
            ui.label(RichText::new(err).font(theme::body(12.5)).color(theme::AO_VIVO));
            ui.horizontal(|ui| {
                if let Some(ch) = self.voice.retry {
                    if ui_kit::button_sized(ui, Kind::Secondary, Size::Small, None, "Tentar de novo").clicked() {
                        self.join_voice(ch);
                    }
                }
                if ui_kit::button_sized(ui, Kind::Ghost, Size::Small, None, "Fechar").clicked() {
                    self.voice.error = None;
                    self.voice.retry = None;
                }
            });
            return;
        }
        if let Some(j) = self.voice.joining {
            let secs = self.voice.joining_since.map(|t| t.elapsed().as_secs()).unwrap_or(0);
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    ui.label(RichText::new("Conectando à voz...").font(theme::semibold(13.0)).color(theme::AVISO));
                    ui.label(RichText::new(format!("{} · {secs} s", room(self, Some(j)))).font(theme::body(12.0)).color(theme::NEVOA));
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui_kit::icon_button(ui, ph::X, "Cancelar", IconState::Normal).clicked() {
                        self.leave_voice();
                    }
                });
            });
            return;
        }
        let (state, color) = match self.voice.link {
            super::Link::Up => ("Voz conectada", theme::ONLINE),
            super::Link::Reconnecting => ("Voz: reconectando...", theme::AVISO),
        };
        let name = room(self, self.voice.channel);
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                ui.label(RichText::new(state).font(theme::semibold(13.0)).color(color));
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 5.0;
                    // O sinal da conexão, como no Discord: as barras caem conforme o ping
                    // piora; clicar abre os números.
                    if self.conn_signal(ui) {
                        self.voice.conn_open = true;
                        self.voice.conn_ping_at = None;
                    }
                    let r = ui.add(egui::Label::new(RichText::new(&name).font(theme::body(12.0)).color(theme::NEVOA)).truncate().sense(Sense::click()));
                    if r.on_hover_text("Abrir a sala").on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        self.show_call();
                    }
                });
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let (hover, dm) = if self.in_dm_call() { ("Desligar", true) } else { ("Sair da voz", false) };
                if ui_kit::icon_button(ui, ph::PHONE_X, hover, IconState::Off).clicked() {
                    if dm {
                        self.hang_up_dm_call();
                    } else {
                        self.leave_voice();
                    }
                }
            });
        });
        if let Some(w) = self.voice.warning.clone() {
            ui.label(RichText::new(format!("{}  {w}", super::ICON_WARN)).font(theme::body(12.0)).color(theme::AVISO));
        }
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let (icon, hover, state) = match &self.voice.camera {
                CameraUi::Off => (ph::VIDEO_CAMERA_SLASH, "Ligar a câmera".to_string(), IconState::Normal),
                CameraUi::Starting => (ph::VIDEO_CAMERA, "Ligando a câmera...".to_string(), IconState::Normal),
                CameraUi::On { info, .. } => (ph::VIDEO_CAMERA, format!("Câmera ligada ({info}). Clique para desligar"), IconState::On),
            };
            let cam_soon = self.voice.camera == CameraUi::Off && crate::platform::camera_soon();
            let hover = if cam_soon { format!("Câmera: {}", crate::platform::SOON_MAC.to_lowercase()) } else { hover };
            let cam = ui.add_enabled_ui(self.voice.camera != CameraUi::Starting && !cam_soon, |ui| ui_kit::icon_button(ui, icon, &hover, state)).inner;
            if cam.clicked() {
                self.toggle_camera();
            }
            match self.voice.share {
                Share::Off if crate::platform::screen_soon() => {
                    ui.add_enabled_ui(false, |ui| ui_kit::icon_button(ui, ph::SCREENCAST, &format!("Transmitir: {}", crate::platform::SOON_MAC.to_lowercase()), IconState::Normal));
                }
                Share::Off => {
                    if ui_kit::icon_button(ui, ph::SCREENCAST, "Transmitir (Go Live)", IconState::Normal).clicked() {
                        self.open_golive();
                    }
                }
                Share::Starting => {
                    ui.add_enabled_ui(false, |ui| ui_kit::icon_button(ui, ph::SCREENCAST, "Iniciando transmissão...", IconState::On));
                }
                Share::Live { .. } => {
                    if ui_kit::icon_button(ui, ph::SCREENCAST, "Ao vivo: clique para parar a transmissão", IconState::On).clicked() {
                        self.stop_share();
                    }
                    ui_kit::live_tag(ui);
                }
            }
        });
    }

    // ------------------------------------------------------------ the "você" panel

    fn me_panel(&mut self, ui: &mut egui::Ui) {
        let Some(me) = self.me.clone() else { return };
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            let pic = crate::avatars::avatar_widget(&mut self.avatars.store, ui, &me, 32.0, crate::avatars::Surface::Sidebar);
            if self.profile.supported == Some(true) {
                let presence = self.my_presence();
                ui.painter().circle(pic.rect.right_bottom() - Vec2::splat(3.0), 5.0, crate::profile::presence_color(presence), egui::Stroke::new(2.0, PANEL_BG));
            }
            let pic = self.profile_hit(ui, pic, &me);
            self.menu_hit(&pic, &me, Origin::Sidebar, Lead::None);
            // The icons take their room first (right to left), the name and status what is left.
            let icons_w = 3.0 * 30.0 + 2.0 * 2.0;
            let text_w = (ui.available_width() - icons_w - 8.0).max(30.0);
            ui.allocate_ui_with_layout(Vec2::new(text_w, 34.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.set_width(text_w);
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    let name = ui.add(egui::Label::new(RichText::new(me.shown_name()).font(theme::semibold(13.5)).color(theme::GIZ)).truncate().sense(Sense::click()));
                    let name = self.profile_hit(ui, name, &me);
                    self.menu_hit(&name, &me, Origin::Sidebar, Lead::None);
                    if self.roles.supported {
                        crate::roles::role_badge(ui, me.role);
                    }
                });
                self.me_status(ui);
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                if ui_kit::icon_button(ui, ph::GEAR_SIX, "Configurações", IconState::Normal).clicked() {
                    self.open_settings();
                }
                self.me_voice_buttons(ui);
            });
        });
    }

    /// The status line under my name: the voice room I am in (green), else my presence (a click
    /// changes it).
    fn me_status(&mut self, ui: &mut egui::Ui) {
        let font = theme::body(12.0);
        if let Some(ch) = self.voice.channel {
            let text = if ch == super::dms::DM_CALL_CHANNEL {
                "em uma chamada".to_string()
            } else {
                format!("na {}", self.channels.iter().find(|c| c.id == ch).map(|c| c.name.as_str()).unwrap_or("voz"))
            };
            ui.add(egui::Label::new(RichText::new(text).font(font).color(theme::ONLINE)).truncate().selectable(false));
            return;
        }
        if self.profile.supported != Some(true) {
            ui.label(RichText::new("online").font(font).color(theme::ONLINE));
            return;
        }
        let current = self.my_presence();
        let r = ui.add(egui::Label::new(RichText::new(format!("{} {}", crate::profile::presence_label(current), ph::CARET_DOWN)).font(font).color(presence_text_color(current))).sense(Sense::click()));
        let r = r.on_hover_text(format!("{}. Clique para mudar.", crate::profile::presence_label(current)));
        let mut choose = None;
        egui::Popup::menu(&r).frame(ui_kit::popup_frame(ui.style())).style(ui_kit::menu_style).show(|ui| {
            ui.set_min_width(200.0);
            for p in vox_proto::Presence::ALL {
                // The presence's colour on its dot; the current one checked (as in the person menu).
                let icon = if current == p { ph::CHECK } else { ph::CIRCLE };
                let row = ui_kit::menu_row_ex(ui, icon, Some(crate::profile::presence_color(p)), crate::profile::presence_label(p), false, false);
                if row.on_hover_text(crate::profile::presence_hint(p)).clicked() {
                    choose = Some(p);
                    ui.close();
                }
            }
        });
        if let Some(p) = choose.filter(|p| *p != current) {
            self.set_presence(p);
        }
    }

    /// Microphone and deafen (right to left: deafen first). They act on the call, so without one
    /// they rest.
    fn me_voice_buttons(&mut self, ui: &mut egui::Ui) {
        let in_call = self.voice.channel.is_some();
        let deaf = self.voice.deafened;
        let r = ui
            .add_enabled_ui(in_call, |ui| ui_kit::icon_button(ui, super::ICON_DEAF, if deaf { "Ouvir de novo" } else { "Ensurdecer" }, if deaf { IconState::Off } else { IconState::Normal }))
            .inner
            .on_disabled_hover_text("Entre em uma sala de voz para usar");
        if r.clicked() {
            self.toggle_deafen();
        }
        let ptt = self.ptt_active();
        let server_muted = self.i_am_server_muted();
        let muted = self.voice.muted && !ptt;
        let (icon, hover) = if server_muted {
            (super::ICON_MUTED, "Silenciado pelo servidor".to_string())
        } else if self.voice.no_mic {
            (super::ICON_MUTED, "Sem microfone".to_string())
        } else if ptt {
            (ph::MICROPHONE, format!("Push-to-talk ({})", self.settings.ptt_name))
        } else if muted {
            (super::ICON_MUTED, "Microfone mutado: clique para falar".to_string())
        } else {
            (ph::MICROPHONE, "Mutar".to_string())
        };
        let state = if server_muted || muted || self.voice.no_mic { IconState::Off } else { IconState::Normal };
        let enabled = in_call && !server_muted && !ptt && !deaf && !self.voice.no_mic;
        let disabled_hover = if !in_call {
            "Entre em uma sala de voz para usar".to_string()
        } else if server_muted {
            "Um moderador silenciou você no servidor: só um moderador pode liberar".to_string()
        } else {
            hover.clone()
        };
        let r = ui.add_enabled_ui(enabled, |ui| ui_kit::icon_button(ui, icon, &hover, state)).inner.on_disabled_hover_text(disabled_hover);
        if r.clicked() {
            self.toggle_mute();
        }
    }

    // ------------------------------------------------------------ the member list (right of the chat)

    pub(super) fn online_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            ui_kit::section(ui, &format!("Online — {}", self.online.len()));
        });
        ui.add_space(4.0);
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            let mut users = self.online.clone();
            users.sort_by_key(|u| u.shown_name().to_lowercase());
            for u in users {
                self.online_row(ui, &u);
            }
        });
    }

    fn online_row(&mut self, ui: &mut egui::Ui, u: &User) {
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), ONLINE_H), Sense::click());
        if !ui.is_rect_visible(rect) {
            return;
        }
        let in_voice = self.voice_members.contains_key(&u.id);
        let with_me = self.voice.channel.is_some() && self.my_id() != Some(u.id) && self.voice_members.get(&u.id).is_some_and(|m| m.channel_id == self.voice.channel);
        let ctx = ui.ctx().clone();
        let painter = ui.painter_at(rect);
        let bg = if resp.hovered() { theme::ROW_HOVER } else { theme::PAREDE };
        if resp.hovered() {
            painter.rect_filled(rect, radius::ROW, bg);
        }
        let cy = rect.center().y;
        let center = Pos2::new(rect.left() + 8.0 + ONLINE_AVATAR / 2.0, cy);
        crate::avatars::draw_avatar(&mut self.avatars.store, &painter, &ctx, center, ONLINE_AVATAR / 2.0, u, crate::avatars::Surface::Online);
        // The presence dot on the picture's corner, as on Discord.
        let presence = self.presence_of(u.id);
        painter.circle(center + Vec2::splat(ONLINE_AVATAR / 2.0 * 0.72), 5.0, crate::profile::presence_color(presence), egui::Stroke::new(2.0, bg));
        let mut x = rect.right() - 8.0;
        let mut voice_rect = None;
        if in_voice {
            voice_rect = Some(icon_at(&painter, &mut x, cy, super::ICON_VOICE, theme::NEVOA));
        }
        let mut local = None;
        if with_me {
            let pref = self.settings.user_audio(&u.username);
            if pref.muted {
                local = Some((icon_at(&painter, &mut x, cy, super::ICON_SPEAKER_OFF, theme::GIZ), "Silenciado só para você".to_string()));
            } else if pref.volume_pct != 100 {
                local = Some((icon_at(&painter, &mut x, cy, super::ICON_VOL, theme::GIZ), format!("Volume ajustado só para você: {}%", pref.volume_pct)));
            }
        }
        let status = self.status_of(u.id);
        let mut status_rect = None;
        if let Some(st) = &status {
            let e = if st.emoji.is_empty() { ph::CHAT_CENTERED_TEXT } else { st.emoji.as_str() };
            status_rect = Some(icon_at(&painter, &mut x, cy, e, theme::GIZ));
        }
        let role = if self.roles.supported { u.role } else { Default::default() };
        let role_rect = match (crate::roles::role_icon(role), crate::roles::role_color(role)) {
            (Some(icon), Some(color)) => Some(icon_at(&painter, &mut x, cy, icon, color)),
            _ => None,
        };
        let name_left = center.x + ONLINE_AVATAR / 2.0 + 10.0;
        let color = crate::roles::role_color(role).unwrap_or(if resp.hovered() { theme::GIZ } else { theme::TEXTO });
        let name = crate::app::notify::fit_name(&painter, u.shown_name(), theme::body(14.0), color, x - name_left - 2.0);
        painter.galley(Pos2::new(name_left, cy - name.size().y / 2.0), name, color);
        if let Some(r) = voice_rect {
            ui.interact(r, ui.id().with(("in_voice", u.id)), Sense::hover()).on_hover_text("Em um canal de voz");
        }
        if let Some((r, t)) = local {
            ui.interact(r, ui.id().with(("local", u.id)), Sense::hover()).on_hover_text(t);
        }
        if let (Some(r), Some(st)) = (status_rect, status) {
            ui.interact(r, ui.id().with(("status", u.id)), Sense::hover()).on_hover_text(crate::profile::status_line(&st));
        }
        if let Some(r) = role_rect {
            ui.interact(r, ui.id().with(("role", u.id)), Sense::hover()).on_hover_text(crate::roles::role_label(role));
        }
        let resp = self.profile_hit(ui, resp, u);
        self.menu_hit(&resp, u, Origin::Online, Lead::None);
    }
}

/// The colour of the presence line under my name: the dot's colour, but readable (the switched-off
/// grey of "Invisível" is for dots, too dark for text).
fn presence_text_color(p: vox_proto::Presence) -> Color32 {
    let c = crate::profile::presence_color(p);
    if c == crate::profile::OFFLINE_COLOR {
        theme::NEVOA
    } else {
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The drawn order a test would build: two loose text channels, then a category holding two
    /// of them, and finally the voice rooms (loose, then the one inside the category) — voice
    /// always after text, loose always before the categories.
    fn flat() -> Vec<vox_proto::Channel> {
        let cat = ChannelId(90);
        let ch = |id: i64, kind: ChannelKind, category_id: Option<ChannelId>| vox_proto::Channel {
            id: ChannelId(id),
            name: format!("c{id}"),
            kind,
            position: id as i32,
            private: None,
            category_id,
        };
        vec![
            ch(1, ChannelKind::Text, None),
            ch(2, ChannelKind::Text, None),
            ch(3, ChannelKind::Text, Some(cat)),
            ch(4, ChannelKind::Text, Some(cat)),
            ch(5, ChannelKind::Voice, None),
            ch(6, ChannelKind::Voice, Some(cat)),
        ]
    }

    /// The order as a list of ids (what the wire takes).
    fn ids(order: &[ChannelId]) -> Vec<i64> {
        order.iter().map(|c| c.into_inner()).collect()
    }

    /// Move `id` and unwrap the result's order.
    fn moved(order: &[vox_proto::Channel], id: i64, at: i64, after: bool) -> Vec<i64> {
        let (moved, _) = moved_order(order, ChannelId(id), ChannelId(at), after).expect("move aceito");
        ids(&moved)
    }

    #[test]
    fn dropping_after_the_next_channel_moves_one_slot_down() {
        assert_eq!(moved(&flat(), 1, 2, true), vec![2, 1, 3, 4, 5, 6]);
    }

    #[test]
    fn dropping_before_the_previous_channel_moves_one_slot_up() {
        // 3 is before 4 in the drawing; dropping 4 before 3 puts them the other way round.
        assert_eq!(moved(&flat(), 4, 3, false), vec![1, 2, 4, 3, 5, 6]);
    }

    #[test]
    fn dropping_where_it_already_is_changes_nothing() {
        // Before the neighbour, or after the neighbour, from the neighbouring slot: the
        // channel is exactly where the marker says, so nothing moves.
        assert_eq!(moved(&flat(), 1, 2, false), vec![1, 2, 3, 4, 5, 6]);
        assert_eq!(moved(&flat(), 2, 1, true), vec![1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn dropping_over_itself_is_refused() {
        let order = flat();
        assert!(moved_order(&order, ChannelId(1), ChannelId(1), true).is_none());
        assert!(moved_order(&order, ChannelId(1), ChannelId(1), false).is_none());
    }

    #[test]
    fn a_text_channel_never_trades_places_with_a_voice_room() {
        // The sidebar draws text and voice in separate blocks; a drop across them is refused
        // rather than silently ignored.
        let order = flat();
        assert!(moved_order(&order, ChannelId(1), ChannelId(5), true).is_none());
        assert!(moved_order(&order, ChannelId(6), ChannelId(3), false).is_none());
    }

    #[test]
    fn moving_into_a_category_reports_the_new_group() {
        // Channel 1 is loose; dropped inside the category (on channel 3), it belongs there now.
        let (moved, category) = moved_order(&flat(), ChannelId(1), ChannelId(3), false).unwrap();
        assert_eq!(ids(&moved), vec![2, 1, 3, 4, 5, 6]);
        assert_eq!(category, Some(ChannelId(90)), "o canal passa a pertencer à categoria do alvo");
    }

    #[test]
    fn moving_out_of_a_category_reports_being_loose() {
        let (_, category) = moved_order(&flat(), ChannelId(4), ChannelId(1), true).unwrap();
        assert_eq!(category, None, "solto solto sobre um canal solto, o canal fica solto");
    }

    #[test]
    fn only_the_moved_kind_is_rewritten() {
        // A voice room moved: the text rows keep their slots exactly.
        assert_eq!(moved(&flat(), 5, 6, true), vec![1, 2, 3, 4, 6, 5]);
    }

    #[test]
    fn a_row_that_is_gone_is_refused() {
        let order = flat();
        assert!(moved_order(&order, ChannelId(1), ChannelId(999), true).is_none());
        assert!(moved_order(&order, ChannelId(999), ChannelId(1), true).is_none());
    }

    #[test]
    fn categories_reorder_among_themselves() {
        let cats = |a: i64, b: i64, c: i64| {
            vec![
                ChannelCategory::new(ChannelId(a), "a", "#5865F2", 0),
                ChannelCategory::new(ChannelId(b), "b", "#5865F2", 1),
                ChannelCategory::new(ChannelId(c), "c", "#5865F2", 2),
            ]
        };
        let order = cats(1, 2, 3);
        assert_eq!(ids(&moved_category_order(&order, ChannelId(1), ChannelId(2), true).unwrap()), vec![2, 1, 3]);
        assert_eq!(ids(&moved_category_order(&order, ChannelId(3), ChannelId(1), false).unwrap()), vec![3, 1, 2]);
        assert_eq!(ids(&moved_category_order(&order, ChannelId(2), ChannelId(3), false).unwrap()), vec![1, 2, 3]);
        assert!(moved_category_order(&order, ChannelId(1), ChannelId(1), true).is_none());
        assert!(moved_category_order(&order, ChannelId(1), ChannelId(9), true).is_none());
    }
}

