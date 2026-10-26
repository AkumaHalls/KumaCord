//! Airspace clipping of the inline video windows (a child module of `call`).
//!
//! A video is a native child window, so nothing egui draws can show over it. Instead of hiding a
//! video while a menu, popup or window overlaps it, the overlapped part is cut out of the child
//! window (a window region): the video keeps playing everywhere else and egui's popup shows
//! through the hole. Only a video that is covered entirely (or down to slivers) is hidden.
//!
//! Everything here is plain rectangle math in physical pixels of the app's client area (`Px`,
//! as [`super::to_px`] makes them), so it is tested without a window.

use eframe::egui::Rect;

/// x, y, w, h in physical pixels (the same layout as [`super::to_px`]).
pub type Px = (i32, i32, i32, i32);

/// Points cut around each overlay beyond its rect: the frame stroke, the anti-aliased rounded
/// corners and the points -> pixels rounding, so no video shows over the popup's edge.
pub const PAD: f32 = 3.0;

/// What is left of a covered video counts as visible only with a part at least this big
/// (pixels) both ways; thinner slivers are not worth presenting.
pub const MIN_VISIBLE: i32 = 4;

/// What the overlays do to one video.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Clip {
    /// Nothing over it: the window has no region.
    Whole,
    /// These rects (relative to the video's top-left corner, inside it, sorted) are cut out.
    Cut(Vec<Px>),
    /// Covered: the window is hidden.
    Hidden,
}

impl Clip {
    /// For the log: "unclipped" / "clipped" / "hidden".
    pub fn state(&self) -> &'static str {
        match self {
            Clip::Whole => "unclipped",
            Clip::Cut(_) => "clipped",
            Clip::Hidden => "hidden",
        }
    }
}

/// The pixels an overlay at `r` (points) hides: grown by `pad` points on every side, then
/// rounded outwards, so the cut always covers at least what egui paints.
pub fn cut_px(r: Rect, pad: f32, ppp: f32) -> Px {
    let r = r.expand(pad);
    let x0 = (r.min.x * ppp).floor() as i32;
    let y0 = (r.min.y * ppp).floor() as i32;
    let x1 = (r.max.x * ppp).ceil() as i32;
    let y1 = (r.max.y * ppp).ceil() as i32;
    (x0, y0, (x1 - x0).max(0), (y1 - y0).max(0))
}

/// The common part of two rects (`None` when they do not overlap).
pub fn intersect(a: Px, b: Px) -> Option<Px> {
    let x0 = a.0.max(b.0);
    let y0 = a.1.max(b.1);
    let x1 = (a.0 + a.2).min(b.0 + b.2);
    let y1 = (a.1 + a.3).min(b.1 + b.3);
    (x1 > x0 && y1 > y0).then_some((x0, y0, x1 - x0, y1 - y0))
}

/// `base` minus every rect of `cuts`: disjoint rects that together are what is left.
pub fn subtract(base: Px, cuts: &[Px]) -> Vec<Px> {
    let mut left = if base.2 > 0 && base.3 > 0 { vec![base] } else { Vec::new() };
    for &c in cuts {
        let mut next = Vec::with_capacity(left.len() + 3);
        for r in left {
            let Some((ix, iy, iw, ih)) = intersect(r, c) else {
                next.push(r);
                continue;
            };
            let (rx, ry, rw, rh) = r;
            // The full-width bands above and below the cut, then the parts beside it.
            if iy > ry {
                next.push((rx, ry, rw, iy - ry));
            }
            if iy + ih < ry + rh {
                next.push((rx, iy + ih, rw, ry + rh - (iy + ih)));
            }
            if ix > rx {
                next.push((rx, iy, ix - rx, ih));
            }
            if ix + iw < rx + rw {
                next.push((ix + iw, iy, rx + rw - (ix + iw), ih));
            }
        }
        left = next;
    }
    left
}

/// How a video placed at `video` is shown under `overlays` (both in pixels; the overlays
/// already padded, see [`cut_px`]).
pub fn clip_for(video: Px, overlays: &[Px]) -> Clip {
    let mut cuts: Vec<Px> = overlays.iter().filter_map(|o| intersect(video, *o)).collect();
    if cuts.is_empty() {
        return Clip::Whole;
    }
    if !subtract(video, &cuts).iter().any(|r| r.2 >= MIN_VISIBLE && r.3 >= MIN_VISIBLE) {
        return Clip::Hidden;
    }
    for c in &mut cuts {
        c.0 -= video.0;
        c.1 -= video.1;
    }
    // A stable form, so an unchanged overlay set compares equal frame after frame.
    cuts.sort_unstable();
    cuts.dedup();
    Clip::Cut(cuts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::{pos2, Rect};

    fn area(rects: &[Px]) -> i64 {
        rects.iter().map(|r| r.2 as i64 * r.3 as i64).sum()
    }

    #[test]
    fn subtract_a_rect_from_the_middle_leaves_a_frame() {
        let left = subtract((0, 0, 100, 60), &[(20, 10, 30, 20)]);
        assert_eq!(area(&left), 100 * 60 - 30 * 20);
        // Disjoint, inside the base, none overlapping the cut.
        for (i, r) in left.iter().enumerate() {
            assert!(intersect(*r, (20, 10, 30, 20)).is_none());
            assert_eq!(intersect(*r, (0, 0, 100, 60)), Some(*r));
            for o in &left[i + 1..] {
                assert!(intersect(*r, *o).is_none(), "{r:?} overlaps {o:?}");
            }
        }
    }

    #[test]
    fn subtract_overlapping_cuts_and_cuts_outside() {
        let base = (10, 10, 200, 100);
        let cuts = [(0, 0, 50, 50), (30, 30, 50, 50), (500, 500, 10, 10)];
        let left = subtract(base, &cuts);
        // Count the pixels by hand: base minus the union of the two cuts inside it.
        let mut n = 0i64;
        for y in 10..110 {
            for x in 10..210 {
                let inside = |c: &Px| x >= c.0 && x < c.0 + c.2 && y >= c.1 && y < c.1 + c.3;
                if !cuts.iter().any(inside) {
                    n += 1;
                    assert!(left.iter().any(inside), "pixel {x},{y} lost");
                }
            }
        }
        assert_eq!(area(&left), n);
        assert!(subtract(base, &[base]).is_empty());
        assert_eq!(subtract(base, &[]), vec![base]);
    }

    #[test]
    fn points_to_pixels_with_padding_rounds_outwards() {
        let r = Rect::from_min_max(pos2(100.2, 50.0), pos2(200.0, 80.6));
        // 1.0: grown by 3 points.
        assert_eq!(cut_px(r, PAD, 1.0), (97, 47, 106, 37));
        // 150 %: floor of the start, ceil of the end, in pixels.
        let (x, y, w, h) = cut_px(r, 3.0, 1.5);
        assert_eq!((x, y), ((97.2f32 * 1.5).floor() as i32, 70));
        assert_eq!((x + w, y + h), ((203.0f32 * 1.5).ceil() as i32, (83.6f32 * 1.5).ceil() as i32));
        // 125 % with no padding still covers every pixel egui may paint.
        let (x, y, w, h) = cut_px(r, 0.0, 1.25);
        assert!(x as f32 <= 100.2 * 1.25 && (x + w) as f32 >= 200.0 * 1.25);
        assert!(y as f32 <= 50.0 * 1.25 && (y + h) as f32 >= 80.6 * 1.25);
    }

    #[test]
    fn padded_cut_covers_the_rounded_video_edge() {
        // A popup whose edge sits exactly on the video's: with the pad the cut still reaches
        // 3 points past it, whatever the scale.
        for ppp in [1.0f32, 1.25, 1.5, 1.75, 2.0] {
            let video = super::super::to_px(Rect::from_min_max(pos2(10.0, 10.0), pos2(410.0, 235.0)), ppp);
            let popup = Rect::from_min_max(pos2(300.0, 200.0), pos2(500.0, 400.0));
            let Clip::Cut(c) = clip_for(video, &[cut_px(popup, PAD, ppp)]) else { panic!("expected a cut at {ppp}") };
            assert_eq!(c.len(), 1);
            let (cx, cy, cw, ch) = c[0];
            // Starts at least PAD points before the popup, ends at the video's own edge.
            assert!(((video.0 + cx) as f32) <= (300.0 - PAD) * ppp + 0.01, "{ppp}: {c:?}");
            assert!(((video.1 + cy) as f32) <= (200.0 - PAD) * ppp + 0.01, "{ppp}: {c:?}");
            assert_eq!((cx + cw, cy + ch), (video.2, video.3), "{ppp}");
        }
    }

    #[test]
    fn clip_is_relative_sorted_and_stable() {
        let video = (100, 100, 400, 300);
        assert_eq!(clip_for(video, &[]), Clip::Whole);
        assert_eq!(clip_for(video, &[(0, 0, 50, 50), (600, 600, 10, 10)]), Clip::Whole);
        let a = (450, 350, 100, 100);
        let b = (90, 90, 20, 20);
        let one = clip_for(video, &[a, b]);
        assert_eq!(one, clip_for(video, &[b, a, a]));
        assert_eq!(one, Clip::Cut(vec![(0, 0, 10, 10), (350, 250, 50, 50)]));
        assert_eq!(one.state(), "clipped");
    }

    #[test]
    fn full_cover_hides_and_slivers_do_not_count() {
        let video = (100, 100, 400, 300);
        assert_eq!(clip_for(video, &[(0, 0, 1000, 1000)]), Clip::Hidden);
        assert_eq!(clip_for(video, &[video]), Clip::Hidden);
        // Two halves together cover it.
        assert_eq!(clip_for(video, &[(100, 100, 200, 300), (300, 0, 900, 900)]), Clip::Hidden);
        // A 3-pixel strip left over is not worth showing; a 4-pixel one is.
        assert_eq!(clip_for(video, &[(103, 0, 900, 900)]), Clip::Hidden);
        assert!(matches!(clip_for(video, &[(104, 0, 900, 900)]), Clip::Cut(_)));
        assert_eq!(Clip::Hidden.state(), "hidden");
        assert_eq!(Clip::Whole.state(), "unclipped");
    }
}
