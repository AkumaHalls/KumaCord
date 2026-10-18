//! Small helpers: randomness, hex, hashing, time.

use sha2::{Digest, Sha256};

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("OS randomness unavailable");
    b
}

pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

pub fn random_hex<const N: usize>() -> String {
    hex(&random_bytes::<N>())
}

pub fn sha256_hex(s: &str) -> String {
    hex(&Sha256::digest(s.as_bytes()))
}

/// Constant-time equality (length leak is fine here).
pub fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Run blocking work while holding `permit` (a concurrency slot): the permit moves into the work
/// and is released when the work ends, not when the awaiting request is dropped (a client that
/// disconnects cannot free the slot of a decode / argon2 run that keeps going).
pub async fn spawn_blocking_with<T: Send + 'static>(
    permit: tokio::sync::OwnedSemaphorePermit,
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, tokio::task::JoinError> {
    tokio::task::spawn_blocking(move || {
        let r = f();
        drop(permit);
        r
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    /// A request that gives up (its future is dropped) while its blocking work runs keeps the
    /// slot until the work really ends.
    #[tokio::test]
    async fn the_slot_lives_as_long_as_the_blocking_work() {
        let sem = Arc::new(tokio::sync::Semaphore::new(1));
        let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
        let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
        let permit = sem.clone().acquire_owned().await.unwrap();
        let req = tokio::spawn(spawn_blocking_with(permit, move || {
            started_tx.send(()).unwrap();
            go_rx.recv().unwrap();
        }));
        tokio::task::spawn_blocking(move || started_rx.recv().unwrap()).await.unwrap();
        req.abort(); // the client went away
        let _ = req.await;
        assert_eq!(sem.available_permits(), 0, "the work still runs: its slot is still taken");
        go_tx.send(()).unwrap();
        for _ in 0..200 {
            if sem.available_permits() == 1 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the slot was not given back when the work ended");
    }
}
