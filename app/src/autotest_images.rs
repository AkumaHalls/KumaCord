//! Chat-image autotest (a child module of `autotest`).
//!
//! - `img-a` (name `imgA...`): in the first text channel, "pastes" a generated screenshot (a
//!   bitmap, the Ctrl+V path) and "drops" a generated photo with an EXIF block (the file path),
//!   waits for both to be compressed (logs "x MB -> y KB"), sends them with a caption, checks the
//!   message came back with its two images and the strip emptied, then waits for B's "ok".
//! - `img-b` (name `imgB...`): waits for A's message, checks both thumbnails were drawn in the
//!   chat, opens the viewer on each (the full image decoded at the size the message announced),
//!   closes it (everything freed) and says "ok".
//!
//! Generated pictures only (never anybody's real images); silent (no sound, no microphone).

use vox_proto::{Attachment, UserId};

use super::Auto;
use crate::app::App;
use crate::chat_images::Target;

#[derive(Default)]
pub struct State {
    channel: Option<i64>,
    atts: Vec<Attachment>,
    author: Option<UserId>,
}

fn log(what: &str) {
    tracing::info!(target: "autotest", "AUTOTEST images: {what}");
}

/// A 1600x900 screenshot-like picture: dark UI, panels and rows of "text".
fn screenshot() -> image::DynamicImage {
    let mut img = image::RgbImage::from_pixel(1600, 900, image::Rgb([49, 51, 56]));
    for y in 0..900 {
        for x in 0..1600 {
            let side = !(260..1400).contains(&x);
            let text = (40..860).contains(&y) && (y % 22) < 9 && (300..1300).contains(&x) && ((x / 7 + y / 22) % 5 != 0) && (x % 7) < 4;
            if text {
                img.put_pixel(x, y, image::Rgb([220, 221, 222]));
            } else if side {
                img.put_pixel(x, y, image::Rgb([43, 45, 49]));
            }
        }
    }
    image::DynamicImage::ImageRgb8(img)
}

/// A 2400x1600 photo-like JPEG (gradients and grain) with an EXIF block carrying a fake GPS tag.
fn photo() -> Vec<u8> {
    let mut seed = 7u32;
    let img = image::RgbImage::from_fn(2400, 1600, |x, y| {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let n = (seed >> 27) as u8;
        image::Rgb([(x / 10 % 256) as u8 / 2 + n, (y / 7 % 256) as u8 / 2 + n, 120u8.wrapping_add(n)])
    });
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 90).encode_image(&img).expect("encode the test photo");
    let exif = b"Exif\0\0MM\0\x2a\0\0\0\x08\0\x00SECRET-GPS-AUTOTEST";
    let mut out = jpeg[..2].to_vec();
    out.extend([0xff, 0xe1]);
    out.extend(((exif.len() + 2) as u16).to_be_bytes());
    out.extend(exif);
    out.extend(&jpeg[2..]);
    out
}

impl Auto {
    /// `--show hold --hold N`: seconds to linger at the strip (A), the thumbnails and the viewer
    /// (B), for a look.
    fn img_hold(&self) -> u64 {
        if self.args.show.as_deref() == Some("hold") { self.args.hold_secs.min(30) } else { 0 }
    }

    fn img_tag(&self, what: &str) -> String {
        format!("imgtest {what} {}", self.args.run_id)
    }

    /// Logged in and the hub's answer about images arrived. `Some(Err)` = failed.
    fn img_ready(&mut self, app: &mut App) -> Option<Result<i64, i32>> {
        if !self.logged_in(app) || !app.images.supported {
            if self.waited(30) {
                let why = if self.logged_in(app) { "the hub does not accept images" } else { "could not log in" };
                return Some(Err(self.fail(why).unwrap_or(1)));
            }
            return None;
        }
        match Self::first_text(app) {
            Some(ch) => Some(Ok(ch)),
            None => Some(Err(self.fail("no text channel").unwrap_or(1))),
        }
    }

    pub(super) fn run_img_a(&mut self, app: &mut App) -> Option<i32> {
        match self.step {
            0 => self.connect(app),
            1 => match self.img_ready(app) {
                Some(Err(code)) => return Some(code),
                Some(Ok(ch)) => {
                    self.img.channel = Some(ch);
                    app.select_text(ch);
                    let Some(ctx) = app.repaint.ctx() else { return self.fail("no egui context") };
                    let target = Target::Channel(ch);
                    let shot = screenshot();
                    let bitmap_bytes = u64::from(shot.width()) * u64::from(shot.height()) * 4;
                    app.attach_picture(target, "imagem colada".into(), shot, bitmap_bytes, &ctx);
                    app.attach_bytes(target, "foto.jpg".into(), photo(), &ctx);
                    self.goto(2, "pasted a screenshot and dropped a photo; compressing");
                }
                None => {}
            },
            2 => {
                let target = Target::Channel(self.img.channel.unwrap_or_default());
                if app.images.ready(target) && self.waited(self.img_hold()) {
                    let ready = app.images.ready_images(target);
                    if ready.len() != 2 {
                        return self.fail(&format!("{} pictures ready, expected 2", ready.len()));
                    }
                    for c in &ready {
                        log(&format!(
                            "compressed {}x{} {} -> {}x{} {} ({}, screenshot={}, {} ms)",
                            c.original_w,
                            c.original_h,
                            crate::chat_images::human_size(c.original_bytes),
                            c.width,
                            c.height,
                            crate::chat_images::human_size(c.bytes.len() as u64),
                            c.encoding.label(),
                            c.screenshot,
                            c.millis
                        ));
                        if c.bytes.windows(6).any(|w| w == b"SECRET") {
                            return self.fail("metadata survived the compression");
                        }
                    }
                    app.input = self.img_tag("fotos");
                    app.send_message();
                    if !app.images.sending_to(target) {
                        return self.fail("the send did not start");
                    }
                    self.goto(3, "sending the message with two images");
                } else if self.waited(60 + self.img_hold()) {
                    return self.fail("compression took more than 60 s");
                }
            }
            3 => {
                let (ch, tag) = (self.img.channel.unwrap_or_default(), self.img_tag("fotos"));
                let mine = app.logs.get(&ch).and_then(|l| l.msgs.iter().find(|m| m.caption() == tag).cloned());
                match mine {
                    Some(m) if m.attachments.len() == 2 && app.images.sent == 1 => {
                        if app.images.pending_for(Target::Channel(ch)) != 0 {
                            return self.fail("the strip still has pictures after sending");
                        }
                        log(&format!("message {} has images {:?}", m.id, m.attachments.iter().map(|a| (a.id, a.width, a.height, a.size)).collect::<Vec<_>>()));
                        self.goto(4, "sent; waiting for B");
                    }
                    Some(m) if m.attachments.len() != 2 => return self.fail(&format!("the message came back with {} images", m.attachments.len())),
                    _ if self.waited(60) => return self.fail(&format!("the message did not come back (notice: {:?})", app.notice)),
                    _ => {}
                }
            }
            4 => {
                let ok = self.img_tag("ok");
                if app.logs.values().any(|l| l.msgs.iter().any(|m| m.content == ok)) {
                    return self.pass();
                }
                if self.waited(120) {
                    return self.fail("B never said ok");
                }
            }
            _ => {}
        }
        None
    }

    pub(super) fn run_img_b(&mut self, app: &mut App) -> Option<i32> {
        match self.step {
            0 => self.connect(app),
            1 => match self.img_ready(app) {
                Some(Err(code)) => return Some(code),
                Some(Ok(ch)) => {
                    self.img.channel = Some(ch);
                    app.select_text(ch);
                    self.goto(2, "waiting for A's images");
                }
                None => {}
            },
            2 => {
                let (ch, tag) = (self.img.channel.unwrap_or_default(), self.img_tag("fotos"));
                if let Some(m) = app.logs.get(&ch).and_then(|l| l.msgs.iter().find(|m| m.caption() == tag)) {
                    if m.attachments.len() != 2 {
                        return self.fail(&format!("A's message has {} images", m.attachments.len()));
                    }
                    if !m.content.ends_with("[2 imagens]") {
                        return self.fail("no marker for older apps in the text");
                    }
                    self.img.atts = m.attachments.clone();
                    self.img.author = Some(m.author.id);
                    self.goto(3, "got the message; waiting for the thumbnails to be drawn");
                } else if self.waited(120) {
                    return self.fail("A's message never arrived");
                }
            }
            3 => {
                let drawn = self.img.atts.iter().all(|a| app.images.store.thumb_shown(crate::chat_images::ImgKey::of(a)));
                if drawn && self.img.author.take().is_some() {
                    self.goto(3, "both thumbnails drawn");
                }
                if drawn && self.waited(self.img_hold()) {
                    log(&format!("both thumbnails drawn; store (known, textures, full) = {:?}", app.images.store.stats()));
                    app.open_viewer(self.img.atts.clone(), 0);
                    self.goto(4, "viewer opened on the first image");
                } else if !drawn && self.waited(30) {
                    return self.fail("the thumbnails were not drawn");
                }
            }
            4 | 5 => {
                let i = (self.step - 4) as usize;
                let a = &self.img.atts[i];
                match app.images.store.full_size(crate::chat_images::ImgKey::of(a)) {
                    Some(size) if size == (a.width, a.height) && !(i == 0 && !self.waited(self.img_hold())) => {
                        let shown = app.images.viewer.as_ref().map(|v| v.current().id);
                        if shown != Some(a.id) {
                            return self.fail("the viewer shows another image");
                        }
                        log(&format!("viewer shows image {} at {}x{}", a.id, size.0, size.1));
                        if i == 0 {
                            app.open_viewer(self.img.atts.clone(), 1);
                            self.goto(5, "viewer on the second image");
                        } else {
                            app.close_viewer();
                            if app.images.store.stats().2 != 0 {
                                return self.fail("closing the viewer did not free the full images");
                            }
                            let ok = self.img_tag("ok");
                            Self::say(app, &ok);
                            self.goto(6, "viewer closed; told A");
                        }
                    }
                    Some(size) if size == (a.width, a.height) => {}
                    Some(size) => return self.fail(&format!("full image is {size:?}, expected {}x{}", a.width, a.height)),
                    None if self.waited(30) => return self.fail("the full image never loaded"),
                    None => {}
                }
            }
            6 if self.waited(2) => return self.pass(),
            _ => {}
        }
        None
    }
}
