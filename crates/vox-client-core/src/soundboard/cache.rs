//! Downloaded soundboard clips: files named `{id}-{hash}.wav` in one directory per server, and
//! the most recently used few kept decoded in memory.
//!
//! The hash is the clip's fingerprint from the sound list, so a file is valid forever: a renamed
//! sound keeps its file, a deleted-and-re-added one has a new id. A damaged file is deleted and
//! fetched again. [`SoundCache::retain`] drops files of sounds that no longer exist.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use parking_lot::Mutex;
use vox_proto::{parse_wav, wav_samples, SoundId};

/// Decoded clips kept in memory (mono 5 s is 480 KB, stereo 960 KB).
const MEM_CLIPS: usize = 12;

/// A clip as the mixer plays it: 48 kHz, interleaved, mono or stereo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoundPcm {
    pub channels: usize,
    pub samples: Arc<[i16]>,
}

impl SoundPcm {
    /// Parse a stored clip (the hub's canonical WAV).
    pub fn from_wav(bytes: &[u8]) -> Result<Self> {
        let info = parse_wav(bytes).map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(SoundPcm { channels: info.channels as usize, samples: Arc::from(wav_samples(bytes, &info)) })
    }

    pub fn frames(&self) -> usize {
        self.samples.len() / self.channels.max(1)
    }

    pub fn duration_ms(&self) -> u32 {
        (self.frames() as u64 * 1000 / vox_proto::SOUND_RATE as u64) as u32
    }

    /// Interleaved stereo (the format of the mixer's alert sounds), for a local preview.
    pub fn to_stereo(&self) -> Arc<[i16]> {
        if self.channels == 2 {
            self.samples.clone()
        } else {
            Arc::from(self.samples.iter().flat_map(|&s| [s, s]).collect::<Vec<_>>())
        }
    }
}

type CacheKey = (SoundId, String);

pub struct SoundCache {
    dir: PathBuf,
    mem: Mutex<VecDeque<(CacheKey, Arc<SoundPcm>)>>,
}

impl SoundCache {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into(), mem: Mutex::new(VecDeque::new()) }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn file(&self, id: SoundId, hash: &str) -> PathBuf {
        // The hash comes from the hub: keep it from escaping the directory.
        let safe: String = hash.chars().filter(|c| c.is_ascii_alphanumeric()).take(64).collect();
        self.dir.join(format!("{id}-{safe}.wav"))
    }

    /// The clip, from memory or from disk; `None` when it was never downloaded (or the file is
    /// damaged, in which case it is removed).
    pub fn get(&self, id: SoundId, hash: &str) -> Option<Arc<SoundPcm>> {
        let key = (id, hash.to_string());
        {
            let mut mem = self.mem.lock();
            if let Some(i) = mem.iter().position(|(k, _)| *k == key) {
                let hit = mem.remove(i)?;
                let pcm = hit.1.clone();
                mem.push_back(hit);
                return Some(pcm);
            }
        }
        let path = self.file(id, hash);
        let bytes = std::fs::read(&path).ok()?;
        match SoundPcm::from_wav(&bytes) {
            Ok(pcm) => Some(self.remember(key, pcm)),
            Err(e) => {
                tracing::warn!(id, error = %e, "cached sound is damaged; removing it");
                let _ = std::fs::remove_file(&path);
                None
            }
        }
    }

    /// Is the clip on disk or in memory (without decoding it)?
    pub fn has(&self, id: SoundId, hash: &str) -> bool {
        self.mem.lock().iter().any(|(k, _)| k.0 == id && k.1 == hash) || self.file(id, hash).is_file()
    }

    /// Keep a downloaded clip. Checked first: a hub that sends something that is not a sound
    /// leaves nothing behind.
    pub fn put(&self, id: SoundId, hash: &str, wav: &[u8]) -> Result<Arc<SoundPcm>> {
        let pcm = SoundPcm::from_wav(wav).context("the downloaded sound is not valid")?;
        std::fs::create_dir_all(&self.dir).with_context(|| format!("creating {}", self.dir.display()))?;
        let path = self.file(id, hash);
        let tmp = path.with_extension("part");
        std::fs::write(&tmp, wav).and_then(|_| std::fs::rename(&tmp, &path)).context("saving the sound")?;
        Ok(self.remember((id, hash.to_string()), pcm))
    }

    fn remember(&self, key: CacheKey, pcm: SoundPcm) -> Arc<SoundPcm> {
        let pcm = Arc::new(pcm);
        let mut mem = self.mem.lock();
        mem.retain(|(k, _)| *k != key);
        mem.push_back((key, pcm.clone()));
        while mem.len() > MEM_CLIPS {
            mem.pop_front();
        }
        pcm
    }

    /// Delete the files (and memory) of every clip not in `keep` (the current sound list).
    pub fn retain(&self, keep: &[(SoundId, String)]) {
        self.mem.lock().retain(|(k, _)| keep.contains(k));
        let Ok(entries) = std::fs::read_dir(&self.dir) else { return };
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().is_none_or(|x| x != "wav" && x != "part") {
                continue;
            }
            let wanted = keep.iter().any(|(id, hash)| self.file(*id, hash) == path);
            if !wanted {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vox_proto::canonical_wav;

    fn wav(v: i16, mono_frames: usize) -> Vec<u8> {
        canonical_wav(1, &vec![v; mono_frames])
    }

    #[test]
    fn put_get_and_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let cache = SoundCache::new(dir.path().join("srv"));
        assert!(cache.get(1, "abc").is_none() && !cache.has(1, "abc"));
        let pcm = cache.put(1, "abc", &wav(7, 9600)).unwrap();
        assert_eq!((pcm.channels, pcm.frames(), pcm.duration_ms()), (1, 9600, 200));
        assert!(cache.has(1, "abc"));
        assert_eq!(cache.get(1, "abc").unwrap(), pcm);
        // A new process: from disk.
        let again = SoundCache::new(dir.path().join("srv"));
        assert!(again.has(1, "abc"));
        assert_eq!(again.get(1, "abc").unwrap().samples[0], 7);
        // Another hash of the same id is another clip.
        assert!(again.get(1, "def").is_none());
        // Mono is expanded to stereo for previews only.
        assert_eq!(pcm.to_stereo().len(), 2 * 9600);
    }

    #[test]
    fn bad_downloads_and_damaged_files_leave_nothing_usable() {
        let dir = tempfile::tempdir().unwrap();
        let cache = SoundCache::new(dir.path());
        assert!(cache.put(2, "x", b"not a wav").is_err());
        assert!(!cache.has(2, "x"));
        cache.put(3, "y", &wav(1, 9600)).unwrap();
        std::fs::write(dir.path().join("3-y.wav"), b"RIFFdamaged").unwrap();
        let fresh = SoundCache::new(dir.path());
        assert!(fresh.get(3, "y").is_none());
        assert!(!dir.path().join("3-y.wav").exists(), "the damaged file is removed");
        // A hostile hash cannot leave the directory.
        cache.put(4, "../../evil", &wav(1, 9600)).unwrap();
        assert!(dir.path().join("4-evil.wav").exists());
    }

    #[test]
    fn memory_is_bounded_and_retain_prunes_files() {
        let dir = tempfile::tempdir().unwrap();
        let cache = SoundCache::new(dir.path());
        for id in 0..(MEM_CLIPS as i64 + 5) {
            cache.put(id, "h", &wav(id as i16, 4800)).unwrap();
        }
        assert_eq!(cache.mem.lock().len(), MEM_CLIPS);
        assert!(cache.get(0, "h").is_some(), "evicted from memory, still on disk");
        cache.retain(&[(0, "h".into()), (1, "h".into())]);
        let mut left: Vec<String> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(left, ["0-h.wav", "1-h.wav"]);
        assert!(cache.get(7, "h").is_none());
    }
}
