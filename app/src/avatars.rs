//! Profile pictures ("Foto de perfil") in the app.
//!
//! The hub stores every picture as a 64 px and a 256 px square and puts its version (a hash) on
//! the person's `User`. This module
//! - keeps the pictures of the people we see: [`AvatarStore`] decides what to ask the hub for,
//!   remembers the answers by `(person, version, size)` and keeps the number of egui textures
//!   bounded. It knows nothing about the network: [`AvatarStore::take_requests`] hands out what
//!   should be asked and the app feeds the answers back with [`AvatarStore::on_image`], so the
//!   rules (queueing, 404 = keep the initial, retries, texture freeing) are unit-testable;
//! - draws them as circles ([`draw_avatar`], [`avatar_widget`]), falling back to the coloured
//!   initial while a picture loads, when there is none, and on a hub that predates pictures;
//! - is the "Foto de perfil" section of the settings: a native file dialog, a circular preview of
//!   the centre crop, "Salvar" and "Remover".

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use eframe::egui::{self, epaint, Align2, Color32, FontId, Pos2, Rect, Sense, TextureHandle, TextureId, TextureOptions, Vec2};
use vox_client_core::hub::{AvatarReply, HubClient};
use vox_proto::{User, UserId, AVATAR_MAX_UPLOAD_BYTES};

use crate::app::{name_color, App};
use crate::backend::Cmd;

/// Requests in flight at once.
const MAX_IN_FLIGHT: usize = 4;
/// After a failed download, ask again no sooner than this (and only if the person is on screen).
const RETRY_AFTER: Duration = Duration::from_secs(30);
/// A texture nobody drew for this long is freed (the downloaded bytes stay, a few KB).
const TEXTURE_IDLE: Duration = Duration::from_secs(60);
/// Textures alive at once, whatever is on screen: 48 x 256 KB at most.
pub const MAX_TEXTURES: usize = 48;
/// Downloaded pictures remembered per session; the least recently seen go first.
pub const MAX_ENTRIES: usize = 160;
/// Textures decoded per frame, so a screen full of new faces doesn't hitch.
const DECODES_PER_FRAME: u32 = 4;
/// The hub promises 64 / 256 px squares; a decoded picture bigger than this is refused.
const MAX_SIDE: u32 = 512;

// ------------------------------------------------------------------ the store

/// Which of the two stored sizes to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Class {
    Small,
    Big,
}

impl Class {
    /// The size (px) the hub is asked for.
    pub fn px(self) -> u32 {
        match self {
            Class::Small => 64,
            Class::Big => 256,
        }
    }

    /// The 64 px picture is enough up to 64 physical pixels; beyond that the 256 px one.
    pub fn for_diameter(points: f32, pixels_per_point: f32) -> Class {
        if points * pixels_per_point > 64.0 {
            Class::Big
        } else {
            Class::Small
        }
    }

    fn other(self) -> Class {
        match self {
            Class::Small => Class::Big,
            Class::Big => Class::Small,
        }
    }
}

/// What the store wants asked of the hub.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub user: UserId,
    pub version: String,
    pub class: Class,
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
    tex: Option<TextureHandle>,
    last_seen: Instant,
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

#[derive(Default)]
pub struct AvatarStore {
    entries: HashMap<UserId, Vec<Entry>>,
    /// Pictures (not initials) drawn since the last [`AvatarStore::clear_drawn`].
    drawn: HashSet<(UserId, Surface)>,
    queue: VecDeque<Request>,
    in_flight: usize,
    decodes_left: u32,
    last_sweep: Option<Instant>,
}

impl AvatarStore {
    /// A new hub session: forget everything (another server has other pictures).
    pub fn reset(&mut self) {
        *self = AvatarStore::default();
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
        let version = user.avatar.as_deref()?;
        let list = self.entries.entry(user.id).or_default();
        let idx = match list.iter().position(|e| e.version == version && e.class == class) {
            Some(i) => i,
            None => {
                list.push(Entry { version: version.to_string(), class, state: State::Loading, tex: None, last_seen: now });
                self.queue.push_back(Request { user: user.id, version: version.to_string(), class });
                list.len() - 1
            }
        };
        let e = &mut list[idx];
        e.last_seen = now;
        if matches!(e.state, State::Retry { at } if now >= at) {
            e.state = State::Loading;
            self.queue.push_back(Request { user: user.id, version: version.to_string(), class });
        }
        if e.tex.is_none() && matches!(e.state, State::Bytes(_)) {
            if self.decodes_left > 0 {
                self.decodes_left -= 1;
                let decoded = match &e.state {
                    State::Bytes(b) => decode(b),
                    _ => None,
                };
                match decoded {
                    Some(img) => e.tex = Some(ctx.load_texture(format!("avatar-{}-{}-{}", user.id, e.version, class.px()), img, TextureOptions::LINEAR)),
                    None => e.state = State::Broken,
                }
            } else {
                ctx.request_repaint(); // out of decode budget this frame: the rest next frame
            }
        }
        if let Some(t) = &e.tex {
            return Some(t.id());
        }
        // The other size of the same picture will do until this one arrives.
        let other = class.other();
        let e = list.iter_mut().find(|e| e.version == version && e.class == other)?;
        e.last_seen = now;
        e.tex.as_ref().map(|t| t.id())
    }

    /// `user`'s current picture if it is already on the GPU (any size); never asks the hub for it
    /// and marks nothing as seen (the Calladex shows what the app already has, nothing more).
    pub fn loaded(&self, user: &User) -> Option<TextureId> {
        let version = user.avatar.as_deref()?;
        self.entries.get(&user.id)?.iter().filter(|e| e.version == version).find_map(|e| e.tex.as_ref().map(|t| t.id()))
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
                tracing::debug!(user, why, "avatar download failed");
                State::Retry { at: now + RETRY_AFTER }
            }
        };
    }

    /// Housekeeping: free textures nobody drew for a while or beyond [`MAX_TEXTURES`], and trim
    /// the store to [`MAX_ENTRIES`] (what is being downloaded stays).
    pub fn sweep(&mut self, now: Instant) {
        for e in self.entries.values_mut().flatten() {
            if e.tex.is_some() && now.duration_since(e.last_seen) > TEXTURE_IDLE {
                e.tex = None;
            }
        }
        let mut with_tex: Vec<(Instant, UserId, usize)> =
            self.entries.iter().flat_map(|(u, l)| l.iter().enumerate().filter(|(_, e)| e.tex.is_some()).map(|(i, e)| (e.last_seen, *u, i))).collect();
        if with_tex.len() > MAX_TEXTURES {
            with_tex.sort();
            let excess = with_tex.len() - MAX_TEXTURES;
            for (_, u, i) in with_tex.into_iter().take(excess) {
                if let Some(e) = self.entries.get_mut(&u).and_then(|l| l.get_mut(i)) {
                    e.tex = None;
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

    fn textures(&self) -> usize {
        self.entries.values().flatten().filter(|e| e.tex.is_some()).count()
    }

    /// Was `user`'s real picture (not the initial) drawn on `surface` since [`Self::clear_drawn`]?
    pub fn was_drawn(&self, user: UserId, surface: Surface) -> bool {
        self.drawn.contains(&(user, surface))
    }

    pub fn clear_drawn(&mut self) {
        self.drawn.clear();
    }

    /// (pictures remembered, textures alive) for the autotest log.
    pub fn stats(&self) -> (usize, usize) {
        (self.entries.values().map(Vec::len).sum(), self.textures())
    }

    /// Is `user`'s picture of this version ready to draw (at either size)?
    #[cfg(test)]
    pub fn has_picture(&self, user: &User) -> bool {
        let Some(v) = user.avatar.as_deref() else { return false };
        self.entries.get(&user.id).is_some_and(|l| l.iter().any(|e| e.version == v && e.tex.is_some()))
    }

    /// Has the hub answered for `user`'s picture of this version (any size), whatever the answer?
    #[cfg(test)]
    pub fn answered(&self, user: &User) -> bool {
        let Some(v) = user.avatar.as_deref() else { return false };
        self.entries.get(&user.id).is_some_and(|l| l.iter().any(|e| e.version == v && !matches!(e.state, State::Loading)))
    }
}

/// Decode the hub's JPEG / PNG square. Anything bigger than [`MAX_SIDE`] or not square-ish is
/// refused.
pub fn decode(bytes: &[u8]) -> Option<egui::ColorImage> {
    let img = image::load_from_memory(bytes).ok()?;
    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE {
        return None;
    }
    let rgba = img.to_rgba8();
    Some(egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], rgba.as_raw()))
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
    Fetch(Request),
}

#[derive(Debug)]
pub enum AvatarEv {
    /// My upload / removal finished: the new version (`None` after a removal), or a message in
    /// Portuguese.
    Changed(Result<Option<String>, String>),
    /// A moderator removal finished.
    ModRemoved(Result<(), String>),
    Image { request: Request, reply: AvatarReply },
}

impl AvatarCmd {
    /// The event for a command that cannot run (no hub connection).
    pub fn offline(self) -> AvatarEv {
        let msg = "Sem conexão com o servidor.".to_string();
        match self {
            AvatarCmd::Set(_) | AvatarCmd::Clear => AvatarEv::Changed(Err(msg)),
            AvatarCmd::ModRemove { .. } => AvatarEv::ModRemoved(Err(msg)),
            AvatarCmd::Fetch(request) => AvatarEv::Image { request, reply: AvatarReply::Failed("offline".into()) },
        }
    }
}

pub async fn run(hub: &HubClient, cmd: AvatarCmd) -> AvatarEv {
    match cmd {
        AvatarCmd::Set(file) => {
            let r = hub.set_avatar(file.0.as_ref().clone()).await;
            AvatarEv::Changed(r.map(|a| a.avatar).map_err(|e| error_text(&e)))
        }
        AvatarCmd::Clear => AvatarEv::Changed(hub.clear_avatar().await.map(|a| a.avatar).map_err(|e| error_text(&e))),
        AvatarCmd::ModRemove { user } => AvatarEv::ModRemoved(hub.mod_remove_avatar(user).await.map_err(|e| error_text(&e))),
        AvatarCmd::Fetch(request) => {
            let reply = hub.avatar_image(request.user, request.class.px(), &request.version).await;
            AvatarEv::Image { request, reply }
        }
    }
}

/// The hub's refusals, for the people.
pub fn error_text(e: &anyhow::Error) -> String {
    let full = format!("{e:#}");
    let l = full.to_lowercase();
    tracing::warn!(error = %full, "profile picture call failed");
    if l.contains("http 429") || l.contains("too many picture changes") {
        let wait = l.split("wait ").nth(1).and_then(|r| r.split_whitespace().next()).and_then(|n| n.parse::<u64>().ok());
        match wait {
            Some(s) if s >= 60 => format!("Você trocou a foto muitas vezes. Tente de novo em {} min.", s.div_ceil(60)),
            Some(s) => format!("Você trocou a foto muitas vezes. Tente de novo em {s} s."),
            None => "Você trocou a foto muitas vezes. Tente de novo mais tarde.".into(),
        }
    } else if l.contains("http 413") || l.contains("picture too large") {
        "A imagem é grande demais (no máximo 5 MB).".into()
    } else if l.contains("dimensions are too large") {
        "A imagem tem dimensões grandes demais.".into()
    } else if l.contains("http 400") || l.contains("not a supported image") || l.contains("could not read the image") {
        "Esse arquivo não é uma imagem que o servidor aceite (use PNG, JPEG, WebP ou GIF).".into()
    } else if l.contains("http 404") || l.contains("http 405") {
        "Este servidor é de uma versão antiga e ainda não aceita foto de perfil.".into()
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

/// A chosen file, ready to show and to upload.
#[derive(Debug)]
pub struct Prepared {
    /// What is sent: the file as it is, or (files over 5 MB) a smaller JPEG of the same picture.
    pub upload: Vec<u8>,
    /// The centre crop the hub will make, [`PREVIEW_PX`] square.
    pub preview: egui::ColorImage,
}

/// Decode the chosen file (under the hub's limits), compute the preview and make it fit 5 MB.
/// Errors are Portuguese, for the people.
pub fn prepare(bytes: Vec<u8>) -> Result<Prepared, String> {
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
    let square = img.resize_to_fill(PREVIEW_PX, PREVIEW_PX, image::imageops::FilterType::Triangle);
    let rgba = square.to_rgba8();
    let preview = egui::ColorImage::from_rgba_unmultiplied([rgba.width() as usize, rgba.height() as usize], rgba.as_raw());
    let upload = if bytes.len() <= AVATAR_MAX_UPLOAD_BYTES {
        bytes
    } else {
        // Over 5 MB: send a 1024 px JPEG of the same picture instead.
        let small = img.resize_to_fill(1024, 1024, image::imageops::FilterType::Triangle).to_rgb8();
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90).encode_image(&small).map_err(|_| not_image())?;
        out
    };
    Ok(Prepared { upload, preview })
}

mod dialog {
    /// Ask for one image file. `Ok(None)`: the person cancelled. Blocks until the dialog closes
    /// (callers run it on its own thread), with `owner` as the dialog's parent window.
    pub fn pick_image(owner: Option<isize>) -> Result<Option<std::path::PathBuf>, String> {
        crate::filedlg::open(owner, "Escolher foto de perfil", crate::filedlg::IMAGE_FILTER, false).map(|v| v.into_iter().next())
    }
}

/// What the picking thread reports.
enum Picked {
    Cancelled,
    Failed(String),
    Ready { name: String, prepared: Prepared },
}

// ------------------------------------------------------------------ the settings section

/// The "Foto de perfil" section's state.
#[derive(Default)]
pub struct ProfileUi {
    rx: Option<mpsc::Receiver<Picked>>,
    /// The file dialog (or the decoding after it) is running.
    pub picking: bool,
    /// A chosen, not yet saved picture.
    pub picked: Option<PickedFile>,
    /// An upload / removal is running.
    pub busy: bool,
    /// (ok, text) to show under the buttons.
    pub msg: Option<(bool, String)>,
}

pub struct PickedFile {
    pub name: String,
    upload: Blob,
    preview: TextureHandle,
}

/// Everything about pictures an `App` holds.
#[derive(Default)]
pub struct Avatars {
    pub store: AvatarStore,
    pub profile: ProfileUi,
}

impl App {
    /// Send the backend whatever the store wants asked right now.
    pub fn pump_avatars(&mut self) {
        for r in self.avatars.store.take_requests() {
            self.backend.send(Cmd::Avatar(AvatarCmd::Fetch(r)));
        }
    }

    /// The backend's answers about pictures.
    pub fn on_avatar_event(&mut self, ev: AvatarEv) {
        match ev {
            AvatarEv::Image { request, reply } => {
                self.avatars.store.on_image(request.user, &request.version, request.class, reply, Instant::now());
                self.pump_avatars();
            }
            AvatarEv::Changed(result) => {
                let p = &mut self.avatars.profile;
                p.busy = false;
                match result {
                    Ok(version) => {
                        p.msg = Some((true, if version.is_some() { "Foto salva." } else { "Foto removida." }.into()));
                        p.picked = None;
                        // The hub's `UserUpdated` tells everybody (me included); this keeps my own
                        // screen right even if that event is late.
                        if let Some(me) = self.me.as_mut() {
                            me.avatar = version;
                        }
                    }
                    Err(e) => p.msg = Some((false, e)),
                }
            }
            AvatarEv::ModRemoved(result) => match result {
                Ok(()) => self.toast("Foto de perfil removida."),
                Err(e) => self.notice = Some(e),
            },
        }
    }

    /// "Escolher imagem...": the native dialog on its own thread, then decoding.
    pub fn pick_profile_picture(&mut self, ctx: &egui::Context) {
        if self.avatars.profile.picking {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let (owner, ctx) = (self.video.parent, ctx.clone());
        std::thread::spawn(move || {
            let msg = match dialog::pick_image(owner) {
                Ok(None) => Picked::Cancelled,
                Err(e) => Picked::Failed(e),
                Ok(Some(path)) => match read_file(&path) {
                    Ok(bytes) => match prepare(bytes) {
                        Ok(prepared) => Picked::Ready { name: path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(), prepared },
                        Err(e) => Picked::Failed(e),
                    },
                    Err(e) => Picked::Failed(e),
                },
            };
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
        let p = &mut self.avatars.profile;
        p.rx = Some(rx);
        p.picking = true;
        p.msg = None;
    }

    /// Collect the result of the picking thread (called every frame while the settings are open).
    fn poll_profile_picture(&mut self, ctx: &egui::Context) {
        let Some(msg) = self.avatars.profile.rx.as_ref().and_then(|rx| rx.try_recv().ok()) else { return };
        let p = &mut self.avatars.profile;
        p.rx = None;
        p.picking = false;
        match msg {
            Picked::Cancelled => {}
            Picked::Failed(e) => p.msg = Some((false, e)),
            Picked::Ready { name, prepared } => {
                let preview = ctx.load_texture("profile-picture-preview", prepared.preview, TextureOptions::LINEAR);
                p.picked = Some(PickedFile { name, upload: Blob(Arc::new(prepared.upload)), preview });
            }
        }
    }

    /// The chosen picture as a file's bytes (the dialog's path, and the autotest's). Errors show
    /// in the section.
    pub fn choose_profile_picture_bytes(&mut self, ctx: &egui::Context, name: &str, bytes: Vec<u8>) {
        let p = &mut self.avatars.profile;
        p.msg = None;
        match prepare(bytes) {
            Ok(prepared) => {
                let preview = ctx.load_texture("profile-picture-preview", prepared.preview, TextureOptions::LINEAR);
                p.picked = Some(PickedFile { name: name.to_string(), upload: Blob(Arc::new(prepared.upload)), preview });
            }
            Err(e) => p.msg = Some((false, e)),
        }
    }

    /// "Salvar": upload the chosen picture.
    pub fn save_profile_picture(&mut self) {
        let p = &mut self.avatars.profile;
        let Some(file) = p.picked.as_ref().filter(|_| !p.busy) else { return };
        let blob = file.upload.clone();
        p.busy = true;
        p.msg = None;
        self.backend.send(Cmd::Avatar(AvatarCmd::Set(blob)));
    }

    /// "Remover foto": delete my picture.
    pub fn remove_profile_picture(&mut self) {
        let p = &mut self.avatars.profile;
        if p.busy {
            return;
        }
        p.busy = true;
        p.msg = None;
        self.backend.send(Cmd::Avatar(AvatarCmd::Clear));
    }

    /// A moderator removes somebody's picture ("Remover foto" in the member menu).
    pub fn mod_remove_picture(&mut self, user: UserId) {
        self.backend.send(Cmd::Avatar(AvatarCmd::ModRemove { user }));
    }

    /// "Foto de perfil" in Configurações → Conta e perfil. Returns whether it was shown (an older
    /// hub has no pictures).
    pub fn profile_picture_section(&mut self, ui: &mut egui::Ui) -> bool {
        use crate::theme::{self, ph, space};
        use crate::ui_kit::{self, Kind, Size};
        // Pictures came with the same hub release as roles: an older hub has neither.
        if !self.roles.supported {
            return false;
        }
        let ctx = ui.ctx().clone();
        self.poll_profile_picture(&ctx);
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
                        save = ui.add_enabled_ui(!busy, |ui| ui_kit::button_sized(ui, Kind::Primary, Size::Small, None, if busy { "Salvando..." } else { "Salvar foto" })).inner.clicked();
                        cancel = ui.add_enabled_ui(!busy, |ui| ui_kit::button_sized(ui, Kind::Ghost, Size::Small, None, "Cancelar")).inner.clicked();
                    });
                }
                ui.horizontal(|ui| {
                    let label = if picking { "Escolhendo..." } else { "Escolher imagem…" };
                    choose = ui.add_enabled_ui(!picking && !busy, |ui| ui_kit::button_sized(ui, Kind::Secondary, Size::Small, Some(ph::IMAGE), label)).inner.clicked();
                    let has = me.as_ref().is_some_and(|m| m.avatar.is_some());
                    if has && self.avatars.profile.picked.is_none() {
                        remove = ui.add_enabled_ui(!busy, |ui| ui_kit::button_sized(ui, Kind::Ghost, Size::Small, Some(ph::TRASH), "Remover foto")).inner.clicked();
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
            self.avatars.profile.picked = None;
        }
        if save {
            self.save_profile_picture();
        }
        if remove {
            self.remove_profile_picture();
        }
        true
    }
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

    fn user(id: i64, avatar: Option<&str>) -> User {
        User { avatar: avatar.map(Into::into), ..User::new(id, format!("u{id}")) }
    }

    /// Ask for everything pending and answer it all with `reply`.
    fn answer_all(s: &mut AvatarStore, now: Instant, reply: impl Fn(&Request) -> AvatarReply) {
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
        assert_eq!(s.texture(&ctx, &user(1, None), Class::Small, now), None);
        assert!(s.take_requests().is_empty());
        assert_eq!(s.stats(), (0, 0));
    }

    #[test]
    fn a_picture_is_asked_once_however_often_it_is_drawn_then_shown() {
        let (mut s, ctx, now) = (AvatarStore::default(), egui::Context::default(), Instant::now());
        let ana = user(1, Some("v1"));
        for _ in 0..30 {
            s.begin_frame(now);
            assert_eq!(s.texture(&ctx, &ana, Class::Small, now), None, "the initial while it loads");
        }
        assert_eq!(s.take_requests(), vec![Request { user: 1, version: "v1".into(), class: Class::Small }]);
        assert!(s.take_requests().is_empty());
        assert!(!s.answered(&ana));
        s.on_image(1, "v1", Class::Small, AvatarReply::Image(jpeg(64, 1)), now);
        assert!(s.answered(&ana) && !s.has_picture(&ana), "answered, not decoded until drawn");
        s.begin_frame(now);
        assert!(s.texture(&ctx, &ana, Class::Small, now).is_some());
        assert!(s.has_picture(&ana));
        assert_eq!(s.stats(), (1, 1));
        assert!(s.take_requests().is_empty(), "nothing more to ask");
    }

    #[test]
    fn at_most_four_requests_run_at_once() {
        let (mut s, ctx, now) = (AvatarStore::default(), egui::Context::default(), Instant::now());
        s.begin_frame(now);
        for id in 0..10 {
            s.texture(&ctx, &user(id, Some("v")), Class::Small, now);
        }
        assert_eq!(s.take_requests().len(), MAX_IN_FLIGHT);
        assert!(s.take_requests().is_empty(), "the window is full");
        s.on_image(0, "v", Class::Small, AvatarReply::NotFound, now);
        assert_eq!(s.take_requests().len(), 1);
    }

    #[test]
    fn the_other_size_stands_in_while_one_loads() {
        let (mut s, ctx, now) = (AvatarStore::default(), egui::Context::default(), Instant::now());
        let ana = user(1, Some("v1"));
        s.begin_frame(now);
        s.texture(&ctx, &ana, Class::Small, now);
        s.take_requests();
        s.on_image(1, "v1", Class::Small, AvatarReply::Image(jpeg(64, 1)), now);
        let small = s.texture(&ctx, &ana, Class::Small, now).expect("small is ready");
        // The big one is wanted now (a tile): until it arrives, the small picture is drawn.
        assert_eq!(s.texture(&ctx, &ana, Class::Big, now), Some(small));
        assert_eq!(s.take_requests(), vec![Request { user: 1, version: "v1".into(), class: Class::Big }]);
        s.on_image(1, "v1", Class::Big, AvatarReply::Image(jpeg(256, 1)), now);
        let big = s.texture(&ctx, &ana, Class::Big, now).expect("big is ready");
        assert_ne!(big, small);
    }

    #[test]
    fn a_404_keeps_the_initial_and_is_final_for_that_version() {
        let (mut s, ctx, now) = (AvatarStore::default(), egui::Context::default(), Instant::now());
        let ana = user(1, Some("v1"));
        s.begin_frame(now);
        s.texture(&ctx, &ana, Class::Small, now);
        answer_all(&mut s, now, |_| AvatarReply::NotFound);
        let later = now + Duration::from_secs(3600);
        s.begin_frame(later);
        assert_eq!(s.texture(&ctx, &ana, Class::Small, later), None);
        assert!(s.take_requests().is_empty(), "not asked again, however long it takes");
        // A new version is a new picture.
        let newer = user(1, Some("v2"));
        s.texture(&ctx, &newer, Class::Small, later);
        assert_eq!(s.take_requests(), vec![Request { user: 1, version: "v2".into(), class: Class::Small }]);
    }

    #[test]
    fn failures_are_retried_later_and_only_while_drawn() {
        let (mut s, ctx, now) = (AvatarStore::default(), egui::Context::default(), Instant::now());
        let ana = user(1, Some("v1"));
        s.begin_frame(now);
        s.texture(&ctx, &ana, Class::Small, now);
        answer_all(&mut s, now, |_| AvatarReply::Failed("boom".into()));
        s.texture(&ctx, &ana, Class::Small, now + Duration::from_secs(5));
        assert!(s.take_requests().is_empty(), "too soon");
        // Nobody draws her for a while: nothing is asked in the background...
        assert!(s.take_requests().is_empty());
        // ...and the first frame that draws her after the wait asks again.
        let later = now + RETRY_AFTER + Duration::from_secs(1);
        s.texture(&ctx, &ana, Class::Small, later);
        assert_eq!(s.take_requests().len(), 1);
    }

    #[test]
    fn a_broken_picture_keeps_the_initial_without_looping() {
        let (mut s, ctx, now) = (AvatarStore::default(), egui::Context::default(), Instant::now());
        let ana = user(1, Some("v1"));
        s.begin_frame(now);
        s.texture(&ctx, &ana, Class::Small, now);
        s.take_requests();
        s.on_image(1, "v1", Class::Small, AvatarReply::Image(b"<html>nope</html>".to_vec()), now);
        for _ in 0..5 {
            s.begin_frame(now);
            assert_eq!(s.texture(&ctx, &ana, Class::Small, now), None);
        }
        assert!(s.take_requests().is_empty());
        // A picture bigger than the hub ever sends is refused too.
        s.texture(&ctx, &user(2, Some("v")), Class::Big, now);
        s.take_requests();
        s.on_image(2, "v", Class::Big, AvatarReply::Image(jpeg(600, 1)), now);
        s.begin_frame(now);
        assert_eq!(s.texture(&ctx, &user(2, Some("v")), Class::Big, now), None);
    }

    #[test]
    fn late_answers_for_forgotten_pictures_are_harmless() {
        let (mut s, ctx, now) = (AvatarStore::default(), egui::Context::default(), Instant::now());
        s.begin_frame(now);
        s.texture(&ctx, &user(1, Some("v1")), Class::Small, now);
        s.take_requests();
        s.reset(); // another server
        s.on_image(1, "v1", Class::Small, AvatarReply::Image(jpeg(64, 1)), now);
        assert_eq!(s.stats(), (0, 0));
        assert!(s.take_requests().is_empty());
    }

    #[test]
    fn idle_textures_are_freed_and_come_back_from_the_kept_bytes() {
        let (mut s, ctx, now) = (AvatarStore::default(), egui::Context::default(), Instant::now());
        let ana = user(1, Some("v1"));
        s.begin_frame(now);
        s.texture(&ctx, &ana, Class::Small, now);
        answer_all(&mut s, now, |_| AvatarReply::Image(jpeg(64, 1)));
        s.begin_frame(now);
        assert!(s.texture(&ctx, &ana, Class::Small, now).is_some());
        s.sweep(now + Duration::from_secs(10));
        assert_eq!(s.stats().1, 1, "recently drawn: kept");
        s.sweep(now + TEXTURE_IDLE + Duration::from_secs(1));
        assert_eq!(s.stats(), (1, 0), "nobody looked for a minute: texture freed, bytes kept");
        let later = now + TEXTURE_IDLE + Duration::from_secs(5);
        s.begin_frame(later);
        assert!(s.texture(&ctx, &ana, Class::Small, later).is_some());
        assert!(s.take_requests().is_empty(), "rebuilt without asking the hub again");
    }

    #[test]
    fn textures_and_entries_stay_bounded() {
        let (mut s, ctx, now) = (AvatarStore::default(), egui::Context::default(), Instant::now());
        let people = (MAX_ENTRIES + 40) as i64;
        for id in 0..people {
            let t = now + Duration::from_millis(id as u64);
            s.begin_frame(t);
            let u = user(id, Some("v"));
            s.texture(&ctx, &u, Class::Small, t);
            answer_all(&mut s, t, |_| AvatarReply::Image(jpeg(64, 1)));
            s.texture(&ctx, &u, Class::Small, t);
            s.sweep(t);
            let (entries, textures) = s.stats();
            assert!(textures <= MAX_TEXTURES && entries <= MAX_ENTRIES, "{id}: {entries} entries, {textures} textures");
        }
        let (entries, textures) = s.stats();
        assert_eq!((entries, textures), (MAX_ENTRIES, MAX_TEXTURES));
        // The most recently seen kept their textures; the oldest were dropped first.
        assert!(s.has_picture(&user(people - 1, Some("v"))));
        assert!(!s.answered(&user(0, Some("v"))));
    }

    #[test]
    fn size_class_follows_the_pixels_not_the_points() {
        assert_eq!(Class::for_diameter(20.0, 1.0), Class::Small);
        assert_eq!(Class::for_diameter(32.0, 1.0), Class::Small);
        assert_eq!(Class::for_diameter(32.0, 2.0), Class::Small, "64 physical pixels still fit the small one");
        assert_eq!(Class::for_diameter(33.0, 2.0), Class::Big);
        assert_eq!(Class::for_diameter(96.0, 1.0), Class::Big);
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
    fn a_file_over_5_mb_is_shrunk_before_the_upload() {
        // Noise does not compress: ~8 MB of PNG for a 1900 x 1400 picture.
        let mut seed = 12345u32;
        let img = image::RgbImage::from_fn(1900, 1400, |_, _| {
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
        let e = |s: &str| error_text(&anyhow::anyhow!("{s}"));
        assert!(e("set avatar failed: HTTP 429 Too Many Requests: too many picture changes, wait 540 s").contains("9 min"));
        assert!(e("set avatar failed: HTTP 429 Too Many Requests: too many picture changes, wait 12 s").contains("12 s"));
        assert!(e("set avatar failed: HTTP 413 Payload Too Large: picture too large (at most 5 MB)").contains("5 MB"));
        assert!(e("set avatar failed: HTTP 400 Bad Request: not a supported image (use PNG, JPEG, WebP or GIF)").contains("PNG, JPEG"));
        assert!(e("set avatar failed: HTTP 404 Not Found: ").contains("versão antiga"));
        assert!(e("set avatar failed: HTTP 405 Method Not Allowed: ").contains("versão antiga"));
    }
}
