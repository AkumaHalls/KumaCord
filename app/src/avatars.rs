//! Profile pictures ("Foto de perfil") and profile banners in the app.
//!
//! The hub stores every picture — an avatar as a 64 px and a 256 px square, a banner as one
//! 600x240 — and puts its version (a hash) on the person's `User`. This module
//! - keeps the pictures of the people we see: [`AvatarStore`] and [`BannerStore`] — the same
//!   machine ([`Store`]) over two wire paths, so there is one set of rules, not two — decide what
//!   to ask the hub for, remember the answers by `(person, version, size)` and keep the number of
//!   egui textures bounded. They know nothing about the network: [`Store::take_requests`] hands
//!   out what should be asked and the app feeds the answers back with [`Store::on_image`], so the
//!   rules (queueing, 404 = keep the initial, retries, texture freeing) are unit-testable;
//! - draws them as circles ([`draw_avatar`], [`avatar_widget`]), falling back to the coloured
//!   initial while a picture loads, when there is none, and on a hub that predates pictures;
//! - draws a banner as the band on top of a profile card ([`draw_banner`]), falling back to the
//!   flat colour it always was;
//! - plays a picture the hub kept whole (a GIF, an animated WebP, an APNG) frame by frame, the way
//!   the image viewer does — see [`decode_frames`];
//! - is the "Foto de perfil" and "Banner do perfil" sections of the settings: a native file
//!   dialog, a preview of the centre crop, "Salvar" and "Remover".

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{mpsc, Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use eframe::egui::{self, epaint, Align2, Color32, FontId, Pos2, Rect, Sense, TextureHandle, TextureId, TextureOptions, Vec2};
use vox_client_core::hub::{AvatarReply, HubClient, MAX_AVATAR_IMAGE_BYTES};
use vox_proto::{ApiError, User, UserId, AVATAR_MAX_UPLOAD_BYTES};

use crate::app::{name_color, App};
use crate::backend::Cmd;

/// Requests in flight at once.
const MAX_IN_FLIGHT: usize = 4;
/// After a failed download, ask again no sooner than this (and only if the person is on screen).
const RETRY_AFTER: Duration = Duration::from_secs(30);
/// A texture nobody drew for this long is freed (the downloaded bytes stay, a few KB).
const TEXTURE_IDLE: Duration = Duration::from_secs(60);
/// People with a texture alive at once, whatever is on screen. One person's animation is many
/// textures, so people are counted here and frames are counted on their own by [`MAX_FRAMES`]:
/// a 20-frame GIF must not crowd nineteen other faces off the GPU.
pub const MAX_TEXTURES: usize = 48;
/// Animation frames alive at once across every person (240 x 128² x 4 B is ~15 MB of texture RAM).
pub const MAX_FRAMES: usize = 240;
/// Downloaded pictures remembered per session; the least recently seen go first.
pub const MAX_ENTRIES: usize = 160;
/// Textures decoded per frame, so a screen full of new faces doesn't hitch.
const DECODES_PER_FRAME: u32 = 4;
/// The hub promises 64 / 256 px squares; a decoded picture bigger than this is refused.
const MAX_SIDE: u32 = 512;
/// An animated frame is shrunk to at most this before it becomes a texture: an avatar is drawn at
/// ~68 pt and the hub sends a 256² square, so 128² is visually identical and cuts the texture RAM
/// (and the GPU upload) 4x. A banner is wide, so it keeps its 600x240.
const ANIM_SIDE: u32 = 128;
/// An animated original (a GIF, an animated WebP) is usually bigger than the 512 KB a still may
/// be, so the `?anim=1` answer gets its own — still bounded — cap.
pub const MAX_ANIM_IMAGE_BYTES: usize = 8 * 1024 * 1024;
/// And the ceiling an animated original's canvas may declare before it is decoded at all. It is
/// NOT the decompression-bomb line on its own: every frame is decoded into a whole-canvas RGBA
/// buffer, so 240 frames at this ceiling is ~16 GB of decoded bytes regardless of how small the
/// file is (240 sub-rectangle GIF frames cost ~5.5 KB on the wire). What actually contains the
/// bomb is [`MAX_ANIM_DECODED`], the aggregate budget counted as the frames come off the
/// iterator, plus `set_limits` so the decoder reserves against a budget instead of nothing.
const MAX_ANIM_SIDE: u32 = 4096;
/// Decoded RGBA bytes one animation may cost before [`decode_frames`] stops reading frames (the
/// same aggregate line `chat_images::decode_full` uses, at the viewer's budget). 384 MB is 6x
/// what a legitimate animation needs — 240 frames at 128² is ~15 MB of texture — so it only ever
/// bites on something that was never going to be drawn.
const MAX_ANIM_DECODED: u64 = 384 * 1024 * 1024;

// ------------------------------------------------------------------ the store

/// Which picture a [`Store`] keeps. The two kinds are the same machine over different wire paths:
/// only the route, the `User` field that carries the version, the stored sizes and the biggest an
/// animated frame may be differ, so a kind is what a store is built with.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum Kind {
    /// The profile picture: 64 and 256 px squares, drawn as a circle.
    #[default]
    Avatar,
    /// The profile banner: one 600x240 picture behind the top of a card.
    Banner,
}

impl Kind {
    /// The sizes (px) the hub stores this kind at.
    pub const fn sizes(self) -> &'static [u32] {
        match self {
            Kind::Avatar => &[64, 256],
            Kind::Banner => &[600],
        }
    }

    /// The path under `api/users/{id}/`.
    pub const fn path(self) -> &'static str {
        match self {
            Kind::Avatar => "avatar",
            Kind::Banner => "banner",
        }
    }

    /// The path under `api/me/` (the upload and the removal).
    pub const fn upload_path(self) -> &'static str {
        match self {
            Kind::Avatar => "avatar",
            Kind::Banner => "banner",
        }
    }

    /// The field of an upload's answer that carries the new version.
    pub const fn version_field(self) -> &'static str {
        match self {
            Kind::Avatar => "avatar",
            Kind::Banner => "banner",
        }
    }

    /// The version string on the person's `User` (`None` = they have no such picture).
    pub fn version(self, user: &User) -> Option<&str> {
        match self {
            Kind::Avatar => user.avatar.as_deref(),
            Kind::Banner => user.banner.as_deref(),
        }
    }

    /// Did the hub keep this picture whole (a GIF / animated WebP / APNG)? Only then is there an
    /// `?anim=1` answer to ask for.
    pub fn animated(self, user: &User) -> bool {
        match self {
            Kind::Avatar => user.avatar_animated,
            Kind::Banner => user.banner_animated,
        }
    }

    /// The biggest an animated frame may be (width, height) before it becomes a texture.
    pub const fn frame_cap(self) -> (u32, u32) {
        match self {
            Kind::Avatar => (ANIM_SIDE, ANIM_SIDE),
            Kind::Banner => (600, 240),
        }
    }

    /// The biggest a *still* of this kind may come back as (width, height). The hub renders an
    /// avatar at 64 / 256, so 512 is already generous; a banner comes back 600 wide and its height
    /// is whatever the picture the person uploaded had — the crop happens when it is drawn.
    pub const fn still_cap(self) -> (u32, u32) {
        match self {
            Kind::Avatar => (MAX_SIDE, MAX_SIDE),
            Kind::Banner => (600, 1024),
        }
    }

    /// The one size to ask for when there is no diameter to fit (the banner's).
    pub const fn only(self) -> Class {
        Class(self.sizes()[0])
    }

    /// What the settings call it.
    pub const fn label(self) -> &'static str {
        match self {
            Kind::Avatar => "foto de perfil",
            Kind::Banner => "banner",
        }
    }
}

/// Which of a kind's stored sizes to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Class(u32);

impl Class {
    /// The 64 px square: a list row, a message author, the Calladex.
    pub const SMALL: Class = Class(64);
    /// The 256 px square: a call tile, the settings section.
    pub const BIG: Class = Class(256);

    /// The size (px) the hub is asked for.
    pub fn px(self) -> u32 {
        self.0
    }

    /// The 64 px picture is enough up to 64 physical pixels; beyond that the 256 px one.
    pub fn for_diameter(points: f32, pixels_per_point: f32) -> Class {
        Class(if points * pixels_per_point > 64.0 { 256 } else { 64 })
    }
}

/// What the store wants asked of the hub.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub user: UserId,
    pub version: String,
    pub class: Class,
    /// Which picture this is (the answer has to reach the right store).
    pub kind: Kind,
    /// Ask for the animated original (`?anim=1`) instead of the still the hub renders.
    pub anim: bool,
}

enum State {
    /// Asked, no answer yet.
    Loading,
    /// The downloaded picture, not decoded yet (or its texture was freed).
    Bytes(Vec<u8>),
    /// `404`: no such picture (old hub, removed, stale version). Final for this version.
    NotFound,
    /// Failed or busy; asked again once `at` has passed and the person is drawn again.
    Retry { at: Instant },
    /// Downloaded but not a usable picture: the initial stays.
    Broken,
}

struct Entry {
    version: String,
    class: Class,
    state: State,
    /// The decoded frames as textures: exactly one for a still, many for an animation.
    frames: Vec<TextureHandle>,
    /// Each frame's delay in ms (the 20 ms floor already applied).
    delays: Vec<u32>,
    /// Which frame is showing, and since when (an animation only).
    frame: usize,
    frame_at: Instant,
    last_seen: Instant,
}

impl Entry {
    fn new(version: &str, class: Class, now: Instant) -> Self {
        Entry { version: version.to_string(), class, state: State::Loading, frames: Vec::new(), delays: Vec::new(), frame: 0, frame_at: now, last_seen: now }
    }

    /// Forget the textures (the downloaded bytes stay): nothing is on the GPU until it is drawn
    /// again.
    fn drop_textures(&mut self) {
        self.frames.clear();
        self.delays.clear();
        self.frame = 0;
    }
}

/// The machine both stores share: what to ask the hub, what to remember, what to free.
pub struct Store {
    kind: Kind,
    entries: HashMap<UserId, Vec<Entry>>,
    /// Pictures (not initials) drawn since the last [`Store::clear_drawn`].
    drawn: HashSet<(UserId, Surface)>,
    queue: VecDeque<Request>,
    in_flight: usize,
    decodes_left: u32,
    last_sweep: Option<Instant>,
}

/// The profile pictures ("Foto de perfil"). A newtype over [`Store`] so the app can hold one of
/// each kind: `Deref` keeps every call site (`texture`, `loaded`, `reset`, …) unchanged.
pub struct AvatarStore(Store);

impl Default for AvatarStore {
    fn default() -> Self {
        AvatarStore(Store::new(Kind::Avatar))
    }
}

impl std::ops::Deref for AvatarStore {
    type Target = Store;
    fn deref(&self) -> &Store {
        &self.0
    }
}

impl std::ops::DerefMut for AvatarStore {
    fn deref_mut(&mut self) -> &mut Store {
        &mut self.0
    }
}

/// Where a picture is drawn (the autotest checks that a picture really reached each of them).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Surface {
    /// A person's tile in the call view.
    Tile,
    /// The voice members under a voice channel (sidebar).
    Sidebar,
    /// The "ONLINE" list.
    Online,
    /// The author of a chat message group.
    Chat,
    /// The settings section.
    Settings,
    /// The big picture on a profile card.
    Popout,
}

impl Store {
    fn new(kind: Kind) -> Self {
        Store { kind, entries: HashMap::new(), drawn: HashSet::new(), queue: VecDeque::new(), in_flight: 0, decodes_left: DECODES_PER_FRAME, last_sweep: None }
    }

    /// A new hub session: forget everything (another server has other pictures).
    pub fn reset(&mut self) {
        *self = Store::new(self.kind);
    }

    /// Call once per frame: refills the per-frame decode budget and, now and then, sweeps.
    pub fn begin_frame(&mut self, now: Instant) {
        self.decodes_left = DECODES_PER_FRAME;
        if self.last_sweep.is_none_or(|t| now.duration_since(t) >= Duration::from_secs(1)) {
            self.last_sweep = Some(now);
            self.sweep(now);
        }
    }

    /// The texture to draw for `user` at `class`, if their picture is ready; `None` means "draw
    /// the initial". Marks the person as seen and makes sure the picture gets asked for. Cheap
    /// to call every frame for everybody on screen.
    pub fn texture(&mut self, ctx: &egui::Context, user: &User, class: Class, now: Instant) -> Option<TextureId> {
        let version = self.kind.version(user)?;
        let anim = self.kind.animated(user);
        let list = self.entries.entry(user.id).or_default();
        let idx = match list.iter().position(|e| e.version == version && e.class == class) {
            Some(i) => i,
            None => {
                list.push(Entry::new(version, class, now));
                self.queue.push_back(Request { user: user.id, version: version.to_string(), class, kind: self.kind, anim });
                list.len() - 1
            }
        };
        let e = &mut list[idx];
        e.last_seen = now;
        if matches!(e.state, State::Retry { at } if now >= at) {
            e.state = State::Loading;
            self.queue.push_back(Request { user: user.id, version: version.to_string(), class, kind: self.kind, anim });
        }
        if e.frames.is_empty() && matches!(e.state, State::Bytes(_)) {
            if self.decodes_left > 0 {
                self.decodes_left -= 1;
                // An animation decodes into every frame at once; anything that is not one (or a
                // budget that left a single frame) goes through the still path, unchanged.
                let decoded = match &e.state {
                    State::Bytes(b) => decode_frames(b, self.kind),
                    _ => None,
                };
                match decoded {
                    Some((images, delays)) => {
                        let name = format!("{}-{}-{}", self.kind.path(), user.id, e.version);
                        e.frames = images
                            .into_iter()
                            .enumerate()
                            .map(|(i, img)| ctx.load_texture(format!("{name}-{i}"), img, TextureOptions::LINEAR))
                            .collect();
                        e.delays = delays;
                        if e.frames.len() > 1 {
                            e.frame = 0;
                            e.frame_at = now;
                        }
                    }
                    None => match &e.state {
                        State::Bytes(b) => match decode_still(b, self.kind) {
                            Some(img) => {
                                let name = format!("{}-{}-{}", self.kind.path(), user.id, e.version);
                                e.frames = vec![ctx.load_texture(name, img, TextureOptions::LINEAR)];
                            }
                            None => e.state = State::Broken,
                        },
                        _ => e.state = State::Broken,
                    },
                }
            } else {
                ctx.request_repaint(); // out of decode budget this frame: the rest next frame
            }
        }
        // (The entry's borrow ends here: advancing an animation needs the whole store no more, but
        // it does need `ctx`, which is not part of it.)
        if let Some(tex) = self.entry_frame(ctx, user.id, class, now) {
            return Some(tex);
        }
        // The other size of the same picture will do until this one arrives.
        let list = self.entries.get_mut(&user.id)?;
        for px in self.kind.sizes() {
            if *px == class.px() {
                continue;
            }
            if let Some(e) = list.iter_mut().find(|e| e.version == version && e.class.px() == *px) {
                e.last_seen = now;
                return frame_now(e, ctx, now);
            }
        }
        None
    }

    /// The frame `user`'s picture of `class` is showing now, if it has any.
    fn entry_frame(&mut self, ctx: &egui::Context, user: UserId, class: Class, now: Instant) -> Option<TextureId> {
        let e = self.entries.get_mut(&user)?.iter_mut().find(|e| e.class == class)?;
        frame_now(e, ctx, now)
    }

    /// `user`'s current picture if it is already on the GPU (any size); never asks the hub for it
    /// and marks nothing as seen (the Calladex shows what the app already has, nothing more).
    pub fn loaded(&self, user: &User) -> Option<TextureId> {
        let version = self.kind.version(user)?;
        self.entries.get(&user.id)?.iter().filter(|e| e.version == version).find_map(|e| e.frames.first().map(TextureHandle::id))
    }

    /// The pixel size of the picture already on the GPU (a banner is cropped to the card's band).
    pub fn size(&self, user: &User) -> Option<[usize; 2]> {
        let version = self.kind.version(user)?;
        let e = self.entries.get(&user.id)?.iter().find(|e| e.version == version)?;
        e.frames.first().map(TextureHandle::size)
    }

    /// What to ask the hub right now (at most [`MAX_IN_FLIGHT`] outstanding in total).
    pub fn take_requests(&mut self) -> Vec<Request> {
        let mut out = Vec::new();
        while self.in_flight < MAX_IN_FLIGHT {
            let Some(r) = self.queue.pop_front() else { break };
            self.in_flight += 1;
            out.push(r);
        }
        out
    }

    /// The hub's answer to a [`Request`].
    pub fn on_image(&mut self, user: UserId, version: &str, class: Class, reply: AvatarReply, now: Instant) {
        self.in_flight = self.in_flight.saturating_sub(1);
        let Some(e) = self
            .entries
            .get_mut(&user)
            .and_then(|l| l.iter_mut().find(|e| e.version == version && e.class == class))
            .filter(|e| matches!(e.state, State::Loading))
        else {
            return;
        };
        e.state = match reply {
            AvatarReply::Image(bytes) => State::Bytes(bytes),
            AvatarReply::NotFound => State::NotFound,
            AvatarReply::Failed(why) => {
                tracing::debug!(%user, size = class.px(), why, "picture download failed");
                State::Retry { at: now + RETRY_AFTER }
            }
        };
    }

    /// Housekeeping: free textures nobody drew for a while, keep at most [`MAX_TEXTURES`] people
    /// and [`MAX_FRAMES`] frames on the GPU, and trim the store to [`MAX_ENTRIES`] (what is being
    /// downloaded stays).
    pub fn sweep(&mut self, now: Instant) {
        for e in self.entries.values_mut().flatten() {
            if !e.frames.is_empty() && now.duration_since(e.last_seen) > TEXTURE_IDLE {
                e.drop_textures();
            }
        }
        // An animation is many textures for one person, so the two budgets are separate: people
        // first (everybody on screen gets a face), then the frames those people cost.
        let mut people = self.people_with_textures();
        if people.len() > MAX_TEXTURES {
            let excess = people.len() - MAX_TEXTURES;
            for (_, u) in people.drain(..excess) {
                if let Some(l) = self.entries.get_mut(&u) {
                    for e in l.iter_mut() {
                        e.drop_textures();
                    }
                }
            }
        }
        let mut frames: usize = self.entries.values().flatten().map(|e| e.frames.len()).sum();
        if frames > MAX_FRAMES {
            // One animation can eat the whole frame budget, so drop the whole person, most
            // recently seen last: it comes back the next time they are drawn.
            for (_, u) in people {
                if frames <= MAX_FRAMES {
                    break;
                }
                if let Some(l) = self.entries.get_mut(&u) {
                    for e in l.iter_mut() {
                        frames = frames.saturating_sub(e.frames.len());
                        e.drop_textures();
                    }
                }
            }
        }
        let total: usize = self.entries.values().map(Vec::len).sum();
        if total > MAX_ENTRIES {
            let mut idle: Vec<(Instant, UserId, String, Class)> = self
                .entries
                .iter()
                .flat_map(|(u, l)| l.iter().filter(|e| !matches!(e.state, State::Loading)).map(|e| (e.last_seen, *u, e.version.clone(), e.class)))
                .collect();
            idle.sort();
            for (_, u, v, c) in idle.into_iter().take(total - MAX_ENTRIES) {
                if let Some(l) = self.entries.get_mut(&u) {
                    l.retain(|e| !(e.version == v && e.class == c));
                }
            }
        }
        self.entries.retain(|_, l| !l.is_empty());
    }

    /// Who has a texture, with the moment their oldest textured picture was last drawn (the least
    /// recently seen first is the order textures are freed in).
    fn people_with_textures(&self) -> Vec<(Instant, UserId)> {
        let mut out: Vec<(Instant, UserId)> = self
            .entries
            .iter()
            .filter_map(|(u, l)| l.iter().filter(|e| !e.frames.is_empty()).map(|e| e.last_seen).min().map(|t| (t, *u)))
            .collect();
        out.sort();
        out
    }

    /// Was `user`'s real picture (not the initial) drawn on `surface` since [`Self::clear_drawn`]?
    pub fn was_drawn(&self, user: UserId, surface: Surface) -> bool {
        self.drawn.contains(&(user, surface))
    }

    pub fn clear_drawn(&mut self) {
        self.drawn.clear();
    }

    /// (pictures remembered, people with a texture) for the autotest log.
    pub fn stats(&self) -> (usize, usize) {
        (self.entries.values().map(Vec::len).sum(), self.people_with_textures().len())
    }

    /// Is `user`'s picture of this version ready to draw (at either size)?
    #[cfg(test)]
    pub fn has_picture(&self, user: &User) -> bool {
        let Some(v) = self.kind.version(user) else { return false };
        self.entries.get(&user.id).is_some_and(|l| l.iter().any(|e| e.version == v && !e.frames.is_empty()))
    }

    /// Has the hub answered for `user`'s picture of this version (any size), whatever the answer?
    #[cfg(test)]
    pub fn answered(&self, user: &User) -> bool {
        let Some(v) = self.kind.version(user) else { return false };
        self.entries.get(&user.id).is_some_and(|l| l.iter().any(|e| e.version == v && !matches!(e.state, State::Loading)))
    }
}

/// The entry's texture right now, advancing an animation by the elapsed time — the viewer's trick
/// (`chat_images::full_frame`): ask for a repaint exactly when the next frame is due, so a still
/// costs nothing and an animation repaints at its own rate.
fn frame_now(e: &mut Entry, ctx: &egui::Context, now: Instant) -> Option<TextureId> {
    if e.frames.len() > 1 {
        e.frame %= e.frames.len();
        let delay = Duration::from_millis(u64::from(e.delays[e.frame].max(20)));
        let elapsed = now.saturating_duration_since(e.frame_at);
        if elapsed >= delay {
            e.frame = (e.frame + 1) % e.frames.len();
            e.frame_at = now;
        }
        ctx.request_repaint_after(delay.saturating_sub(now.saturating_duration_since(e.frame_at)));
    }
    e.frames.get(e.frame).map(TextureHandle::id)
}

/// Decode the hub's JPEG / PNG square. Anything bigger than [`MAX_SIDE`] or not square-ish is
/// refused.
pub fn decode(bytes: &[u8]) -> Option<egui::ColorImage> {
    decode_still(bytes, Kind::Avatar)
}

/// [`decode`] with the size ceiling of the kind: the hub renders an avatar at 64 / 256, but a
/// banner comes back 600 wide, and 600 is a legitimate banner, not a broken picture.
pub fn decode_still(bytes: &[u8], kind: Kind) -> Option<egui::ColorImage> {
    let img = image::load_from_memory(bytes).ok()?;
    let (w, h) = (img.width(), img.height());
    let (max_w, max_h) = kind.still_cap();
    if w == 0 || h == 0 || w > max_w || h > max_h {
        return None;
    }
    let rgba = img.to_rgba8();
    Some(egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], rgba.as_raw()))
}

/// An animated original (a GIF, an animated WebP, an APNG) as its frames, each shrunk to at most
/// [`Kind::frame_cap`] on its long side, with the delay in ms of each (the 20 ms floor from
/// `chat_images` applied — a 0 ms frame is a dropped frame, not a blur).
///
/// `None` when the bytes are not an animation (or not a picture at all): the caller then goes
/// through [`decode`], so a still is exactly what it was. A budget that leaves a single frame also
/// returns `None`: an animation of one frame is a still, and a still must not pay for the extra
/// machinery.
///
/// The budget is the point of this function, so it is worth being exact about what it is. Three
/// independent lines, because the three ways to be expensive are independent:
///
/// 1. **`MAX_ANIM_SIDE`** bounds the canvas a frame is decoded *at*. It is NOT a decompression
///    bomb line on its own: `image` allocates a whole-canvas RGBA buffer per frame
///    (`codecs/gif.rs` composites a sub-rectangle frame onto the canvas, `codecs/webp/decoder.rs`
///    does `RgbaImage::new(width, height)`), so 240 frames at 4096² is ~16 GB of *decoded* bytes.
/// 2. **`set_limits`** makes the decoder reserve against a budget instead of nothing.
///    `GifDecoder::new` ships `Limits::no_limits()` (`codecs/gif.rs:59-66`) and `WebPDecoder`
///    does not override `set_limits` at all (the trait default is a no-op), so without this the
///    per-frame reservation is unbounded.
/// 3. **`MAX_ANIM_DECODED`** is the aggregate line: it counts bytes as they come off the iterator
///    and stops. It is the only one that cannot be argued away by a big canvas with small frames
///    — 240 sub-rectangle GIF frames cost ~5.5 KB on the wire and still decode to 16 GB.
///
/// Each frame is shrunk **as it comes off the iterator**, before the next one is decoded, so at
/// most one full-canvas buffer is alive at a time. Collecting first and shrinking afterwards (what
/// this used to do) holds every full-size frame at once, which is exactly the 16 GB.
pub fn decode_frames(bytes: &[u8], kind: Kind) -> Option<(Vec<egui::ColorImage>, Vec<u32>)> {
    let format = image::guess_format(bytes).ok()?;
    let (cap_w, cap_h) = kind.frame_cap();
    let cursor = std::io::Cursor::new(bytes.to_vec());
    // Each decoder says whether it is an animation at all; the still path draws everything else,
    // exactly as before. The original is bigger than the rendered still (a 600 px GIF is an
    // everyday avatar), so the ceiling here is the decompression-bomb one, not the cap the frames
    // end up at: every frame is shrunk to `frame_cap` right after.
    let fits = |(w, h): (u32, u32)| w != 0 && h != 0 && w <= MAX_ANIM_SIDE && h <= MAX_ANIM_SIDE;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_ANIM_SIDE);
    limits.max_image_height = Some(MAX_ANIM_SIDE);
    let frames: Vec<(egui::ColorImage, u32)> = match format {
        image::ImageFormat::Gif => {
            use image::{AnimationDecoder, ImageDecoder};
            let mut d = image::codecs::gif::GifDecoder::new(cursor).ok()?;
            if !fits(d.dimensions()) {
                return None;
            }
            // The same pattern as `chat_images::decode_full`: without this the GIF decoder's own
            // `no_limits` stands and every frame reserves against nothing.
            d.set_limits(limits).ok()?;
            // (A GIF says nothing about being an animation: one frame simply comes back as one,
            // and one frame is the still path's job.)
            shrink_frames(d.into_frames(), cap_w, cap_h)?
        }
        image::ImageFormat::WebP => {
            use image::{AnimationDecoder, ImageDecoder};
            let mut d = image::codecs::webp::WebPDecoder::new(cursor).ok()?;
            if !fits(d.dimensions()) {
                return None;
            }
            if !d.has_animation() {
                return None;
            }
            // `WebPDecoder` does not implement `set_limits` — the trait default is a no-op — so
            // this buys nothing for WebP today and is here for the day it does. What actually
            // contains WebP is the `fits` above plus the aggregate budget below.
            d.set_limits(limits).ok()?;
            shrink_frames(d.into_frames(), cap_w, cap_h)?
        }
        image::ImageFormat::Png => {
            use image::{AnimationDecoder, ImageDecoder};
            let mut d = image::codecs::png::PngDecoder::new(cursor).ok()?;
            if !fits(d.dimensions()) {
                return None;
            }
            if !d.is_apng().unwrap_or(false) {
                return None;
            }
            d.set_limits(limits).ok()?;
            shrink_frames(d.apng().ok()?.into_frames(), cap_w, cap_h)?
        }
        _ => return None,
    };
    if frames.len() < 2 {
        return None; // one frame (or none): the still path draws it, unchanged
    }
    Some(frames.into_iter().unzip())
}

/// The frames of an animation, at most [`MAX_FRAMES`] of them and at most [`MAX_ANIM_DECODED`]
/// decoded bytes, each shrunk to the cap before the next frame is decoded (a 500-frame GIF is a
/// screensaver, not an avatar: the budget keeps the texture RAM bounded and the still path draws
/// the rest as one picture). `None` on a decode error, so a broken animation falls back like a
/// broken still.
fn shrink_frames(it: impl Iterator<Item = image::ImageResult<image::Frame>>, cap_w: u32, cap_h: u32) -> Option<Vec<(egui::ColorImage, u32)>> {
    let mut out: Vec<(egui::ColorImage, u32)> = Vec::new();
    let mut decoded = 0u64;
    for f in it.take(MAX_FRAMES) {
        let f = f.ok()?;
        let (num, den) = f.delay().numer_denom_ms();
        let delay = num.checked_div(den).map_or(100, |ms| ms.max(20));
        let img = f.into_buffer();
        decoded += img.as_raw().len() as u64;
        // A frame is composited onto the whole canvas already, so shrinking it is the whole job;
        // `resize` fits it inside the cap without ever enlarging.
        let small = image::imageops::resize(&img, cap_w, cap_h, image::imageops::FilterType::Triangle);
        out.push((egui::ColorImage::from_rgba_unmultiplied([small.width() as usize, small.height() as usize], small.as_raw()), delay));
        drop(img);
        if decoded > MAX_ANIM_DECODED {
            break;
        }
    }
    Some(out)
}

// ------------------------------------------------------------------ drawing

/// Segments of the circle outline.
const CIRCLE_SEGMENTS: usize = 48;

/// A texture clipped to a circle with a soft one-pixel edge: a triangle fan with the picture's
/// UVs, and an outer ring that fades to transparent.
pub fn paint_circle_image(painter: &egui::Painter, center: Pos2, radius: f32, tex: TextureId) {
    let rect = Rect::from_center_size(center, Vec2::splat(radius * 2.0));
    let uv = |p: Pos2| Pos2::new((p.x - rect.min.x) / rect.width(), (p.y - rect.min.y) / rect.height());
    let (inner, outer) = ((radius - 0.5).max(0.0), radius + 0.5);
    let mut mesh = epaint::Mesh::with_texture(tex);
    mesh.vertices.push(epaint::Vertex { pos: center, uv: uv(center), color: Color32::WHITE });
    let ring = |mesh: &mut epaint::Mesh, r: f32, color: Color32| {
        for i in 0..CIRCLE_SEGMENTS {
            let a = i as f32 / CIRCLE_SEGMENTS as f32 * std::f32::consts::TAU;
            let pos = center + Vec2::new(a.cos(), a.sin()) * r;
            mesh.vertices.push(epaint::Vertex { pos, uv: uv(pos), color });
        }
    };
    ring(&mut mesh, inner, Color32::WHITE);
    ring(&mut mesh, outer, Color32::TRANSPARENT);
    let n = CIRCLE_SEGMENTS as u32;
    for i in 0..n {
        let (a, b) = (1 + i, 1 + (i + 1) % n);
        mesh.indices.extend([0, a, b]); // the fan
        let (oa, ob) = (a + n, b + n);
        mesh.indices.extend([a, oa, b, b, oa, ob]); // the soft edge
    }
    painter.add(epaint::Shape::mesh(mesh));
}

/// The coloured circle with the first letter of the name (what everybody had before pictures).
pub fn paint_initial(painter: &egui::Painter, center: Pos2, radius: f32, user: &User) {
    painter.circle_filled(center, radius, name_color(user.id));
    let initial = user.shown_name().chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default();
    painter.text(center, Align2::CENTER_CENTER, initial, FontId::proportional(radius * 1.1), Color32::BLACK);
}

/// Draw `user`'s picture as a circle, or their coloured initial while it loads / when they have
/// none.
pub fn draw_avatar(store: &mut AvatarStore, painter: &egui::Painter, ctx: &egui::Context, center: Pos2, radius: f32, user: &User, surface: Surface) {
    let class = Class::for_diameter(radius * 2.0, ctx.pixels_per_point());
    match store.texture(ctx, user, class, Instant::now()) {
        Some(tex) => {
            paint_circle_image(painter, center, radius, tex);
            store.drawn.insert((user.id, surface));
        }
        None => paint_initial(painter, center, radius, user),
    }
}

/// A `diameter`-point avatar in the layout, as a widget (hover shows the name).
pub fn avatar_widget(store: &mut AvatarStore, ui: &mut egui::Ui, user: &User, diameter: f32, surface: Surface) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(diameter), Sense::click());
    if ui.is_rect_visible(rect) {
        let painter = ui.painter_at(rect.expand(1.0));
        draw_avatar(store, &painter, ui.ctx(), rect.center(), diameter / 2.0, user, surface);
    }
    resp
}

// ------------------------------------------------------------------ backend side

/// A picture file on its way to the hub. Prints as its size, never as 5 MB of bytes.
#[derive(Clone)]
pub struct Blob(pub Arc<Vec<u8>>);

impl std::fmt::Debug for Blob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Blob({} bytes)", self.0.len())
    }
}

/// The picture calls the backend runs ([`Cmd::Avatar`]); the answer is an [`AvatarEv`].
#[derive(Debug, Clone)]
pub enum AvatarCmd {
    /// Upload my picture (the hub crops, resizes and re-encodes it).
    Set(Blob),
    /// Remove my picture.
    Clear,
    /// A moderator removes somebody else's.
    ModRemove { user: UserId },
    /// Upload my banner.
    BannerSet(Blob),
    /// Remove my banner.
    BannerClear,
    Fetch(Request),
}

#[derive(Debug)]
pub enum AvatarEv {
    /// My upload / removal finished: the new version (`None` after a removal), or a message in
    /// Portuguese.
    Changed(Result<Option<String>, String>),
    /// A moderator removal finished.
    ModRemoved(Result<(), String>),
    /// My banner upload / removal finished (same shape, other store).
    BannerChanged(Result<Option<String>, String>),
    Image { request: Request, reply: AvatarReply },
}

impl AvatarCmd {
    /// The event for a command that cannot run (no hub connection).
    pub fn offline(self) -> AvatarEv {
        let msg = "Sem conexão com o servidor.".to_string();
        match self {
            AvatarCmd::Set(_) | AvatarCmd::Clear => AvatarEv::Changed(Err(msg)),
            AvatarCmd::BannerSet(_) | AvatarCmd::BannerClear => AvatarEv::BannerChanged(Err(msg)),
            AvatarCmd::ModRemove { .. } => AvatarEv::ModRemoved(Err(msg)),
            AvatarCmd::Fetch(request) => AvatarEv::Image { request, reply: AvatarReply::Failed("offline".into()) },
        }
    }
}

pub async fn run(hub: &HubClient, cmd: AvatarCmd) -> AvatarEv {
    match cmd {
        AvatarCmd::Set(file) => AvatarEv::Changed(hub.set_avatar(file.0.as_ref().clone()).await.map(|a| a.avatar).map_err(|e| error_text(&e, Kind::Avatar))),
        AvatarCmd::Clear => AvatarEv::Changed(hub.clear_avatar().await.map(|a| a.avatar).map_err(|e| error_text(&e, Kind::Avatar))),
        AvatarCmd::ModRemove { user } => AvatarEv::ModRemoved(hub.mod_remove_avatar(user).await.map_err(|e| error_text(&e, Kind::Avatar))),
        AvatarCmd::BannerSet(file) => AvatarEv::BannerChanged(upload(hub, Kind::Banner, &file.0).await.map_err(|e| error_text(&e, Kind::Banner))),
        AvatarCmd::BannerClear => AvatarEv::BannerChanged(remove(hub, Kind::Banner).await.map_err(|e| error_text(&e, Kind::Banner))),
        AvatarCmd::Fetch(request) => {
            // The hub's own wrapper asks for a still avatar at 64 / 256 px and caps it at
            // [`MAX_AVATAR_IMAGE_BYTES`], and its URL builder is private, so it serves the avatar
            // only; the animated original (`?anim=1`) and the banner are asked for here.
            let reply = match (request.kind, request.anim) {
                (Kind::Avatar, false) => hub.avatar_image(request.user, request.class.px(), &request.version).await,
                _ => fetch(hub, &request).await,
            };
            AvatarEv::Image { request, reply }
        }
    }
}

/// The app's own HTTP client, for the calls the hub's wrapper does not carry yet (its `reqwest`
/// features are unified across the workspace, so this is the same stack).
static HTTP: OnceLock<reqwest::Client> = OnceLock::new();

fn http() -> &'static reqwest::Client {
    HTTP.get_or_init(|| reqwest::Client::builder().timeout(Duration::from_secs(60)).build().unwrap_or_default())
}

/// `GET api/users/{user}/{path}?size=&v=[&anim=1]`: the same answer as the hub's own wrapper
/// ([`AvatarReply`]), with the animated original's own, still bounded, cap.
async fn fetch(hub: &HubClient, request: &Request) -> AvatarReply {
    let path = format!("api/users/{}/{}", request.user.0, request.kind.path());
    let mut url = match hub.base_url().join(&path) {
        Ok(u) => u,
        Err(e) => return AvatarReply::Failed(format!("{e:#}")),
    };
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("size", &request.class.px().to_string()).append_pair("v", &request.version);
        if request.anim {
            q.append_pair("anim", "1");
        }
    }
    let Some(token) = hub.token() else { return AvatarReply::Failed("not logged in".into()) };
    let cap = if request.anim { MAX_ANIM_IMAGE_BYTES } else { MAX_AVATAR_IMAGE_BYTES };
    let resp = match http().get(url).bearer_auth(token).send().await {
        Ok(r) => r,
        Err(e) => return AvatarReply::Failed(format!("picture request: {e}")),
    };
    match resp.status().as_u16() {
        404 => AvatarReply::NotFound,
        s if (200..300).contains(&s) => match resp.bytes().await {
            Ok(b) if b.len() <= cap => AvatarReply::Image(b.to_vec()),
            Ok(_) => AvatarReply::Failed("picture too large".into()),
            Err(e) => AvatarReply::Failed(format!("picture body: {e}")),
        },
        s => AvatarReply::Failed(format!("picture: HTTP {s}")),
    }
}

/// `PUT api/me/{path}`, the file as the body: the new version of that picture.
async fn upload(hub: &HubClient, kind: Kind, file: &[u8]) -> Result<Option<String>> {
    let what = format!("set {}", kind.label());
    let path = format!("api/me/{}", kind.upload_path());
    let body = send(hub, http().put(hub.base_url().join(&path).with_context(|| format!("bad path {path}"))?), &what, file).await?;
    Ok(version_of(kind, &body))
}

/// `DELETE api/me/{path}`: the picture's version is now nothing.
async fn remove(hub: &HubClient, kind: Kind) -> Result<Option<String>> {
    let what = format!("clear {}", kind.label());
    let path = format!("api/me/{}", kind.upload_path());
    let body = send(hub, http().delete(hub.base_url().join(&path).with_context(|| format!("bad path {path}"))?), &what, &[]).await?;
    Ok(version_of(kind, &body))
}

/// Send a request with the session token and read the answer as text, refusing it the way the
/// hub's own wrapper does (the Portuguese depends on those exact words).
async fn send(hub: &HubClient, req: reqwest::RequestBuilder, what: &str, body: &[u8]) -> Result<String> {
    let token = hub.token().ok_or_else(|| anyhow::anyhow!("not logged in"))?;
    let resp = req.bearer_auth(token).body(body.to_vec()).send().await.with_context(|| format!("{what} request"))?;
    let status = resp.status();
    if status.is_success() {
        return resp.text().await.with_context(|| format!("{what}: invalid response body"));
    }
    let body = resp.text().await.unwrap_or_default();
    let msg = serde_json::from_str::<ApiError>(&body).map(|e| e.error).unwrap_or(body);
    Err(anyhow::anyhow!("{what} failed: HTTP {status}: {msg}"))
}

/// The version the hub gave back, out of the field this kind uses (`avatar` / `banner`).
fn version_of(kind: Kind, body: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(body).ok()?.get(kind.version_field())?.as_str().map(Into::into)
}

/// The hub's refusals, for the people.
pub fn error_text(e: &anyhow::Error, kind: Kind) -> String {
    let full = format!("{e:#}");
    let l = full.to_lowercase();
    let picture = kind.label();
    tracing::warn!(error = %full, %picture, "picture call failed");
    if l.contains("http 429") || l.contains("too many picture changes") {
        let wait = l.split("wait ").nth(1).and_then(|r| r.split_whitespace().next()).and_then(|n| n.parse::<u64>().ok());
        match wait {
            Some(s) if s >= 60 => format!("Você trocou a {picture} muitas vezes. Tente de novo em {} min.", s.div_ceil(60)),
            Some(s) => format!("Você trocou a {picture} muitas vezes. Tente de novo em {s} s."),
            None => format!("Você trocou a {picture} muitas vezes. Tente de novo mais tarde."),
        }
    } else if l.contains("http 413") || l.contains("picture too large") {
        "A imagem é grande demais (no máximo 5 MB).".into()
    } else if l.contains("dimensions are too large") {
        "A imagem tem dimensões grandes demais.".into()
    } else if l.contains("http 400") || l.contains("not a supported image") || l.contains("could not read the image") {
        "Esse arquivo não é uma imagem que o servidor aceite (use PNG, JPEG, WebP ou GIF).".into()
    } else if l.contains("http 404") || l.contains("http 405") {
        format!("Este servidor é de uma versão antiga e ainda não aceita {picture}.")
    } else if l.contains("http 403") {
        "Seu cargo não permite isso.".into()
    } else {
        crate::backend::friendly(e)
    }
}

// ------------------------------------------------------------------ choosing and preparing a file

/// Files bigger than this are not even read.
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// The hub refuses pictures with more pixels than this (a 5000x4000 photo is 20 MP).
const MAX_PIXELS: u64 = 20_000_000;
/// Size of the circular preview (px).
const PREVIEW_PX: u32 = 256;
/// Size of the banner preview (px): what the hub crops a banner to.
const BANNER_PREVIEW: (u32, u32) = (600, 240);

/// A chosen file, ready to show and to upload.
#[derive(Debug)]
pub struct Prepared {
    /// What is sent: the file as it is, or (files over 5 MB) a smaller JPEG of the same picture.
    pub upload: Vec<u8>,
    /// The centre crop the hub will make, at this kind's preview size.
    pub preview: egui::ColorImage,
}

/// Decode the chosen file (under the hub's limits), compute the preview and make it fit
/// [`AVATAR_MAX_UPLOAD_BYTES`]. Errors are Portuguese, for the people.
pub fn prepare(bytes: Vec<u8>) -> Result<Prepared, String> {
    prepare_with(bytes, Kind::Avatar)
}

/// The same, for a banner: a 600x240 preview instead of the circle's square.
pub fn prepare_banner(bytes: Vec<u8>) -> Result<Prepared, String> {
    prepare_with(bytes, Kind::Banner)
}

fn prepare_with(bytes: Vec<u8>, kind: Kind) -> Result<Prepared, String> {
    let not_image = || "Esse arquivo não é uma imagem que dê para usar (use PNG, JPEG, WebP ou GIF).".to_string();
    let mut reader = image::ImageReader::new(std::io::Cursor::new(&bytes)).with_guessed_format().map_err(|_| not_image())?;
    reader.limits({
        let mut l = image::Limits::default();
        l.max_image_width = Some(10_000);
        l.max_image_height = Some(10_000);
        l.max_alloc = Some(256 * 1024 * 1024);
        l
    });
    let mut decoder = image::ImageReader::into_decoder(reader).map_err(|_| not_image())?;
    {
        use image::ImageDecoder;
        let (w, h) = decoder.dimensions();
        if u64::from(w) * u64::from(h) > MAX_PIXELS {
            return Err("A imagem tem pixels demais (no máximo 20 megapixels).".into());
        }
    }
    let orientation = image::ImageDecoder::orientation(&mut decoder).unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut img = image::DynamicImage::from_decoder(decoder).map_err(|_| not_image())?;
    img.apply_orientation(orientation);
    let (pw, ph) = match kind {
        Kind::Avatar => (PREVIEW_PX, PREVIEW_PX),
        Kind::Banner => BANNER_PREVIEW,
    };
    let crop = img.resize_to_fill(pw, ph, image::imageops::FilterType::Triangle);
    let rgba = crop.to_rgba8();
    let preview = egui::ColorImage::from_rgba_unmultiplied([rgba.width() as usize, rgba.height() as usize], rgba.as_raw());
    let upload = if bytes.len() <= AVATAR_MAX_UPLOAD_BYTES {
        bytes
    } else {
        // Over the limit: send a 1024 px JPEG of the same picture instead (the proportion kept: the
        // hub does its own centre crop).
        let small = img.resize(1024, 1024, image::imageops::FilterType::Triangle).to_rgb8();
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90).encode_image(&small).map_err(|_| not_image())?;
        out
    };
    Ok(Prepared { upload, preview })
}

mod dialog {
    /// Ask for one image file. `Ok(None)`: the person cancelled. Blocks until the dialog closes
    /// (callers run it on its own thread), with `owner` as the dialog's parent window.
    pub fn pick_image(owner: Option<isize>, title: &str) -> Result<Option<std::path::PathBuf>, String> {
        crate::filedlg::open(owner, title, crate::filedlg::IMAGE_FILTER, false).map(|v| v.into_iter().next())
    }
}

/// What the picking thread reports.
pub(crate) enum Picked {
    Cancelled,
    Failed(String),
    Ready { name: String, prepared: Prepared, kind: Kind },
}

// ------------------------------------------------------------------ the settings section

/// The state of one of the settings sections ("Foto de perfil", "Banner do perfil"): the same
/// machine over a different kind, so there is one struct and a kind, not two copies.
#[derive(Default)]
pub struct PictureUi {
    rx: Option<mpsc::Receiver<Picked>>,
    /// The file dialog (or the decoding after it) is running.
    pub picking: bool,
    /// A chosen, not yet saved picture.
    pub picked: Option<PickedFile>,
    /// An upload / removal is running.
    pub busy: bool,
    /// (ok, text) to show under the buttons.
    pub msg: Option<(bool, String)>,
    /// Which picture this section is about (`Default` = the avatar, the first variant).
    kind: Kind,
}

impl PictureUi {
    /// The section for `kind`.
    pub fn new(kind: Kind) -> Self {
        PictureUi { kind, ..PictureUi::default() }
    }

    /// Which picture this section edits.
    pub fn kind(&self) -> Kind {
        self.kind
    }
}

/// The avatar's section (the name the settings and the autotest know it by).
pub type ProfileUi = PictureUi;

/// A chosen file: what it is called, what is sent, what the preview shows.
pub struct PickedFile {
    pub name: String,
    upload: Blob,
    preview: TextureHandle,
}

/// Everything about pictures an `App` holds.
pub struct Avatars {
    pub store: AvatarStore,
    /// The "Foto de perfil" section.
    pub profile: ProfileUi,
    /// The profile banners: the same store machine over the banner's wire path.
    pub banners: Banners,
}

impl Default for Avatars {
    fn default() -> Self {
        Avatars { store: AvatarStore::default(), profile: ProfileUi::new(Kind::Avatar), banners: Banners::default() }
    }
}

impl App {
    /// The section that edits `kind`'s picture.
    fn picture(&mut self, kind: Kind) -> &mut PictureUi {
        match kind {
            Kind::Avatar => &mut self.avatars.profile,
            Kind::Banner => &mut self.avatars.banners.ui,
        }
    }

    /// Send the backend whatever either store wants asked right now.
    pub fn pump_avatars(&mut self) {
        for r in self.avatars.store.take_requests() {
            self.backend.send(Cmd::Avatar(AvatarCmd::Fetch(r)));
        }
        for r in self.avatars.banners.store.take_requests() {
            self.backend.send(Cmd::Avatar(AvatarCmd::Fetch(r)));
        }
    }

    /// The backend's answers about pictures.
    pub fn on_avatar_event(&mut self, ev: AvatarEv) {
        match ev {
            AvatarEv::Image { request, reply } => {
                // The answer belongs to whichever store asked (the banner's is a different one).
                match request.kind {
                    Kind::Avatar => self.avatars.store.on_image(request.user, &request.version, request.class, reply, Instant::now()),
                    Kind::Banner => self.avatars.banners.store.on_image(request.user, &request.version, request.class, reply, Instant::now()),
                }
                self.pump_avatars();
            }
            AvatarEv::Changed(result) => self.picture_saved(Kind::Avatar, result),
            AvatarEv::BannerChanged(result) => self.picture_saved(Kind::Banner, result),
            AvatarEv::ModRemoved(result) => match result {
                Ok(()) => self.toast("Foto de perfil removida."),
                Err(e) => self.notice = Some(e),
            },
        }
    }

    /// An upload / removal finished: the section says so and my own `User` carries the new version
    /// (the hub's `UserUpdated` tells everybody; this keeps my own screen right even if it is late).
    fn picture_saved(&mut self, kind: Kind, result: Result<Option<String>, String>) {
        let p = self.picture(kind);
        p.busy = false;
        match result {
            Ok(version) => {
                let what = match kind {
                    Kind::Avatar => "Foto",
                    Kind::Banner => "Banner",
                };
                p.msg = Some((true, if version.is_some() { format!("{what} salva.") } else { format!("{what} removida.") }));
                p.picked = None;
                if let Some(me) = self.me.as_mut() {
                    match kind {
                        Kind::Avatar => me.avatar = version,
                        Kind::Banner => me.banner = version,
                    }
                }
            }
            Err(e) => p.msg = Some((false, e)),
        }
    }

    /// "Escolher imagem...": the native dialog on its own thread, then decoding.
    pub fn pick_profile_picture(&mut self, ctx: &egui::Context) {
        self.pick_picture(ctx, Kind::Avatar);
    }

    /// The same for a banner.
    pub fn pick_banner(&mut self, ctx: &egui::Context) {
        self.pick_picture(ctx, Kind::Banner);
    }

    fn pick_picture(&mut self, ctx: &egui::Context, kind: Kind) {
        if self.picture(kind).picking {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let (owner, ctx) = (self.video.parent, ctx.clone());
        let title = match kind {
            Kind::Avatar => "Escolher foto de perfil".to_string(),
            Kind::Banner => "Escolher banner do perfil".to_string(),
        };
        std::thread::spawn(move || {
            let msg = match dialog::pick_image(owner, &title) {
                Ok(None) => Picked::Cancelled,
                Err(e) => Picked::Failed(e),
                Ok(Some(path)) => match read_file(&path) {
                    Ok(bytes) => {
                        let prepared = match kind {
                            Kind::Avatar => prepare(bytes),
                            Kind::Banner => prepare_banner(bytes),
                        };
                        match prepared {
                            Ok(prepared) => Picked::Ready { name: path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(), prepared, kind },
                            Err(e) => Picked::Failed(e),
                        }
                    }
                    Err(e) => Picked::Failed(e),
                },
            };
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
        let p = self.picture(kind);
        p.rx = Some(rx);
        p.picking = true;
        p.msg = None;
    }

    /// Collect the result of the picking thread (called every frame while the settings are open).
    pub fn poll_pictures(&mut self, ctx: &egui::Context) {
        for kind in [Kind::Avatar, Kind::Banner] {
            let Some(msg) = self.picture(kind).rx.as_ref().and_then(|rx| rx.try_recv().ok()) else { continue };
            let p = self.picture(kind);
            p.rx = None;
            p.picking = false;
            match msg {
                Picked::Cancelled => {}
                Picked::Failed(e) => p.msg = Some((false, e)),
                Picked::Ready { name, prepared, kind } => {
                    let preview = ctx.load_texture(format!("{}-preview", kind.path()), prepared.preview, TextureOptions::LINEAR);
                    p.picked = Some(PickedFile { name, upload: Blob(Arc::new(prepared.upload)), preview });
                }
            }
        }
    }

    /// The chosen picture as a file's bytes (the dialog's path, and the autotest's). Errors show
    /// in the section.
    pub fn choose_profile_picture_bytes(&mut self, ctx: &egui::Context, name: &str, bytes: Vec<u8>) {
        self.choose_picture_bytes(ctx, Kind::Avatar, name, bytes);
    }

    /// The same for a banner.
    pub fn choose_banner_bytes(&mut self, ctx: &egui::Context, name: &str, bytes: Vec<u8>) {
        self.choose_picture_bytes(ctx, Kind::Banner, name, bytes);
    }

    fn choose_picture_bytes(&mut self, ctx: &egui::Context, kind: Kind, name: &str, bytes: Vec<u8>) {
        let prepared = match kind {
            Kind::Avatar => prepare(bytes),
            Kind::Banner => prepare_banner(bytes),
        };
        let p = self.picture(kind);
        p.msg = None;
        match prepared {
            Ok(prepared) => {
                let preview = ctx.load_texture(format!("{}-preview", kind.path()), prepared.preview, TextureOptions::LINEAR);
                p.picked = Some(PickedFile { name: name.to_string(), upload: Blob(Arc::new(prepared.upload)), preview });
            }
            Err(e) => p.msg = Some((false, e)),
        }
    }

    /// "Salvar": upload the chosen picture.
    pub fn save_profile_picture(&mut self) {
        self.save_picture(Kind::Avatar);
    }

    /// "Salvar banner".
    pub fn save_banner(&mut self) {
        self.save_picture(Kind::Banner);
    }

    fn save_picture(&mut self, kind: Kind) {
        let p = self.picture(kind);
        let Some(file) = p.picked.as_ref().filter(|_| !p.busy) else { return };
        let blob = file.upload.clone();
        p.busy = true;
        p.msg = None;
        let cmd = match kind {
            Kind::Avatar => AvatarCmd::Set(blob),
            Kind::Banner => AvatarCmd::BannerSet(blob),
        };
        self.backend.send(Cmd::Avatar(cmd));
    }

    /// "Remover foto": delete my picture.
    pub fn remove_profile_picture(&mut self) {
        self.remove_picture(Kind::Avatar);
    }

    /// "Remover banner".
    pub fn remove_banner(&mut self) {
        self.remove_picture(Kind::Banner);
    }

    fn remove_picture(&mut self, kind: Kind) {
        if self.picture(kind).busy {
            return;
        }
        let p = self.picture(kind);
        p.busy = true;
        p.msg = None;
        let cmd = match kind {
            Kind::Avatar => AvatarCmd::Clear,
            Kind::Banner => AvatarCmd::BannerClear,
        };
        self.backend.send(Cmd::Avatar(cmd));
    }

    /// Forget a choice ("Cancelar").
    pub fn cancel_picture(&mut self, kind: Kind) {
        self.picture(kind).picked = None;
    }

    /// A moderator removes somebody's picture ("Remover foto" in the member menu).
    pub fn mod_remove_picture(&mut self, user: UserId) {
        self.backend.send(Cmd::Avatar(AvatarCmd::ModRemove { user }));
    }





    /// "Foto de perfil" in Configurações → Conta e perfil, and the "Banner do perfil" section under
    /// it (the two are the same machine, so the banner is drawn here rather than in the settings,
    /// which knows nothing about pictures). Returns whether it was shown (an older hub has no
    /// pictures).
    pub fn profile_picture_section(&mut self, ui: &mut egui::Ui) -> bool {
        use crate::theme::{self, ph, space};
        // (`Kind` here is this module's: the avatar's or the banner's.)
        use crate::ui_kit::{self, Kind as UiKind, Size};
        // Pictures came with the same hub release as roles: an older hub has neither.
        if !self.roles.supported {
            return false;
        }
        let ctx = ui.ctx().clone();
        self.poll_pictures(&ctx);
        crate::forms::group(ui, "Foto de perfil");
        crate::forms::note(ui, "Aparece nas salas de voz, nas listas e no chat. O servidor corta no centro, em círculo, e guarda uma versão pequena.", false);
        ui.add_space(space::M);
        let me = self.me.clone();
        let (picking, busy) = (self.avatars.profile.picking, self.avatars.profile.busy);
        let mut choose = false;
        let mut save = false;
        let mut cancel = false;
        let mut remove = false;
        ui.horizontal(|ui| {
            const SIZE: f32 = 96.0;
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(SIZE), Sense::hover());
            let painter = ui.painter_at(rect.expand(1.0));
            match (&self.avatars.profile.picked, &me) {
                (Some(f), _) => paint_circle_image(&painter, rect.center(), SIZE / 2.0, f.preview.id()),
                (None, Some(me)) => draw_avatar(&mut self.avatars.store, &painter, &ctx, rect.center(), SIZE / 2.0, me, Surface::Settings),
                _ => {}
            }
            ui.add_space(space::L);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = space::M;
                if let Some(f) = &self.avatars.profile.picked {
                    ui.label(egui::RichText::new(format!("{} · prévia do recorte", f.name)).size(theme::size::SMALL + 0.5).color(theme::NEVOA));
                    ui.horizontal(|ui| {
                        save = ui.add_enabled_ui(!busy, |ui| ui_kit::button_sized(ui, UiKind::Primary, Size::Small, None, if busy { "Salvando..." } else { "Salvar foto" })).inner.clicked();
                        cancel = ui.add_enabled_ui(!busy, |ui| ui_kit::button_sized(ui, UiKind::Ghost, Size::Small, None, "Cancelar")).inner.clicked();
                    });
                }
                ui.horizontal(|ui| {
                    let label = if picking { "Escolhendo..." } else { "Escolher imagem…" };
                    choose = ui.add_enabled_ui(!picking && !busy, |ui| ui_kit::button_sized(ui, UiKind::Secondary, Size::Small, Some(ph::IMAGE), label)).inner.clicked();
                    let has = me.as_ref().is_some_and(|m| m.avatar.is_some());
                    if has && self.avatars.profile.picked.is_none() {
                        remove = ui.add_enabled_ui(!busy, |ui| ui_kit::button_sized(ui, UiKind::Ghost, Size::Small, Some(ph::TRASH), "Remover foto")).inner.clicked();
                    }
                });
            });
        });
        if let Some((ok, text)) = &self.avatars.profile.msg {
            let (icon, color) = if *ok { (ph::CHECK, theme::ONLINE) } else { (ph::WARNING, theme::AO_VIVO) };
            crate::forms::note_colored(ui, format!("{icon} {text}"), false, color);
        }
        if choose {
            self.pick_profile_picture(&ctx);
        }
        if cancel {
            self.cancel_picture(Kind::Avatar);
        }
        if save {
            self.save_profile_picture();
        }
        if remove {
            self.remove_profile_picture();
        }
        // The banner lives right under it: same machine, another wire path.
        self.banner_section(ui);
        true
    }
    /// "Banner do perfil" in Configurações → Conta e perfil, under "Foto de perfil". The two are
    /// the same machine over another wire path, so this is drawn from here rather than from the
    /// settings, which knows nothing about pictures.
    pub fn banner_section(&mut self, ui: &mut egui::Ui) {
        use crate::theme::{self, ph, space};
        // (`Kind` here is this module's: the avatar's or the banner's.)
        use crate::ui_kit::{self, Kind as UiKind, Size};
        crate::forms::divider(ui);
        crate::forms::group(ui, "Banner do perfil");
        crate::forms::note(ui, "A faixa no topo do seu cartão de perfil. O servidor corta no centro, em 600x240, e aceita GIF ou WebP animado.", false);
        ui.add_space(space::M);
        let me = self.me.clone();
        let (picking, busy) = (self.avatars.banners.ui.picking, self.avatars.banners.ui.busy);
        let mut choose = false;
        let mut save = false;
        let mut cancel = false;
        let mut remove = false;
        ui.horizontal(|ui| {
            // The crop the hub makes: 600x240, shown at a width that fits the page.
            const W: f32 = 300.0;
            const H: f32 = 120.0;
            let (rect, _) = ui.allocate_exact_size(Vec2::new(W, H), Sense::hover());
            let painter = ui.painter_at(rect.expand(1.0));
            let radius = egui::CornerRadius::same(crate::theme::radius::POPUP);
            painter.rect_filled(rect, radius, crate::theme::PAREDE);
            match &self.avatars.banners.ui.picked {
                Some(f) => {
                    let uv = cover_uv(rect, f.preview.size());
                    painter.image(f.preview.id(), rect, uv, egui::Color32::WHITE);
                }
                None => {
                    if let Some(me) = &me {
                        let class = Kind::Banner.only();
                        if let Some(tex) = self.avatars.banners.store.texture(ui.ctx(), me, class, Instant::now()) {
                            if let Some(size) = self.avatars.banners.store.size(me) {
                                let uv = cover_uv(rect, size);
                                painter.image(tex, rect, uv, egui::Color32::WHITE);
                            }
                        }
                    }
                }
            }
            ui.add_space(space::L);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = space::M;
                if let Some(f) = &self.avatars.banners.ui.picked {
                    ui.label(egui::RichText::new(format!("{} · prévia do recorte", f.name)).size(theme::size::SMALL + 0.5).color(theme::NEVOA));
                    ui.horizontal(|ui| {
                        save = ui.add_enabled_ui(!busy, |ui| ui_kit::button_sized(ui, UiKind::Primary, Size::Small, None, if busy { "Salvando..." } else { "Salvar banner" })).inner.clicked();
                        cancel = ui.add_enabled_ui(!busy, |ui| ui_kit::button_sized(ui, UiKind::Ghost, Size::Small, None, "Cancelar")).inner.clicked();
                    });
                }
                ui.horizontal(|ui| {
                    let label = if picking { "Escolhendo..." } else { "Escolher imagem…" };
                    choose = ui.add_enabled_ui(!picking && !busy, |ui| ui_kit::button_sized(ui, UiKind::Secondary, Size::Small, Some(ph::IMAGE), label)).inner.clicked();
                    let has = me.as_ref().is_some_and(|m| m.banner.is_some());
                    if has && self.avatars.banners.ui.picked.is_none() {
                        remove = ui.add_enabled_ui(!busy, |ui| ui_kit::button_sized(ui, UiKind::Ghost, Size::Small, Some(ph::TRASH), "Remover banner")).inner.clicked();
                    }
                });
            });
        });
        if let Some((ok, text)) = &self.avatars.banners.ui.msg {
            let (icon, color) = if *ok { (ph::CHECK, theme::ONLINE) } else { (ph::WARNING, theme::AO_VIVO) };
            crate::forms::note_colored(ui, format!("{icon} {text}"), false, color);
        }
        if choose {
            self.pick_banner(ui.ctx());
        }
        if cancel {
            self.cancel_picture(Kind::Banner);
        }
        if save {
            self.save_banner();
        }
        if remove {
            self.remove_banner();
        }
    }
}

// ------------------------------------------------------------------ the banners

/// The profile banners ("Banner do perfil"). A newtype over [`Store`], exactly like
/// [`AvatarStore`]: one machine, the banner's wire path, one 600x240 size.
pub struct BannerStore(Store);

impl Default for BannerStore {
    fn default() -> Self {
        BannerStore(Store::new(Kind::Banner))
    }
}

impl std::ops::Deref for BannerStore {
    type Target = Store;
    fn deref(&self) -> &Store {
        &self.0
    }
}

impl std::ops::DerefMut for BannerStore {
    fn deref_mut(&mut self) -> &mut Store {
        &mut self.0
    }
}

/// Everything the banners need: the store that downloads them and the section that changes mine.
pub struct Banners {
    pub store: BannerStore,
    /// The "Banner do perfil" section's state.
    pub ui: PictureUi,
}

impl Default for Banners {
    fn default() -> Self {
        Banners { store: BannerStore::default(), ui: PictureUi::new(Kind::Banner) }
    }
}

impl std::ops::Deref for Banners {
    type Target = BannerStore;
    fn deref(&self) -> &BannerStore {
        &self.store
    }
}

impl std::ops::DerefMut for Banners {
    fn deref_mut(&mut self) -> &mut BannerStore {
        &mut self.store
    }
}

/// The uv sub-rect that fills `rect` with a `size` picture without stretching it: the picture is
/// cropped to the rect's proportion, centred (a 600x240 banner in a card's band shows its middle
/// third, not a squashed whole).
pub fn cover_uv(rect: Rect, size: [usize; 2]) -> Rect {
    let (tw, th) = (size[0].max(1) as f32, size[1].max(1) as f32);
    let (rw, rh) = (rect.width().max(1.0), rect.height().max(1.0));
    let (tex_ar, rect_ar) = (tw / th, rw / rh);
    let (u0, u1, v0, v1) = if tex_ar > rect_ar {
        // Wider than the band: crop the sides.
        let f = rect_ar / tex_ar;
        ((1.0 - f) / 2.0, 1.0 - (1.0 - f) / 2.0, 0.0, 1.0)
    } else {
        // Taller than the band: crop the top and the bottom.
        let f = tex_ar / rect_ar;
        (0.0, 1.0, (1.0 - f) / 2.0, 1.0 - (1.0 - f) / 2.0)
    };
    Rect::from_min_max(Pos2::new(u0, v0), Pos2::new(u1, v1))
}

/// Draw `user`'s banner in `rect`, or leave it empty (the caller paints the flat colour it always
/// had). Returns whether a picture was drawn.
pub fn draw_banner(store: &mut BannerStore, painter: &egui::Painter, ctx: &egui::Context, rect: Rect, user: &User) -> bool {
    let class = Kind::Banner.only();
    let Some(tex) = store.texture(ctx, user, class, Instant::now()) else { return false };
    let Some(size) = store.size(user) else { return false };
    painter.image(tex, rect, cover_uv(rect, size), egui::Color32::WHITE);
    true
}

/// Read a chosen file, refusing the absurdly big ones.
fn read_file(path: &std::path::Path) -> Result<Vec<u8>, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("Não foi possível abrir o arquivo: {e}"))?;
    if meta.len() > MAX_FILE_BYTES {
        return Err("O arquivo é grande demais para ser uma foto.".into());
    }
    std::fs::read(path).map_err(|e| format!("Não foi possível ler o arquivo: {e}"))
}

// ------------------------------------------------------------------ tests

#[cfg(test)]
mod tests {
    use super::*;

    fn jpeg(side: u32, seed: u8) -> Vec<u8> {
        let img = image::RgbImage::from_fn(side, side, |x, y| image::Rgb([(x % 256) as u8, (y % 256) as u8, seed]));
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85).encode_image(&img).unwrap();
        out
    }

    fn png(side: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(side, side, image::Rgba([10, 200, 30, 255]));
        let mut out = Vec::new();
        image::DynamicImage::ImageRgba8(img).write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png).unwrap();
        out
    }

    fn user(id: UserId, avatar: Option<&str>) -> User {
        User { avatar: avatar.map(Into::into), ..User::new(id, format!("u{}", id.0)) }
    }

    /// Ask for everything pending and answer it all with `reply`.
    fn answer_all(s: &mut Store, now: Instant, mut reply: impl FnMut(&Request) -> AvatarReply) {
        loop {
            let reqs = s.take_requests();
            if reqs.is_empty() {
                break;
            }
            for r in reqs {
                let reply = reply(&r);
                s.on_image(r.user, &r.version, r.class, reply, now);
            }
        }
    }

    #[test]
    fn people_without_a_picture_are_never_asked_for() {
        let (mut s, ctx, now) = (AvatarStore::default(), egui::Context::default(), Instant::now());
        s.begin_frame(now);
        assert_eq!(s.texture(&ctx, &user(UserId(1), None), Class::SMALL, now), None);
        assert!(s.take_requests().is_empty());
        assert_eq!(s.stats(), (0, 0));
    }

    #[test]
    fn a_picture_is_asked_once_however_often_it_is_drawn_then_shown() {
        let (mut s, ctx, now) = (AvatarStore::default(), egui::Context::default(), Instant::now());
        let ana = user(UserId(1), Some("v1"));
        for _ in 0..30 {
            s.begin_frame(now);
            assert_eq!(s.texture(&ctx, &ana, Class::SMALL, now), None, "the initial while it loads");
        }
        assert_eq!(s.take_requests(), vec![Request { user: UserId(1), version: "v1".into(), class: Class::SMALL, kind: Kind::Avatar, anim: false }]);
        assert!(s.take_requests().is_empty());
        assert!(!s.answered(&ana));
        s.on_image(UserId(1), "v1", Class::SMALL, AvatarReply::Image(jpeg(64, 1)), now);
        assert!(s.answered(&ana) && !s.has_picture(&ana), "answered, not decoded until drawn");
        s.begin_frame(now);
        assert!(s.texture(&ctx, &ana, Class::SMALL, now).is_some());
        assert!(s.has_picture(&ana));
        assert_eq!(s.stats(), (1, 1));
        assert!(s.take_requests().is_empty(), "nothing more to ask");
    }

    #[test]
    fn at_most_four_requests_run_at_once() {
        let (mut s, ctx, now) = (AvatarStore::default(), egui::Context::default(), Instant::now());
        s.begin_frame(now);
        for id in 0..10 {
            s.texture(&ctx, &user(UserId(id), Some("v")), Class::SMALL, now);
        }
        assert_eq!(s.take_requests().len(), MAX_IN_FLIGHT);
        assert!(s.take_requests().is_empty(), "the window is full");
        s.on_image(UserId(0), "v", Class::SMALL, AvatarReply::NotFound, now);
        assert_eq!(s.take_requests().len(), 1);
    }

    #[test]
    fn the_other_size_stands_in_while_one_loads() {
        let (mut s, ctx, now) = (AvatarStore::default(), egui::Context::default(), Instant::now());
        let ana = user(UserId(1), Some("v1"));
        s.begin_frame(now);
        s.texture(&ctx, &ana, Class::SMALL, now);
        s.take_requests();
        s.on_image(UserId(1), "v1", Class::SMALL, AvatarReply::Image(jpeg(64, 1)), now);
        let small = s.texture(&ctx, &ana, Class::SMALL, now).expect("small is ready");
        // The big one is wanted now (a tile): until it arrives, the small picture is drawn.
        assert_eq!(s.texture(&ctx, &ana, Class::BIG, now), Some(small));
        assert_eq!(s.take_requests(), vec![Request { user: UserId(1), version: "v1".into(), class: Class::BIG, kind: Kind::Avatar, anim: false }]);
        s.on_image(UserId(1), "v1", Class::BIG, AvatarReply::Image(jpeg(256, 1)), now);
        let big = s.texture(&ctx, &ana, Class::BIG, now).expect("big is ready");
        assert_ne!(big, small);
    }

    #[test]
    fn a_404_keeps_the_initial_and_is_final_for_that_version() {
        let (mut s, ctx, now) = (AvatarStore::default(), egui::Context::default(), Instant::now());
        let ana = user(UserId(1), Some("v1"));
        s.begin_frame(now);
        s.texture(&ctx, &ana, Class::SMALL, now);
        answer_all(&mut *s, now, |_| AvatarReply::NotFound);
        let later = now + Duration::from_secs(3600);
        s.begin_frame(later);
        assert_eq!(s.texture(&ctx, &ana, Class::SMALL, later), None);
        assert!(s.take_requests().is_empty(), "not asked again, however long it takes");
        // A new version is a new picture.
        let newer = user(UserId(1), Some("v2"));
        s.texture(&ctx, &newer, Class::SMALL, later);
        assert_eq!(s.take_requests(), vec![Request { user: UserId(1), version: "v2".into(), class: Class::SMALL, kind: Kind::Avatar, anim: false }]);
    }

    #[test]
    fn failures_are_retried_later_and_only_while_drawn() {
        let (mut s, ctx, now) = (AvatarStore::default(), egui::Context::default(), Instant::now());
        let ana = user(UserId(1), Some("v1"));
        s.begin_frame(now);
        s.texture(&ctx, &ana, Class::SMALL, now);
        answer_all(&mut *s, now, |_| AvatarReply::Failed("boom".into()));
        s.texture(&ctx, &ana, Class::SMALL, now + Duration::from_secs(5));
        assert!(s.take_requests().is_empty(), "too soon");
        // Nobody draws her for a while: nothing is asked in the background...
        assert!(s.take_requests().is_empty());
        // ...and the first frame that draws her after the wait asks again.
        let later = now + RETRY_AFTER + Duration::from_secs(1);
        s.texture(&ctx, &ana, Class::SMALL, later);
        assert_eq!(s.take_requests().len(), 1);
    }

    #[test]
    fn a_broken_picture_keeps_the_initial_without_looping() {
        let (mut s, ctx, now) = (AvatarStore::default(), egui::Context::default(), Instant::now());
        let ana = user(UserId(1), Some("v1"));
        s.begin_frame(now);
        s.texture(&ctx, &ana, Class::SMALL, now);
        s.take_requests();
        s.on_image(UserId(1), "v1", Class::SMALL, AvatarReply::Image(b"<html>nope</html>".to_vec()), now);
        for _ in 0..5 {
            s.begin_frame(now);
            assert_eq!(s.texture(&ctx, &ana, Class::SMALL, now), None);
        }
        assert!(s.take_requests().is_empty());
        // A picture bigger than the hub ever sends is refused too.
        s.texture(&ctx, &user(UserId(2), Some("v")), Class::BIG, now);
        s.take_requests();
        s.on_image(UserId(2), "v", Class::BIG, AvatarReply::Image(jpeg(600, 1)), now);
        s.begin_frame(now);
        assert_eq!(s.texture(&ctx, &user(UserId(2), Some("v")), Class::BIG, now), None);
    }

    #[test]
    fn late_answers_for_forgotten_pictures_are_harmless() {
        let (mut s, ctx, now) = (AvatarStore::default(), egui::Context::default(), Instant::now());
        s.begin_frame(now);
        s.texture(&ctx, &user(UserId(1), Some("v1")), Class::SMALL, now);
        s.take_requests();
        s.reset(); // another server
        s.on_image(UserId(1), "v1", Class::SMALL, AvatarReply::Image(jpeg(64, 1)), now);
        assert_eq!(s.stats(), (0, 0));
        assert!(s.take_requests().is_empty());
    }

    #[test]
    fn idle_textures_are_freed_and_come_back_from_the_kept_bytes() {
        let (mut s, ctx, now) = (AvatarStore::default(), egui::Context::default(), Instant::now());
        let ana = user(UserId(1), Some("v1"));
        s.begin_frame(now);
        s.texture(&ctx, &ana, Class::SMALL, now);
        answer_all(&mut *s, now, |_| AvatarReply::Image(jpeg(64, 1)));
        s.begin_frame(now);
        assert!(s.texture(&ctx, &ana, Class::SMALL, now).is_some());
        s.sweep(now + Duration::from_secs(10));
        assert_eq!(s.stats().1, 1, "recently drawn: kept");
        s.sweep(now + TEXTURE_IDLE + Duration::from_secs(1));
        assert_eq!(s.stats(), (1, 0), "nobody looked for a minute: texture freed, bytes kept");
        let later = now + TEXTURE_IDLE + Duration::from_secs(5);
        s.begin_frame(later);
        assert!(s.texture(&ctx, &ana, Class::SMALL, later).is_some());
        assert!(s.take_requests().is_empty(), "rebuilt without asking the hub again");
    }

    #[test]
    fn textures_and_entries_stay_bounded() {
        let (mut s, ctx, now) = (AvatarStore::default(), egui::Context::default(), Instant::now());
        let people = (MAX_ENTRIES + 40) as i64;
        for id in 0..people {
            let t = now + Duration::from_millis(id as u64);
            s.begin_frame(t);
            let u = user(UserId(id), Some("v"));
            s.texture(&ctx, &u, Class::SMALL, t);
            answer_all(&mut *s, t, |_| AvatarReply::Image(jpeg(64, 1)));
            s.texture(&ctx, &u, Class::SMALL, t);
            s.sweep(t);
            let (entries, textures) = s.stats();
            assert!(textures <= MAX_TEXTURES && entries <= MAX_ENTRIES, "{id}: {entries} entries, {textures} textures");
        }
        let (entries, textures) = s.stats();
        assert_eq!((entries, textures), (MAX_ENTRIES, MAX_TEXTURES));
        // The most recently seen kept their textures; the oldest were dropped first.
        assert!(s.has_picture(&user(UserId(people - 1), Some("v"))));
        assert!(!s.answered(&user(UserId(0), Some("v"))));
    }

    #[test]
    fn size_class_follows_the_pixels_not_the_points() {
        assert_eq!(Class::for_diameter(20.0, 1.0), Class::SMALL);
        assert_eq!(Class::for_diameter(32.0, 1.0), Class::SMALL);
        assert_eq!(Class::for_diameter(32.0, 2.0), Class::SMALL, "64 physical pixels still fit the small one");
        assert_eq!(Class::for_diameter(33.0, 2.0), Class::BIG);
        assert_eq!(Class::for_diameter(96.0, 1.0), Class::BIG);
    }

    #[test]
    fn a_chosen_file_is_previewed_as_its_centre_crop_and_sent_as_it_is() {
        // 3 columns of colour in a wide picture: the preview shows the middle one.
        let img = image::RgbImage::from_fn(900, 300, |x, _| match x / 300 {
            0 => image::Rgb([255, 0, 0]),
            1 => image::Rgb([0, 255, 0]),
            _ => image::Rgb([0, 0, 255]),
        });
        let mut file = Vec::new();
        image::DynamicImage::ImageRgb8(img).write_to(&mut std::io::Cursor::new(&mut file), image::ImageFormat::Png).unwrap();
        let p = prepare(file.clone()).unwrap();
        assert_eq!(p.preview.size, [256, 256]);
        for (x, y) in [(5, 5), (128, 128), (250, 250)] {
            let c = p.preview.pixels[y * 256 + x];
            assert!(c.g() > 200 && c.r() < 60 && c.b() < 60, "({x},{y}) {c:?}");
        }
        assert_eq!(p.upload, file, "the file goes to the hub untouched");
    }

    #[test]
    fn files_that_are_not_pictures_or_too_big_are_refused_politely() {
        assert!(prepare(b"MZ not a picture".to_vec()).unwrap_err().contains("não é uma imagem"));
        assert!(prepare(Vec::new()).is_err());
        let mut cut = png(300);
        cut.truncate(40);
        assert!(prepare(cut).is_err());
        // A header that claims a 40000 x 40000 picture is refused before any pixels exist.
        let mut bomb = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        bomb.extend(40_000u32.to_be_bytes());
        bomb.extend(40_000u32.to_be_bytes());
        bomb.extend([8, 2, 0, 0, 0, 0, 0, 0, 0]);
        assert!(prepare(bomb).is_err());
    }

    #[test]
    fn a_file_over_the_upload_limit_is_shrunk_before_the_upload() {
        // Noise does not compress: ~18 MB of PNG for a 2400 x 1800 picture, well over the hub's
        // AVATAR_MAX_UPLOAD_BYTES.
        let mut seed = 12345u32;
        let img = image::RgbImage::from_fn(2400, 1800, |_, _| {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            image::Rgb([(seed >> 8) as u8, (seed >> 16) as u8, (seed >> 24) as u8])
        });
        let mut file = Vec::new();
        image::DynamicImage::ImageRgb8(img).write_to(&mut std::io::Cursor::new(&mut file), image::ImageFormat::Png).unwrap();
        assert!(file.len() > AVATAR_MAX_UPLOAD_BYTES, "{} bytes", file.len());
        let p = prepare(file).unwrap();
        assert!(p.upload.len() <= AVATAR_MAX_UPLOAD_BYTES && p.upload.len() > 1000);
        assert_eq!(&p.upload[..3], &[0xff, 0xd8, 0xff], "a JPEG now");
        assert_eq!(image::load_from_memory(&p.upload).unwrap().width(), 1024);
    }

    #[test]
    fn the_blob_never_prints_its_bytes() {
        let b = Blob(Arc::new(vec![0; 5_000_000]));
        assert_eq!(format!("{:?}", AvatarCmd::Set(b)), "Set(Blob(5000000 bytes))");
    }

    #[test]
    fn hub_refusals_are_explained_in_portuguese() {
        let e = |s: &str| error_text(&anyhow::anyhow!("{s}"), Kind::Avatar);
        assert!(e("set avatar failed: HTTP 429 Too Many Requests: too many picture changes, wait 540 s").contains("9 min"));
        assert!(e("set avatar failed: HTTP 429 Too Many Requests: too many picture changes, wait 12 s").contains("12 s"));
        assert!(e("set avatar failed: HTTP 413 Payload Too Large: picture too large (at most 5 MB)").contains("5 MB"));
        assert!(e("set avatar failed: HTTP 400 Bad Request: not a supported image (use PNG, JPEG, WebP or GIF)").contains("PNG, JPEG"));
        assert!(e("set avatar failed: HTTP 404 Not Found: ").contains("versão antiga"));
        assert!(e("set avatar failed: HTTP 405 Method Not Allowed: ").contains("versão antiga"));
        // The banner's refusals say "banner" where the avatar's say "foto de perfil".
        let b = |s: &str| error_text(&anyhow::anyhow!("{s}"), Kind::Banner);
        assert!(b("set banner failed: HTTP 404 Not Found: ").contains("banner"));
        assert!(b("set banner failed: HTTP 429 Too Many Requests: too many picture changes, wait 90 s").contains("banner"));
    }

    // ---------------------------------------------------------------- animation

    /// A GIF of `n` frames of `side` px squares, each a different colour, `ms` apart.
    fn gif(n: usize, side: u32, ms: u32) -> Vec<u8> {
        use image::codecs::gif::GifEncoder;
        let frames: Vec<image::Frame> = (0..n)
            .map(|i| {
                let img = image::RgbaImage::from_pixel(side, side, image::Rgba([(i * 40) as u8, 200, 30, 255]));
                image::Frame::from_parts(img, 0, 0, image::Delay::from_numer_denom_ms(ms, 1))
            })
            .collect();
        let mut out = Vec::new();
        GifEncoder::new(&mut out).encode_frames(frames.into_iter()).unwrap();
        out
    }

    #[test]
    fn a_gif_decodes_into_its_frames_with_their_delays() {
        let bytes = gif(4, 96, 50);
        let (images, delays) = decode_frames(&bytes, Kind::Avatar).expect("an animation");
        assert_eq!(images.len(), 4);
        assert_eq!(delays, vec![50, 50, 50, 50]);
        // Every frame is a picture of its own.
        assert!(images.iter().all(|i| i.size[0] > 0 && i.size[1] > 0));
    }

    #[test]
    fn a_frame_delay_under_20_ms_is_lifted_to_the_floor() {
        let bytes = gif(3, 32, 5);
        let (_, delays) = decode_frames(&bytes, Kind::Avatar).unwrap();
        assert!(delays.iter().all(|d| *d >= 20), "{delays:?}");
    }

    #[test]
    fn an_animated_frame_is_shrunk_to_the_kinds_cap() {
        // A 600 px avatar GIF comes back as 128² frames; a 600x240 banner keeps its size.
        let (avatar, _) = decode_frames(&gif(2, 600, 40), Kind::Avatar).expect("avatar frames");
        assert!(avatar.iter().all(|i| i.size[0] <= 128 && i.size[1] <= 128), "{:?}", avatar[0].size);
        let (banner, _) = decode_frames(&gif(2, 600, 40), Kind::Banner).expect("banner frames");
        assert!(banner.iter().all(|i| i.size[0] <= 600 && i.size[1] <= 240), "{:?}", banner[0].size);
    }

    #[test]
    fn a_still_is_not_an_animation_and_goes_through_the_old_path() {
        // A one-frame GIF, a PNG and a JPEG are all stills: `decode_frames` says no and the
        // caller falls back to `decode`, so nothing changes for them.
        let one = gif(1, 64, 100);
        assert!(decode_frames(&one, Kind::Avatar).is_none());
        assert!(decode_frames(&png(64), Kind::Avatar).is_none());
        assert!(decode_frames(&jpeg(64, 3), Kind::Avatar).is_none());
        assert!(decode_frames(b"not a picture at all", Kind::Avatar).is_none());
        assert!(decode_frames(&one, Kind::Avatar).is_none(), "and it stays a still");
    }

    #[test]
    fn an_animation_is_asked_for_only_when_the_hub_kept_it_whole() {
        let (mut s, ctx, now) = (AvatarStore::default(), egui::Context::default(), Instant::now());
        s.begin_frame(now);
        // A still: `?anim=1` is never asked for.
        s.texture(&ctx, &user(UserId(1), Some("v1")), Class::SMALL, now);
        assert_eq!(s.take_requests(), vec![Request { user: UserId(1), version: "v1".into(), class: Class::SMALL, kind: Kind::Avatar, anim: false }]);
        // The hub said this one is animated: now it is.
        let mut ana = user(UserId(1), Some("v2"));
        ana.avatar_animated = true;
        s.texture(&ctx, &ana, Class::SMALL, now);
        assert_eq!(s.take_requests(), vec![Request { user: UserId(1), version: "v2".into(), class: Class::SMALL, kind: Kind::Avatar, anim: true }]);
    }

    #[test]
    fn an_animation_plays_frame_by_frame() {
        let (mut s, ctx, now) = (AvatarStore::default(), egui::Context::default(), Instant::now());
        let mut ana = user(UserId(1), Some("v1"));
        ana.avatar_animated = true;
        s.begin_frame(now);
        s.texture(&ctx, &ana, Class::SMALL, now);
        answer_all(&mut *s, now, |_| AvatarReply::Image(gif(3, 64, 100)));
        s.begin_frame(now);
        let first = s.texture(&ctx, &ana, Class::SMALL, now).expect("the first frame");
        assert!(s.has_picture(&ana));
        // Still the same frame while its delay has not passed...
        assert_eq!(s.texture(&ctx, &ana, Class::SMALL, now + Duration::from_millis(50)), Some(first));
        // ...and the next one after it.
        let second = s.texture(&ctx, &ana, Class::SMALL, now + Duration::from_millis(150)).expect("the second frame");
        assert_ne!(second, first);
        let third = s.texture(&ctx, &ana, Class::SMALL, now + Duration::from_millis(300)).expect("the third frame");
        assert_ne!(third, second);
        // And back to the first: it loops.
        let again = s.texture(&ctx, &ana, Class::SMALL, now + Duration::from_millis(450)).expect("loops");
        assert_eq!(again, first);
    }

    #[test]
    fn a_single_frame_animation_costs_one_texture_and_never_advances() {
        let (mut s, ctx, now) = (AvatarStore::default(), egui::Context::default(), Instant::now());
        let mut ana = user(UserId(1), Some("v1"));
        ana.avatar_animated = true;
        s.begin_frame(now);
        s.texture(&ctx, &ana, Class::SMALL, now);
        answer_all(&mut *s, now, |_| AvatarReply::Image(gif(1, 64, 100)));
        s.begin_frame(now);
        let a = s.texture(&ctx, &ana, Class::SMALL, now).expect("decoded as a still");
        assert_eq!(s.texture(&ctx, &ana, Class::SMALL, now + Duration::from_secs(5)), Some(a));
        assert_eq!(s.stats(), (1, 1), "one person, one texture");
    }

    #[test]
    fn frames_stay_bounded_across_people() {
        let (mut s, ctx, now) = (AvatarStore::default(), egui::Context::default(), Instant::now());
        // Each person costs 8 frames: 240 / 8 = 30 people may hold textures at once.
        for id in 0..60i64 {
            let t = now + Duration::from_millis(id as u64);
            s.begin_frame(t);
            let mut u = user(UserId(id), Some("v"));
            u.avatar_animated = true;
            s.texture(&ctx, &u, Class::SMALL, t);
            answer_all(&mut *s, t, |_| AvatarReply::Image(gif(8, 64, 60)));
            s.texture(&ctx, &u, Class::SMALL, t);
            s.sweep(t);
            let frames: usize = s.entries.values().flatten().map(|e| e.frames.len()).sum();
            assert!(frames <= MAX_FRAMES, "{id}: {frames} frames");
        }
        let frames: usize = s.entries.values().flatten().map(|e| e.frames.len()).sum();
        assert_eq!(frames, MAX_FRAMES);
    }

    // ---------------------------------------------------------------- banners

    fn banner_user(id: UserId, banner: Option<&str>) -> User {
        User { banner: banner.map(Into::into), ..User::new(id, format!("u{}", id.0)) }
    }

    #[test]
    fn the_banner_store_asks_for_its_own_picture_at_its_own_size() {
        let (mut s, ctx, now) = (BannerStore::default(), egui::Context::default(), Instant::now());
        s.begin_frame(now);
        assert_eq!(s.texture(&ctx, &banner_user(UserId(1), Some("b1")), Kind::Banner.only(), now), None);
        // (Answering is what takes the request out of the queue, so the answer records it: a
        // 600-wide still, which the still path must accept — 600 is a banner's own size.)
        let mut asked: Vec<Request> = Vec::new();
        answer_all(&mut *s, now, |r| {
            asked.push(r.clone());
            AvatarReply::Image(jpeg(600, 7))
        });
        assert_eq!(
            asked,
            vec![Request { user: UserId(1), version: "b1".into(), class: Kind::Banner.only(), kind: Kind::Banner, anim: false }]
        );
        s.begin_frame(now);
        assert!(s.texture(&ctx, &banner_user(UserId(1), Some("b1")), Kind::Banner.only(), now).is_some());
        assert_eq!(s.stats(), (1, 1));
    }

    #[test]
    fn the_two_stores_are_independent() {
        // A picture of one kind never answers for the other.
        let (mut a, ctx, now) = (AvatarStore::default(), egui::Context::default(), Instant::now());
        let (mut b, _, _) = (BannerStore::default(), egui::Context::default(), Instant::now());
        let u = User { avatar: Some("a".into()), banner: Some("b".into()), ..User::new(UserId(1), "ana") };
        a.begin_frame(now);
        b.begin_frame(now);
        a.texture(&ctx, &u, Class::SMALL, now);
        b.texture(&ctx, &u, Kind::Banner.only(), now);
        let reqs = [a.take_requests(), b.take_requests()].concat();
        assert_eq!(reqs.len(), 2);
        assert!(reqs.iter().all(|r| r.kind == Kind::Avatar || r.kind == Kind::Banner));
        for r in reqs {
            let reply = match r.kind {
                Kind::Avatar => AvatarReply::Image(jpeg(64, 1)),
                Kind::Banner => AvatarReply::Image(jpeg(600, 2)),
            };
            match r.kind {
                Kind::Avatar => a.on_image(r.user, &r.version, r.class, reply, now),
                Kind::Banner => b.on_image(r.user, &r.version, r.class, reply, now),
            }
        }
        // Each store decodes its own answer and only its own.
        assert!(a.texture(&ctx, &u, Class::SMALL, now).is_some(), "the avatar is drawn");
        assert!(b.texture(&ctx, &u, Kind::Banner.only(), now).is_some(), "the banner is drawn");
        assert!(a.has_picture(&u) && b.has_picture(&u));
        assert_eq!(a.stats(), (1, 1));
        assert_eq!(b.stats(), (1, 1));
    }

    #[test]
    fn a_banner_without_one_draws_nothing() {
        let (mut s, ctx, now) = (BannerStore::default(), egui::Context::default(), Instant::now());
        s.begin_frame(now);
        assert_eq!(s.texture(&ctx, &banner_user(UserId(1), None), Kind::Banner.only(), now), None);
        assert!(s.take_requests().is_empty());
        assert_eq!(s.size(&banner_user(UserId(1), None)), None);
    }

    #[test]
    fn cover_uv_crops_instead_of_stretching() {
        let band = Rect::from_min_size(Pos2::ZERO, Vec2::new(320.0, 72.0));
        // A square picture in a wide band: the band is filled edge to edge and the picture is
        // cropped top and bottom — never stretched, never letterboxed.
        let square = cover_uv(band, [600, 600]);
        assert!((square.left()).abs() < 1e-6 && (square.right() - 1.0).abs() < 1e-6, "{square:?}");
        assert!(square.height() < 0.5, "{:?}", square);
        assert!((square.center().y - 0.5).abs() < 1e-6, "centred");
        // A tall picture behaves the same way.
        let tall = cover_uv(band, [100, 600]);
        assert!((tall.left()).abs() < 1e-6 && (tall.right() - 1.0).abs() < 1e-6);
        assert!((tall.center().y - 0.5).abs() < 1e-6);
        // A picture of the band's own proportion: all of it.
        let exact = cover_uv(band, [600, 135]);
        assert!((exact.width() - 1.0).abs() < 1e-6 && (exact.height() - 1.0).abs() < 1e-6, "{exact:?}");
        // A very wide picture in a wide band: the full height, the sides cropped.
        let wide = cover_uv(band, [1200, 120]);
        assert!((wide.top()).abs() < 1e-6 && (wide.bottom() - 1.0).abs() < 1e-6, "{wide:?}");
        assert!(wide.width() < 0.5, "{:?}", wide);
        assert!((wide.center().x - 0.5).abs() < 1e-6);
    }

    #[test]
    fn a_banner_is_previewed_as_a_wide_crop_and_sent_as_it_is() {
        // Three horizontal bands of colour: the preview shows the middle one.
        let img = image::RgbImage::from_fn(900, 900, |_, y| match y / 300 {
            0 => image::Rgb([255, 0, 0]),
            1 => image::Rgb([0, 255, 0]),
            _ => image::Rgb([0, 0, 255]),
        });
        let mut file = Vec::new();
        image::DynamicImage::ImageRgb8(img).write_to(&mut std::io::Cursor::new(&mut file), image::ImageFormat::Png).unwrap();
        let p = prepare_banner(file.clone()).unwrap();
        assert_eq!(p.preview.size, [600, 240]);
        for (x, y) in [(5, 120), (300, 120), (595, 120)] {
            let c = p.preview.pixels[y * 600 + x];
            assert!(c.g() > 200 && c.r() < 60 && c.b() < 60, "({x},{y}) {c:?}");
        }
        assert_eq!(p.upload, file, "the file goes to the hub untouched");
        // The avatar's preview is still the circle's square.
        let a = prepare(file).unwrap();
        assert_eq!(a.preview.size, [256, 256]);
    }
}
