//! The call stage's layout (v1.0, a child module of `call`): which cards are on the stage, in
//! what order, and where. Pure functions of plain numbers and rectangles, tested without egui.
//!
//! - **Pins** ("posso clicar em várias telas e ir stackando telas maximizadas"): any number of
//!   cards can be pinned. The pinned ones share the big area in a balanced grid (16:9 kept); the
//!   others go to a strip of small cards below it ([`STRIP_H`] tall), which scrolls one card at a
//!   time with arrows at its ends when they do not all fit. Nothing pinned: one balanced grid of
//!   every card.
//! - **Hidden cards**: a card without video (just the avatar) can be hidden ("Ocultar este card",
//!   or "Ocultar todos sem vídeo", which also covers people who join later). Hidden cards take no
//!   space. A card with video is never hidden this way (the camera has its own "Ocultar câmera
//!   para mim", a stream its "Parar de assistir").
//!
//! [`layout`] is only called again when its [`Input`] changes (the stage keeps the last result).

use std::collections::HashSet;

use eframe::egui::{Pos2, Rect, Vec2};
use vox_proto::UserId;

/// Space between cards (points).
pub const GAP: f32 = 10.0;
/// Cards are 16:9.
pub const ASPECT: f32 = 16.0 / 9.0;
/// Height of the strip of unpinned cards under the pinned ones.
pub const STRIP_H: f32 = 112.0;
/// Width of the strip's scroll arrows.
pub const ARROW_W: f32 = 28.0;
/// The strip never takes more than this share of the stage's height (small windows).
const STRIP_MAX_SHARE: f32 = 0.35;

/// One card of the stage: a person (avatar or camera) or a person's stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TileKey {
    Member(UserId),
    Stream(UserId),
}

impl TileKey {
    pub fn user(self) -> UserId {
        match self {
            TileKey::Member(u) | TileKey::Stream(u) => u,
        }
    }
}

/// The cards hidden by the user (only cards without video).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Hidden {
    /// "Ocultar este card", by person.
    pub cards: HashSet<UserId>,
    /// "Ocultar todos sem vídeo" (people who join later included).
    pub all_without_video: bool,
}

impl Hidden {
    /// May this card be hidden? Only a person's card without video.
    pub fn can_hide(key: TileKey, has_video: bool) -> bool {
        matches!(key, TileKey::Member(_)) && !has_video
    }

    /// Is this card hidden right now? (A hidden person whose camera comes on shows again.)
    pub fn hides(&self, key: TileKey, has_video: bool) -> bool {
        Self::can_hide(key, has_video) && (self.all_without_video || self.cards.contains(&key.user()))
    }

    /// "Mostrar ocultos".
    pub fn clear(&mut self) {
        self.cards.clear();
        self.all_without_video = false;
    }
}

/// Split the cards (in their natural order, each with "has video") into the pinned ones (in pin
/// order) and the others, leaving out the hidden ones. Fills `pinned` and `rest` with indexes
/// into `cards` (cleared first, so the same vectors serve every frame) and returns how many cards
/// are hidden.
pub fn arrange(cards: &[(TileKey, bool)], pins: &[TileKey], hidden: &Hidden, pinned: &mut Vec<usize>, rest: &mut Vec<usize>) -> usize {
    pinned.clear();
    rest.clear();
    let mut n_hidden = 0;
    for p in pins {
        if let Some(i) = cards.iter().position(|(k, _)| k == p) {
            if !hidden.hides(cards[i].0, cards[i].1) && !pinned.contains(&i) {
                pinned.push(i);
            }
        }
    }
    for (i, (k, video)) in cards.iter().enumerate() {
        if hidden.hides(*k, *video) {
            n_hidden += 1;
        } else if !pinned.contains(&i) {
            rest.push(i);
        }
    }
    n_hidden
}

/// What the layout depends on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Input {
    /// Pinned cards on the stage.
    pub pinned: usize,
    /// The other cards on the stage.
    pub rest: usize,
    /// The area for the cards.
    pub area: Rect,
    /// The first card shown in the strip (scrolled), clamped by [`layout`].
    pub strip_first: usize,
}

/// Where everything goes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Layout {
    /// The pinned cards in pin order, or every card when nothing is pinned.
    pub main: Vec<Rect>,
    /// With pins: one entry per other card, its rect in the strip, `None` when scrolled out.
    pub strip: Vec<Option<Rect>>,
    /// The strip's band (the whole width under the pinned cards).
    pub band: Option<Rect>,
    /// The scroll arrows, when the strip does not fit.
    pub prev: Option<Rect>,
    pub next: Option<Rect>,
    /// The first strip card shown (`Input::strip_first`, clamped) and how many are shown.
    pub strip_first: usize,
    pub strip_shown: usize,
}

impl Layout {
    /// Can the strip scroll back / forward?
    pub fn can_scroll(&self) -> (bool, bool) {
        (self.strip_first > 0, self.strip_first + self.strip_shown < self.strip.len())
    }
}

/// Lay out `n` cards of `aspect` (w/h) in `area` as large as possible: the column count that
/// gives the biggest cards wins, and the block is centred.
pub fn grid_layout(n: usize, area: Rect, gap: f32, aspect: f32) -> Vec<Rect> {
    if n == 0 || area.width() <= 0.0 || area.height() <= 0.0 {
        return Vec::new();
    }
    let mut best = (0.0f32, 1usize);
    for cols in 1..=n {
        let rows = n.div_ceil(cols);
        let w_by_width = (area.width() - gap * (cols as f32 - 1.0)) / cols as f32;
        let h_by_height = (area.height() - gap * (rows as f32 - 1.0)) / rows as f32;
        let w = w_by_width.min(h_by_height * aspect);
        if w > best.0 {
            best = (w, cols);
        }
    }
    let (w, cols) = best;
    let w = w.max(1.0);
    let h = w / aspect;
    let rows = n.div_ceil(cols);
    let block_h = rows as f32 * h + (rows as f32 - 1.0) * gap;
    let top = area.top() + (area.height() - block_h) / 2.0;
    let mut out = Vec::with_capacity(n);
    for r in 0..rows {
        let in_row = (n - r * cols).min(cols);
        let row_w = in_row as f32 * w + (in_row as f32 - 1.0) * gap;
        let left = area.left() + (area.width() - row_w) / 2.0;
        for c in 0..in_row {
            let min = Pos2::new(left + c as f32 * (w + gap), top + r as f32 * (h + gap));
            out.push(Rect::from_min_size(min, Vec2::new(w, h)));
        }
    }
    out
}

/// How many cards `w` wide fit in `space`, with gaps between them.
fn fitting(space: f32, w: f32) -> usize {
    if space < w {
        0
    } else {
        ((space + GAP) / (w + GAP)).floor() as usize
    }
}

/// `n` cards of `size` in a row centred in `band` (vertically too).
fn row(n: usize, band: Rect, size: Vec2) -> impl Iterator<Item = Rect> {
    let row_w = n as f32 * size.x + n.saturating_sub(1) as f32 * GAP;
    let left = band.left() + (band.width() - row_w) / 2.0;
    let top = band.top() + (band.height() - size.y) / 2.0;
    (0..n).map(move |i| Rect::from_min_size(Pos2::new(left + i as f32 * (size.x + GAP), top), size))
}

/// The stage's layout for `i` (see the module docs).
pub fn layout(i: Input) -> Layout {
    let area = i.area;
    if i.pinned == 0 || i.rest == 0 {
        return Layout { main: grid_layout(i.pinned.max(i.rest), area, GAP, ASPECT), ..Default::default() };
    }
    let strip_h = STRIP_H.min((area.height() - GAP) * STRIP_MAX_SHARE).max(1.0);
    let main_area = Rect::from_min_size(area.min, Vec2::new(area.width(), (area.height() - strip_h - GAP).max(1.0)));
    let band = Rect::from_min_size(Pos2::new(area.left(), area.bottom() - strip_h), Vec2::new(area.width(), strip_h));
    let card = Vec2::new(strip_h * ASPECT, strip_h);
    let mut out = Layout { main: grid_layout(i.pinned, main_area, GAP, ASPECT), band: Some(band), ..Default::default() };
    if i.rest <= fitting(band.width(), card.x) {
        out.strip = row(i.rest, band, card).map(Some).collect();
        out.strip_shown = i.rest;
        return out;
    }
    // Too many: arrows at both ends, as many whole cards as fit between them.
    let inner = band.shrink2(Vec2::new(ARROW_W + GAP, 0.0));
    let n = fitting(inner.width(), card.x).max(1);
    // Not even one at full size (a very narrow stage): one card, shrunk to the space.
    let size = if inner.width() < card.x { Vec2::new(inner.width().max(1.0), (inner.width() / ASPECT).max(1.0)) } else { card };
    let first = i.strip_first.min(i.rest - n);
    out.prev = Some(Rect::from_min_size(band.min, Vec2::new(ARROW_W, strip_h)));
    out.next = Some(Rect::from_min_size(Pos2::new(band.right() - ARROW_W, band.top()), Vec2::new(ARROW_W, strip_h)));
    out.strip = vec![None; i.rest];
    for (k, r) in row(n, inner, size).enumerate() {
        out.strip[first + k] = Some(r);
    }
    out.strip_first = first;
    out.strip_shown = n;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(w: f32, h: f32) -> Rect {
        Rect::from_min_size(Pos2::new(10.0, 20.0), Vec2::new(w, h))
    }

    fn input(pinned: usize, rest: usize, a: Rect) -> Input {
        Input { pinned, rest, area: a, strip_first: 0 }
    }

    fn aspect_ok(r: &Rect) -> bool {
        (r.width() / r.height() - ASPECT).abs() < 0.01
    }

    fn no_overlap(rects: &[Rect]) -> bool {
        rects.iter().enumerate().all(|(i, r)| rects[i + 1..].iter().all(|o| !r.shrink(0.01).intersects(*o)))
    }

    const AREAS: [(f32, f32); 6] = [(800.0, 450.0), (600.0, 900.0), (1600.0, 300.0), (1200.0, 700.0), (420.0, 360.0), (1900.0, 1000.0)];

    #[test]
    fn grid_fits_and_does_not_overlap() {
        for n in 1..=12 {
            for (w, h) in [(800.0, 450.0), (600.0, 900.0), (1600.0, 300.0), (200.0, 200.0)] {
                let a = area(w, h);
                let rects = grid_layout(n, a, GAP, ASPECT);
                assert_eq!(rects.len(), n);
                assert!(rects.iter().all(|r| a.expand(0.01).contains_rect(*r)), "n={n} {w}x{h}");
                assert!(rects.iter().all(aspect_ok));
                assert!(no_overlap(&rects), "overlap n={n}");
            }
        }
    }

    #[test]
    fn grid_picks_sensible_columns() {
        // Two cards in a wide area sit side by side; in a tall one, stacked.
        let wide = grid_layout(2, area(1200.0, 400.0), GAP, ASPECT);
        assert_eq!(wide[0].top(), wide[1].top());
        let tall = grid_layout(2, area(400.0, 900.0), GAP, ASPECT);
        assert_eq!(tall[0].left(), tall[1].left());
        // One card fills the width of a 16:9 area.
        let one = grid_layout(1, area(1600.0, 900.0), GAP, ASPECT);
        assert!((one[0].width() - 1600.0).abs() < 0.01);
        assert!(grid_layout(0, area(100.0, 100.0), GAP, ASPECT).is_empty());
    }

    #[test]
    fn nothing_pinned_is_one_balanced_grid_of_every_card() {
        for (w, h) in AREAS {
            for n in 1..=9 {
                let a = area(w, h);
                let l = layout(input(0, n, a));
                assert_eq!(l.main, grid_layout(n, a, GAP, ASPECT));
                assert!(l.strip.is_empty() && l.band.is_none() && l.prev.is_none());
            }
        }
        assert_eq!(layout(input(0, 0, area(800.0, 450.0))), Layout::default());
    }

    #[test]
    fn some_pinned_share_the_big_area_and_the_rest_go_to_the_strip() {
        for (w, h) in AREAS {
            let a = area(w, h);
            for pinned in 1..=4 {
                for rest in 1..=3 {
                    let l = layout(input(pinned, rest, a));
                    let band = l.band.expect("a strip");
                    assert_eq!(l.main.len(), pinned);
                    assert_eq!(l.strip.len(), rest);
                    // The strip is at the bottom, at most STRIP_H tall, and the pinned cards above it.
                    assert!((band.bottom() - a.bottom()).abs() < 0.01 && band.height() <= STRIP_H + 0.01);
                    assert!(l.main.iter().all(|r| r.bottom() <= band.top() - GAP + 0.01 && a.expand(0.01).contains_rect(*r)));
                    let shown: Vec<Rect> = l.strip.iter().flatten().copied().collect();
                    assert!(shown.iter().all(|r| band.expand(0.01).contains_rect(*r)));
                    let mut all = l.main.clone();
                    all.extend(&shown);
                    assert!(no_overlap(&all));
                    // Pinned cards are bigger than the strip's.
                    if h >= 450.0 && pinned <= 2 {
                        assert!(l.main.iter().all(|m| shown.iter().all(|s| m.area() > s.area())), "{w}x{h} {pinned}/{rest}");
                    }
                }
            }
        }
        // The mockup: two pinned side by side over a strip of 112.
        let l = layout(input(2, 2, area(1000.0, 560.0)));
        assert_eq!(l.main[0].top(), l.main[1].top());
        assert!((l.band.unwrap().height() - STRIP_H).abs() < 0.01);
    }

    #[test]
    fn all_pinned_take_the_whole_area_without_a_strip() {
        for (w, h) in AREAS {
            let a = area(w, h);
            for n in 1..=6 {
                let l = layout(input(n, 0, a));
                assert_eq!(l.main, grid_layout(n, a, GAP, ASPECT));
                assert!(l.band.is_none() && l.strip.is_empty());
            }
        }
    }

    #[test]
    fn every_card_keeps_16_by_9() {
        for (w, h) in AREAS {
            for (p, r) in [(0, 5), (1, 1), (2, 3), (3, 0), (1, 12), (5, 2)] {
                let l = layout(input(p, r, area(w, h)));
                assert!(l.main.iter().all(aspect_ok), "main {w}x{h} {p}/{r}");
                assert!(l.strip.iter().flatten().all(aspect_ok), "strip {w}x{h} {p}/{r}");
            }
        }
    }

    #[test]
    fn a_strip_that_does_not_fit_scrolls_one_card_at_a_time() {
        let a = area(900.0, 600.0);
        // 112 * 16/9 = 199 wide: 4 fit in 900 (4 * 199 + 3 * 10 = 826), so 4 need no arrows.
        let l = layout(input(1, 4, a));
        assert!(l.prev.is_none() && l.next.is_none());
        assert_eq!((l.strip_shown, l.can_scroll()), (4, (false, false)));
        // 10 do not: arrows, whole cards between them, the others scrolled out.
        let l = layout(input(1, 10, a));
        let (prev, next) = (l.prev.unwrap(), l.next.unwrap());
        let shown: Vec<(usize, Rect)> = l.strip.iter().enumerate().filter_map(|(i, r)| r.map(|r| (i, r))).collect();
        assert_eq!(l.strip.len(), 10);
        assert_eq!(shown.len(), l.strip_shown);
        assert_eq!(l.strip_shown, 3, "(900 - 2 * 38) fits 3 cards of 199");
        assert_eq!(shown.iter().map(|(i, _)| *i).collect::<Vec<_>>(), vec![0, 1, 2]);
        assert!(shown.iter().all(|(_, r)| r.left() >= prev.right() && r.right() <= next.left()));
        assert_eq!(l.can_scroll(), (false, true));
        // Scrolled: the window moves, and stops at the end.
        let l = layout(Input { strip_first: 4, ..input(1, 10, a) });
        assert_eq!(l.strip_first, 4);
        assert!(l.strip[3].is_none() && l.strip[4].is_some() && l.strip[6].is_some() && l.strip[7].is_none());
        assert_eq!(l.can_scroll(), (true, true));
        let l = layout(Input { strip_first: 99, ..input(1, 10, a) });
        assert_eq!((l.strip_first, l.strip_shown), (7, 3));
        assert_eq!(l.can_scroll(), (true, false));
        assert!(l.strip[9].is_some());
        // A very narrow stage still shows one (shrunk) card between the arrows.
        let l = layout(input(1, 5, area(200.0, 500.0)));
        assert_eq!(l.strip_shown, 1);
        let r = l.strip.iter().flatten().next().unwrap();
        assert!(r.left() >= l.prev.unwrap().right() && r.right() <= l.next.unwrap().left() && aspect_ok(r));
    }

    #[test]
    fn a_short_stage_gets_a_shorter_strip() {
        let a = area(1200.0, 200.0);
        let l = layout(input(1, 2, a));
        let band = l.band.unwrap();
        assert!(band.height() < STRIP_H && band.height() <= (200.0 - GAP) * STRIP_MAX_SHARE + 0.01);
        assert!(l.main[0].height() > band.height());
    }

    fn m(u: UserId) -> TileKey {
        TileKey::Member(u)
    }

    fn s(u: UserId) -> TileKey {
        TileKey::Stream(u)
    }

    #[test]
    fn hide_rules_only_touch_cards_without_video() {
        let mut h = Hidden::default();
        assert!(Hidden::can_hide(m(1), false));
        assert!(!Hidden::can_hide(m(1), true), "a camera card is not hidden this way");
        assert!(!Hidden::can_hide(s(1), false) && !Hidden::can_hide(s(1), true), "nor a stream");
        h.cards.insert(1);
        assert!(h.hides(m(1), false));
        assert!(!h.hides(m(1), true), "their camera came on: the card shows again");
        assert!(!h.hides(s(1), false) && !h.hides(m(2), false));
        h.all_without_video = true;
        assert!(h.hides(m(2), false) && h.hides(m(3), false), "everybody without video, newcomers too");
        assert!(!h.hides(m(2), true) && !h.hides(s(2), false));
        h.clear();
        assert_eq!(h, Hidden::default());
        assert!(!h.hides(m(1), false));
    }

    #[test]
    fn arrange_puts_pins_first_in_pin_order_and_drops_hidden_cards() {
        // Natural order: Ana, Ana's stream, Bia (camera), Caio, Caio's stream.
        let cards = [(m(1), false), (s(1), true), (m(2), true), (m(3), false), (s(3), false)];
        let (mut p, mut r) = (Vec::new(), Vec::new());
        assert_eq!(arrange(&cards, &[], &Hidden::default(), &mut p, &mut r), 0);
        assert!(p.is_empty() && r == vec![0, 1, 2, 3, 4]);
        // Pinned in the order they were pinned; a pin of a card that is not there is ignored.
        assert_eq!(arrange(&cards, &[s(3), m(9), s(1)], &Hidden::default(), &mut p, &mut r), 0);
        assert_eq!((p.clone(), r.clone()), (vec![4, 1], vec![0, 2, 3]));
        // Hidden cards take no space, pinned or not; video cards stay.
        let h = Hidden { cards: [1].into(), all_without_video: false };
        assert_eq!(arrange(&cards, &[m(1)], &h, &mut p, &mut r), 1);
        assert_eq!((p.clone(), r.clone()), (vec![], vec![1, 2, 3, 4]));
        let all = Hidden { all_without_video: true, ..Default::default() };
        assert_eq!(arrange(&cards, &[s(1)], &all, &mut p, &mut r), 2);
        assert_eq!((p.clone(), r.clone()), (vec![1], vec![2, 4]));
        // The same vectors are reused (cleared each time).
        assert_eq!(arrange(&cards, &[], &Hidden::default(), &mut p, &mut r), 0);
        assert_eq!(r.len(), 5);
    }
}
