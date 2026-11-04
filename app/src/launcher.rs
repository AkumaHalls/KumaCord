//! The launch cinematic: the community's own edit as the background of the home screen, with the
//! login card over it — the League of Legends launcher's idea, in the community's own footage.
//!
//! The whole movie is a sequence of WebP frames packed at build time (`intro.pack`: magic, size,
//! fps, frame lengths, then the images), shipped **next to the exe** (`intro\intro.pack`, put
//! there by the installer and the portable zip) instead of inside it: a three-minute cinematic is
//! ~60 MB, and the exe stays small for the auto-update. No video decoder is linked either: one
//! worker thread reads and decodes the frame for the current moment into an egui texture while the
//! UI stays responsive, and only two textures at most are alive at once (960 × 405 each).
//!
//! The music is the movie's own track (`intro\intro.mp3`), decoded once by the backend
//! ([`vox_client_core::audio::clip`]) and looped through the mixer.
//!
//! Everything here is best-effort and silent: a missing or damaged pack simply leaves the plain
//! gradient background, and autotests get a still first frame (no decode loop, no music) so they
//! stay fast.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, Pos2, Rect, TextureHandle, TextureOptions};

/// The pack format's magic (see `tools/intro/pack.py`).
const MAGIC: &[u8; 5] = b"AKUI1";
/// The background of the home screen before the first frame arrives (and behind a still one).
pub const BACKDROP: Color32 = Color32::from_rgb(0x08, 0x09, 0x12);
/// The scrims' colour (near-black, a touch blue).
const SCRIM: Color32 = Color32::from_rgb(0x06, 0x08, 0x14);

/// Where this build keeps the cinematic and its music: `intro\` next to the exe (the installer and
/// the portable zip put them there), or the source tree when running from `cargo run`, the
/// autotests or a dev build.
pub(crate) fn asset_dir() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("intro"));
            candidates.push(dir.to_path_buf());
        }
    }
    candidates.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("assets").join("intro"));
    candidates.into_iter().find(|d| d.join("intro.pack").is_file())
}

/// The movie's file, when this build has one.
pub(crate) fn pack_path() -> Option<PathBuf> {
    asset_dir().map(|d| d.join("intro.pack")).filter(|p| p.is_file())
}

/// The movie's soundtrack, when this build has one.
pub(crate) fn music_path() -> Option<PathBuf> {
    asset_dir().map(|d| d.join("intro.mp3")).filter(|p| p.is_file())
}

/// One frame: where it sits in the pack and how long it is.
#[derive(Clone, Copy)]
struct Frame {
    offset: u64,
    len: usize,
}

/// The pack's header, parsed once (the frames are read on demand, never all at once).
struct Frames {
    frames: Vec<Frame>,
    width: usize,
    height: usize,
    fps: f32,
}

impl Frames {
    fn parse(path: &Path) -> Option<Self> {
        let mut file = File::open(path).ok()?;
        // Only the header and the frame table are read here (a few KB, however long the movie).
        let mut head = [0u8; 15];
        file.read_exact(&mut head).ok()?;
        if &head[..5] != MAGIC {
            return None;
        }
        let u16_at = |i: usize| u16::from_le_bytes([head[i], head[i + 1]]);
        let u32_at = |i: usize| u32::from_le_bytes([head[i], head[i + 1], head[i + 2], head[i + 3]]);
        let width = u16_at(5) as usize;
        let height = u16_at(7) as usize;
        let fps = u16_at(9);
        let count = u32_at(11) as usize;
        let file_len = file.metadata().ok()?.len();
        // The table itself must fit the file: a damaged header cannot ask for a huge allocation.
        if width == 0 || height == 0 || fps == 0 || count == 0 || count as u64 * 4 > file_len.saturating_sub(15) {
            return None;
        }
        let mut table = vec![0u8; count * 4];
        file.read_exact(&mut table).ok()?;
        let mut frames = Vec::with_capacity(count);
        let mut offset = 15 + count as u64 * 4;
        for i in 0..count {
            let len = u32::from_le_bytes(table[i * 4..i * 4 + 4].try_into().ok()?) as usize;
            let frame = Frame { offset, len };
            if len == 0 || frame.offset + frame.len as u64 > file_len {
                return None;
            }
            frames.push(frame);
            offset += len as u64;
        }
        Some(Frames { frames, width, height, fps: fps as f32 })
    }
}

/// The home screen's background movie.
pub struct Intro {
    frames: Option<Frames>,
    /// The pack the worker reads (`None` when there is nothing to play).
    pack: Option<PathBuf>,
    /// Where the clock started (the loop's phase is measured from here).
    started: Instant,
    texture: Option<TextureHandle>,
    /// The frame the texture holds.
    shown: usize,
    /// A frame asked of the worker and not received yet.
    pending: Option<usize>,
    /// The worker's request channel (dropped to end the thread).
    tx: Option<Sender<usize>>,
    rx: Option<Receiver<(usize, egui::ColorImage)>>,
    /// The movie is on screen (not stopped by entering, not disabled).
    playing: bool,
    /// The still frame was reached (autotests, or "Pular"): no more repaints for it.
    still: bool,
    /// Sound wanted (the speaker button over the movie turns it off).
    pub sound: bool,
    /// Preferences update when the button is used, so the choice outlives the session.
    sound_changed: bool,
}

impl Intro {
    /// Start the movie (unless the settings turn it off). On any problem this is an inert
    /// background: nothing fails, nothing is logged loudly.
    pub fn new(enabled: bool, sound: bool) -> Self {
        let pack = pack_path();
        let frames = pack.as_deref().and_then(Frames::parse).filter(|f| !f.frames.is_empty());
        let pack = pack.filter(|_| frames.is_some());
        // Autotests: a still first frame keeps their timing (and the screenshots) intact.
        let still = vox_client_core::testmode::background();
        let mut this = Self {
            frames,
            pack,
            started: Instant::now(),
            texture: None,
            shown: usize::MAX,
            pending: None,
            tx: None,
            rx: None,
            playing: false,
            still,
            sound,
            sound_changed: false,
        };
        if enabled {
            this.restart();
        }
        this
    }

    /// Whether this build has a movie to show at all.
    pub fn available(&self) -> bool {
        self.frames.is_some()
    }

    /// Start (or restart) the movie from its first frame.
    pub fn restart(&mut self) {
        let (Some(path), Some(frames)) = (self.pack.clone(), self.frames.as_ref()) else {
            return;
        };
        self.started = Instant::now();
        self.playing = true;
        self.pending = None;
        // A previous stop left the still flag set; only autotests keep it now.
        self.still = vox_client_core::testmode::background();
        if self.tx.is_none() {
            let (req_tx, req_rx) = mpsc::channel::<usize>();
            let (frame_tx, frame_rx) = mpsc::channel::<(usize, egui::ColorImage)>();
            let table: Vec<Frame> = frames.frames.clone();
            let spawned = std::thread::Builder::new()
                .name("vox-intro".into())
                .spawn(move || worker(path, table, req_rx, frame_tx));
            if spawned.is_ok() {
                self.tx = Some(req_tx);
                self.rx = Some(frame_rx);
            }
        }
    }

    /// Enter (or a failed login): the movie and its music stop, and the still frame stays as the
    /// background until the next visit to the home screen.
    pub fn stop(&mut self) {
        if !self.playing {
            return;
        }
        self.playing = false;
        self.tx = None;
        self.rx = None;
        self.pending = None;
        self.still = true;
    }

    /// "Pular": no more frames and no music, but the current picture stays.
    pub fn skip(&mut self) {
        self.stop();
        self.sound = false;
    }

    pub fn playing(&self) -> bool {
        self.playing
    }

    /// Seconds since the movie (re)started; the home screen fades its text in with it.
    pub fn elapsed(&self) -> f32 {
        self.started.elapsed().as_secs_f32()
    }

    /// The first frame is on screen (so a stopped background can show its text at once).
    pub fn shown_first(&self) -> bool {
        self.texture.is_some()
    }

    /// Whether the music should be playing right now.
    pub fn wants_music(&self) -> bool {
        self.playing && self.sound && !self.still && !vox_client_core::testmode::background()
    }

    /// Turn the movie's sound off or on (the speaker button). The choice is saved by the app
    /// after [`Intro::tick`] when [`Intro::take_sound_change`] says so.
    pub fn toggle_sound(&mut self) {
        self.sound = !self.sound;
        self.sound_changed = true;
    }

    pub fn take_sound_change(&mut self) -> Option<bool> {
        std::mem::take(&mut self.sound_changed).then_some(self.sound)
    }

    /// The frame the clock asks for (looping).
    fn index_at(&self, elapsed: f32) -> usize {
        let Some(f) = &self.frames else { return 0 };
        let n = f.frames.len();
        if self.still {
            return 0;
        }
        ((elapsed * f.fps) as usize) % n
    }

    /// Ask for and collect frames. Cheap when stopped (no channels, no repaint request).
    pub fn tick(&mut self, ctx: &egui::Context) {
        if !self.playing {
            return;
        }
        // Keep only the newest frame the worker sent (it may be a little ahead).
        let mut newest = None;
        if let Some(rx) = &self.rx {
            while let Ok(frame) = rx.try_recv() {
                newest = Some(frame);
            }
        }
        if let Some((index, image)) = newest {
            self.pending = None;
            let texture = match self.texture.take() {
                Some(mut t) => {
                    t.set(image, TextureOptions::LINEAR);
                    t
                }
                None => ctx.load_texture("intro-frame", image, TextureOptions::LINEAR),
            };
            self.texture = Some(texture);
            self.shown = index;
        }
        // A still frame (autotests) needs one decode and then nothing else.
        if self.still && self.texture.is_some() {
            self.playing = false;
            self.tx = None;
            self.rx = None;
            self.pending = None;
            return;
        }
        let want = self.index_at(self.started.elapsed().as_secs_f32());
        if self.pending.is_none() && self.shown != want {
            if let Some(tx) = &self.tx {
                if tx.send(want).is_ok() {
                    self.pending = Some(want);
                }
            }
        }
        ctx.request_repaint_after(Duration::from_millis(24));
    }

    /// Paint the movie (or the plain backdrop) filling `rect`, with the scrims that make the text
    /// over it readable: darker at the bottom and on the left, where the brand and the headline go.
    pub fn paint(&self, ui: &egui::Ui, rect: Rect) {
        let painter = ui.painter();
        painter.rect_filled(rect, 0.0, BACKDROP);
        if let (Some(t), Some(f)) = (&self.texture, &self.frames) {
            let image = f.width as f32 / f.height as f32;
            let box_ratio = rect.width() / rect.height().max(1.0);
            let uv = if box_ratio > image {
                // The window is wider than the frame: crop the top and the bottom.
                let h = (image / box_ratio) as f32;
                Rect::from_min_max(Pos2::new(0.0, (1.0 - h) / 2.0), Pos2::new(1.0, 1.0 - (1.0 - h) / 2.0))
            } else {
                // Taller: crop the sides of the cinematic frame, keeping the middle.
                let w = (box_ratio / image) as f32;
                Rect::from_min_max(Pos2::new((1.0 - w) / 2.0, 0.0), Pos2::new(1.0 - (1.0 - w) / 2.0, 1.0))
            };
            painter.image(t.id(), rect, uv, Color32::WHITE);
        }
        // Bottom: the headline and the brand line sit on this.
        let bottom = rect.height() * 0.62;
        gradient(painter, Rect::from_min_max(Pos2::new(rect.left(), rect.bottom() - bottom), rect.max), SCRIM.gamma_multiply(0.0), SCRIM.gamma_multiply(0.93), false);
        // Left: a long, soft wash behind the wordmark and the headline.
        let left = rect.width() * 0.60;
        gradient(painter, Rect::from_min_max(rect.min, Pos2::new(rect.left() + left, rect.bottom())), SCRIM.gamma_multiply(0.82), SCRIM.gamma_multiply(0.0), true);
        // Top: just enough for the wordmark and the corner buttons.
        gradient(painter, Rect::from_min_max(rect.min, Pos2::new(rect.right(), rect.top() + 150.0)), SCRIM.gamma_multiply(0.55), SCRIM.gamma_multiply(0.0), false);
    }
}

/// A vertical (`horizontal == false`) or horizontal linear gradient over `rect`.
fn gradient(painter: &egui::Painter, rect: Rect, from: Color32, to: Color32, horizontal: bool) {
    if rect.width() <= 0.0 || rect.height() <= 0.0 || from == to {
        return;
    }
    let mut mesh = egui::Mesh::default();
    let uv = Pos2::ZERO;
    let corners = [rect.left_top(), rect.right_top(), rect.right_bottom(), rect.left_bottom()];
    for (i, pos) in corners.into_iter().enumerate() {
        let far = if horizontal { i == 1 || i == 2 } else { i >= 2 };
        mesh.vertices.push(egui::epaint::Vertex { pos, uv, color: if far { to } else { from } });
    }
    mesh.indices.extend_from_slice(&[0u32, 1, 2, 0, 2, 3]);
    painter.add(egui::Shape::mesh(mesh));
}

/// Read, decode and send the frames asked for; ends when the app drops the channels.
fn worker(path: PathBuf, frames: Vec<Frame>, rx: Receiver<usize>, tx: Sender<(usize, egui::ColorImage)>) {
    let Ok(mut file) = File::open(&path) else {
        tracing::warn!(path = %path.display(), "launcher movie: cannot open the pack");
        return;
    };
    let mut buf: Vec<u8> = Vec::new();
    while let Ok(index) = rx.recv() {
        let Some(frame) = frames.get(index) else { return };
        buf.resize(frame.len, 0);
        // A frame that cannot be read or decoded is skipped, not fatal: the movie goes on.
        if file.seek(SeekFrom::Start(frame.offset)).is_err() || file.read_exact(&mut buf).is_err() {
            tracing::debug!(index, "launcher movie: frame read failed");
            continue;
        }
        let Ok(image) = image::load_from_memory_with_format(&buf, image::ImageFormat::WebP) else {
            tracing::debug!(index, "launcher movie: frame decode failed");
            continue;
        };
        let image = image.into_rgba8();
        let color = egui::ColorImage::from_rgba_unmultiplied([image.width() as usize, image.height() as usize], image.as_raw());
        if tx.send((index, color)).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_pack() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("assets").join("intro").join("intro.pack")
    }

    #[test]
    fn the_shipped_pack_parses_and_holds_every_frame() {
        let pack = repo_pack();
        let frames = Frames::parse(&pack).expect("the pack this build ships");
        assert_eq!((frames.width, frames.height), (960, 404));
        assert_eq!(frames.fps, 15.0);
        // The whole cinematic (~3:50), not a clip: at least three minutes of frames.
        assert!(frames.frames.len() > 15 * 180, "expected the full movie, got {} frames", frames.frames.len());
        // Every frame starts where the one before it ended (after the header's frame table).
        let mut end = 15 + frames.frames.len() as u64 * 4;
        for f in &frames.frames {
            assert!(f.len > 0, "an empty frame");
            assert_eq!(f.offset, end);
            end = f.offset + f.len as u64;
        }
        assert_eq!(end, std::fs::metadata(&pack).unwrap().len());
    }

    #[test]
    fn the_first_frame_is_a_webp_image() {
        let frames = Frames::parse(&repo_pack()).unwrap();
        let f = frames.frames[0];
        let mut buf = vec![0u8; f.len];
        let mut file = File::open(repo_pack()).unwrap();
        file.seek(SeekFrom::Start(f.offset)).unwrap();
        file.read_exact(&mut buf).unwrap();
        let image = image::load_from_memory_with_format(&buf, image::ImageFormat::WebP).expect("frame 0 decodes");
        assert_eq!((image.width(), image.height()), (960, 404));
    }

    #[test]
    fn a_damaged_header_is_refused() {
        let dir = std::env::temp_dir().join(format!("kumacord-intro-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let junk = dir.join("junk.pack");
        std::fs::write(&junk, b"nope!").unwrap();
        assert!(Frames::parse(&junk).is_none());
        // A header that promises a table larger than the file.
        std::fs::write(&junk, b"AKUI1\xc0\x03\x1c\x02\x14\x00\x01\x00\x00\x00").unwrap();
        assert!(Frames::parse(&junk).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_dev_tree_finds_the_movie() {
        // `cargo test` runs from `target\debug\deps`, so this is the source-tree fallback.
        assert!(asset_dir().is_some(), "the repo's assets/intro must resolve");
        assert!(pack_path().is_some());
    }
}
