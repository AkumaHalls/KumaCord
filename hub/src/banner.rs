//! Profile banners ("banner do perfil").
//!
//! A banner is the wide picture behind a profile card — the same idea as the avatar, a different
//! shape: a rectangle in Discord's 600x240 proportion instead of a square, no crop to a circle,
//! and drawn in one place (the profile card) rather than a dozen.
//!
//! The wire contract existed before this module did: [`User::banner`] (the version) and
//! `users.banner_version` have been in the protocol and the schema since v1.1 and were never
//! written by anything — the column was always NULL and the field always `None`. This is the
//! first writer of both, which is why there is no data migration to run.
//!
//! Storage mirrors `avatar.rs`: a table of blobs (`banner_blobs`, one row per person) plus
//! `users.banner_version` and `users.banner_animated`. The pipeline is the same one too — decode
//! under the link-preview limits, fit inside the box, re-encode from scratch so nothing of the
//! original survives — and an animation keeps its cleaned container exactly like an avatar's.
//! Every change broadcasts `UserUpdated`, because the version already rides on every [`User`].

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use image::imageops::FilterType;
use image::{DynamicImage, ImageEncoder};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use vox_proto::*;

use crate::api::{ApiErr, AuthUser, Authed};
use crate::db::{self, UserRow};
use crate::linkpreview::thumb::{decode_untrusted, ThumbError};
use crate::state::AppState;
use crate::util::{hex, now_ms};

type ApiResult<T> = Result<Json<T>, ApiErr>;

/// Pictures are decoded this many at a time (the host's PC is also somebody's gaming PC). The
/// avatar's semaphore is separate on purpose: one person's banner must not eat the other's
/// avatar slot, and both are cheap to keep apart.
const MAX_CONCURRENT_DECODES: usize = 2;
const JPEG_QUALITY: u8 = 86;
/// Characters of the version: 48 bits of the stored image's hash.
const VERSION_BYTES: usize = 6;
/// The box a banner is resized into: Discord's proportion, at twice the size the profile card
/// draws it at, so it stays sharp on a HiDPI screen.
pub const BANNER_BOX: (u32, u32) = (600, 240);

// ---------------------------------------------------------------- the picture pipeline

/// A processed banner, ready to store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Processed {
    /// The stored image, inside [`BANNER_BOX`], `mime`.
    pub image: Vec<u8>,
    pub mime: &'static str,
    /// Hash of `image`: the same banner uploaded twice has the same version.
    pub version: String,
    /// The animation, when the upload was one: the container with every metadata chunk cut out
    /// and every frame decoded inside the budget. `None` for a still — which is what every
    /// banner was before, so a still costs exactly what it always cost.
    pub anim: Option<Anim>,
}

/// One stored animation (the cleaned container and its type).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anim {
    pub bytes: Vec<u8>,
    pub mime: &'static str,
}

/// Untrusted file bytes in, the stored banner out.
pub fn process(bytes: &[u8]) -> Result<Processed, ThumbError> {
    let img = decode_untrusted(bytes)?;
    let (w, h) = (img.width(), img.height());
    // Fit inside the box keeping the aspect: a banner is never cropped away. A 16:9 photo and a
    // tall phone shot both keep every edge, letterboxed, the way Discord shows them.
    let fitted = img.resize(BANNER_BOX.0, BANNER_BOX.1, FilterType::Lanczos3);
    drop(img);
    // Transparency is decided on the FITTED image, not on the decoded one. The fitted buffer is
    // at most 600x240x4 = 576 KB; the decoded one is allowed 20 MP (80 MB of RGBA), and the
    // decoder's `max_alloc` budget only covers the decoder's own buffer — not a second copy of
    // it made here. Two uploads decoding at once therefore used to cost the hub ~336 MB of peak
    // RSS on a small VPS, once every rate-limit window, from any authenticated member. The price
    // is that a picture whose only transparency is smaller than a pixel after the resample now
    // lands as JPEG — a cosmetic difference on an already-degenerate upload, not a crossed
    // control.
    let transparent = has_transparency(&fitted);
    let (mime, image) = if transparent {
        ("image/png", encode_png(&fitted)?)
    } else {
        ("image/jpeg", encode_jpeg(&fitted)?)
    };
    let version = hex(&Sha256::digest(&image)[..VERSION_BYTES]);
    Ok(Processed { image, mime, version, anim: keep_animation(bytes, w, h)? })
}

/// The uploaded file's animation, cleaned and budgeted — or `None` when it has none to keep.
///
/// The still above comes from `decode_untrusted`, which takes the first frame: without this the
/// rest of a GIF or an animated WebP would be thrown away with the container. Here the container
/// is kept instead, but only after the treatment chat attachments already get — every metadata
/// chunk cut out (so nothing of the original survives except the frames) and every frame decoded
/// inside the pixel budget (so a file that only *looks* fine is refused).
///
/// Only GIF and WebP: `strip` cuts an APNG's animation chunks along with the rest of the
/// ancillary data, so an APNG degrades to the still it already was.
fn keep_animation(bytes: &[u8], w: u32, h: u32) -> Result<Option<Anim>, ThumbError> {
    let kind = match image::guess_format(bytes) {
        Ok(image::ImageFormat::Gif) => crate::attachments::Kind::Gif,
        Ok(image::ImageFormat::WebP) => crate::attachments::Kind::WebP,
        // A PNG or a JPEG banner is a still whatever it claims: nothing to keep.
        _ => return Ok(None),
    };
    let Some((clean, animated)) = crate::attachments::strip(kind, bytes) else {
        return Ok(None);
    };
    if !animated || clean.len() > BANNER_MAX_ANIMATED_BYTES {
        return Ok(None);
    }
    // The cleaned container is what gets stored, so it is the thing that has to fit the budget.
    // This used to verify the RAW bytes and store the cleaned ones unverified: `strip` only
    // removes whole blocks and copies image blocks verbatim, so a clean container can only be
    // smaller than the raw one — but that is a reading of `strip`, not a proof, and this is the
    // last trusted decision point before the blob reaches the database and every client that
    // opens the profile. Decoding the clean container is also the cheaper half of the two.
    match crate::attachments::verify_animation(kind, &clean, w, h) {
        Ok(n) if n > 1 => {}
        // A single frame is a still whatever the container says: nothing to keep, and no reason
        // to spend the bytes on a second copy of the same picture.
        Ok(_) => return Ok(None),
        Err(e) => {
            // Refusing is the honest answer for a bomb (the budget is a limit, not a warning),
            // but a container our stripper cannot read is still a decodable picture: it keeps
            // its still, which is what it would have got before animations existed.
            return Err(match e {
                crate::attachments::Refusal::TooManyFrames | crate::attachments::Refusal::Dimensions => ThumbError::TooLarge,
                _ => return Ok(None),
            });
        }
    }
    Ok(Some(Anim { bytes: clean, mime: kind.mime() }))
}

/// Does any pixel let the background through? The two layouts that already ARE the answer are
/// read in place; anything else converts, which on a banner-sized image is at most 576 KB.
/// (`to_rgba8` on an `ImageRgba8` clones the whole buffer, which is exactly the copy this
/// avoids.)
fn has_transparency(img: &DynamicImage) -> bool {
    if !img.color().has_alpha() {
        return false;
    }
    match img {
        DynamicImage::ImageRgba8(b) => b.pixels().any(|p| p[3] < 255),
        DynamicImage::ImageLumaA8(b) => b.pixels().any(|p| p[3] < 255),
        _ => img.to_rgba8().pixels().any(|p| p[3] < 255),
    }
}

fn encode_jpeg(img: &DynamicImage) -> Result<Vec<u8>, ThumbError> {
    let rgb = img.to_rgb8();
    let mut out = Vec::with_capacity(48 * 1024);
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY).encode_image(&rgb).map_err(|_| ThumbError::Decode)?;
    Ok(out)
}

fn encode_png(img: &DynamicImage) -> Result<Vec<u8>, ThumbError> {
    let rgba = img.to_rgba8();
    let mut out = Vec::with_capacity(96 * 1024);
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(rgba.as_raw(), rgba.width(), rgba.height(), image::ExtendedColorType::Rgba8)
        .map_err(|_| ThumbError::Decode)?;
    Ok(out)
}

// ---------------------------------------------------------------- state: rate limit

/// Per-hub state: who changed a banner lately, and how many banners are being decoded.
pub struct BannerState {
    hits: Mutex<HashMap<UserId, VecDeque<Instant>>>,
    /// Decodes running at once; the slot moves into the blocking decode (see `util::spawn_blocking_with`).
    pub(crate) decodes: std::sync::Arc<tokio::sync::Semaphore>,
    limit: usize,
    window: Duration,
}

impl Default for BannerState {
    fn default() -> Self {
        BannerState::new(BANNER_RATE_LIMIT, Duration::from_secs(BANNER_RATE_WINDOW_SECS))
    }
}

impl BannerState {
    pub fn new(limit: usize, window: Duration) -> Self {
        BannerState { hits: Mutex::new(HashMap::new()), decodes: std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_DECODES)), limit, window }
    }

    /// Count one upload attempt of `user`; `Err(wait)` when they are over the limit (the attempt
    /// is then not counted).
    pub fn allow(&self, user: UserId, now: Instant) -> Result<(), Duration> {
        let mut g = self.hits.lock().unwrap_or_else(|e| e.into_inner());
        let q = g.entry(user).or_default();
        while q.front().is_some_and(|t| now.saturating_duration_since(*t) >= self.window) {
            q.pop_front();
        }
        if q.len() >= self.limit {
            return Err(self.window.saturating_sub(now.saturating_duration_since(q[0])));
        }
        q.push_back(now);
        if g.len() > 256 {
            let window = self.window;
            g.retain(|_, q| q.back().is_some_and(|t| now.saturating_duration_since(*t) < window));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- database

/// The banners table (called by the migration; safe to run any number of times).
pub fn init(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS banner_blobs (
            user_id INTEGER PRIMARY KEY REFERENCES users(id),
            version TEXT NOT NULL,
            mime TEXT NOT NULL,
            blob BLOB NOT NULL,
            anim BLOB,
            anim_mime TEXT,
            updated_ms INTEGER NOT NULL
        );",
    )?;
    // The animation columns arrived with the first version of this table; an older row reads
    // back with no animation, which is what it always had. Additive, like every other column.
    if !crate::db::has_column(conn, "banner_blobs", "anim")? {
        conn.execute_batch("ALTER TABLE banner_blobs ADD COLUMN anim BLOB")?;
    }
    if !crate::db::has_column(conn, "banner_blobs", "anim_mime")? {
        conn.execute_batch("ALTER TABLE banner_blobs ADD COLUMN anim_mime TEXT")?;
    }
    Ok(())
}

/// Store (replace) the user's banner; returns the version it had before, if any.
pub fn store(conn: &mut Connection, user_id: UserId, p: &Processed) -> rusqlite::Result<Option<String>> {
    let tx = conn.transaction()?;
    let before: Option<String> = tx.query_row("SELECT banner_version FROM users WHERE id = ?", [user_id], |r| r.get(0)).optional()?.flatten();
    tx.execute(
        "INSERT OR REPLACE INTO banner_blobs (user_id, version, mime, blob, anim, anim_mime, updated_ms) VALUES (?, ?, ?, ?, ?, ?, ?)",
        params![
            user_id,
            p.version,
            p.mime,
            p.image,
            p.anim.as_ref().map(|a| &a.bytes),
            p.anim.as_ref().map(|a| a.mime),
            now_ms()
        ],
    )?;
    // The flag rides on `users` so `UserRow::user()` can put it on the wire without a join.
    tx.execute("UPDATE users SET banner_version = ?, banner_animated = ? WHERE id = ?", params![p.version, p.anim.is_some(), user_id])?;
    tx.commit()?;
    Ok(before)
}

/// Remove the user's banner; returns the version it had, if any.
pub fn clear(conn: &mut Connection, user_id: UserId) -> rusqlite::Result<Option<String>> {
    let tx = conn.transaction()?;
    let before: Option<String> = tx.query_row("SELECT banner_version FROM users WHERE id = ?", [user_id], |r| r.get(0)).optional()?.flatten();
    tx.execute("DELETE FROM banner_blobs WHERE user_id = ?", [user_id])?;
    tx.execute("UPDATE users SET banner_version = NULL, banner_animated = 0 WHERE id = ?", [user_id])?;
    tx.commit()?;
    Ok(before)
}

/// (version, mime, image bytes) of the stored banner.
pub fn load(conn: &Connection, user_id: UserId) -> rusqlite::Result<Option<(String, String, Vec<u8>)>> {
    conn.query_row("SELECT version, mime, blob FROM banner_blobs WHERE user_id = ?", [user_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()
}

/// (version, mime, animation bytes), or `None` when the stored banner is a still.
pub fn load_anim(conn: &Connection, user_id: UserId) -> rusqlite::Result<Option<(String, String, Vec<u8>)>> {
    conn.query_row(
        "SELECT version, anim_mime, anim FROM banner_blobs WHERE user_id = ? AND anim IS NOT NULL",
        [user_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .optional()
}

// ---------------------------------------------------------------- HTTP

fn rejection(e: BytesRejection) -> ApiErr {
    let status = e.status();
    if status == StatusCode::PAYLOAD_TOO_LARGE {
        ApiErr::new(status, format!("banner too large (at most {} MB)", BANNER_MAX_UPLOAD_BYTES / (1024 * 1024)))
    } else {
        ApiErr::new(StatusCode::BAD_REQUEST, "could not read the request body")
    }
}

fn refusal(e: ThumbError) -> ApiErr {
    match e {
        ThumbError::Unsupported => ApiErr::new(StatusCode::BAD_REQUEST, "not a supported image (use PNG, JPEG, WebP or GIF)"),
        ThumbError::TooLarge => ApiErr::new(StatusCode::BAD_REQUEST, "image dimensions are too large"),
        ThumbError::Decode => ApiErr::new(StatusCode::BAD_REQUEST, "could not read the image (damaged file?)"),
    }
}

/// Tell everybody the user's banner changed.
async fn announce(s: &AppState, id: UserId) -> Result<(), ApiErr> {
    let row = s.db.call(move |c| db::get_user(c, id)).await?.ok_or_else(|| ApiErr::new(StatusCode::NOT_FOUND, "user not found"))?;
    s.update_user(&row.user(&s.owner()));
    Ok(())
}

/// `PUT /api/me/banner`
pub async fn put_banner(State(s): State<AppState>, me: Authed, body: Result<Bytes, BytesRejection>) -> ApiResult<BannerResponse> {
    let bytes = body.map_err(rejection)?;
    if bytes.is_empty() {
        return Err(ApiErr::new(StatusCode::BAD_REQUEST, "empty request body"));
    }
    if let Err(wait) = s.banners.allow(me.user.id, Instant::now()) {
        let secs = wait.as_secs_f64().ceil().max(1.0) as u64;
        tracing::warn!(user = %me.user.username, "banner change refused: rate limit");
        return Err(ApiErr::new(StatusCode::TOO_MANY_REQUESTS, format!("too many banner changes, wait {secs} s")));
    }
    let slot = s.banners.decodes.clone().acquire_owned().await.map_err(|e| anyhow::anyhow!("{e}"))?;
    let (id, len) = (me.user.id, bytes.len());
    let processed = crate::util::spawn_blocking_with(slot, move || process(&bytes))
        .await
        .map_err(|_| ApiErr::new(StatusCode::BAD_REQUEST, "could not read the image (damaged file?)"))?
        .map_err(|e| {
            tracing::info!(user = %me.user.username, len, ?e, "banner refused");
            refusal(e)
        })?;
    let version = processed.version.clone();
    let animated = processed.anim.is_some();
    let before = s.db.call(move |c| store(c, id, &processed)).await?;
    tracing::info!(user = %me.user.username, len, animated, %version, "banner saved");
    if before.as_deref() != Some(version.as_str()) {
        announce(&s, id).await?;
    }
    Ok(Json(BannerResponse { banner: Some(version) }))
}

/// `DELETE /api/me/banner`
pub async fn delete_banner(State(s): State<AppState>, me: Authed) -> ApiResult<BannerResponse> {
    let id = me.user.id;
    if s.db.call(move |c| clear(c, id)).await?.is_some() {
        tracing::info!(user = %me.user.username, "banner removed");
        announce(&s, id).await?;
    }
    Ok(Json(BannerResponse { banner: None }))
}

/// `POST /api/mod/users/{id}/remove-banner` (Moderador and up, for people below their role).
pub async fn remove_banner(State(s): State<AppState>, me: Authed, Path(id): Path<UserId>) -> ApiResult<OkResponse> {
    let target: UserRow = crate::admin::moderation_target(&s, &me, id).await?;
    let Some(version) = s.db.call(move |c| clear(c, id)).await? else {
        return Err(ApiErr::new(StatusCode::NOT_FOUND, "no banner"));
    };
    announce(&s, id).await?;
    crate::admin::audit(&s, Some(me.user.id), AUDIT_REMOVE_BANNER, Some(target.id), version).await;
    Ok(Json(OkResponse { ok: true }))
}

#[derive(Deserialize)]
pub struct BannerQuery {
    /// `1` asks for the animation instead of the still. A banner that is not animated answers
    /// `404`, which is what tells the client to draw the still it already has.
    anim: Option<u8>,
    v: Option<String>,
}

/// `GET /api/users/{id}/banner[?anim=1][&v=<version>]`
pub async fn get_banner(State(s): State<AppState>, _: AuthUser, Path(id): Path<UserId>, Query(q): Query<BannerQuery>, headers: HeaderMap) -> Result<Response, ApiErr> {
    let animated = q.anim.is_some_and(|n| n != 0);
    let (version, mime, bytes) = if animated {
        let Some(found) = s.db.call(move |c| load_anim(c, id)).await? else {
            return Err(ApiErr::new(StatusCode::NOT_FOUND, "the banner is not animated"));
        };
        found
    } else {
        let Some(found) = s.db.call(move |c| load(c, id)).await? else {
            return Err(ApiErr::new(StatusCode::NOT_FOUND, "no banner"));
        };
        found
    };
    // The URL with the version is the cache key: a stale version is gone, not "the new banner".
    if q.v.as_deref().is_some_and(|v| v != version) {
        return Err(ApiErr::new(StatusCode::NOT_FOUND, "that version of the banner is gone"));
    }
    // The animation has no size, so it uses 0: the still and the animation of one banner never
    // share an ETag.
    let etag = format!("\"{version}-{}\"", if animated { 0 } else { BANNER_BOX.0 });
    let cache = if q.v.is_some() { "private, max-age=31536000, immutable" } else { "private, no-cache" };
    let unchanged = headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()).is_some_and(|v| v.split(',').any(|t| t.trim() == etag));
    let mut resp = if unchanged { StatusCode::NOT_MODIFIED.into_response() } else { bytes.into_response() };
    let h = resp.headers_mut();
    if !unchanged {
        h.insert(header::CONTENT_TYPE, mime.parse().map_err(|_| anyhow::anyhow!("bad mime"))?);
    }
    h.insert(header::CACHE_CONTROL, header::HeaderValue::from_static(cache));
    h.insert(header::ETAG, etag.parse().map_err(|_| anyhow::anyhow!("bad etag"))?);
    h.insert(header::X_CONTENT_TYPE_OPTIONS, header::HeaderValue::from_static("nosniff"));
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::AnimationDecoder;
    use std::io::Cursor;
    use image::{Rgb, RgbImage, Rgba, RgbaImage};

    fn png_of(img: &DynamicImage) -> Vec<u8> {
        let mut out = Vec::new();
        img.write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png).unwrap();
        out
    }

    fn decode(bytes: &[u8]) -> DynamicImage {
        image::load_from_memory(bytes).unwrap()
    }

    #[test]
    fn output_fits_the_box_keeping_the_aspect() {
        // A 16:9 photo and a tall phone shot both land inside 600x240 without losing an edge.
        for (w, h) in [(1600, 900), (600, 1200), (600, 240), (100, 40)] {
            let src = DynamicImage::ImageRgb8(RgbImage::from_fn(w, h, |x, y| Rgb([(x % 256) as u8, (y % 256) as u8, 90])));
            let p = process(&png_of(&src)).unwrap();
            let img = decode(&p.image);
            let (bw, bh) = (img.width(), img.height());
            assert!(bw <= BANNER_BOX.0 && bh <= BANNER_BOX.1, "{w}x{h} -> {bw}x{bh}");
            let (sw, sh) = (w as f32, h as f32);
            let ratio_out = bw as f32 / bh as f32;
            assert!((sw / sh - ratio_out).abs() < 0.02, "{w}x{h}: aspect kept ({} -> {ratio_out})", sw / sh);
            assert_eq!(p.anim, None, "a still has nothing to keep");
            assert_eq!(p.version.len(), VERSION_BYTES * 2);
        }
    }

    #[test]
    fn transparency_is_kept_as_png_and_opaque_alpha_becomes_jpeg() {
        let hole = DynamicImage::ImageRgba8(RgbaImage::from_fn(600, 240, |x, _| if x < 300 { Rgba([255, 0, 0, 0]) } else { Rgba([0, 0, 255, 255]) }));
        let p = process(&png_of(&hole)).unwrap();
        assert_eq!(p.mime, "image/png");
        assert_eq!(&p.image[1..4], b"PNG");
        let img = decode(&p.image).to_rgba8();
        assert_eq!(img.get_pixel(5, 120)[3], 0, "the transparent half stays transparent");
        assert_eq!(img.get_pixel(500, 120)[3], 255);
        let solid = DynamicImage::ImageRgba8(RgbaImage::from_pixel(600, 240, Rgba([10, 200, 30, 255])));
        assert_eq!(process(&png_of(&solid)).unwrap().mime, "image/jpeg");
    }

    #[test]
    fn a_transparent_banner_bigger_than_the_box_still_lands_as_png() {
        // The transparency decision moved to AFTER the resize (a 20 MP decode must not be
        // copied to check for alpha: the decoder's `max_alloc` does not cover a copy of it, and
        // two concurrent decodes used to cost the hub ~336 MB of peak RSS). Moving it must not
        // change the answer for a banner whose alpha is not a sub-pixel speck.
        let big = RgbaImage::from_fn(1600, 640, |x, _| if x < 800 { Rgba([0, 0, 0, 0]) } else { Rgba([255, 255, 255, 255]) });
        let p = process(&png_of(&DynamicImage::ImageRgba8(big))).unwrap();
        assert_eq!(p.mime, "image/png", "transparency survives the resample");
        let img = decode(&p.image);
        assert!(img.width() <= BANNER_BOX.0 && img.height() <= BANNER_BOX.1, "{}x{}", img.width(), img.height());
        assert_eq!(img.to_rgba8().get_pixel(5, 120)[3], 0, "the transparent half stays transparent");
        assert_eq!(img.to_rgba8().get_pixel(500, 120)[3], 255);
    }

    #[test]
    fn nothing_of_the_original_file_survives() {
        // A JPEG with an EXIF block (and a comment) goes in; a plain re-encoded JPEG comes out.
        let src = RgbImage::from_fn(600, 240, |x, y| Rgb([(x % 256) as u8, (y % 256) as u8, 7]));
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 90).encode_image(&src).unwrap();
        let mut dirty = jpeg[..2].to_vec();
        let exif = b"Exif\0\0SECRET-GPS-DATA";
        dirty.extend([0xff, 0xe1]);
        dirty.extend(((exif.len() + 2) as u16).to_be_bytes());
        dirty.extend(exif);
        dirty.extend(&jpeg[2..]);
        assert!(dirty.windows(6).any(|w| w == b"SECRET"));
        let p = process(&dirty).unwrap();
        assert_eq!(&p.image[..3], &[0xff, 0xd8, 0xff]);
        assert!(!p.image.windows(6).any(|w| w == b"SECRET" || w == b"Exif\0\0"));
    }

    #[test]
    fn an_animated_gif_keeps_its_frames_and_a_still_does_not() {
        use image::codecs::gif::{GifEncoder, Repeat};
        use image::Frame;
        let mut gif = Vec::new();
        {
            let mut enc = GifEncoder::new(&mut gif);
            enc.set_repeat(Repeat::Infinite).unwrap();
            enc.encode_frame(Frame::new(RgbaImage::from_pixel(64, 64, Rgba([255, 0, 0, 255])))).unwrap();
            enc.encode_frame(Frame::new(RgbaImage::from_pixel(64, 64, Rgba([0, 0, 255, 255])))).unwrap();
            enc.encode_frame(Frame::new(RgbaImage::from_pixel(64, 64, Rgba([0, 255, 0, 255])))).unwrap();
        }
        let p = process(&gif).unwrap();
        let anim = p.anim.as_ref().expect("an animated GIF keeps its container");
        assert_eq!(anim.mime, "image/gif");
        assert_eq!(&anim.bytes[..3], b"GIF", "the cleaned container is still a GIF");
        // Every frame survived the cleaning (the point of keeping it at all).
        let frames = image::codecs::gif::GifDecoder::new(Cursor::new(&anim.bytes)).unwrap().into_frames().collect_frames().unwrap();
        assert_eq!(frames.len(), 3, "cleaning must not drop frames");
        // The still is the first frame: red, not blue or green. (A 64x64 animation fitted into
        // 600x240 is height-bound, so it lands at 240x240.)
        let px = *decode(&p.image).to_rgb8().get_pixel(120, 120);
        assert!(px[0] > 200 && px[2] < 60, "first frame (red), got {px:?}");
        // A single-frame GIF is a still: nothing to keep.
        let mut one = Vec::new();
        GifEncoder::new(&mut one).encode_frame(Frame::new(RgbaImage::from_pixel(64, 64, Rgba([9, 9, 9, 255])))).unwrap();
        assert_eq!(process(&one).unwrap().anim, None, "one frame is a still");
    }

    #[test]
    fn a_metadata_laden_gif_is_stripped_before_it_is_stored() {
        // The whole reason the animation is re-stripped rather than stored as uploaded: a GIF
        // comment must not ride along into the stored blob.
        use image::codecs::gif::{GifEncoder, Repeat};
        use image::Frame;
        let mut gif = Vec::new();
        {
            let mut enc = GifEncoder::new(&mut gif);
            enc.set_repeat(Repeat::Infinite).unwrap();
            enc.encode_frame(Frame::new(RgbaImage::from_pixel(64, 64, Rgba([255, 0, 0, 255])))).unwrap();
            enc.encode_frame(Frame::new(RgbaImage::from_pixel(64, 64, Rgba([0, 0, 255, 255])))).unwrap();
        }
        // A comment extension (0x21 0xFE) spliced in after the header and the colour table, the
        // way `attachments::validate`'s own test does it.
        let flags = gif[10];
        let gct = if flags & 0x80 != 0 { 3 * (1usize << ((flags & 7) + 1)) } else { 0 };
        let at = 13 + gct;
        let comment = b"SECRET-COMMENT";
        let mut dirty = gif[..at].to_vec();
        dirty.extend([0x21, 0xfe, comment.len() as u8]);
        dirty.extend(comment);
        dirty.push(0);
        dirty.extend(&gif[at..]);
        assert!(dirty.windows(6).any(|w| w == b"SECRET"));
        let p = process(&dirty).unwrap();
        let anim = p.anim.expect("still animated");
        assert!(!anim.bytes.windows(6).any(|w| w == b"SECRET"), "the comment is gone");
        let frames = image::codecs::gif::GifDecoder::new(Cursor::new(&anim.bytes)).unwrap().into_frames().collect_frames().unwrap();
        assert_eq!(frames.len(), 2, "and the frames are still there");
    }

    #[test]
    fn junk_bombs_and_truncated_files_are_refused() {
        use crate::linkpreview::thumb::test_images::*;
        assert_eq!(process(b"<html>not an image</html>"), Err(ThumbError::Unsupported));
        assert_eq!(process(b""), Err(ThumbError::Unsupported));
        assert_eq!(process(&png_claiming(20_000, 20_000)), Err(ThumbError::TooLarge));
        assert_eq!(process(&png_claiming(9_000, 9_000)), Err(ThumbError::TooLarge));
        let mut cut = png(800, 600);
        cut.truncate(cut.len() / 2);
        assert_eq!(process(&cut), Err(ThumbError::Decode));
    }

    #[test]
    fn the_same_banner_has_the_same_version_and_another_one_does_not() {
        let a = DynamicImage::ImageRgb8(RgbImage::from_pixel(600, 240, Rgb([200, 10, 10])));
        let b = DynamicImage::ImageRgb8(RgbImage::from_pixel(600, 240, Rgb([10, 10, 200])));
        let (pa1, pa2, pb) = (process(&png_of(&a)).unwrap(), process(&png_of(&a)).unwrap(), process(&png_of(&b)).unwrap());
        assert_eq!(pa1.version, pa2.version);
        assert_ne!(pa1.version, pb.version);
    }

    #[test]
    fn the_rate_limit_is_a_sliding_window_per_person() {
        let st = BannerState::new(3, Duration::from_secs(60));
        let t0 = Instant::now();
        for i in 0..3 {
            assert!(st.allow(UserId(1), t0 + Duration::from_secs(i)).is_ok());
        }
        let wait = st.allow(UserId(1), t0 + Duration::from_secs(10)).unwrap_err();
        assert!(wait > Duration::from_secs(49) && wait <= Duration::from_secs(50), "{wait:?}");
        assert!(st.allow(UserId(2), t0 + Duration::from_secs(10)).is_ok(), "somebody else is unaffected");
        assert!(st.allow(UserId(1), t0 + Duration::from_secs(60)).is_ok(), "the oldest attempt aged out");
        assert!(st.allow(UserId(1), t0 + Duration::from_secs(61)).is_ok());
        assert!(st.allow(UserId(1), t0 + Duration::from_secs(62)).is_ok());
        assert!(st.allow(UserId(1), t0 + Duration::from_secs(63)).is_err());
    }
}
