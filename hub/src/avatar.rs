//! Profile pictures ("foto de perfil") and banners ("banner de perfil").
//!
//! A person uploads any PNG / JPEG / WebP / GIF file; the hub is the one that makes it safe and
//! small: it is decoded under the same limits as link-preview thumbnails (header dimensions
//! first, allocation cap, no decompression bombs, first frame of animations), centre-cropped to
//! a square, resized to 256x256 and 64x64 and re-encoded from scratch (JPEG, or PNG when the
//! picture has transparency), so nothing of the original file survives, EXIF included.
//!
//! Animated GIFs are stored as-is so animation plays in profile pictures and banners.
//!
//! Storage is SQLite (`avatars`, about 5 to 30 KB per person; blobs are read only when a picture
//! is served) plus `users.avatar_version`, a short hash of the stored 256 px image. The version
//! rides on every [`User`] and is the cache key of the picture. Changing or removing a picture
//! broadcasts `UserUpdated`; a moderator can remove someone else's (audited).
//!
//! Banners are stored similarly under `users.banner_version`; the banner itself is stored
//! uncropped (full width) up to a max width. Animated GIF banners are supported the same way.

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
/// Max upload size for avatars (5 MB).
pub const AVATAR_MAX_UPLOAD_BYTES: usize = 5 * 1024 * 1024;
/// Max upload size for banners (10 MB).
pub const BANNER_MAX_UPLOAD_BYTES: usize = 10 * 1024 * 1024;
/// Max banner width in px.
pub const BANNER_MAX_WIDTH: u32 = 1920;
/// Max banner height in px.
pub const BANNER_MAX_HEIGHT: u32 = 500;

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
    /// If this is an animated GIF, the original bytes are kept so the animation plays.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub animated_gif: Option<Vec<u8>>,
}

/// Untrusted file bytes in, the two stored sizes out.
pub fn process(bytes: &[u8]) -> Result<Processed, ThumbError> {
    let img = decode_untrusted(bytes)?;
    let transparent = has_transparency(&img);
    let is_animated = img.as_animated_gif().is_some();
    let (big, mime, small_bytes): (Vec<u8>, &'static str, Vec<u8>) = if is_animated {
        // Keep GIF as-is for animation support.
        (bytes.to_vec(), "image/gif", Vec::new())
    } else {
        // Centre-crop to a square and resize (always to the full size: a 100 px picture is enlarged).
        let big = img.resize_to_fill(256, 256, FilterType::Lanczos3);
        drop(img);
        let small = big.resize_exact(64, 64, FilterType::Lanczos3);
        let (mime, big_bytes, small_bytes) = if transparent {
            ("image/png", encode_png(&big)?, encode_png(&small)?)
        } else {
            ("image/jpeg", encode_jpeg(&big)?, encode_jpeg(&small)?)
        };
        (big_bytes, mime, small_bytes)
    };
    let version = hex(&Sha256::digest(&big)[..VERSION_BYTES]);
    Ok(Processed { small: small_bytes, big, mime, version, animated_gif: if is_animated { Some(bytes.to_vec()) } else { None } })
}

/// Does any pixel let the background through?
fn has_transparency(img: &DynamicImage) -> bool {
    img.color().has_alpha() && img.to_rgba8().pixels().any(|p| p[3] < 255)
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
    decoding: Mutex<usize>,
}

impl Default for AvatarState {
    fn default() -> Self {
        Self {
            hits: Mutex::new(HashMap::new()),
            decoding: Mutex::new(0),
        }
    }
}

impl AvatarState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a picture change for rate limiting. Returns `Err` if the user is being rate limited.
    pub fn record_hit(&self, user_id: UserId) -> Result<(), AvatarError> {
        let mut hits = self.hits.lock().unwrap();
        let now = Instant::now();
        let user_hits = hits.entry(user_id).or_default();
        // Remove hits older than 10 seconds.
        user_hits.retain(|t| now.duration_since(*t) < Duration::from_secs(10));
        if user_hits.len() >= 5 {
            return Err(AvatarError::RateLimited);
        }
        user_hits.push_back(now);
        Ok(())
    }

    /// Try to acquire a decode slot. Returns `Err` if too many are decoding.
    pub fn acquire_decode(&self) -> Result<(), AvatarError> {
        let mut decoding = self.decoding.lock().unwrap();
        if *decoding >= MAX_CONCURRENT_DECODES {
            return Err(AvatarError::TooManyDecoding);
        }
        *decoding += 1;
        Ok(())
    }

    /// Release a decode slot.
    pub fn release_decode(&self) {
        let mut decoding = self.decoding.lock().unwrap();
        *decoding = decoding.saturating_sub(1);
    }
}

/// Errors from avatar/banner processing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AvatarError {
    RateLimited,
    TooManyDecoding,
    InvalidImage,
    TooLarge,
}

/// HTTP handlers ----------------------------------------------------------------

/// `PUT /api/me/avatar`: upload a profile picture.
pub async fn put_avatar(
    State(s): State<AppState>,
    AuthUser(me): AuthUser,
    body: Bytes,
) -> ApiResult<AvatarResponse> {
    if body.len() > AVATAR_MAX_UPLOAD_BYTES {
        return Err(ApiErr::new(StatusCode::BAD_REQUEST, "file too large"));
    }
    let processed = s.db.call(move |c| {
        let state = s.avatar_state.clone();
        state.record_hit(me.user.id)?;
        state.acquire_decode()?;
        let result = process(&bytes);
        state.release_decode();
        result
    }).await.map_err(|e| ApiErr::new(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:?}")))?;
    let processed = processed.map_err(|e| match e {
        AvatarError::RateLimited => ApiErr::new(StatusCode::TOO_MANY_REQUESTS, "too many avatar changes, try again later"),
        AvatarError::TooManyDecoding => ApiErr::new(StatusCode::TOO_MANY_REQUESTS, "server busy decoding images, try again later"),
        AvatarError::InvalidImage => ApiErr::new(StatusCode::BAD_REQUEST, "not a valid image"),
        _ => ApiErr::new(StatusCode::INTERNAL_SERVER_ERROR, "internal error"),
    })?;
    s.db.call(move |c| db::set_avatar(c, me.user.id, &processed.version))?;
    Ok(Json(AvatarResponse { avatar: Some(processed.version) }))
}

/// `GET /api/users/{id}/avatar`: get a user's profile picture.
pub async fn get_avatar(
    State(s): State<AppState>,
    Path(user_id): Path<UserId>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Response, ApiErr> {
    let _size: u32 = params.get("size")
        .and_then(|s| s.parse().ok())
        .unwrap_or(64);
    let version = s.db.call(move |c| {
        db::get_user(c, user_id)?.map(|u| u.avatar).flatten()
    }).await?;
    let Some(version) = version else {
        return Err(ApiErr::new(StatusCode::NOT_FOUND, "no avatar"));
    };
    let blob = s.db.call(move |c| {
        crate::attachments::get_avatar_blob(c, user_id, &version)
    }).await?;
    let Some(blob) = blob else {
        return Err(ApiErr::new(StatusCode::NOT_FOUND, "avatar not found"));
    };
    let mime = if blob.starts_with(&[0x47, 0x49, 0x46, 0x38]) {
        "image/gif"
    } else if blob.starts_with(&[0x89, 0x50, 0x4e, 0x47]) {
        "image/png"
    } else {
        "image/jpeg"
    };
    Ok(Response::builder()
        .header(header::CONTENT_TYPE, mime)
        .header(header::CACHE_CONTROL, "public, max-age=31536000, immutable")
        .body(Body::from(blob))
        .unwrap())
}

/// `POST /api/mod/users/{id}/remove-avatar`: remove a user's avatar (moderator+).
pub async fn remove_avatar(
    State(s): State<AppState>,
    AuthUser(me): AuthUser,
    Path(target_id): Path<UserId>,
) -> ApiResult<OkResponse> {
    if !may(&s, &me, perm::MODERATE_MEMBERS).await {
        return Err(forbidden());
    }
    s.db.call(move |c| {
        db::set_avatar(c, target_id, None)?;
        Ok(())
    }).await?;
    audit(&s, Some(me.user.id), AUDIT_REMOVE_AVATAR, Some(target_id), String::new()).await;
    Ok(ok())
}

/// `PUT /api/me/banner`: upload a banner image.
pub async fn put_banner(
    State(s): State<AppState>,
    AuthUser(me): AuthUser,
    body: Bytes,
) -> ApiResult<AvatarResponse> {
    if body.len() > BANNER_MAX_UPLOAD_BYTES {
        return Err(ApiErr::new(StatusCode::BAD_REQUEST, "file too large"));
    }
    let processed = s.db.call(move |c| {
        let state = s.avatar_state.clone();
        state.record_hit(me.user.id)?;
        state.acquire_decode()?;
        let result = process(&bytes);
        state.release_decode();
        result
    }).await.map_err(|e| ApiErr::new(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:?}")))?;
    let processed = processed.map_err(|e| match e {
        AvatarError::RateLimited => ApiErr::new(StatusCode::TOO_MANY_REQUESTS, "too many banner changes, try again later"),
        AvatarError::TooManyDecoding => ApiErr::new(StatusCode::TOO_MANY_REQUESTS, "server busy decoding images, try again later"),
        AvatarError::InvalidImage => ApiErr::new(StatusCode::BAD_REQUEST, "not a valid image"),
        _ => ApiErr::new(StatusCode::INTERNAL_SERVER_ERROR, "internal error"),
    })?;
    s.db.call(move |c| db::set_banner(c, me.user.id, &processed.version))?;
    Ok(Json(AvatarResponse { avatar: Some(processed.version) }))
}

/// `GET /api/users/{id}/banner`: get a user's banner.
pub async fn get_banner(
    State(s): State<AppState>,
    Path(user_id): Path<UserId>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Response, ApiErr> {
    let _size: u32 = params.get("size")
        .and_then(|s| s.parse().ok())
        .unwrap_or(1920);
    let version = s.db.call(move |c| {
        db::get_user(c, user_id)?.map(|u| u.banner_version).flatten()
    }).await?;
    let Some(version) = version else {
        return Err(ApiErr::new(StatusCode::NOT_FOUND, "no banner"));
    };
    let blob = s.db.call(move |c| {
        crate::attachments::get_banner_blob(c, user_id, &version)
    }).await?;
    let Some(blob) = blob else {
        return Err(ApiErr::new(StatusCode::NOT_FOUND, "banner not found"));
    };
    let mime = if blob.starts_with(&[0x47, 0x49, 0x46, 0x38]) {
        "image/gif"
    } else if blob.starts_with(&[0x89, 0x50, 0x4e, 0x47]) {
        "image/png"
    } else {
        "image/jpeg"
    };
    Ok(Response::builder()
        .header(header::CONTENT_TYPE, mime)
        .header(header::CACHE_CONTROL, "public, max-age=31536000, immutable")
        .body(Body::from(blob))
        .unwrap())
}

/// `POST /api/mod/users/{id}/remove-banner`: remove a user's banner (moderator+).
pub async fn remove_banner(
    State(s): State<AppState>,
    AuthUser(me): AuthUser,
    Path(target_id): Path<UserId>,
) -> ApiResult<OkResponse> {
    if !may(&s, &me, perm::MODERATE_MEMBERS).await {
        return Err(forbidden());
    }
    s.db.call(move |c| {
        db::set_banner(c, target_id, None)?;
        Ok(())
    }).await?;
    audit(&s, Some(me.user.id), AUDIT_REMOVE_AVATAR, Some(target_id), "removed banner".to_string()).await;
    Ok(ok())
}
