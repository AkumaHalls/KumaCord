//! Profile pictures ("foto de perfil").
//!
//! A person uploads any PNG / JPEG / WebP / GIF file; the hub is the one that makes it safe and
//! small: it is decoded under the same limits as link-preview thumbnails (header dimensions
//! first, allocation cap, no decompression bombs, first frame of animations), centre-cropped to
//! a square, resized to 256x256 and 64x64 and re-encoded from scratch (JPEG, or PNG when the
//! picture has transparency), so nothing of the original file survives, EXIF included.
//!
//! Storage is SQLite (`avatars`, about 5 to 30 KB per person; blobs are read only when a picture
//! is served) plus `users.avatar_version`, a short hash of the stored 256 px image. The version
//! rides on every [`User`] and is the cache key of the picture. Changing or removing a picture
//! broadcasts `UserUpdated`; a moderator can remove someone else's (audited).

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

/// Pictures are decoded this many at a time (the host's PC is also somebody's gaming PC).
const MAX_CONCURRENT_DECODES: usize = 2;
const JPEG_QUALITY: u8 = 88;
/// Characters of the version: 48 bits of the picture's hash.
const VERSION_BYTES: usize = 6;

// ---------------------------------------------------------------- the picture pipeline

/// A processed picture, ready to store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Processed {
    /// 64x64 and 256x256, both `mime`.
    pub small: Vec<u8>,
    pub big: Vec<u8>,
    pub mime: &'static str,
    /// Hash of `big`: the same picture uploaded twice has the same version.
    pub version: String,
    /// The animation, when the upload was one: the container with every metadata chunk cut out
    /// and every frame decoded inside the budget. `None` for a still — which is what every
    /// avatar was before, so a still costs exactly what it always cost.
    pub anim: Option<Anim>,
}

/// One stored animation (the cleaned container and its type).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anim {
    pub bytes: Vec<u8>,
    pub mime: &'static str,
}

/// Untrusted file bytes in, the two stored sizes out (and the animation, if there is one).
pub fn process(bytes: &[u8]) -> Result<Processed, ThumbError> {
    let img = decode_untrusted(bytes)?;
    let (w, h) = (img.width(), img.height());
    // Centre-crop to a square and resize (always to the full size: a 100 px picture is enlarged).
    let big = img.resize_to_fill(256, 256, FilterType::Lanczos3);
    drop(img);
    let small = big.resize_exact(64, 64, FilterType::Lanczos3);
    // Transparency is decided on the RESIZED image, not on the decoded one: the decoded buffer is
    // allowed 20 MP (80 MB of RGBA) and the decoder's `max_alloc` budget does not cover a second
    // copy of it made here — which is what `to_rgba8` did, on every upload with an alpha channel,
    // before the resize released it. The resized buffer is 256x256x4 = 256 KB at most.
    let transparent = has_transparency(&big);
    let (mime, big_bytes, small_bytes) = if transparent {
        ("image/png", encode_png(&big)?, encode_png(&small)?)
    } else {
        ("image/jpeg", encode_jpeg(&big)?, encode_jpeg(&small)?)
    };
    let version = hex(&Sha256::digest(&big_bytes)[..VERSION_BYTES]);
    Ok(Processed { small: small_bytes, big: big_bytes, mime, version, anim: keep_animation(bytes, w, h)? })
}

/// The uploaded file's animation, cleaned and budgeted — or `None` when it has none to keep.
///
/// The stills above come from `decode_untrusted`, which takes the first frame: until now the rest
/// of a GIF or an animated WebP was thrown away with the container and every avatar arrived as a
/// still. Here the container is kept instead, but only after the treatment chat attachments
/// already get — every metadata chunk cut out (so nothing of the original survives except the
/// frames) and every frame decoded inside the pixel budget (so a file that only *looks* fine is
/// refused).
///
/// Only GIF and WebP: `strip` cuts an APNG's animation chunks along with the rest of the
/// ancillary data, so an APNG degrades to the still it already was. GIF and animated WebP are
/// what people upload and what every picker exports.
fn keep_animation(bytes: &[u8], w: u32, h: u32) -> Result<Option<Anim>, ThumbError> {
    let kind = match image::guess_format(bytes) {
        Ok(image::ImageFormat::Gif) => crate::attachments::Kind::Gif,
        Ok(image::ImageFormat::WebP) => crate::attachments::Kind::WebP,
        // A PNG or a JPEG avatar is a still whatever it claims: nothing to keep.
        _ => return Ok(None),
    };
    let Some((clean, animated)) = crate::attachments::strip(kind, bytes) else {
        return Ok(None);
    };
    if !animated || clean.len() > AVATAR_MAX_ANIMATED_BYTES {
        return Ok(None);
    }
    // The cleaned container is what gets stored, so it is the thing that has to fit the budget.
    // This used to verify the RAW bytes and store the cleaned ones unverified: `strip` only
    // removes whole blocks and copies image blocks verbatim, so a clean container can only be
    // smaller than the raw one — but that is a reading of `strip`, not a proof, and this is the
    // last trusted decision point before the blob reaches the database and every client that
    // draws the avatar. Decoding the clean container is also the cheaper half of the two.
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
/// read in place; anything else converts, which on a resized avatar is at most 256 KB.
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
    let mut out = Vec::with_capacity(24 * 1024);
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY).encode_image(&rgb).map_err(|_| ThumbError::Decode)?;
    Ok(out)
}

fn encode_png(img: &DynamicImage) -> Result<Vec<u8>, ThumbError> {
    let rgba = img.to_rgba8();
    let mut out = Vec::with_capacity(48 * 1024);
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(rgba.as_raw(), rgba.width(), rgba.height(), image::ExtendedColorType::Rgba8)
        .map_err(|_| ThumbError::Decode)?;
    Ok(out)
}

// ---------------------------------------------------------------- state: rate limit

/// Per-hub state: who changed a picture lately, and how many pictures are being decoded.
pub struct AvatarState {
    hits: Mutex<HashMap<UserId, VecDeque<Instant>>>,
    /// Decodes running at once; the slot moves into the blocking decode (see `util::spawn_blocking_with`).
    pub(crate) decodes: std::sync::Arc<tokio::sync::Semaphore>,
    limit: usize,
    window: Duration,
}

impl Default for AvatarState {
    fn default() -> Self {
        AvatarState::new(AVATAR_RATE_LIMIT, Duration::from_secs(AVATAR_RATE_WINDOW_SECS))
    }
}

impl AvatarState {
    pub fn new(limit: usize, window: Duration) -> Self {
        AvatarState { hits: Mutex::new(HashMap::new()), decodes: std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_DECODES)), limit, window }
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

/// The pictures table (called by the migration; safe to run any number of times).
pub fn init(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS avatars (
            user_id INTEGER PRIMARY KEY REFERENCES users(id),
            version TEXT NOT NULL,
            mime TEXT NOT NULL,
            small BLOB NOT NULL,
            big BLOB NOT NULL,
            anim BLOB,
            anim_mime TEXT,
            updated_ms INTEGER NOT NULL
        );",
    )?;
    // The animation columns arrived after the first avatar tables existed. Additive like every
    // other column here: an older row reads back with no animation, which is what it always had.
    if !crate::db::has_column(conn, "avatars", "anim")? {
        conn.execute_batch("ALTER TABLE avatars ADD COLUMN anim BLOB")?;
    }
    if !crate::db::has_column(conn, "avatars", "anim_mime")? {
        conn.execute_batch("ALTER TABLE avatars ADD COLUMN anim_mime TEXT")?;
    }
    Ok(())
}

/// Store (replace) the user's picture; returns the version it had before, if any.
pub fn store(conn: &mut Connection, user_id: UserId, p: &Processed) -> rusqlite::Result<Option<String>> {
    let tx = conn.transaction()?;
    let before: Option<String> = tx.query_row("SELECT avatar_version FROM users WHERE id = ?", [user_id], |r| r.get(0)).optional()?.flatten();
    tx.execute(
        "INSERT OR REPLACE INTO avatars (user_id, version, mime, small, big, anim, anim_mime, updated_ms) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        params![user_id, p.version, p.mime, p.small, p.big, p.anim.as_ref().map(|a| &a.bytes), p.anim.as_ref().map(|a| a.mime), now_ms()],
    )?;
    // The flag rides on `users` so `UserRow::user()` can put it on the wire without a join.
    tx.execute("UPDATE users SET avatar_version = ?, avatar_animated = ? WHERE id = ?", params![p.version, p.anim.is_some(), user_id])?;
    tx.commit()?;
    Ok(before)
}

/// Remove the user's picture; returns the version it had, if any.
pub fn clear(conn: &mut Connection, user_id: UserId) -> rusqlite::Result<Option<String>> {
    let tx = conn.transaction()?;
    let before: Option<String> = tx.query_row("SELECT avatar_version FROM users WHERE id = ?", [user_id], |r| r.get(0)).optional()?.flatten();
    tx.execute("DELETE FROM avatars WHERE user_id = ?", [user_id])?;
    tx.execute("UPDATE users SET avatar_version = NULL, avatar_animated = 0 WHERE id = ?", [user_id])?;
    tx.commit()?;
    Ok(before)
}

/// (version, mime, image bytes) at `size` (64 or 256).
pub fn load(conn: &Connection, user_id: UserId, size: u32) -> rusqlite::Result<Option<(String, String, Vec<u8>)>> {
    let col = if size <= 64 { "small" } else { "big" };
    conn.query_row(&format!("SELECT version, mime, {col} FROM avatars WHERE user_id = ?"), [user_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()
}

/// (version, mime, animation bytes), or `None` when the stored picture is a still.
pub fn load_anim(conn: &Connection, user_id: UserId) -> rusqlite::Result<Option<(String, String, Vec<u8>)>> {
    conn.query_row("SELECT version, anim_mime, anim FROM avatars WHERE user_id = ? AND anim IS NOT NULL", [user_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()
}

// ---------------------------------------------------------------- HTTP

fn rejection(e: BytesRejection) -> ApiErr {
    let status = e.status();
    if status == StatusCode::PAYLOAD_TOO_LARGE {
        ApiErr::new(status, format!("picture too large (at most {} MB)", AVATAR_MAX_UPLOAD_BYTES / (1024 * 1024)))
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

/// Tell everybody the user's picture changed. Returns whether anything did.
async fn announce(s: &AppState, id: UserId) -> Result<(), ApiErr> {
    let row = s.db.call(move |c| db::get_user(c, id)).await?.ok_or_else(|| ApiErr::new(StatusCode::NOT_FOUND, "user not found"))?;
    s.update_user(&row.user(&s.owner()));
    Ok(())
}

/// `PUT /api/me/avatar`
pub async fn put_avatar(State(s): State<AppState>, me: Authed, body: Result<Bytes, BytesRejection>) -> ApiResult<AvatarResponse> {
    let bytes = body.map_err(rejection)?;
    if bytes.is_empty() {
        return Err(ApiErr::new(StatusCode::BAD_REQUEST, "empty request body"));
    }
    if let Err(wait) = s.avatars.allow(me.user.id, Instant::now()) {
        let secs = wait.as_secs_f64().ceil().max(1.0) as u64;
        tracing::warn!(user = %me.user.username, "picture change refused: rate limit");
        return Err(ApiErr::new(StatusCode::TOO_MANY_REQUESTS, format!("too many picture changes, wait {secs} s")));
    }
    let slot = s.avatars.decodes.clone().acquire_owned().await.map_err(|e| anyhow::anyhow!("{e}"))?;
    let (id, len) = (me.user.id, bytes.len());
    let processed = crate::util::spawn_blocking_with(slot, move || process(&bytes))
        .await
        .map_err(|_| ApiErr::new(StatusCode::BAD_REQUEST, "could not read the image (damaged file?)"))?
        .map_err(|e| {
            tracing::info!(user = %me.user.username, len, ?e, "picture refused");
            refusal(e)
        })?;
    let version = processed.version.clone();
    let (small, big) = (processed.small.len(), processed.big.len());
    let before = s.db.call(move |c| store(c, id, &processed)).await?;
    tracing::info!(user = %me.user.username, len, small, big, %version, "picture saved");
    if before.as_deref() != Some(version.as_str()) {
        announce(&s, id).await?;
    }
    Ok(Json(AvatarResponse { avatar: Some(version) }))
}

/// `DELETE /api/me/avatar`
pub async fn delete_avatar(State(s): State<AppState>, me: Authed) -> ApiResult<AvatarResponse> {
    let id = me.user.id;
    if s.db.call(move |c| clear(c, id)).await?.is_some() {
        tracing::info!(user = %me.user.username, "picture removed");
        announce(&s, id).await?;
    }
    Ok(Json(AvatarResponse { avatar: None }))
}

/// `POST /api/mod/users/{id}/remove-avatar` (Moderador and up, for people below their role).
pub async fn remove_avatar(State(s): State<AppState>, me: Authed, Path(id): Path<UserId>) -> ApiResult<OkResponse> {
    let target: UserRow = crate::admin::moderation_target(&s, &me, id).await?;
    let Some(version) = s.db.call(move |c| clear(c, id)).await? else {
        return Err(ApiErr::new(StatusCode::NOT_FOUND, "no picture"));
    };
    announce(&s, id).await?;
    crate::admin::audit(&s, Some(me.user.id), AUDIT_REMOVE_AVATAR, Some(target.id), version).await;
    Ok(Json(OkResponse { ok: true }))
}

#[derive(Deserialize)]
pub struct AvatarQuery {
    size: Option<u32>,
    v: Option<String>,
    /// `1` asks for the animation instead of the still. A picture that is not animated answers
    /// `404`, which is what tells the client to draw the still it already has.
    anim: Option<u8>,
}

/// `GET /api/users/{id}/avatar?size=64|256[&v=<version>]`, plus `?anim=1` for the animation.
pub async fn get_avatar(State(s): State<AppState>, _: AuthUser, Path(id): Path<UserId>, Query(q): Query<AvatarQuery>, headers: HeaderMap) -> Result<Response, ApiErr> {
    let animated = q.anim.is_some_and(|n| n != 0);
    let (version, mime, bytes) = if animated {
        let Some(found) = s.db.call(move |c| load_anim(c, id)).await? else {
            return Err(ApiErr::new(StatusCode::NOT_FOUND, "the picture is not animated"));
        };
        found
    } else {
        let size = match q.size {
            None => 256,
            Some(n) if AVATAR_SIZES.contains(&n) => n,
            Some(_) => return Err(ApiErr::new(StatusCode::BAD_REQUEST, "size must be 64 or 256")),
        };
        let Some(found) = s.db.call(move |c| load(c, id, size)).await? else {
            return Err(ApiErr::new(StatusCode::NOT_FOUND, "no picture"));
        };
        found
    };
    // The URL with the version is the cache key: a stale version is gone, not "the new picture".
    if q.v.as_deref().is_some_and(|v| v != version) {
        return Err(ApiErr::new(StatusCode::NOT_FOUND, "that version of the picture is gone"));
    }
    // The size is part of the ETag so the still and the animation of the same picture never share
    // one; the animation has no size, so it uses 0.
    let etag = format!("\"{version}-{}\"", if animated { 0 } else { q.size.unwrap_or(256) });
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
    fn output_is_square_at_both_sizes_whatever_the_input_shape() {
        for (w, h) in [(1200, 600), (600, 1200), (300, 300), (50, 20)] {
            let src = DynamicImage::ImageRgb8(RgbImage::from_fn(w, h, |x, y| Rgb([(x % 256) as u8, (y % 256) as u8, 90])));
            let p = process(&png_of(&src)).unwrap();
            assert_eq!(p.mime, "image/jpeg", "no transparency: JPEG");
            let (big, small) = (decode(&p.big), decode(&p.small));
            assert_eq!((big.width(), big.height()), (256, 256), "{w}x{h}");
            assert_eq!((small.width(), small.height()), (64, 64), "{w}x{h}");
            assert_eq!(p.version.len(), VERSION_BYTES * 2);
        }
    }

    #[test]
    fn the_crop_is_taken_from_the_centre() {
        // Left third red, middle third green, right third blue: a wide picture keeps the green.
        let src = DynamicImage::ImageRgb8(RgbImage::from_fn(900, 300, |x, _| match x / 300 {
            0 => Rgb([255, 0, 0]),
            1 => Rgb([0, 255, 0]),
            _ => Rgb([0, 0, 255]),
        }));
        let big = decode(&process(&png_of(&src)).unwrap().big).to_rgb8();
        for (x, y) in [(10, 10), (128, 128), (245, 245), (10, 245)] {
            let p = big.get_pixel(x, y);
            assert!(p[1] > 200 && p[0] < 60 && p[2] < 60, "({x},{y}) {p:?}");
        }
    }

    #[test]
    fn transparency_is_kept_as_png_and_opaque_alpha_becomes_jpeg() {
        let hole = DynamicImage::ImageRgba8(RgbaImage::from_fn(100, 100, |x, _| if x < 50 { Rgba([255, 0, 0, 0]) } else { Rgba([0, 0, 255, 255]) }));
        let p = process(&png_of(&hole)).unwrap();
        assert_eq!(p.mime, "image/png");
        assert_eq!(&p.big[1..4], b"PNG");
        let big = decode(&p.big).to_rgba8();
        assert_eq!(big.get_pixel(5, 128)[3], 0, "the transparent half stays transparent");
        assert_eq!(big.get_pixel(250, 128)[3], 255);
        let solid = DynamicImage::ImageRgba8(RgbaImage::from_pixel(100, 100, Rgba([10, 200, 30, 255])));
        assert_eq!(process(&png_of(&solid)).unwrap().mime, "image/jpeg");
    }

    #[test]
    fn nothing_of_the_original_file_survives() {
        // A JPEG with an EXIF block (and a comment) goes in; plain re-encoded JPEGs come out.
        let src = RgbImage::from_fn(300, 300, |x, y| Rgb([(x % 256) as u8, (y % 256) as u8, 7]));
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
        for out in [&p.big, &p.small] {
            assert_eq!(&out[..3], &[0xff, 0xd8, 0xff]);
            assert!(!out.windows(6).any(|w| w == b"SECRET" || w == b"Exif\0\0"));
        }
    }

    #[test]
    fn exif_orientation_is_applied_before_cropping() {
        // A 400x200 JPEG marked "rotate 90 degrees": it is really a 200x400 picture.
        let src = RgbImage::from_fn(400, 200, |x, _| if x < 200 { Rgb([255, 0, 0]) } else { Rgb([0, 0, 255]) });
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 95).encode_image(&src).unwrap();
        // APP1/EXIF, big-endian TIFF, one IFD entry: Orientation (0x0112) = 6 (rotate 90 CW).
        let mut exif = b"Exif\0\0MM\0\x2a\0\0\0\x08\0\x01\x01\x12\0\x03\0\0\0\x01\0\x06\0\0\0\0\0\0".to_vec();
        let mut dirty = jpeg[..2].to_vec();
        dirty.extend([0xff, 0xe1]);
        dirty.extend(((exif.len() + 2) as u16).to_be_bytes());
        dirty.append(&mut exif);
        dirty.extend(&jpeg[2..]);
        let big = decode(&process(&dirty).unwrap().big).to_rgb8();
        // Rotated 90 CW: the red left half is now the top, the blue right half the bottom.
        let (top, bottom) = (big.get_pixel(128, 20), big.get_pixel(128, 235));
        assert!(top[0] > 200 && top[2] < 60, "top {top:?}");
        assert!(bottom[2] > 200 && bottom[0] < 60, "bottom {bottom:?}");
    }

    #[test]
    fn gif_first_frame_is_taken_and_the_rest_is_kept() {
        use image::codecs::gif::{GifDecoder, GifEncoder, Repeat};
        use image::{AnimationDecoder, Frame};
        let mut gif = Vec::new();
        {
            let mut enc = GifEncoder::new(&mut gif);
            enc.set_repeat(Repeat::Infinite).unwrap();
            let red = RgbaImage::from_pixel(64, 64, Rgba([255, 0, 0, 255]));
            let blue = RgbaImage::from_pixel(64, 64, Rgba([0, 0, 255, 255]));
            enc.encode_frame(Frame::new(red)).unwrap();
            enc.encode_frame(Frame::new(blue)).unwrap();
        }
        let p = process(&gif).unwrap();
        let px = *decode(&p.big).to_rgb8().get_pixel(128, 128);
        assert!(px[0] > 200 && px[2] < 60, "first frame (red), got {px:?}");
        // The still is the first frame; the animation rides along, cleaned, so a client that can
        // play it gets the whole thing and one that cannot sees exactly what it always saw.
        let anim = p.anim.as_ref().expect("an animated GIF keeps its container");
        assert_eq!(anim.mime, "image/gif");
        let frames = GifDecoder::new(Cursor::new(&anim.bytes)).unwrap().into_frames().collect_frames().unwrap();
        assert_eq!(frames.len(), 2, "cleaning must not drop frames");
        assert_eq!(p.anim.as_ref().map(|a| a.bytes.len()), Some(anim.bytes.len()));
    }

    #[test]
    fn webp_input_works() {
        use base64::Engine;
        // A 1x1 lossless WebP (the image crate can decode WebP but not encode it).
        let webp = base64::engine::general_purpose::STANDARD.decode("UklGRhoAAABXRUJQVlA4TA0AAAAvAAAAEAcQERGIiP4HAA==").unwrap();
        let p = process(&webp).unwrap();
        assert_eq!((decode(&p.big).width(), decode(&p.small).width()), (256, 64));
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
    fn the_same_picture_has_the_same_version_and_another_one_does_not() {
        let a = DynamicImage::ImageRgb8(RgbImage::from_pixel(80, 80, Rgb([200, 10, 10])));
        let b = DynamicImage::ImageRgb8(RgbImage::from_pixel(80, 80, Rgb([10, 10, 200])));
        let (pa1, pa2, pb) = (process(&png_of(&a)).unwrap(), process(&png_of(&a)).unwrap(), process(&png_of(&b)).unwrap());
        assert_eq!(pa1.version, pa2.version);
        assert_ne!(pa1.version, pb.version);
    }

    #[test]
    fn the_rate_limit_is_a_sliding_window_per_person() {
        let st = AvatarState::new(3, Duration::from_secs(60));
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
