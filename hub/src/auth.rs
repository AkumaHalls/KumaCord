//! Identity (item 12): personal passwords, per-device tokens, one-time reset codes and the login
//! brakes.
//!
//! - A name can be protected by a personal password (argon2id, hashed on the blocking pool).
//!   Required for every role above Membro; optional for Membros. A protected name needs the
//!   password, a reset code or a remembered device token on top of the server password.
//! - Device tokens and session tokens are random 256-bit values stored as sha256 hashes.
//! - Reset codes (the owner's "Redefinir senha pessoal", or `concord-server --reset-password`) are
//!   random 50-bit codes, stored hashed, valid for [`RESET_CODE_TTL_MS`], compared in constant time.
//! - [`Throttle`]: after a few failures in a row, a name or an address must wait (1 s, doubling,
//!   at most a minute) before its next attempt is even checked. A password check reserves its
//!   attempt first ([`Throttle::begin`]): checks that are still running count as failures, so a
//!   burst of simultaneous guesses gets no more checks than guesses one after the other would.
//! - At most [`ARGON2_AT_ONCE`] argon2 runs at a time (`AppState::argon2`).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;

use crate::db::UserRow;
use crate::util::{ct_eq, random_bytes, sha256_hex};

/// A reset code works for this long.
pub const RESET_CODE_TTL_MS: i64 = 3 * 24 * 3600 * 1000;
/// argon2 runs at once (each ~20-50 ms and 19 MiB); the rest wait, briefly (see `api::argon2_blocking`).
pub const ARGON2_AT_ONCE: usize = 2;

/// argon2id with the crate's defaults (19 MiB, 2 passes): ~20-50 ms here, run off the runtime.
pub fn hash_password(password: &str) -> anyhow::Result<String> {
    let salt = SaltString::encode_b64(&random_bytes::<16>()).map_err(|e| anyhow::anyhow!("salt: {e}"))?;
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| anyhow::anyhow!("hashing password: {e}"))
}

/// Constant-time verification (inside argon2).
pub fn verify_password(password: &str, phc: &str) -> bool {
    match PasswordHash::new(phc) {
        Ok(h) => Argon2::default().verify_password(password.as_bytes(), &h).is_ok(),
        Err(_) => false,
    }
}

/// `XXXX-XXXX-XX` from an alphabet without look-alikes (no 0/O, 1/I/L).
pub fn new_reset_code() -> String {
    const ALPHA: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
    let bytes = random_bytes::<10>();
    let mut s = String::with_capacity(12);
    for (i, b) in bytes.iter().enumerate() {
        if i == 4 || i == 8 {
            s.push('-');
        }
        s.push(ALPHA[*b as usize % ALPHA.len()] as char);
    }
    s
}

/// Codes are typed by people: ignore case, dashes and spaces.
pub fn reset_code_hash(code: &str) -> String {
    let norm: String = code.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_uppercase()).collect();
    // The salt keeps its pre-concord name: stored reset codes were hashed with it.
    sha256_hex(&format!("voxhub-reset:{norm}"))
}

/// What a login attempt proved about a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proof {
    /// The name is not protected (new, or a member without a personal password).
    Open,
    Password,
    DeviceToken,
    ResetCode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Denied {
    /// Protected and nothing (valid) was offered: ask for the personal password.
    Required,
    /// A password / code was offered and is wrong (counts as a failure).
    Wrong,
    /// The right reset code, but too late.
    Expired,
}

/// Check the personal-password part of a login. `device_ok` says whether the device token that
/// was sent (if any) is a live token of this user. Password checks are slow (argon2): call this
/// on the blocking pool.
pub fn check(row: &UserRow, password: Option<&str>, device_ok: bool, now_ms: i64) -> Result<Proof, Denied> {
    if let Some(phc) = &row.password_hash {
        if device_ok {
            return Ok(Proof::DeviceToken);
        }
        return match password {
            Some(p) if verify_password(p, phc) => Ok(Proof::Password),
            Some(_) => Err(Denied::Wrong),
            None => Err(Denied::Required),
        };
    }
    if let Some(code_hash) = &row.reset_hash {
        return match password {
            Some(p) if ct_eq(&reset_code_hash(p), code_hash) => {
                if row.reset_expires_ms.is_some_and(|e| e < now_ms) {
                    Err(Denied::Expired)
                } else {
                    Ok(Proof::ResetCode)
                }
            }
            Some(_) => Err(Denied::Wrong),
            None => Err(Denied::Required),
        };
    }
    Ok(Proof::Open)
}

// ---------------------------------------------------------------- throttle

/// Failures allowed in a row before waits start.
const FREE_FAILURES: u32 = 3;
const FIRST_WAIT: Duration = Duration::from_secs(1);
const MAX_WAIT: Duration = Duration::from_secs(60);
/// A key with no failure for this long starts over.
const FORGET_AFTER: Duration = Duration::from_secs(15 * 60);

/// One key's record.
#[derive(Debug, Clone, Copy)]
struct Entry {
    failures: u32,
    /// The last failure (or when the entry was made).
    last: Instant,
    /// Checks reserved with [`Throttle::begin`] that have not finished.
    in_flight: u32,
}

/// Per-key login brake (keys: `ip:<address>` and `user:<lowercased name>`).
#[derive(Default)]
pub struct Throttle {
    inner: Mutex<HashMap<String, Entry>>,
}

/// A reserved check ([`Throttle::begin`]). Say how it went with [`Attempt::fail`] or
/// [`Attempt::succeed`]; dropped without either (the check proved nothing, or the request was
/// cancelled), the reservation is just released.
#[must_use]
pub struct Attempt<'a> {
    t: &'a Throttle,
    key: String,
    done: bool,
}

impl Attempt<'_> {
    pub fn fail(mut self) {
        self.done = true;
        self.t.finish(&self.key, Some(true));
    }

    pub fn succeed(mut self) {
        self.done = true;
        self.t.finish(&self.key, Some(false));
    }
}

impl Drop for Attempt<'_> {
    fn drop(&mut self) {
        if !self.done {
            self.t.finish(&self.key, None);
        }
    }
}

impl Throttle {
    fn wait_for(failures: u32) -> Duration {
        if failures < FREE_FAILURES {
            return Duration::ZERO;
        }
        let shift = (failures - FREE_FAILURES).min(16);
        (FIRST_WAIT * (1u32 << shift)).min(MAX_WAIT)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// How long `key` must still wait before an attempt is checked (`None`: go ahead).
    pub fn must_wait(&self, key: &str) -> Option<Duration> {
        let g = self.lock();
        let e = g.get(key)?;
        let since = e.last.elapsed();
        if since >= FORGET_AFTER {
            return None;
        }
        Self::wait_for(e.failures).checked_sub(since).filter(|d| !d.is_zero())
    }

    /// Check-and-reserve, atomically: `Err(wait)` when `key` must wait, counting the checks of
    /// `key` still running as failures (a running check could be one); otherwise one check is
    /// reserved until the [`Attempt`] is resolved or dropped.
    pub fn begin(&self, key: &str) -> Result<Attempt<'_>, Duration> {
        let mut g = self.lock();
        let e = g.entry(key.to_string()).or_insert(Entry { failures: 0, last: Instant::now(), in_flight: 0 });
        if e.in_flight == 0 && e.last.elapsed() >= FORGET_AFTER {
            e.failures = 0;
        }
        let projected = e.failures.saturating_add(e.in_flight);
        if projected >= FREE_FAILURES {
            let wait = Self::wait_for(projected).saturating_sub(e.last.elapsed());
            if e.in_flight > 0 {
                // Wait for the running checks first.
                return Err(wait.max(FIRST_WAIT));
            }
            if !wait.is_zero() {
                return Err(wait);
            }
        }
        e.in_flight += 1;
        Ok(Attempt { t: self, key: key.to_string(), done: false })
    }

    /// A reserved check ended: `Some(true)` failed, `Some(false)` succeeded, `None` proved
    /// nothing. One lock, so no other check sees the reservation gone before the result counts.
    fn finish(&self, key: &str, failed: Option<bool>) {
        let mut g = self.lock();
        let Some(e) = g.get_mut(key) else { return };
        e.in_flight = e.in_flight.saturating_sub(1);
        match failed {
            Some(true) => {
                if e.last.elapsed() >= FORGET_AFTER {
                    e.failures = 0;
                }
                e.failures = e.failures.saturating_add(1);
                e.last = Instant::now();
            }
            Some(false) => e.failures = 0,
            None => {}
        }
        if e.in_flight == 0 && e.failures == 0 {
            g.remove(key);
        }
    }

    pub fn fail(&self, key: &str) {
        let mut g = self.lock();
        if g.len() > 4096 {
            g.retain(|_, e| e.in_flight > 0 || e.last.elapsed() < FORGET_AFTER);
        }
        let e = g.entry(key.to_string()).or_insert(Entry { failures: 0, last: Instant::now(), in_flight: 0 });
        if e.last.elapsed() >= FORGET_AFTER {
            e.failures = 0;
        }
        e.failures = e.failures.saturating_add(1);
        e.last = Instant::now();
    }

    pub fn succeed(&self, key: &str) {
        let mut g = self.lock();
        match g.get_mut(key) {
            Some(e) if e.in_flight > 0 => e.failures = 0,
            Some(_) => {
                g.remove(key);
            }
            None => {}
        }
    }
}

/// The throttle key of a peer address. v1.0.2: an IPv6 peer by its /64 network (one connection
/// gets a whole /64), see [`crate::listen::peer_group`].
pub fn ip_key(ip: std::net::IpAddr) -> String {
    format!("ip:{}", crate::listen::peer_group(ip))
}

pub fn user_key(name: &str) -> String {
    format!("user:{}", name.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use vox_proto::Role;

    fn row() -> UserRow {
        UserRow {
            id: 1,
            username: "Ana".into(),
            stored_role: Role::Member,
            password_hash: None,
            reset_hash: None,
            reset_expires_ms: None,
            server_muted: false,
            created_at_ms: 0,
            avatar: None,
            display_name: None,
            banner_version: None,
            badges: 0,
        }
    }

    #[test]
    fn passwords_hash_and_verify() {
        let h = hash_password("s3nha boa").unwrap();
        assert!(h.starts_with("$argon2id$"));
        assert!(verify_password("s3nha boa", &h));
        assert!(!verify_password("s3nha bo", &h));
        assert!(!verify_password("x", "not a hash"));
    }

    #[test]
    fn login_proofs() {
        let mut r = row();
        assert_eq!(check(&r, None, false, 0), Ok(Proof::Open));
        assert_eq!(check(&r, Some("whatever"), false, 0), Ok(Proof::Open));
        r.password_hash = Some(hash_password("abcdef").unwrap());
        assert_eq!(check(&r, None, false, 0), Err(Denied::Required));
        assert_eq!(check(&r, Some("abcdeg"), false, 0), Err(Denied::Wrong));
        assert_eq!(check(&r, Some("abcdef"), false, 0), Ok(Proof::Password));
        assert_eq!(check(&r, None, true, 0), Ok(Proof::DeviceToken));
        let mut r = row();
        let code = new_reset_code();
        assert_eq!(code.len(), 12);
        r.reset_hash = Some(reset_code_hash(&code));
        r.reset_expires_ms = Some(1000);
        assert_eq!(check(&r, None, true, 0), Err(Denied::Required), "device tokens do not survive a reset");
        assert_eq!(check(&r, Some(&code.to_lowercase().replace('-', " ")), false, 999), Ok(Proof::ResetCode));
        assert_eq!(check(&r, Some(&code), false, 1001), Err(Denied::Expired));
        assert_eq!(check(&r, Some("AAAA-AAAA-AA"), false, 0), Err(Denied::Wrong));
    }

    #[test]
    fn throttle_waits_grow_and_reset() {
        let t = Throttle::default();
        assert_eq!(t.must_wait("k"), None);
        t.fail("k");
        t.fail("k");
        assert_eq!(t.must_wait("k"), None, "two failures are free");
        t.fail("k");
        let w = t.must_wait("k").unwrap();
        assert!(w <= FIRST_WAIT && w > Duration::from_millis(900));
        t.fail("k");
        assert!(t.must_wait("k").unwrap() > Duration::from_millis(1900));
        t.succeed("k");
        assert_eq!(t.must_wait("k"), None);
        assert_eq!(Throttle::wait_for(40), MAX_WAIT);
    }

    #[test]
    fn simultaneous_checks_count_as_failures() {
        let t = Throttle::default();
        // Three checks may run at once (the free failures); a fourth must wait for them.
        let a = t.begin("k").unwrap();
        let b = t.begin("k").unwrap();
        let c = t.begin("k").unwrap();
        assert!(t.begin("k").err().unwrap() >= FIRST_WAIT);
        // They all fail: now the usual wait applies, and only one check at a time after it.
        a.fail();
        b.fail();
        c.fail();
        assert!(t.begin("k").is_err(), "three failures: wait");
        t.lock().get_mut("k").unwrap().last -= FIRST_WAIT;
        let d = t.begin("k").unwrap();
        assert!(t.begin("k").is_err(), "one check at a time once the brake is on");
        drop(d);
        // A success clears the name; a dropped attempt only releases its reservation.
        let e = t.begin("k").unwrap();
        e.succeed();
        assert!(t.lock().get("k").is_none());
        let f = t.begin("k").unwrap();
        drop(f);
        assert!(t.lock().get("k").is_none());
    }
}
