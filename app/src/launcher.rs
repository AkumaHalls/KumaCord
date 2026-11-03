//! The launch cinematic: the community's own edit (`assets/intro/intro.pack`, 20 s of it) as the
//! background of the home screen, with the login card over it — the League of Legends launcher's
//! idea, in the AkumaLabs' own footage.
//!
//! The pack is a sequence of WebP frames packed at build time (`intro.pack`: magic, size, fps,
//! frame lengths, then the images), so no video decoder is linked: one worker thread decodes the
//! frame for the current second into an egui texture while the UI stays responsive, and two
//! textures at most are alive at once (960 × 540 each). The music is a 20 s MP3 next to the pack,
//! decoded once by the backend ([`vox_client_core::audio::clip`]) and looped through the mixer.
//!
//! Everything here is best-effort and silent: a missing or damaged pack simply leaves the plain
//! gradient background, and autotests get a still first frame (no decode loop, no music) so they
//! stay fast.

use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, Pos2, Rect, TextureHandle, TextureOptions};

/// The packed frames (see `brag-output/work/pack_intro.py`).
const PACK: &[u8] = include_bytes!("../assets/intro/intro.pack");
const MAGIC: &[u8; 5] = b"AKUI1";
/// The background of the home screen before the first frame arrives (and behind a still one).
pub const BACKDROP: Color32 = Color32::from_rgb(0x08, 0x09, 0x12);
/// The scrims' colour (near-black, a touch blue).
const SCRIM: Color32 = Color32::from_rgb(0x06, 0x08, 0x14);

/// One frame: where it sits in [`PACK`] and how long it is.
#[derive(Clone, Copy)]
struct Frame {
    offset: usize,
    len: usize,
}

/// The pack's header, parsed once.
struct Frames {
    frames: Vec<Frame>,
    width: usize,
    height: usize,
    fps: f32,
}

impl Frames {
    fn parse(pack: &'static [u8]) -> Option<Self> {
        let head = pack.get(..5)?;
        if head != MAGIC {
            return None;
        }
        let u16_at = |i: usize| pack.get(i..i + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
        let u32_at = |i: usize| pack.get(i..i + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
        let width = u16_at(5)? as usize;
        let height = u16_at(7)? as usize;
        let fps = u16_at(9)?;
        let count = u32_at(11)? as usize;
        if width == 0 || height == 0 || count == 0 || fps == 0 {
            return None;
        }
        let table = 15;
        let mut frames = Vec::with_capacity(count);
        let mut offset = table + count * 4;
        for i in 0..count {
            let len = u32_at(table + i * 4)? as usize;
            let frame = Frame { offset, len };
            if frame.offset + frame.len > pack.len() {
                return None;
            }
            frames.push(frame);
            offset += len;
        }
        Some(Frames { frames, width, height, fps: fps as f32 })
    }
}

/// The home screen's background movie.
pub struct Intro {
    frames: Option<Frames>,
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
        let frames = Frames::parse(PACK).filter(|f| !f.frames.is_empty());
        // Autotests: a still first frame keeps their timing (and the screenshots) intact.
        let still = vox_client_core::testmode::background();
        let mut this = Self {
            frames,
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

    /// Start (or restart) the movie from its first frame.
    pub fn restart(&mut self) {
        if self.frames.is_none() {
            return;
        }
        self.started = Instant::now();
        self.playing = true;
        self.pending = None;
        // A previous stop left the still flag set; only autotests keep it now.
        self.still = vox_client_core::testmode::background();
        if self.tx.is_none() {
            let (req_tx, req_rx) = mpsc::channel::<usize>();
            let (frame_tx, frame_rx) = mpsc::channel::<(usize, egui::ColorImage)>();
            let frames: Vec<Frame> = self.frames.as_ref().map(|f| f.frames.clone()).unwrap_or_default();
            let spawned = std::thread::Builder::new().name("vox-intro".into()).spawn(move || worker(&frames, req_rx, frame_tx));
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
                // Taller: crop the sides of the 16:9 frame, keeping the middle.
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

/// Decode the frame asked for and send it back; ends when the app drops the channels.
fn worker(frames: &[Frame], rx: Receiver<usize>, tx: Sender<(usize, egui::ColorImage)>) {
    while let Ok(index) = rx.recv() {
        let Some(frame) = frames.get(index) else { return };
        let Some(bytes) = PACK.get(frame.offset..frame.offset + frame.len) else { return };
        let Ok(image) = image::load_from_memory_with_format(bytes, image::ImageFormat::WebP) else { return };
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

    #[test]
    fn the_shipped_pack_parses_and_holds_every_frame() {
        let frames = Frames::parse(PACK).expect("the pack this build embeds");
        assert_eq!((frames.width, frames.height), (960, 540));
        assert_eq!(frames.fps, 20.0);
        assert!(frames.frames.len() >= 20, "at least a second of movie");
        // Every frame starts where the one before it ended (after the header's frame table).
        let mut end = 15 + frames.frames.len() * 4;
        for f in &frames.frames {
            assert!(f.len > 0, "an empty frame");
            assert_eq!(f.offset, end);
            end = f.offset + f.len;
        }
        assert_eq!(end, PACK.len());
    }

    #[test]
    fn the_first_frame_is_a_webp_image() {
        let frames = Frames::parse(PACK).unwrap();
        let f = frames.frames[0];
        let image = image::load_from_memory_with_format(&PACK[f.offset..f.offset + f.len], image::ImageFormat::WebP).expect("frame 0 decodes");
        assert_eq!((image.width(), image.height()), (960, 540));
    }

    #[test]
    fn a_damaged_header_is_refused() {
        assert!(Frames::parse(b"nope!").is_none());
        assert!(Frames::parse(b"AKUI1\xc0\x03\x1c\x02\x14\x00\x01\x00\x00\x00\xff\xff\xff\x7f").is_none());
    }
}
