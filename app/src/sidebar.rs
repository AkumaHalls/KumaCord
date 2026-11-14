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

/// The bottom panels' background: the sidebar's own grey. Was a hard-coded `#17222D`, a
/// leftover of the v1.0 night-lamp navy that no token in `theme` carried any more — so the
/// "você" and voice panels read as a blue slab under a grey list.
const PANEL_BG: Color32 = theme::PAINEL;
/// A person in a voice room.
const MEMBER_H: f32 = 28.0;
const MEMBER_INDENT: f32 = 30.0;
const MEMBER_AVATAR: f32 = 22.0;
/// A row of the member list.
const ONLINE_H: f32 = 40.0;
const ONLINE_AVATAR: f32 = 30.0;

/// The thickness of the insertion marker of a drag & drop.
const DROP_MARK: f32 = 2.0;

/// One text line's height for `font`. Rows that hold two lines (name + "Jogando X") are sized
/// from it, so the second line is never cut by the row's own clip (`painter_at(rect)` clips
/// to `rect`).
fn line_h(ui: &egui::Ui, font: egui::FontId) -> f32 {
    ui.painter().layout_no_wrap(" ".to_string(), font, Color32::WHITE).size().y
}

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
/// drop rebuilds exactly what the screen shows and sends that back.
///
/// v1.6: ONE interleaved list, the way Discord draws it. A standalone channel and a category
/// header share a single `position` sequence (which is exactly what
/// `POST /api/admin/structure` writes: `db::reorder_structure` numbers `top` `0..n` across
/// both tables), so the order on screen is that sequence and nothing else. Before this the
/// plan carried two lists — every loose channel first, then every category — which threw the
/// interleaving away: a category could never sit between two loose channels, no matter what
/// the server said.
struct SidebarPlan {
    /// The top level in the order drawn: standalone channels and categories interleaved.
    top: Vec<TopItem>,
}

/// v1.6: one entry of the top level — a standalone channel, or a category header with the
/// channels inside it (text first, then voice, the order Discord draws them).
#[derive(Debug, Clone)]
enum TopItem {
    Channel(vox_proto::Channel),
    Category {
        cat: ChannelCategory,
        text: Vec<vox_proto::Channel>,
        voice: Vec<vox_proto::Channel>,
    },
}

impl TopItem {
    /// The id a drag names this row by (a channel's or the category's).
    fn key(&self) -> Top {
        match self {
            TopItem::Channel(c) => Top::Channel(c.id),
            TopItem::Category { cat, .. } => Top::Category(cat.id),
        }
    }
}

/// v1.6: which kind of row a drag is over — the two id spaces are separate (the hub mints a
/// category and a channel from one counter, but a drag says which table it means).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Top {
    Channel(ChannelId),
    Category(ChannelId),
}

impl SidebarPlan {
    /// Every channel, in the order drawn: the flat truth behind `Channel::position`.
    fn flat(&self) -> Vec<vox_proto::Channel> {
        let mut all = Vec::new();
        for item in &self.top {
            match item {
                TopItem::Channel(c) => all.push(c.clone()),
                TopItem::Category { text, voice, .. } => {
                    all.extend(text.iter().cloned());
                    all.extend(voice.iter().cloned());
                }
            }
        }
        all
    }

    /// The whole order as ONE batch (what a drag & drop sends). Positions are densest first
    /// (0, 1, 2...), which is what the hub writes back — it ignores what it is sent and
    /// renumbers, so what matters here is the ORDER of the lists, not the numbers.
    fn request(&self) -> vox_proto::StructureReorderRequest {
        let mut top = Vec::with_capacity(self.top.len());
        for (pos, item) in self.top.iter().enumerate() {
            let id = match item {
                TopItem::Channel(c) => c.id,
                TopItem::Category { cat, .. } => cat.id,
            };
            top.push(vox_proto::StructureItem { id, position: pos as i32 });
        }
        // A category with no channel keeps an empty list: the batch must cover every category
        // exactly once, so an empty one is said so instead of being left out.
        let groups = self
            .top
            .iter()
            .filter_map(|item| match item {
                TopItem::Category { cat, text, voice } => Some(vox_proto::StructureGroup {
                    category_id: cat.id,
                    ids: text.iter().chain(voice).map(|c| c.id).collect(),
                }),
                TopItem::Channel(_) => None,
            })
            .collect();
        vox_proto::StructureReorderRequest { top, groups }
    }
}

/// v1.6: the order the sidebar is drawn in, from the channels and the categories as the app
/// knows them right now. Pure, so a test can build one without a whole `App`.
///
/// Text comes before voice inside every group, and a standalone channel wins a tie against a
/// category at the same `position` — which is the order an old server (two separate position
/// sequences, both starting at 0) was drawn in before the unified endpoint existed.
fn plan_of(channels: &[vox_proto::Channel], categories: &[ChannelCategory]) -> SidebarPlan {
    let mut by_category: HashMap<Option<ChannelId>, Vec<vox_proto::Channel>> = HashMap::new();
    for c in channels {
        by_category.entry(c.category_id).or_default().push(c.clone());
    }
    let known: HashMap<ChannelId, ChannelCategory> = categories.iter().map(|c| (c.id, c.clone())).collect();
    // A channel whose `category_id` names a category that is not in the list counts as loose:
    // that is the shape of a category just deleted (the hub leaves its channels standalone and
    // broadcasts `ChannelsChanged`, which may not have landed yet). Left out of both lists it
    // would fall out of the batch entirely, and the hub refuses a batch that misses a channel.
    let mut loose: Vec<vox_proto::Channel> = Vec::new();
    let mut groups: HashMap<ChannelId, Vec<vox_proto::Channel>> = HashMap::new();
    for (cat, mut chans) in by_category {
        chans.sort_by_key(|c| (c.kind == ChannelKind::Voice, c.position, c.id));
        match cat {
            Some(id) if known.contains_key(&id) => {
                groups.insert(id, chans);
            }
            _ => loose.extend(chans),
        }
    }
    loose.sort_by_key(|c| (c.kind == ChannelKind::Voice, c.position, c.id));
    let mut items: Vec<TopItem> = loose.into_iter().map(TopItem::Channel).collect();
    let mut cats: Vec<ChannelCategory> = categories.to_vec();
    cats.sort_by_key(|c| (c.position, c.id));
    for cat in cats {
        let (mut text, mut voice) = (Vec::new(), Vec::new());
        for c in groups.remove(&cat.id).unwrap_or_default() {
            if c.kind == ChannelKind::Voice {
                voice.push(c);
            } else {
                text.push(c);
            }
        }
        items.push(TopItem::Category { cat, text, voice });
    }
    // ONE sequence: the position a category and a standalone channel share. The kind breaks a
    // tie (channel first, the order an old server was drawn in), then the id, so the sort is
    // total and the drawing is the same in every frame.
    items.sort_by_key(|item| match item {
        TopItem::Channel(c) => (c.position, 0, c.id),
        TopItem::Category { cat, .. } => (cat.position, 1, cat.id),
    });
    SidebarPlan { top: items }
}

/// v1.6: the plan after a channel was dropped at `target`: the top level keeps the slots it had
/// (a channel move never moves a category), the channels that stayed loose are re-ordered to
/// `flat`'s drawn order, and a channel that just became loose is inserted next to the row it was
/// dropped on. `flat` is every channel with the moved channel's new group already written in.
///
/// The server's `position`s still describe the OLD order here, so the plan is rebuilt from the
/// order the drag produced rather than re-derived from them — re-deriving would undo the move.
fn plan_after_channel_drop(old: &[TopItem], flat: &[vox_proto::Channel], moved: ChannelId, target: Top, after: bool) -> SidebarPlan {
    let by_id: HashMap<ChannelId, &vox_proto::Channel> = flat.iter().map(|c| (c.id, c)).collect();
    let loose = |id: ChannelId| by_id.get(&id).is_some_and(|c| c.category_id.is_none());
    let mut out: Vec<TopItem> = Vec::with_capacity(old.len() + 1);
    let mut slots: Vec<usize> = Vec::new();
    for item in old {
        match item {
            TopItem::Category { cat, .. } => {
                let mut text = Vec::new();
                let mut voice = Vec::new();
                for c in flat.iter().filter(|c| c.category_id == Some(cat.id)) {
                    if c.kind == ChannelKind::Voice {
                        voice.push(c.clone());
                    } else {
                        text.push(c.clone());
                    }
                }
                out.push(TopItem::Category { cat: cat.clone(), text, voice });
            }
            // A channel that moved into a category is no longer a top-level slot.
            TopItem::Channel(c) if loose(c.id) => {
                slots.push(out.len());
                out.push(TopItem::Channel(c.clone()));
            }
            TopItem::Channel(_) => {}
        }
    }
    // The surviving slots, re-ordered to the drawn order: a swap between two loose channels
    // changes the top level, not only the group.
    let kept: Vec<ChannelId> = slots.iter().filter_map(|i| match &out[*i] { TopItem::Channel(c) => Some(c.id), _ => None }).collect();
    let ordered: Vec<vox_proto::Channel> = flat.iter().filter(|c| c.category_id.is_none() && kept.contains(&c.id)).cloned().collect();
    for (slot, c) in slots.into_iter().zip(ordered) {
        out[slot] = TopItem::Channel(c);
    }
    // A channel that just became loose is a NEW slot, next to the row it was dropped on.
    if let Some(c) = by_id.get(&moved).copied().filter(|c| c.category_id.is_none() && !kept.contains(&moved)) {
        match out.iter().position(|it| it.key() == target) {
            Some(i) => out.insert(if after { i + 1 } else { i }, TopItem::Channel(c.clone())),
            None => out.push(TopItem::Channel(c.clone())),
        }
    }
    SidebarPlan { top: out }
}

/// v1.6: the top level with `id` moved into `at`'s slot — the same before/after arithmetic
/// [`moved_slot`] does for a list of channel ids, over the interleaved list.
fn moved_top(items: &[TopItem], id: Top, at: Top, after: bool) -> Option<Vec<TopItem>> {
    let keys: Vec<Top> = items.iter().map(TopItem::key).collect();
    let moved = moved_slot(&keys, id, at, after)?;
    // `moved` is a permutation of `keys`: take each row out of the pool in the new order.
    let mut pool: Vec<Option<TopItem>> = items.to_vec().into_iter().map(Some).collect();
    let mut out = Vec::with_capacity(items.len());
    for k in moved {
        let i = pool.iter().position(|s| s.as_ref().is_some_and(|it| it.key() == k))?;
        out.push(pool[i].take()?);
    }
    Some(out)
}

/// v1.2: `order` with `id` moved into `at`'s slot — the before/after arithmetic, once.
/// `None`: over itself, or one of the two is not in the list.
///
/// This is where the off-by-one lives: taking `id` out shifts every later slot left, so `at`
/// moves one step closer when it came after `id`; and "after" means one slot further still.
///
/// Generic over the row's key, so the same arithmetic serves a list of channel ids, a list of
/// category ids and the interleaved top level.
fn moved_slot<T: PartialEq + Copy>(order: &[T], id: T, at: T, after: bool) -> Option<Vec<T>> {
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

/// v1.2: what a collapsed category's header carries: the number of unread mentions inside it
/// (the pill) and whether anything at all inside is unread (the name is drawn bright).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CollapsedBadge {
    pub mentions: u32,
    pub unread: bool,
}

/// v1.2: the badge of category `cat` from the counters — only its own channels, and only the text
/// ones (a voice room has nothing unread). `None` when nothing inside is waiting: a quiet closed
/// category looks exactly like a quiet open one.
pub(crate) fn collapsed_counts_of(
    cat: ChannelId,
    text_channels: &[vox_proto::Channel],
    unread: &HashMap<ChannelId, u32>,
    mentions: &HashMap<ChannelId, u32>,
) -> Option<CollapsedBadge> {
    let mut n = 0;
    let mut any = false;
    for c in text_channels.iter().filter(|c| c.category_id == Some(cat)) {
        n += mentions.get(&c.id).copied().unwrap_or(0);
        any |= unread.get(&c.id).copied().unwrap_or(0) > 0;
    }
    (n > 0 || any).then_some(CollapsedBadge { mentions: n, unread: any })
}

/// v1.2: the colour a collapsed category's name is drawn in — bright when something inside is
/// unread (a mention or a plain unread line), the category's own colour when everything is read.
pub(crate) fn badge_name_color(badge: Option<CollapsedBadge>, own: Color32) -> Color32 {
    match badge {
        Some(_) => theme::TEXTO,
        None => own,
    }
}

/// v1.2: paint a collapsed category's badge at the right edge of `header` — Discord's white pill
/// with the mention count, leaving the room the "+" needs (`plus_w`). Pure paint: no `interact`, so
/// a click on the badge still reaches the header underneath and collapses the category.
///
/// Returns the pill's left edge (the name truncates to what is left of it), or `None` when there is
/// no pill to paint — a plain unread brightens the name and takes no room.
pub(crate) fn paint_collapsed_badge(painter: &egui::Painter, header: Rect, badge: CollapsedBadge, plus_w: f32) -> Option<f32> {
    if badge.mentions == 0 {
        return None;
    }
    let text = badge.mentions.to_string();
    let galley = painter.layout_no_wrap(text, theme::semibold(11.0), theme::NOITE);
    let pad = Vec2::new(7.0, 2.0);
    let size = galley.size() + pad * 2.0;
    // Right edge, then the "+"'s room, then a gap — never over the label and never over the button.
    let right = header.right() - plus_w - 4.0;
    let rect = Rect::from_min_size(Pos2::new(right - size.x, header.center().y - size.y / 2.0), size);
    painter.rect_filled(rect, radius::PILL, theme::TEXTO);
    painter.galley(Pos2::new(rect.center().x - galley.size().x / 2.0, rect.center().y - galley.size().y / 2.0), galley, theme::NOITE);
    Some(rect.left())
}

/// Paint a small icon at the right edge `x` (moving left), returning its rect.
fn icon_at(painter: &egui::Painter, x: &mut f32, cy: f32, icon: &str, color: Color32) -> Rect {    let g = painter.layout_no_wrap(icon.to_owned(), theme::body(14.0), color);
    let r = Rect::from_min_size(Pos2::new(*x - g.size().x, cy - g.size().y / 2.0), g.size());
    painter.galley(r.min, g, color);
    *x = r.left() - 5.0;
    r
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
        // An older hub has no structure endpoint, so its rows are not grabbable either — dragging
        // one would be refused with a 404 nobody can act on (the flag gates the whole feature, the
        // same way `activity` gates "Jogando X" and `channel_extras` the channel's editor).
        let draggable = self.chatx.server_features.structure_reorder && self.my_role().can_manage_channels();
        // The marker belongs to this frame: every row rewrites it while the pointer is over it.
        self.sidebar_drop = None;
        // v1.6: ONE interleaved list, so a category can sit between two loose channels exactly
        // where the server says it does (before this the two were drawn in two blocks).
        let plan = self.sidebar_plan();
        for item in &plan.top {
            match item {
                TopItem::Channel(c) => {
                    if c.kind == ChannelKind::Text {
                        self.category_channel_row(ui, c, open, draggable);
                    } else {
                        self.voice_channel_row(ui, c, open, draggable);
                    }
                }
                // An empty category is only drawn for those who can manage channels (a member has
                // nothing to do in it — Discord hides it too).
                TopItem::Category { cat, text, voice } => {
                    if text.is_empty() && voice.is_empty() && !draggable {
                        continue;
                    }
                    self.category_section(ui, cat, text, voice, open, draggable);
                }
            }
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
        plan_of(&self.channels, &self.categories)
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
        // v1.2: a closed category keeps its unread count on the header (Discord's white pill), so
        // nobody has to open every category to find out something is waiting. Counted before the
        // name is drawn, because the mention makes the name bright too.
        let badge = (!is_open).then(|| self.collapsed_counts(cat.id, text_channels)).flatten();
        // v1.2: Discord's "+" on the category's header — a new channel inside THIS category, no
        // trip to the server window. Only for whoever may create channels; everybody else gets
        // the plain header (the room the label takes is given back when there is no button).
        let may_create = self.roles.supported && self.my_role().can_manage_channels();
        let plus_w = if may_create { 22.0 } else { 0.0 };

        // The header: caret + NAME IN CAPS, in the category's colour. Click toggles open/closed.
        let sense = if draggable { Sense::click_and_drag() } else { Sense::click() };
        // 2026-11-11: an EXPLICIT id, not egui's automatic counter. The auto id depends on how
        // many widgets were allocated before this header in the same frame, and that count moves
        // whenever a DM arrives or somebody joins/leaves a voice room above us: the press lands on
        // one id and the release on another, and egui swallows the click without a trace (the
        // toggle only worked when the sidebar above was perfectly still). Derived from the
        // category's own id, it is the same in every frame.
        let header_rect = Rect::from_min_size(ui.cursor().min, Vec2::new(ui.available_width(), 22.0));
        let header_resp = ui.interact(header_rect, egui::Id::new(("sidebar_category_header", cat.id)), sense);
        ui.advance_cursor_after_rect(header_rect);
        if header_resp.hovered() {
            ui.painter().rect_filled(header_rect, radius::ROW, theme::ROW_HOVER);
        }
        if header_resp.clicked() {
            // Remembered across sessions, like Discord's `CategoryCollapseStore`.
            self.set_category_collapsed(cat.id, is_open);
        }
        // Unread inside a closed category brightens the name, exactly like a channel row.
        let own = Color32::from_hex(cat.color.get(1..).unwrap_or("5865F2")).unwrap_or(theme::NEVOA);
        let color = badge_name_color(badge, own);
        // The pill is painted before the label so its exact width is known: the name truncates to
        // the room that is left, never running under the badge.
        let badge_left = badge.map(|b| paint_collapsed_badge(ui.painter(), header_rect, b, plus_w)).flatten();
        let room_for_name = badge_left.map_or(header_rect.right(), |l| l - 4.0);
        let label_w = (room_for_name - 8.0 - plus_w - header_rect.left()).max(24.0);
        // Too long for the room the "+" leaves: ellipsis, like a truncated row's name.
        let label = crate::app::notify::fit_name(ui.painter(), &format!("{caret} {}", cat.name.to_uppercase()), theme::semibold(11.5), color, label_w);
        ui.painter().galley(
            Pos2::new(header_rect.left() + 8.0, header_rect.center().y - label.size().y / 2.0),
            label,
            color,
        );
        if may_create {
            // The button's own rect: an explicit id again (the header above it is one widget this
            // frame and another the next, and an automatic id would move with it).
            let plus = Rect::from_min_size(
                Pos2::new(header_rect.right() - plus_w + 2.0, header_rect.center().y - 9.0),
                Vec2::splat(18.0),
            );
            let plus_resp = ui.interact(plus, egui::Id::new(("category_plus", cat.id)), Sense::click());
            let plus_color = if plus_resp.hovered() { theme::GIZ } else { theme::NEVOA };
            if plus_resp.hovered() {
                ui.painter().rect_filled(plus, radius::ROW, theme::ROW_HOVER);
            }
            ui.painter().text(plus.center(), egui::Align2::CENTER_CENTER, ph::PLUS, theme::semibold(13.0), plus_color);
            let plus_resp = plus_resp.on_hover_text("Criar um canal nesta categoria").on_hover_cursor(egui::CursorIcon::PointingHand);
            if plus_resp.clicked() {
                // A text channel is what the "+" makes (the right click has the voice one).
                self.create_channel_in(Some(cat.id), false);
            }
        }
        if draggable {
            self.sidebar_category_dnd(ui, &header_resp, cat.id);
        }
        // Discord's right-click on a category: edit it, create a channel inside it, delete it.
        self.chan_menu_hit(&header_resp, crate::context_menu::ChanTarget::Category(cat.clone()));

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

    /// v1.2: what a collapsed category shows on its header (see [`collapsed_counts_of`]).
    fn collapsed_counts(&self, cat: ChannelId, text_channels: &[vox_proto::Channel]) -> Option<CollapsedBadge> {
        collapsed_counts_of(cat, text_channels, &self.unread, &self.chatx.mention_unread)
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
            id: Some(egui::Id::new(("sidebar_channel_row", c.id))),
        };
        let resp = if draggable { ui_kit::sidebar_row_drag(ui, row) } else { ui_kit::sidebar_row(ui, row) };
        if resp.0.clicked() {
            self.select_text(c.id);
        }
        // Discord's right-click on a channel: read it, invite, copy its id, and (for an admin)
        // edit, create inside it and delete it.
        self.chan_menu_hit(&resp.0, crate::context_menu::ChanTarget::Channel(c.clone()));
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
            id: Some(egui::Id::new(("sidebar_channel_row", c.id))),
        };
        let (resp, _drawn) = if draggable { ui_kit::sidebar_row_drag(ui, row) } else { ui_kit::sidebar_row(ui, row) };
        if resp.clicked() {
            self.join_voice(c.id);
        }
        // The voice room's own menu (the same items a text channel has, minus "marcar como lido").
        self.chan_menu_hit(&resp, crate::context_menu::ChanTarget::Channel(c.clone()));
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
    /// channel is dragged. A row never accepts a channel of the other kind — the sidebar always
    /// keeps text and voice apart. v1.6: a category header DOES accept one: it is a slot in the
    /// top level, and a channel dropped there becomes loose at that slot.
    fn sidebar_channel_dnd(&mut self, ui: &egui::Ui, resp: &egui::Response, id: ChannelId) {
        if resp.drag_started() {
            self.sidebar_drag = Some(SidebarDrag::Channel(id));
        }
        if self.sidebar_drag == Some(SidebarDrag::Channel(id)) {
            return; // over itself: no marker, no move
        }
        // Only a standalone channel is a top-level slot (a channel inside a category is a child,
        // and dropping a category "between" two children means nothing).
        let loose = self.channel_of(id).and_then(|c| c.category_id).is_none();
        self.sidebar_mark_drop(ui, SidebarDrag::Channel(id), resp.rect, loose);
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
        self.sidebar_mark_drop(ui, SidebarDrag::Category(id), resp.rect, true);
    }

    /// v1.2: the pointer is over `rect`, a row that accepts this drag: that is where it would
    /// land. The half of the row decides "before" or "after". The hit test is by pointer
    /// position rather than `hovered()`, because a widget that owns the drag reports itself
    /// hovered above everything else.
    ///
    /// v1.6: what accepts what. A channel takes a channel of the same kind and a category
    /// header; a category takes another header and a STANDALONE channel's row. `top_level` says
    /// whether this row is such a slot.
    fn sidebar_mark_drop(&mut self, ui: &egui::Ui, kind: SidebarDrag, rect: Rect, top_level: bool) {
        let Some(dragged) = self.sidebar_drag else { return };
        let accepts = matches!(
            (dragged, kind),
            (SidebarDrag::Channel(_), SidebarDrag::Channel(_))
                | (SidebarDrag::Channel(_), SidebarDrag::Category(_))
                | (SidebarDrag::Category(_), SidebarDrag::Category(_))
                | (SidebarDrag::Category(_), SidebarDrag::Channel(_)) if top_level
        );
        if !accepts {
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

    /// v1.2: move channel `id` to `at` in the drawn order and hand back the whole new order.
    /// `None`: nothing to do — over itself, over the other kind (a text channel never trades
    /// places with a voice room), or over a row that is gone.
    fn sidebar_channel_move(&self, id: ChannelId, at: ChannelId, after: bool) -> Option<ChannelMove> {
        let (ids, category) = moved_order(&self.sidebar_plan().flat(), id, at, after)?;
        Some(ChannelMove { ids, category })
    }

    /// v1.2: the drop as ONE batch (`POST /api/admin/structure`) instead of the pair
    /// reorder-channels + reorder-categories. Those two rewrite their own list's positions
    /// `0..n`, so moving a standalone channel renumbered every category and the sidebar jumped;
    /// one transaction carrying the whole state keeps the interleaving by construction.
    ///
    /// v1.6: the batch is the whole plan AFTER the move, so what the hub writes back is exactly
    /// what the screen shows — a category that just landed between two loose channels included.
    fn sidebar_drop_apply(&mut self) {
        let (Some(drag), Some(drop)) = (self.sidebar_drag, self.sidebar_drop) else { return };
        let plan = self.sidebar_plan();
        // Which table the row under the pointer lives in: a channel id or a category id.
        let target = if self.channels.iter().any(|c| c.id == drop.at) { Top::Channel(drop.at) } else { Top::Category(drop.at) };
        let req = match drag {
            SidebarDrag::Channel(id) => self.channel_drop_plan(&plan, id, target, drop.after).map(|p| p.request()),
            // A category only ever moves among the top level's slots: its children stay put.
            SidebarDrag::Category(id) => moved_top(&plan.top, Top::Category(id), target, drop.after).map(|top| SidebarPlan { top }.request()),
        };
        if let Some(req) = req {
            self.admin(crate::roles::AdminCmd::ReorderStructure(req));
        }
    }

    /// v1.6: the plan after channel `id` is dropped at `target`.
    ///
    /// Over another channel of the same kind: it takes that kind's new order and the target's
    /// group (dropped on a channel inside a category, it joins it — Discord's gesture,
    /// unchanged). Over a category HEADER: the channel leaves whatever group it was in and takes
    /// that slot in the top level, between the rows around the header.
    fn channel_drop_plan(&mut self, plan: &SidebarPlan, id: ChannelId, target: Top, after: bool) -> Option<SidebarPlan> {
        if let Top::Channel(at) = target {
            let moved = self.sidebar_channel_move(id, at, after)?;
            // A channel that crossed from one group to another needs its new category first
            // (the hub broadcasts `ChannelsChanged` and the row moves).
            if self.channel_of(id).and_then(|c| c.category_id) != moved.category {
                self.admin(crate::roles::AdminCmd::SetChannelCategory { id, category: moved.category });
            }
            // `moved.ids` is every channel in the new drawn order; the new group is not in
            // `self.channels` yet (the hub answers later), so it is written here — otherwise the
            // batch would send the channel back where it was.
            let by_id: HashMap<ChannelId, vox_proto::Channel> = self.channels.iter().map(|c| (c.id, c.clone())).collect();
            let mut flat: Vec<vox_proto::Channel> = moved.ids.iter().filter_map(|i| by_id.get(i).cloned()).collect();
            if let Some(c) = flat.iter_mut().find(|c| c.id == id) {
                c.category_id = moved.category;
            }
            return Some(plan_after_channel_drop(&plan.top, &flat, id, target, after));
        }
        // Over a category header: the channel becomes loose at that slot.
        let mut flat: Vec<vox_proto::Channel> = self.channels.clone();
        if let Some(c) = flat.iter_mut().find(|c| c.id == id) {
            c.category_id = None;
        }
        if self.channel_of(id).and_then(|c| c.category_id).is_some() {
            self.admin(crate::roles::AdminCmd::SetChannelCategory { id, category: None });
        }
        Some(plan_after_channel_drop(&plan.top, &flat, id, target, after))
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
            // v1.2: Discord's quick create, from the sidebar. Both open the same form the server
            // window has, already set up (a standalone channel / an empty category name).
            if ui_kit::menu_row(ui, ph::PLUS, "Criar canal...", false).on_hover_text("Um canal de texto ou de voz, solto no topo (escolha a categoria no formulário)").clicked() {
                self.create_channel_in(None, false);
                ui.close();
            }
            if ui_kit::menu_row(ui, ph::FOLDER_OPEN, "Criar categoria...", false).on_hover_text("Uma categoria nova para organizar os canais").clicked() {
                self.roles.tab = crate::roles::ServerTab::Channels;
                self.roles.new_category.clear();
                self.roles.new_category_color.clear();
                self.open_server_settings();
                ui.close();
            }
            ui_kit::menu_separator(ui);
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
        // Discord's `CATEGORY_COLLAPSE_ALL` / `CATEGORY_EXPAND_ALL`: a server with a dozen
        // categories should not need a dozen clicks to get out of the way.
        if !self.categories.is_empty() {
            let closed = self.all_categories_collapsed();
            let (icon, label) = if closed { (ph::CARET_UP, "Expandir todas as categorias") } else { (ph::CARET_DOWN, "Colapsar todas as categorias") };
            if ui_kit::menu_row(ui, icon, label, false).clicked() {
                self.set_all_categories_collapsed(!closed);
                ui.close();
            }
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
        // 2026-11-11: an explicit id, like the category header and the channel rows: the automatic
        // one moves with the number of rows above, so a click could land on one id at the press and
        // on another at the release (egui then swallows it). Keyed on the member's own id it cannot.
        // v1.2: "Jogando X" under the name — the row grows by one line when a game is on, so the
        // second line is not cut by the row's own clip (`painter_at(rect)` clips to `rect`).
        let act_line = self.activity_line(m.user.id).filter(|l| !l.is_empty());
        let row_h = if act_line.is_some() {
            (line_h(ui, theme::body(13.5)) + line_h(ui, theme::body(10.5)) + 4.0).max(MEMBER_H)
        } else {
            MEMBER_H
        };
        let rect = Rect::from_min_size(ui.cursor().min, Vec2::new(ui.available_width(), row_h));
        let resp = ui.interact(rect, egui::Id::new(("voice_member_row", m.user.id)), Sense::click());
        ui.advance_cursor_after_rect(rect);
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
            ui_kit::paint_speaking_ring(&painter, center, MEMBER_AVATAR / 2.0, if resp.hovered() { theme::ROW_HOVER } else { theme::PAINEL });
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
        // v1.6: the game's own tile left of the two lines when a game is on; the text moves right.
        let game = act_line.as_ref().and_then(|_| self.activity_of(m.user.id)).map(|a| a.name.clone());
        let text_left = if game.is_some() { name_left + crate::game_icons::TILE + 5.0 } else { name_left };
        let color = match crate::roles::role_color(role) {
            Some(c) => c,
            None if speaking => theme::GIZ,
            None => theme::NEVOA,
        };
        let name = crate::app::notify::fit_name(&painter, m.user.shown_name(), theme::body(13.5), color, x - text_left - 2.0);
        let name_h = name.size().y;
        // v1.2: "Jogando X" under the name — both lines centered as one block inside the row
        // (the row was sized for them above); `fit_name` ends a long game with "…".
        if let Some(line) = &act_line {
            let act = crate::app::notify::fit_name(&painter, line, theme::body(10.5), theme::NEVOA, x - text_left - 2.0);
            let act_h = act.size().y;
            let top = cy - (name_h + act_h) / 2.0;
            painter.galley(Pos2::new(text_left, top), name, color);
            let act_top = top + name_h;
            painter.galley(Pos2::new(text_left, act_top), act, theme::NEVOA);
            // v1.6: the tile vertically centered on the activity line.
            if let Some(g) = &game {
                let tile_rect = Rect::from_center_size(Pos2::new(name_left + crate::game_icons::TILE / 2.0, act_top + act_h / 2.0), Vec2::splat(crate::game_icons::TILE));
                crate::game_icons::tile(&ctx, &painter, &mut self.game_icons, &mut self.emoji_textures, g, tile_rect);
            }
            // Hover on the line shows the full game name (the drawn one may end in "…").
            let act_rect = Rect::from_min_size(Pos2::new(text_left, act_top), Vec2::new(x - text_left - 2.0, act_h));
            ui.interact(act_rect, ui.id().with(("activity", m.user.id)), Sense::hover()).on_hover_text(line.clone());
        } else {
            painter.galley(Pos2::new(text_left, cy - name_h / 2.0), name, color);
        }

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
            // v1.2: a third line ("Jogando X") when a game is on, or the block stays two lines.
            // The height is the sum of the real line heights, not a guess — a fixed 46.0 cut the
            // bottom of the third line at the panel's clip rect.
            let act_on = self.my_id().is_some_and(|id| self.activity_line(id).is_some_and(|l| !l.is_empty()));
            let text_h = line_h(ui, theme::semibold(13.5)) + line_h(ui, theme::body(12.0)) + if act_on { line_h(ui, theme::body(11.0)) } else { 0.0 };
            ui.allocate_ui_with_layout(Vec2::new(text_w, text_h), egui::Layout::top_down(egui::Align::Min), |ui| {
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
    /// changes it). Under it, v1.2, the game the detector found me in.
    fn me_status(&mut self, ui: &mut egui::Ui) {
        let font = theme::body(12.0);
        if let Some(ch) = self.voice.channel {
            let text = if ch == super::dms::DM_CALL_CHANNEL {
                "em uma chamada".to_string()
            } else {
                format!("na {}", self.channels.iter().find(|c| c.id == ch).map(|c| c.name.as_str()).unwrap_or("voz"))
            };
            ui.add(egui::Label::new(RichText::new(text).font(font).color(theme::ONLINE)).truncate().selectable(false));
        } else if self.profile.supported != Some(true) {
            ui.label(RichText::new("online").font(font).color(theme::ONLINE));
        } else {
            let current = self.my_presence();
            let r = ui.add(egui::Label::new(RichText::new(format!("{} {}", crate::profile::presence_label(current), ph::CARET_DOWN)).font(font).color(presence_text_color(current))).sense(Sense::click()));
            let r = r
                .on_hover_text(format!("{}. Clique para mudar.", crate::profile::presence_label(current)))
                .on_hover_cursor(egui::CursorIcon::PointingHand);
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
        // v1.2: "Jogando X" under the presence (or the room) line, as on Discord's own panel.
        // `.truncate()` ends the line with "…" inside the available width; hover shows it whole.
        if let Some(me) = self.my_id() {
            if let Some(a) = self.activity_of(me).cloned().filter(|a| !crate::profile::activity_line(a).is_empty()) {
                let line = crate::profile::activity_line(&a);
                // v1.6: the game's own tile left of the line, as on Discord.
                ui.horizontal(|ui| {
                    let (r, _) = ui.allocate_exact_size(Vec2::splat(crate::game_icons::TILE), Sense::hover());
                    crate::game_icons::tile(ui.ctx(), &ui.painter_at(r), &mut self.game_icons, &mut self.emoji_textures, &a.name, r);
                    ui.add(egui::Label::new(RichText::new(line.clone()).font(theme::body(11.0)).color(theme::NEVOA)).truncate().selectable(false))
                        .on_hover_text(line);
                });
            }
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
        // 2026-11-11: an explicit id per person, like every other row of this list: the number of
        // people above changes while the app runs, and egui's automatic id would move with it —
        // the press and the release of a click could then land on different ids and the click would
        // be swallowed. Keyed on the person's own id it cannot.
        let rect = Rect::from_min_size(ui.cursor().min, Vec2::new(ui.available_width(), ONLINE_H));
        let resp = ui.interact(rect, egui::Id::new(("online_row", u.id)), Sense::click());
        ui.advance_cursor_after_rect(rect);
        if !ui.is_rect_visible(rect) {
            return;
        }
        let in_voice = self.voice_members.contains_key(&u.id);
        let with_me = self.voice.channel.is_some() && self.my_id() != Some(u.id) && self.voice_members.get(&u.id).is_some_and(|m| m.channel_id == self.voice.channel);
        let ctx = ui.ctx().clone();
        let painter = ui.painter_at(rect);
        let bg = if resp.hovered() { theme::ROW_HOVER } else { theme::PAINEL };
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
        let act_line = self.activity_line(u.id).filter(|l| !l.is_empty());
        // v1.6: the game's own tile left of the two lines when a game is on; the text moves right.
        let game = act_line.as_ref().and_then(|_| self.activity_of(u.id)).map(|a| a.name.clone());
        let text_left = if game.is_some() { name_left + crate::game_icons::TILE + 5.0 } else { name_left };
        let color = crate::roles::role_color(role).unwrap_or(if resp.hovered() { theme::GIZ } else { theme::TEXTO });
        let name = crate::app::notify::fit_name(&painter, u.shown_name(), theme::body(14.0), color, x - text_left - 2.0);
        let name_h = name.size().y;
        // v1.2: "Jogando X" on its own line under the name, as on Discord's member list. The two
        // lines are centered as one block — drawn from the middle down, the second line fell past
        // the row's bottom and was clipped by `painter_at(rect)`.
        if let Some(line) = &act_line {
            let act = crate::app::notify::fit_name(&painter, line, theme::body(11.0), theme::NEVOA, x - text_left - 2.0);
            let act_h = act.size().y;
            let top = cy - (name_h + act_h + 1.0) / 2.0;
            painter.galley(Pos2::new(text_left, top), name, color);
            let act_top = top + name_h + 1.0;
            painter.galley(Pos2::new(text_left, act_top), act, theme::NEVOA);
            // v1.6: the tile vertically centered on the activity line.
            if let Some(g) = &game {
                let tile_rect = Rect::from_center_size(Pos2::new(name_left + crate::game_icons::TILE / 2.0, act_top + act_h / 2.0), Vec2::splat(crate::game_icons::TILE));
                crate::game_icons::tile(&ctx, &painter, &mut self.game_icons, &mut self.emoji_textures, g, tile_rect);
            }
            // Hover on the line shows the full game name (the drawn one may end in "…").
            let act_rect = Rect::from_min_size(Pos2::new(text_left, act_top), Vec2::new(x - text_left - 2.0, act_h));
            ui.interact(act_rect, ui.id().with(("activity", u.id)), Sense::hover()).on_hover_text(line.clone());
        } else {
            painter.galley(Pos2::new(text_left, cy - name_h / 2.0), name, color);
        }
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
            topic: String::new(),
            bitrate: None,
            user_limit: None,
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

    /// Categories by (id, position) — the one top-level sequence the hub writes.
    fn cats_at(entries: &[(i64, i64)]) -> Vec<ChannelCategory> {
        entries.iter().map(|&(id, pos)| ChannelCategory::new(ChannelId(id), "cat", "#5865F2", pos as i32)).collect()
    }

    /// The same six channels as [`flat`], but with the positions the hub actually writes for a
    /// sidebar where a category sits BETWEEN two standalone channels: channel 1, the category 90
    /// (3, 4 and 6 inside it), channel 2, the empty category 100, and the voice room 5. That
    /// interleaving is the whole point of the unified endpoint, and it is what the sidebar draws.
    fn interleaved() -> Vec<vox_proto::Channel> {
        let mut all = flat();
        for c in all.iter_mut() {
            // Inside the category the channels have their own sequence, which the top level
            // never sees — only the standalone ones (1, 2, 5) share it with the categories.
            c.position = match c.id.into_inner() {
                1 => 0,
                2 => 2,
                5 => 5,
                inside => inside as i32,
            };
        }
        // The order drawn, which is the order the slice carries (`moved_order` reads it).
        [1, 3, 4, 6, 2, 5].iter().map(|&id| all.iter().find(|c| c.id == ChannelId(id)).unwrap().clone()).collect()
    }

    /// The plan the fixture builds.
    fn fixture() -> SidebarPlan {
        plan_of(&interleaved(), &cats_at(&[(90, 1), (100, 4)]))
    }

    /// The top level's rows, as a drag names them.
    fn top_keys(plan: &SidebarPlan) -> Vec<Top> {
        plan.top.iter().map(TopItem::key).collect()
    }

    /// The top level as a list of ids (what the wire takes).
    fn top_ids(plan: &SidebarPlan) -> Vec<i64> {
        top_keys(plan).iter().map(|k| match *k { Top::Channel(c) | Top::Category(c) => c.into_inner() }).collect()
    }

    /// Every channel, in the order drawn.
    fn flat_ids(plan: &SidebarPlan) -> Vec<i64> {
        plan.flat().iter().map(|c| c.id.into_inner()).collect()
    }

    /// `ids` turned back into channels — what a drop rebuilds the plan from.
    fn rebuilt(ids: &[ChannelId]) -> Vec<vox_proto::Channel> {
        let by_id: HashMap<ChannelId, vox_proto::Channel> = interleaved().into_iter().map(|c| (c.id, c)).collect();
        ids.iter().filter_map(|i| by_id.get(i).cloned()).collect()
    }

    /// The fixture's plan after `flat` was dropped at `target`.
    fn dropped(flat: &[vox_proto::Channel], moved: ChannelId, target: Top, after: bool) -> SidebarPlan {
        plan_after_channel_drop(&fixture().top, flat, moved, target, after)
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
        // v1.6: the same before/after arithmetic as the channels, over the interleaved top level
        // — a category is just another row now, not a second list.
        let items: Vec<TopItem> = [1i64, 2, 3]
            .iter()
            .map(|&i| TopItem::Category { cat: ChannelCategory::new(ChannelId(i), "cat", "#5865F2", i as i32), text: vec![], voice: vec![] })
            .collect();
        let order = |id: i64, at: i64, after: bool| top_keys(&SidebarPlan { top: moved_top(&items, Top::Category(ChannelId(id)), Top::Category(ChannelId(at)), after).unwrap() });
        assert_eq!(order(1, 2, true), vec![Top::Category(ChannelId(2)), Top::Category(ChannelId(1)), Top::Category(ChannelId(3))]);
        assert_eq!(order(3, 1, false), vec![Top::Category(ChannelId(3)), Top::Category(ChannelId(1)), Top::Category(ChannelId(2))]);
        assert_eq!(order(2, 3, false), vec![Top::Category(ChannelId(1)), Top::Category(ChannelId(2)), Top::Category(ChannelId(3))]);
        assert!(moved_top(&items, Top::Category(ChannelId(1)), Top::Category(ChannelId(1)), true).is_none());
        assert!(moved_top(&items, Top::Category(ChannelId(1)), Top::Category(ChannelId(9)), true).is_none());
    }

    // ------------------------------------------ the plan: ONE interleaved top level (v1.6)

    #[test]
    fn a_category_sits_between_two_standalone_channels() {
        // The complaint this fixes: the top level was two lists (every loose channel, then every
        // category), so a category could never be drawn between two channels — no matter what the
        // server said. One shared position sequence, one list.
        let plan = fixture();
        assert_eq!(top_ids(&plan), vec![1, 90, 2, 100, 5]);
        assert_eq!(flat_ids(&plan), vec![1, 3, 4, 6, 2, 5], "a ordem desenhada, de cima a baixo");
    }

    #[test]
    fn text_comes_before_voice_inside_a_category() {
        let plan = fixture();
        match plan.top.iter().find(|it| it.key() == Top::Category(ChannelId(90))) {
            Some(TopItem::Category { text, voice, .. }) => {
                let text: Vec<i64> = text.iter().map(|c| c.id.into_inner()).collect();
                let voice: Vec<i64> = voice.iter().map(|c| c.id.into_inner()).collect();
                assert_eq!(text, vec![3, 4]);
                assert_eq!(voice, vec![6]);
            }
            _ => panic!("a categoria 90 deveria estar no nível de cima"),
        }
    }

    #[test]
    fn a_category_moves_next_to_a_standalone_channel() {
        // The gesture that was impossible: drag the category 90 past channel 2, the empty
        // category 100 and channel 5 — it lands after the voice room, still in one list.
        let items = fixture().top;
        let moved = moved_top(&items, Top::Category(ChannelId(90)), Top::Channel(ChannelId(5)), true).expect("movimento aceito");
        assert_eq!(top_ids(&SidebarPlan { top: moved }), vec![1, 2, 100, 5, 90]);
    }

    #[test]
    fn a_category_dropped_where_it_already_is_changes_nothing() {
        let items = fixture().top;
        let moved = moved_top(&items, Top::Category(ChannelId(90)), Top::Channel(ChannelId(2)), false).expect("movimento aceito");
        assert_eq!(top_ids(&SidebarPlan { top: moved }), vec![1, 90, 2, 100, 5], "antes do 2 já é onde ela está");
    }

    #[test]
    fn moving_a_category_over_itself_is_refused() {
        let items = fixture().top;
        assert!(moved_top(&items, Top::Category(ChannelId(90)), Top::Category(ChannelId(90)), true).is_none());
        assert!(moved_top(&items, Top::Category(ChannelId(90)), Top::Channel(ChannelId(999)), true).is_none());
    }

    #[test]
    fn dropping_a_channel_on_a_category_header_makes_it_standalone_there() {
        // Channel 3 lives inside the category 90; dropped on the header of 100 it leaves the group
        // and takes the slot next to that header — between the empty category and channel 5. This
        // is the gesture the old two-list plan could not express at all.
        let mut flat = interleaved();
        for c in flat.iter_mut().filter(|c| c.id == ChannelId(3)) {
            c.category_id = None;
        }
        let plan = dropped(&flat, ChannelId(3), Top::Category(ChannelId(100)), true);
        assert_eq!(top_ids(&plan), vec![1, 90, 2, 100, 3, 5]);
        let (_, groups) = sent(&plan.request());
        assert_eq!(groups, vec![(90, vec![4, 6]), (100, vec![])], "o 3 saiu do grupo e a categoria segue no lote");
    }

    #[test]
    fn a_swap_between_two_standalone_channels_changes_the_top_level() {
        // Channels 1 and 2 trade places: both are top-level slots, so the top level changes with
        // them — not only the group they are in.
        let (order, _) = moved_order(&interleaved(), ChannelId(1), ChannelId(2), true).expect("movimento aceito");
        let plan = dropped(&rebuilt(&order), ChannelId(1), Top::Channel(ChannelId(2)), true);
        assert_eq!(top_ids(&plan), vec![2, 90, 1, 100, 5], "os dois canais trocaram de lugar no nível de cima");
        let (_, groups) = sent(&plan.request());
        assert_eq!(groups, vec![(90, vec![3, 4, 6]), (100, vec![])], "e as categorias ficaram onde estavam");
    }

    // ------------------------------------------------------------ the one-batch structure

    fn cats(a: i64, b: i64) -> Vec<ChannelCategory> {
        vec![
            ChannelCategory::new(ChannelId(a), "a", "#5865F2", 0),
            ChannelCategory::new(ChannelId(b), "b", "#5865F2", 1),
        ]
    }

    /// What the wire says, as (top level, groups).
    fn sent(req: &vox_proto::StructureReorderRequest) -> (Vec<i64>, Vec<(i64, Vec<i64>)>) {
        (req.top.iter().map(|i| i.id.into_inner()).collect(), req.groups.iter().map(|g| (g.category_id.into_inner(), g.ids.iter().map(|i| i.into_inner()).collect())).collect())
    }

    #[test]
    fn the_batch_carries_the_whole_sidebar() {
        // Channels 1, 2 and 5 standalone (text and voice), 3, 4 and 6 in the category 90; the
        // category 100 has none. Everything exactly once, the empty category included.
        let req = fixture().request();
        let (top, groups) = sent(&req);
        assert_eq!(top, vec![1, 90, 2, 100, 5], "a ordem do desenho, a categoria no meio dos canais");
        assert_eq!(groups, vec![(90, vec![3, 4, 6]), (100, vec![])], "uma categoria vazia entra vazia, não falta");
    }

    #[test]
    fn a_channel_that_crossed_groups_lands_in_the_new_one() {
        // Channel 1 (standalone) dropped on channel 3 (inside the category 90): it belongs to 90
        // now, and the batch says so — out of the top level and inside the new group, in the slot
        // where it was dropped.
        let (order, category) = moved_order(&interleaved(), ChannelId(1), ChannelId(3), false).expect("movimento aceito");
        assert_eq!(category, Some(ChannelId(90)));
        let mut flat = rebuilt(&order);
        for c in flat.iter_mut().filter(|c| c.id == ChannelId(1)) {
            c.category_id = category;
        }
        let plan = dropped(&flat, ChannelId(1), Top::Channel(ChannelId(3)), false);
        assert_eq!(top_ids(&plan), vec![90, 2, 100, 5], "o canal 1 não é mais solto");
        let (_, groups) = sent(&plan.request());
        assert_eq!(groups, vec![(90, vec![1, 3, 4, 6]), (100, vec![])], "e caiu no lugar onde foi solto");
    }

    #[test]
    fn the_positions_are_densest_first() {
        let req = fixture().request();
        let positions: Vec<i32> = req.top.iter().map(|i| i.position).collect();
        assert_eq!(positions, vec![0, 1, 2, 3, 4], "o hub escreve o que recebe: 0, 1, 2, 3, 4");
    }

    #[test]
    fn a_channel_whose_category_is_gone_counts_as_loose() {
        // A category just deleted: the hub already left its channels standalone, but the
        // `ChannelsChanged` broadcast has not landed, so channels 3 and 4 still say "category 90"
        // and 90 is no longer in the list. Left out of both lists they would drop out of the batch
        // and the hub would refuse the whole thing for missing a channel.
        let plan = plan_of(&interleaved(), &cats_at(&[(100, 7), (101, 8)]));
        assert_eq!(top_ids(&plan), vec![1, 2, 3, 4, 5, 6, 100, 101], "os órfãos entram como soltos, na ordem em que estavam");
        let (_, groups) = sent(&plan.request());
        assert_eq!(groups, vec![(100, vec![]), (101, vec![])], "e não pertencem a grupo nenhum");
    }

    // ------------------------------------------- a closed category keeps its unread count visible

    fn counters(unread: &[(i64, u32)], mentions: &[(i64, u32)]) -> (HashMap<ChannelId, u32>, HashMap<ChannelId, u32>) {
        (unread.iter().map(|(k, v)| (ChannelId(*k), *v)).collect(), mentions.iter().map(|(k, v)| (ChannelId(*k), *v)).collect())
    }

    /// Only the category's own text channels (3, 4 — 5 is loose, 6 is a voice room).
    fn inside() -> Vec<vox_proto::Channel> {
        flat().into_iter().filter(|c| c.category_id == Some(ChannelId(90)) && c.kind == ChannelKind::Text).collect()
    }

    #[test]
    fn a_quiet_closed_category_shows_nothing() {
        let (u, m) = counters(&[], &[]);
        assert_eq!(collapsed_counts_of(ChannelId(90), &inside(), &u, &m), None);
    }

    #[test]
    fn a_closed_category_counts_the_mentions_inside_it() {
        let (u, m) = counters(&[(3, 2)], &[(3, 1), (4, 3)]);
        assert_eq!(collapsed_counts_of(ChannelId(90), &inside(), &u, &m), Some(CollapsedBadge { mentions: 4, unread: true }));
    }

    #[test]
    fn a_plain_unread_brightens_the_name_without_a_pill() {
        let (u, m) = counters(&[(3, 5)], &[]);
        assert_eq!(collapsed_counts_of(ChannelId(90), &inside(), &u, &m), Some(CollapsedBadge { mentions: 0, unread: true }));
    }

    #[test]
    fn a_channel_of_another_category_is_not_counted() {
        // Channel 1 and 2 are loose; their unread never leaks into the category's badge.
        let (u, m) = counters(&[(1, 9)], &[(2, 9)]);
        assert_eq!(collapsed_counts_of(ChannelId(90), &inside(), &u, &m), None);
    }

    #[test]
    fn an_empty_category_has_no_badge() {
        let (u, m) = counters(&[], &[]);
        assert_eq!(collapsed_counts_of(ChannelId(100), &inside(), &u, &m), None, "categoria sem canal: nada a contar");
    }

    #[test]
    fn the_name_is_bright_only_when_something_is_waiting() {
        let own = Color32::from_rgb(0x58, 0x65, 0xF2);
        assert_eq!(badge_name_color(None, own), own, "tudo lido: a cor da categoria");
        assert_eq!(badge_name_color(Some(CollapsedBadge { mentions: 0, unread: true }), own), theme::TEXTO);
        assert_eq!(badge_name_color(Some(CollapsedBadge { mentions: 2, unread: true }), own), theme::TEXTO);
    }
}

