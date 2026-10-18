//! Soundboard in the backend: the sound list, the download cache, the rules for playing a sound
//! someone else triggered, and the upload pipeline.
//!
//! Playing never goes through LiveKit. The hub says `SoundPlayed` to the members of one voice
//! channel; this client then (1) checks it is for the room it is in, (2) applies the personal
//! rules (soundboard muted, this sound muted, deafened: nothing is played), (3) finds the clip in
//! the cache, downloading it first when it has not been seen yet, and (4) queues it in the voice
//! session's playback mixer, next to the voices, so it reaches the same speakers and the echo
//! canceller's reference. With `VOX_NO_PLAYBACK` (the silent autotests) the mixer has no device;
//! the clip is queued all the same and the mix is pulled by hand to measure what B would have
//! heard (`SoundEv::MixerHeard`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use vox_client_core::audio::MIX_10MS;
use vox_client_core::hub::{HttpStatusError, SoundsReply};
use vox_client_core::soundboard::{prepare_file, ProcessError, SoundCache, SoundPcm, Trim};
use vox_proto::{Sound, SoundId, User};

use super::*;
use crate::soundboard::{Heard, SoundCmd, SoundEv, SoundOp};

/// A clip that arrives later than this after the hub announced it is not played any more (the
/// moment has passed).
const MAX_LATE: Duration = Duration::from_millis(3000);

pub(super) struct SoundState {
    /// The current list (shared with the prefetch task).
    list: Arc<Mutex<Vec<Sound>>>,
    /// `None` until the hub answered once on this connection.
    supported: Option<bool>,
    cache: Option<Arc<SoundCache>>,
    volume_pct: u32,
    muted: bool,
    muted_sounds: HashSet<SoundId>,
    prefetching: Arc<AtomicBool>,
}

impl Default for SoundState {
    fn default() -> Self {
        Self {
            list: Arc::default(),
            supported: None,
            cache: None,
            volume_pct: 50,
            muted: false,
            muted_sounds: HashSet::new(),
            prefetching: Arc::default(),
        }
    }
}

impl SoundState {
    fn gain(&self) -> f32 {
        self.volume_pct.min(100) as f32 / 100.0
    }
}

/// A clip that was not cached when its play event came: it was downloaded and is ready.
pub(super) struct LateSound {
    pub voice_gen: u64,
    pub sound_id: SoundId,
    pub by: User,
    pub pcm: Result<Arc<SoundPcm>, String>,
    pub asked: Instant,
    /// A preview that cannot go through a voice session plays on this output device (`None`: the
    /// default one). Only the trim editor's "Ouvir trecho" (`sound_id` < 0) uses it.
    pub device: Option<String>,
}

/// Where downloaded clips live: `VOX_SOUND_CACHE` (tests), else `%LOCALAPPDATA%\concord\sounds`.
/// An autotest without `VOX_SOUND_CACHE` uses a temporary folder of its own: tests never write
/// into the real profile (a `sounds` folder there would also count as "already migrated").
fn cache_root() -> PathBuf {
    if let Some(p) = std::env::var_os("VOX_SOUND_CACHE").filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    if vox_client_core::testmode::background() {
        return std::env::temp_dir().join(format!("concord-autotest-sounds-{}", std::process::id()));
    }
    let base = std::env::var_os("LOCALAPPDATA").or_else(|| std::env::var_os("APPDATA")).map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("concord").join("sounds")
}

/// One directory per server (ids are per server): its address with the odd characters replaced.
fn server_dir(base: &Path, host: &str, port: u16) -> PathBuf {
    let host = format!("{host}_{port}");
    base.join(host.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' }).collect::<String>())
}

/// A failed hub call for a person: what happened, in Portuguese.
pub(super) fn friendly_error(e: &anyhow::Error) -> String {
    if let Some(h) = e.downcast_ref::<HttpStatusError>() {
        return match h.status {
            400 => format!("O servidor recusou o áudio: {}.", h.message),
            401 => "Sua sessão expirou. Entre de novo.".into(),
            403 => "Só quem enviou o som (ou um moderador) pode mudar ou apagar.".into(),
            404 => "Esse som não existe mais.".into(),
            409 => format!("O soundboard está cheio ({} sons). Apague algum antes.", vox_proto::SOUND_MAX_COUNT),
            413 => "O som passou de 1 MB.".into(),
            429 => "Muitos envios seguidos. Espere um pouco e tente de novo.".into(),
            s => format!("O servidor respondeu com erro {s}."),
        };
    }
    "Não foi possível falar com o servidor.".into()
}

impl Controller {
    fn sound_cache(&mut self) -> Option<Arc<SoundCache>> {
        if self.sb.cache.is_none() {
            let hub = self.hub.as_ref()?;
            let url = hub.base_url();
            self.sb.cache = Some(Arc::new(SoundCache::new(server_dir(&cache_root(), url.host_str().unwrap_or("hub"), url.port_or_known_default().unwrap_or(0)))));
        }
        self.sb.cache.clone()
    }

    fn sb_list(&self) -> Vec<Sound> {
        self.sb.list.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The hub snapshot arrived (login, reconnect): ask for the sound list.
    pub(super) fn sounds_on_ready(&mut self) {
        let Some(hub) = self.hub.clone() else { return };
        let (gen, int_tx) = (self.hub_gen, self.int_tx.clone());
        tokio::spawn(async move {
            let _ = int_tx.send(Internal::Sounds(gen, hub.sounds().await));
        });
    }

    pub(super) fn sounds_listed(&mut self, reply: SoundsReply) {
        match reply {
            SoundsReply::Ready(list) => {
                self.sb.supported = Some(true);
                self.sounds_changed(list);
            }
            SoundsReply::Unsupported => {
                info!("the hub has no soundboard (older server): the Sons button stays hidden");
                self.sb.supported = Some(false);
                self.sb.list.lock().unwrap_or_else(|e| e.into_inner()).clear();
                self.out.send(Ev::Sound(SoundEv::List { supported: false, sounds: Vec::new() }));
            }
            SoundsReply::Failed(why) => warn!(%why, "could not load the sound list"),
        }
    }

    /// A new list (from the hub's `SoundsChanged`, or the answer to `GET /api/sounds`).
    pub(super) fn sounds_changed(&mut self, list: Vec<Sound>) {
        if self.sb.supported.is_none() {
            self.sb.supported = Some(true);
        }
        *self.sb.list.lock().unwrap_or_else(|e| e.into_inner()) = list.clone();
        if let Some(cache) = self.sound_cache() {
            let keep: Vec<_> = list.iter().map(|s| (s.id, s.hash.clone())).collect();
            tokio::task::spawn_blocking(move || cache.retain(&keep));
        }
        self.out.send(Ev::Sound(SoundEv::List { supported: true, sounds: list }));
        self.prefetch_sounds();
    }

    /// Forget everything about the sounds (logout / another server).
    pub(super) fn sounds_reset(&mut self) {
        self.sb.supported = None;
        self.sb.cache = None;
        self.sb.list.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.out.send(Ev::Sound(SoundEv::List { supported: false, sounds: Vec::new() }));
    }

    /// We are in a voice channel now: apply the volume / mute and fetch the clips we do not have.
    pub(super) fn sounds_voice_joined(&mut self) {
        self.apply_sound_gain();
        self.prefetch_sounds();
    }

    fn apply_sound_gain(&self) {
        if let Some(v) = &self.voice {
            v.set_soundboard(self.sb.gain(), self.sb.muted);
        }
    }

    /// Download, in the background and one at a time, the clips of the list that are not cached
    /// (only while in a voice channel: that is the only place they are ever played). A clip that
    /// is played before its turn is fetched on demand instead.
    fn prefetch_sounds(&mut self) {
        if self.voice_channel.is_none() || self.sb.supported != Some(true) {
            return;
        }
        let (Some(hub), Some(cache)) = (self.hub.clone(), self.sound_cache()) else { return };
        if self.sb.prefetching.swap(true, Ordering::AcqRel) {
            return; // the running task re-reads the list when it finishes an item
        }
        let (list, flag) = (self.sb.list.clone(), self.sb.prefetching.clone());
        tokio::spawn(async move {
            loop {
                let next = list.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|s| !cache.has(s.id, &s.hash)).cloned();
                let Some(s) = next else { break };
                match hub.sound_audio(s.id, &s.hash).await.and_then(|wav| cache.put(s.id, &s.hash, &wav).map(|_| ())) {
                    Ok(()) => tracing::debug!(id = s.id, name = %s.name, "sound downloaded"),
                    Err(e) => {
                        warn!(id = s.id, error = %format!("{e:#}"), "sound download failed");
                        break;
                    }
                }
            }
            flag.store(false, Ordering::Release);
        });
    }

    pub(super) fn sound_cmd(&mut self, cmd: SoundCmd) {
        match cmd {
            SoundCmd::Prefs { volume_pct, muted, muted_sounds } => {
                self.sb.volume_pct = volume_pct.min(100);
                self.sb.muted = muted;
                self.sb.muted_sounds = muted_sounds.into_iter().collect();
                self.apply_sound_gain();
            }
            SoundCmd::Play(id) => {
                let sent = self.hub_tx.as_ref().map(|tx| tx.send(ClientEvent::PlaySound { sound_id: id }));
                if !matches!(sent, Some(Ok(()))) {
                    self.out.send(Ev::Notice("Som não enviado: sem conexão com o servidor.".into()));
                }
            }
            SoundCmd::Preview(id) => self.sound_preview(id),
            SoundCmd::Upload { path, name, emoji, trim } => self.sound_upload(path, name, emoji, trim),
            SoundCmd::PreviewClip { path, trim, output_device } => self.sound_preview_clip(path, trim, output_device),
            SoundCmd::Rename { id, name, emoji } => {
                let (Some(hub), out) = (self.hub.clone(), self.out.clone()) else { return };
                tokio::spawn(async move {
                    let r = hub.rename_sound(id, &name, Some(&emoji)).await;
                    done(&out, SoundOp::Rename, r.map(|_| ()));
                });
            }
            SoundCmd::Delete(id) => {
                let (Some(hub), out) = (self.hub.clone(), self.out.clone()) else { return };
                tokio::spawn(async move {
                    let r = hub.delete_sound(id).await;
                    done(&out, SoundOp::Delete, r);
                });
            }
        }
    }

    /// Decode, normalise and upload a file (or just the `trim` part of it, streamed from disk);
    /// the new list arrives by the hub's broadcast.
    fn sound_upload(&mut self, path: PathBuf, name: String, emoji: String, trim: Option<Trim>) {
        let out = self.out.clone();
        let cache = self.sound_cache();
        let Some(hub) = self.hub.clone() else {
            return done(&out, SoundOp::Upload, Err(anyhow::anyhow!("offline")));
        };
        tokio::spawn(async move {
            let fail = |msg: String| out.send(Ev::Sound(SoundEv::Done { op: SoundOp::Upload, result: Err(msg) }));
            let size = tokio::fs::metadata(&path).await.map(|m| m.len()).map_err(|_| ());
            match size {
                Err(()) => return fail("Não consegui abrir esse arquivo. Confira o caminho.".into()),
                Ok(n) if n > vox_client_core::soundboard::process::MAX_SOURCE_BYTES as u64 => return fail(ProcessError::TooBig.to_string()),
                Ok(_) => {}
            }
            let t = Instant::now();
            let prepared = match tokio::task::spawn_blocking(move || prepare_file(&path, trim.as_ref())).await {
                Ok(Ok(p)) => p,
                Ok(Err(e)) => return fail(e.to_string()),
                Err(_) => return fail("Não foi possível processar o áudio.".into()),
            };
            info!(
                ms = t.elapsed().as_millis() as u64,
                duration_ms = prepared.duration_ms,
                channels = prepared.channels,
                source_rate = prepared.source_rate,
                lufs_before = prepared.loudness_before,
                gain_db = prepared.gain_db,
                trimmed_start_ms = prepared.trimmed_start_ms,
                trimmed_end_ms = prepared.trimmed_end_ms,
                selection = ?trim.map(|t| (t.start_ms, t.end_ms)),
                bytes = prepared.wav.len(),
                "sound prepared"
            );
            let wav = prepared.wav.clone();
            match hub.upload_sound(&name, &emoji, prepared.wav).await {
                Ok(sound) => {
                    // The hub stores exactly these samples: keep them, nothing to download.
                    if let Some(cache) = cache {
                        let _ = tokio::task::spawn_blocking(move || cache.put(sound.id, &sound.hash, &wav)).await;
                    }
                    out.send(Ev::Sound(SoundEv::Done { op: SoundOp::Upload, result: Ok(()) }));
                }
                Err(e) => {
                    warn!(error = %format!("{e:#}"), "sound upload failed");
                    fail(friendly_error(&e));
                }
            }
        });
    }

    /// "Ouvir trecho": cut the part of the file exactly as an upload would, then play it here only.
    /// In a voice channel it goes through the session's output (the person's speakers, and the echo
    /// canceller's reference), otherwise to `device`. Headless runs play nothing; they just say the
    /// clip is ready.
    fn sound_preview_clip(&mut self, path: PathBuf, trim: Trim, device: Option<String>) {
        let (out, int_tx, voice_gen, no_playback) = (self.out.clone(), self.int_tx.clone(), self.voice_gen, self.no_playback);
        tokio::spawn(async move {
            let say = |r: Result<u32, String>| out.send(Ev::Sound(SoundEv::ClipPreview(r)));
            let prepared = match tokio::task::spawn_blocking(move || prepare_file(&path, Some(&trim))).await {
                Ok(Ok(p)) => p,
                Ok(Err(e)) => return say(Err(e.to_string())),
                Err(_) => return say(Err("Não foi possível processar o áudio.".into())),
            };
            let pcm = match SoundPcm::from_wav(&prepared.wav) {
                Ok(p) => Arc::new(p),
                Err(e) => return say(Err(format!("{e:#}"))),
            };
            if no_playback {
                info!(ms = pcm.duration_ms(), "clip preview suppressed (VOX_NO_PLAYBACK)");
                return say(Ok(pcm.duration_ms()));
            }
            let late = LateSound { voice_gen, sound_id: -1, by: User::new(-1, ""), pcm: Ok(pcm), asked: Instant::now(), device };
            let _ = int_tx.send(Internal::LateSound(late));
        });
    }

    /// "Ouvir só aqui".
    fn sound_preview(&mut self, id: SoundId) {
        if self.no_playback {
            tracing::debug!(id, "sound preview suppressed (VOX_NO_PLAYBACK)");
            return;
        }
        let Some(sound) = self.sb_list().into_iter().find(|s| s.id == id) else { return };
        let gain = self.sb.gain();
        match self.sound_pcm(&sound) {
            Some(pcm) => {
                if let Some(v) = &self.voice {
                    v.preview_sound(&pcm, gain);
                }
            }
            None => {
                // Download first; the result comes back as a late sound with no `by`: preview.
                self.fetch_late(sound, None);
            }
        }
    }

    fn sound_pcm(&mut self, s: &Sound) -> Option<Arc<SoundPcm>> {
        self.sound_cache()?.get(s.id, &s.hash)
    }

    /// Download a clip nobody had cached. `by`: who played it (`None`: a local preview).
    fn fetch_late(&mut self, sound: Sound, by: Option<User>) {
        let (Some(hub), Some(cache)) = (self.hub.clone(), self.sound_cache()) else { return };
        let (int_tx, voice_gen, asked) = (self.int_tx.clone(), self.voice_gen, Instant::now());
        tokio::spawn(async move {
            let pcm = match hub.sound_audio(sound.id, &sound.hash).await {
                Ok(wav) => cache.put(sound.id, &sound.hash, &wav).map_err(|e| format!("{e:#}")),
                Err(e) => Err(format!("{e:#}")),
            };
            let by = by.unwrap_or_else(|| User::new(-1, ""));
            let _ = int_tx.send(Internal::LateSound(LateSound { voice_gen, sound_id: sound.id, by, pcm, asked, device: None }));
        });
    }

    /// A clip that had to be downloaded first is here.
    pub(super) fn late_sound_ready(&mut self, late: LateSound) {
        if late.voice_gen != self.voice_gen {
            return;
        }
        let pcm = match late.pcm {
            Ok(p) => p,
            Err(e) => return warn!(id = late.sound_id, error = %e, "sound download failed"),
        };
        if late.by.id < 0 {
            // A preview that waited for its download, or the trim editor's clip.
            let gain = self.sb.gain();
            let played = match &self.voice {
                Some(v) => v.preview_sound(&pcm, gain),
                // Not in a voice channel: the output device is opened just for it.
                None if late.sound_id < 0 => self.ui_player.play_pcm(pcm.to_stereo(), gain, late.device.as_deref()),
                None => false,
            };
            if late.sound_id < 0 {
                let r = if played { Ok(pcm.duration_ms()) } else { Err("Não consegui tocar: a saída de áudio não está disponível.".to_string()) };
                self.out.send(Ev::Sound(SoundEv::ClipPreview(r)));
            }
            return;
        }
        if late.asked.elapsed() > MAX_LATE {
            return info!(id = late.sound_id, "downloaded sound arrived too late; not played");
        }
        // The personal rules may have changed while it downloaded.
        if self.sb.muted || self.sb.muted_sounds.contains(&late.sound_id) || self.voice.as_ref().is_none_or(|v| v.is_deafened()) {
            return;
        }
        self.queue_sound(late.sound_id, &pcm);
    }

    /// `SoundPlayed` from the hub. Always tells the UI (the tile flashes even when nothing is
    /// played here); plays the clip when the personal rules allow.
    pub(super) fn sound_played(&mut self, sound_id: SoundId, by: User, channel_id: ChannelId) {
        let heard = self.sound_played_inner(sound_id, &by, channel_id);
        if let Some(heard) = heard {
            self.out.send(Ev::Sound(SoundEv::Played { sound_id, by, heard }));
        }
    }

    fn sound_played_inner(&mut self, sound_id: SoundId, by: &User, channel_id: ChannelId) -> Option<Heard> {
        // Only the room we are in (a stale event after leaving or moving is ignored).
        if self.voice_channel != Some(channel_id) || self.voice.is_none() {
            return None;
        }
        let heard = if self.sb.muted {
            Heard::Muted
        } else if self.sb.muted_sounds.contains(&sound_id) {
            Heard::SoundMuted
        } else if self.voice.as_ref().is_some_and(|v| v.is_deafened()) {
            Heard::Deafened
        } else {
            let Some(sound) = self.sb_list().into_iter().find(|s| s.id == sound_id) else {
                warn!(sound_id, "played sound is not in the list (yet); ignored");
                return None;
            };
            match self.sound_pcm(&sound) {
                Some(pcm) => {
                    if self.queue_sound(sound_id, &pcm) {
                        Heard::Playing
                    } else {
                        Heard::NoOutput
                    }
                }
                None => {
                    self.fetch_late(sound, Some(by.clone()));
                    Heard::Downloading
                }
            }
        };
        info!(sound_id, by = %by.username, ?heard, "sound played in my room");
        Some(heard)
    }

    /// Hand a clip to the voice session's mixer; headless runs also measure the mix.
    fn queue_sound(&mut self, sound_id: SoundId, pcm: &SoundPcm) -> bool {
        let Some(v) = &self.voice else { return false };
        v.set_soundboard(self.sb.gain(), self.sb.muted);
        if !v.play_sound(pcm) {
            return false;
        }
        if self.no_playback {
            measure_headless(v.playback_mixer(), sound_id, pcm.duration_ms(), self.out.clone());
        }
        true
    }
}

fn done(out: &Out, op: SoundOp, r: anyhow::Result<()>) {
    let result = r.map_err(|e| {
        warn!(?op, error = %format!("{e:#}"), "soundboard call failed");
        friendly_error(&e)
    });
    out.send(Ev::Sound(SoundEv::Done { op, result }));
}

/// No output device exists in a silent run: pull the playback mix by hand until the clips are
/// done and report how loud it was. Nothing is ever played.
fn measure_headless(mixer: Arc<vox_client_core::audio::Mixer>, sound_id: SoundId, clip_ms: u32, out: Out) {
    tokio::task::spawn_blocking(move || {
        let mut block = vec![0i16; MIX_10MS];
        let (mut sum, mut n, mut peak) = (0f64, 0u64, 0i16);
        let started = Instant::now();
        while mixer.sounds_active() > 0 && started.elapsed() < Duration::from_secs(10) {
            mixer.mix(&mut block);
            for &s in &block {
                sum += (s as f64 / 32768.0).powi(2);
                peak = peak.max(s.saturating_abs());
            }
            n += block.len() as u64;
        }
        let rms = if n > 0 { (sum / n as f64).sqrt() } else { 0.0 };
        let rms_dbfs = if rms > 0.0 { 20.0 * rms.log10() as f32 } else { f32::NEG_INFINITY };
        let ms = (n / 2 * 1000 / 48_000) as u32;
        tracing::debug!(sound_id, clip_ms, ms, "headless mix measured");
        out.send(Ev::Sound(SoundEv::MixerHeard { sound_id, rms_dbfs, peak: peak as f32 / 32768.0, ms }));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn http(status: u16) -> anyhow::Error {
        anyhow::Error::new(HttpStatusError { status, message: "boom".into() }).context("sound upload failed")
    }

    #[test]
    fn hub_refusals_become_portuguese_sentences() {
        assert!(friendly_error(&http(403)).starts_with("Só quem enviou"));
        assert!(friendly_error(&http(409)).contains("cheio") && friendly_error(&http(409)).contains("50"));
        assert!(friendly_error(&http(413)).contains("1 MB"));
        assert!(friendly_error(&http(429)).contains("Espere"));
        assert!(friendly_error(&http(404)).contains("não existe"));
        assert!(friendly_error(&http(400)).contains("boom"), "the hub's reason is passed on");
        assert!(friendly_error(&http(500)).contains("500"));
        assert!(friendly_error(&anyhow::anyhow!("connection refused")).contains("servidor"));
    }

    #[test]
    fn each_server_has_its_own_safe_cache_directory() {
        let base = Path::new("cache");
        assert_eq!(server_dir(base, "192.168.1.50", 7890), base.join("192.168.1.50_7890"));
        assert_ne!(server_dir(base, "a.example.com", 7890), server_dir(base, "a.example.com", 7891));
        let odd = server_dir(base, "../../x/y:z", 1);
        assert_eq!(odd.parent(), Some(base), "never leaves the cache directory: {odd:?}");
    }

    #[test]
    fn the_default_volume_is_half() {
        let s = SoundState::default();
        assert_eq!((s.volume_pct, s.muted, s.gain()), (50, false, 0.5));
        let loud = SoundState { volume_pct: 400, ..SoundState::default() };
        assert_eq!(loud.gain(), 1.0, "capped at 100 %");
    }
}
