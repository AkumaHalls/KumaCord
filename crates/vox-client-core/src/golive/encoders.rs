//! Automatic video encoder choice (v0.7), shared by Go Live and the camera.
//!
//! vox-capture lists the machine's encoders (NVENC on each NVIDIA GPU, the Media Foundation
//! hardware MFT on each GPU, the processor last) and self-tests them; this module keeps the
//! results (cached in the app's settings folder, keyed by GPUs + drivers + app version), the
//! user's preference (Settings → Vídeo), the encoders demoted during this session, and turns all
//! of it into the ordered list of encoders a share or camera start tries ([`plan`]).
//!
//! The self-test runs once in the background, at low priority, the first time the app starts on
//! a machine (and again when a GPU, a driver or the app changes, or on "Testar de novo"). Until
//! it finished, the static ranking is used: nothing ever waits for it.
//!
//! [`StormDetector`] is the runtime safety net: keyframe requests without pause while the uplink
//! loses nothing *and* a viewer confirming it decodes badly on a clean network ([`RxHealthReport`],
//! sent over the LiveKit data channel) means the bitstream itself is bad; the voice session then
//! demotes the encoder ([`demote`]) and restarts the share / camera with the next one. A storm
//! nobody confirms (a viewer on bad Wi-Fi makes the SFU forward its PLIs to us) keeps the encoder.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use vox_capture::gpu::{self, CandidateStatus, EncoderCandidate, EncoderKind, GpuInfo, RankedCandidate, SelfTestResult, TestOutcome};
use vox_capture::EncoderBackend;

use super::publish::EncoderChoice;

/// The user's choice in Settings → Vídeo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderPref {
    /// "Aceleração de hardware": off = the processor for screen share and camera.
    pub hardware: bool,
    /// A specific encoder ([`EncoderOption::id`]); `None` = automatic.
    pub encoder: Option<String>,
}

impl Default for EncoderPref {
    fn default() -> Self {
        Self { hardware: true, encoder: None }
    }
}

/// One encoder to try.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attempt {
    pub id: String,
    /// Portuguese name ("NVIDIA GeForce GTX 1650 (NVENC)", "Processador (sem aceleração)").
    pub label: String,
    /// `None` = the processor (raw frames, OpenH264 in libwebrtc).
    pub backend: Option<EncoderBackend>,
    /// Adapter of the hardware encoder (the capture device goes there too).
    pub luid: Option<u64>,
}

impl Attempt {
    pub fn is_software(&self) -> bool {
        self.backend.is_none()
    }
}

/// One line of the settings' "Codificador" list.
#[derive(Debug, Clone, PartialEq)]
pub struct EncoderOption {
    pub id: String,
    pub label: String,
    pub kind: EncoderKind,
    pub status: CandidateStatus,
    /// Short Portuguese verdict: "✔ 1,3 ms", "falhou: …", "não testado".
    pub note: String,
    /// Switched away from during this session (unstable video).
    pub demoted: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct EncoderSnapshot {
    /// Best first, the processor last.
    pub options: Vec<EncoderOption>,
    pub testing: bool,
    /// What "Automático" picks right now.
    pub auto_label: String,
    /// Bumped on every change (the settings refresh their copy when it moves).
    pub generation: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct CacheFile {
    key: String,
    #[serde(default)]
    results: Vec<SelfTestResult>,
    /// Runtime failures (encoder switched away from while live), per candidate id.
    #[serde(default)]
    failures: BTreeMap<String, u32>,
    /// For a human reading the file.
    #[serde(default)]
    gpus: Vec<GpuInfo>,
}

#[derive(Default)]
struct State {
    detected: bool,
    gpus: Vec<GpuInfo>,
    cands: Vec<EncoderCandidate>,
    key: String,
    results: Vec<SelfTestResult>,
    failures: BTreeMap<String, u32>,
    demoted: HashSet<String>,
    testing: bool,
    cache: Option<PathBuf>,
    pref: EncoderPref,
    generation: u64,
}

fn state() -> &'static Mutex<State> {
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(Default::default)
}

impl State {
    /// List GPUs and candidates once (tens of ms; enumeration only, nothing is activated).
    fn detect(&mut self) {
        if self.detected {
            return;
        }
        self.detected = true;
        let t0 = Instant::now();
        self.gpus = vox_capture::list_gpus();
        self.cands = vox_capture::encoder_candidates(&self.gpus);
        self.key = gpu::selftest_key(env!("CARGO_PKG_VERSION"), &self.gpus);
        let gpus: Vec<String> = self
            .gpus
            .iter()
            .map(|g| {
                format!(
                    "{} [{:04x}:{:04x}, {}, {} MB, driver {}{}]",
                    g.name,
                    g.vendor_id,
                    g.device_id,
                    if g.integrated { "integrated" } else { "dedicated" },
                    g.dedicated_mb,
                    g.driver,
                    if g.primary { ", primary monitor" } else { "" }
                )
            })
            .collect();
        info!(gpus = %gpus.join("; "), candidates = %self.cands.iter().map(|c| c.id.as_str()).collect::<Vec<_>>().join(" > "), detect_ms = t0.elapsed().as_millis() as u64, "GPUs and video encoders detected");
        self.generation += 1;
    }

    fn ranked(&self) -> Vec<RankedCandidate> {
        gpu::rank(&self.cands, &self.results, &self.failures)
    }

    fn label(&self, c: &EncoderCandidate) -> String {
        c.label_pt(&self.cands)
    }

    fn attempt(&self, c: &EncoderCandidate) -> Attempt {
        Attempt { id: c.id.clone(), label: self.label(c), backend: c.backend(), luid: c.luid() }
    }

    fn load_cache(&mut self) -> bool {
        let Some(p) = &self.cache else { return false };
        let Ok(text) = std::fs::read_to_string(p) else { return false };
        match serde_json::from_str::<CacheFile>(&text) {
            Ok(c) if c.key == self.key => {
                self.results = c.results;
                self.failures = c.failures;
                self.generation += 1;
                true
            }
            Ok(_) => {
                info!("encoder self-test cache is for other GPUs, drivers or app version; testing again");
                false
            }
            Err(e) => {
                warn!(error = %e, "encoder self-test cache unreadable; testing again");
                false
            }
        }
    }

    fn save_cache(&self) {
        let Some(p) = &self.cache else { return };
        let file = CacheFile { key: self.key.clone(), results: self.results.clone(), failures: self.failures.clone(), gpus: self.gpus.clone() };
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = p.with_extension("json.tmp");
        let res = serde_json::to_string_pretty(&file)
            .map_err(|e| e.to_string())
            .and_then(|t| std::fs::write(&tmp, t).map_err(|e| e.to_string()))
            .and_then(|_| std::fs::rename(&tmp, p).map_err(|e| e.to_string()));
        if let Err(e) = res {
            warn!(error = %e, "cannot save the encoder self-test cache");
        }
    }

    fn log_ranking(&self) {
        let ranked = self.ranked();
        let parts: Vec<String> = ranked
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let what = match (&r.result, r.status) {
                    (_, _) if r.candidate.kind == EncoderKind::Software => "fallback".to_string(),
                    (Some(t), CandidateStatus::Passed) => format!("pass {:.1} ms {} kbps", t.ms_per_frame, t.kbps),
                    (_, CandidateStatus::Failed) if r.failures >= gpu::MAX_RUNTIME_FAILURES => format!("failed live {}x", r.failures),
                    (_, CandidateStatus::Failed) => "FAIL".into(),
                    (_, _) => "untested".into(),
                };
                format!("{}. {} [{}]", i + 1, self.label(&r.candidate), what)
            })
            .collect();
        info!(ranking = %parts.join(" > "), "video encoder ranking");
    }
}

/// Start the encoder machinery: detect the GPUs, load the cached self-test results from
/// `cache_file` (`None`: nothing is read or written) and, when they are missing or stale and
/// `self_test` is on, test in the background. Returns at once; idempotent.
pub fn init(cache_file: Option<PathBuf>, self_test: bool) {
    static STARTED: OnceLock<()> = OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }
    state().lock().cache = cache_file;
    spawn_worker(move || {
        let fresh = {
            let mut st = state().lock();
            st.detect();
            st.load_cache()
        };
        if fresh {
            info!("encoder self-test: using cached results");
            state().lock().log_ranking();
        } else if self_test {
            run_self_test();
        } else {
            state().lock().log_ranking();
        }
    });
}

/// "Testar de novo": forget the results and this session's demotions, test again (background).
pub fn retest() {
    {
        let mut st = state().lock();
        if st.testing {
            return;
        }
        st.testing = true;
        st.generation += 1;
    }
    spawn_worker(|| {
        {
            let mut st = state().lock();
            st.detected = false;
            st.detect();
            st.failures.clear();
            st.demoted.clear();
        }
        run_self_test();
    });
}

fn spawn_worker(f: impl FnOnce() + Send + 'static) {
    let r = std::thread::Builder::new().name("vox-enc-selftest".into()).spawn(move || {
        lower_thread_priority();
        f()
    });
    if let Err(e) = r {
        warn!(error = %e, "cannot start the encoder self-test thread");
        state().lock().testing = false;
    }
}

fn lower_thread_priority() {
    #[cfg(windows)]
    // SAFETY: pseudo-handle of the current thread.
    unsafe {
        use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL};
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
    }
}

fn run_self_test() {
    let cands = {
        let mut st = state().lock();
        st.testing = true;
        st.generation += 1;
        st.cands.clone()
    };
    let t0 = Instant::now();
    let results = vox_capture::self_test_encoders(&cands, &mut |r| {
        let mut st = state().lock();
        st.results.retain(|x| x.id != r.id);
        st.results.push(r.clone());
        st.generation += 1;
    });
    let mut st = state().lock();
    // NVENC is only ever skipped when the NVIDIA MFT ran in this process first (a share forced to
    // it): those results are not the machine's, test again next time instead of caching them.
    let complete = !results.iter().any(|r| r.id.starts_with("nvenc:") && matches!(r.outcome, TestOutcome::Skipped { .. }));
    st.results = results;
    st.testing = false;
    st.generation += 1;
    if complete {
        st.save_cache();
    }
    info!(total_ms = t0.elapsed().as_millis() as u64, "encoder self-test finished");
    st.log_ranking();
}

pub fn set_preference(p: EncoderPref) {
    let mut st = state().lock();
    if st.pref != p {
        info!(hardware = p.hardware, encoder = ?p.encoder, "video encoder preference");
        st.pref = p;
        st.generation += 1;
    }
}

pub fn preference() -> EncoderPref {
    state().lock().pref.clone()
}

/// The ordered list of encoders to try for `choice`, the processor always last. `Auto` follows
/// the user's preference and the ranking; `Nvenc` / `MediaFoundation` (vox-cli, autotests) use
/// only that backend's candidates. Demoted encoders are left out.
pub fn plan(choice: EncoderChoice) -> Vec<Attempt> {
    let mut st = state().lock();
    st.detect();
    let ranked = st.ranked();
    let usable = |c: &EncoderCandidate| c.kind != EncoderKind::Software && !st.demoted.contains(&c.id);
    let mut list: Vec<&EncoderCandidate> = match choice {
        EncoderChoice::Software => Vec::new(),
        EncoderChoice::Nvenc | EncoderChoice::MediaFoundation => {
            let want = if choice == EncoderChoice::Nvenc { EncoderBackend::Nvenc } else { EncoderBackend::MediaFoundation };
            ranked.iter().map(|r| &r.candidate).filter(|c| c.backend() == Some(want) && usable(c)).collect()
        }
        EncoderChoice::Auto if !st.pref.hardware || st.pref.encoder.as_deref() == Some(gpu::SOFTWARE_ID) => Vec::new(),
        EncoderChoice::Auto => {
            let auto: Vec<&EncoderCandidate> = gpu::auto_order(&ranked).into_iter().filter(|c| usable(c)).collect();
            match st.pref.encoder.as_deref().and_then(|id| ranked.iter().map(|r| &r.candidate).find(|c| c.id == id)) {
                // The user's pick first (even untested or failed: they asked), then the rest.
                Some(c) if usable(c) => std::iter::once(c).chain(auto.into_iter().filter(|x| x.id != c.id)).collect(),
                _ => auto,
            }
        }
    };
    let sw = EncoderCandidate::software();
    list.push(&sw);
    list.into_iter().map(|c| st.attempt(c)).collect()
}

/// "a > b > Processador" for logs.
pub fn describe(plan: &[Attempt]) -> String {
    plan.iter().map(|a| a.label.as_str()).collect::<Vec<_>>().join(" > ")
}

/// Stop using `id` for the rest of this session; `persist` (viewer-confirmed only) also counts a
/// failure in the cache ([`gpu::MAX_RUNTIME_FAILURES`] of them and it stops being automatic).
/// Returns the label of what an automatic start picks now.
pub fn demote(id: &str, reason: &str, persist: bool) -> String {
    let mut st = state().lock();
    st.demoted.insert(id.to_string());
    if persist {
        *st.failures.entry(id.to_string()).or_default() += 1;
    }
    st.generation += 1;
    let fails = st.failures.get(id).copied().unwrap_or(0);
    warn!(encoder = id, reason, failures = fails, counted = persist, "video encoder demoted for this session");
    if persist {
        st.save_cache();
    }
    drop(st);
    plan(EncoderChoice::Auto).first().map(|a| a.label.clone()).unwrap_or_default()
}

/// Portuguese label of a candidate id.
pub fn label(id: &str) -> String {
    let st = state().lock();
    match st.cands.iter().find(|c| c.id == id) {
        Some(c) => st.label(c),
        None if id == gpu::SOFTWARE_ID => EncoderCandidate::software().label_pt(&[]),
        None => id.to_string(),
    }
}

/// For the settings (cheap; never blocks on the self-test).
pub fn snapshot() -> EncoderSnapshot {
    let mut st = state().lock();
    st.detect();
    let ranked = st.ranked();
    let options = ranked
        .iter()
        .map(|r| {
            let note = match (r.status, &r.result) {
                _ if r.candidate.kind == EncoderKind::Software => "sempre funciona; usa mais CPU".to_string(),
                (CandidateStatus::Failed, _) if r.failures >= gpu::MAX_RUNTIME_FAILURES => "falhou durante o uso".into(),
                (CandidateStatus::Failed, Some(SelfTestResult { outcome: TestOutcome::Failed { reason }, .. })) => format!("falhou: {reason}"),
                (CandidateStatus::Passed, Some(t)) => format!("✔ {:.1} ms por quadro", t.ms_per_frame).replace('.', ","),
                (_, Some(SelfTestResult { outcome: TestOutcome::Skipped { .. }, .. })) => "não testado (reserva)".into(),
                _ if st.testing => "testando…".into(),
                _ => "não testado".into(),
            };
            EncoderOption {
                id: r.candidate.id.clone(),
                label: st.label(&r.candidate),
                kind: r.candidate.kind,
                status: r.status,
                note,
                demoted: st.demoted.contains(&r.candidate.id),
            }
        })
        .collect();
    let auto = gpu::auto_order(&ranked).into_iter().find(|c| c.kind == EncoderKind::Software || !st.demoted.contains(&c.id)).map(|c| st.label(c)).unwrap_or_default();
    EncoderSnapshot { options, testing: st.testing, auto_label: auto, generation: st.generation }
}

/// Cheap change counter for the settings UI.
pub fn generation() -> u64 {
    state().lock().generation
}

// ---------------------------------------------------------------------------------------------
// Runtime safety net
// ---------------------------------------------------------------------------------------------

/// How often the voice session samples a running encoder's RTP stats (and, as a viewer, the
/// decoders of the streams it receives).
pub const STORM_WINDOW: Duration = Duration::from_secs(5);
/// Keyframe requests (PLI + FIR) per second from the SFU that count as a storm. The field-report
/// case: 36-65 PLIs per 30 s at the viewer, with no packet lost.
pub const STORM_PLI_PER_S: f64 = 1.0;
/// Or: WebRTC discarding our encoded frames this often (an encoder that ignores the target).
pub const STORM_DROP_EVENTS_PER_S: f64 = 0.5;
/// ... while the receiver reports at most this much loss: the network is fine, the bitstream is
/// not (with loss, keyframe requests are the network's fault and another encoder won't help).
/// Also the most loss a viewer's own decode-health report may show to count.
pub const STORM_MAX_LOSS: f64 = 0.02;
/// Consecutive stormy windows before switching.
pub const STORM_WINDOWS: u32 = 2;
/// At most one switch per this long per stream, and never back to a demoted encoder (the plan
/// leaves those out): no flapping.
pub const SWITCH_MIN_INTERVAL: Duration = Duration::from_secs(60);
/// "keyframe storm without viewer confirmation" is logged at most this often per stream.
pub const UNCONFIRMED_LOG_INTERVAL: Duration = Duration::from_secs(60);

/// LiveKit data-channel topic of the viewers' decode-health reports ([`RxHealthReport`]).
pub const RX_HEALTH_TOPIC: &str = "vox-rx-health";
/// A viewer's decoder dropping frames this often (WebRTC `framesDropped`) while its own network
/// loses nothing is unhealthy. The field case: ~20 per 30 s (0.7/s), no packet lost.
pub const RX_DECODE_ERRORS_PER_S: f64 = 0.3;
/// A viewer sends at most one report per stream this often (and only unhealthy windows).
pub const RX_REPORT_MIN_INTERVAL: Duration = STORM_WINDOW;
/// How long a viewer's report backs a keyframe storm at the publisher.
pub const RX_CONFIRM_WINDOW: Duration = Duration::from_secs(15);
/// Reports of each of at least two viewers within [`RX_CONFIRM_WINDOW`], none with any loss,
/// that demote the encoder even without a keyframe storm reaching us (two windows of bad decoding).
pub const RX_SUSTAINED_REPORTS: usize = 2;
/// Payloads on the topic bigger than this are not ours (a real one is ~110 bytes).
pub const RX_MAX_PAYLOAD: usize = 512;
/// Reports the publisher keeps (all viewers, both streams); older ones fall off first.
const RX_KEPT: usize = 64;

/// One stats window of a running encoder.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct StormSample {
    pub secs: f64,
    /// PLIs + FIRs received.
    pub plis: u64,
    /// Runs of frames WebRTC dropped before sending (see `EncodedFeedback::drop_events`).
    pub drop_events: u64,
    pub packets_sent: u64,
    /// Lost packets the receiver (the SFU) reported for this window.
    pub packets_lost: u64,
    /// The receiver's last `fractionLost` (0..1).
    pub fraction_lost: f64,
}

impl StormSample {
    /// Loss on our uplink (sender -> SFU). It says nothing about a viewer's downlink.
    pub fn loss(&self) -> f64 {
        let by_count = if self.packets_sent > 0 { self.packets_lost as f64 / self.packets_sent as f64 } else { 0.0 };
        by_count.max(self.fraction_lost)
    }

    /// A window long enough to judge, with a clean uplink.
    pub fn clean(&self) -> bool {
        self.secs >= 1.0 && self.loss() <= STORM_MAX_LOSS
    }

    /// Keyframe requests without pause on a clean uplink (they may still be one viewer's network).
    pub fn pli_storm(&self) -> bool {
        self.clean() && self.plis as f64 / self.secs >= STORM_PLI_PER_S
    }

    /// WebRTC dropping our frames before sending: local evidence, no viewer involved.
    pub fn drop_storm(&self) -> bool {
        self.clean() && self.drop_events as f64 / self.secs >= STORM_DROP_EVENTS_PER_S
    }

    pub fn stormy(&self) -> bool {
        self.pli_storm() || self.drop_storm()
    }
}

/// Which of a participant's videos a report is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StreamKind {
    /// Go Live.
    Share,
    Camera,
    /// Missing or from a newer app: ignored.
    #[default]
    #[serde(other)]
    Other,
}

/// A viewer's decode health of one received video over one window, sent to its publisher over
/// the data channel (reliable, to that identity only, topic [`RX_HEALTH_TOPIC`]) and only when
/// unhealthy ([`RxHealthReport::unhealthy`]). Compact JSON; every field defaults and unknown
/// fields are ignored, so either side may be older or newer. Viewers on v0.6 never send one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RxHealthReport {
    pub kind: StreamKind,
    /// Length of the window (s).
    pub secs: f64,
    /// Frames decoded in the window.
    pub decoded: u64,
    /// Frames the decoder pipeline dropped or failed (WebRTC `framesDropped`).
    pub decode_errors: u64,
    /// The viewer's own downlink (SFU -> viewer) in the window.
    pub packets_lost: u64,
    pub packets_received: u64,
}

impl RxHealthReport {
    /// The viewer's downlink loss (0..1).
    pub fn loss(&self) -> f64 {
        let total = self.packets_lost + self.packets_received;
        if total > 0 {
            self.packets_lost as f64 / total as f64
        } else {
            0.0
        }
    }

    /// Decode errors at a rate that matters while this viewer's own network loses (almost)
    /// nothing: the stream it gets is broken, not its Wi-Fi.
    pub fn unhealthy(&self) -> bool {
        self.secs >= 1.0 && self.decode_errors as f64 / self.secs >= RX_DECODE_ERRORS_PER_S && self.loss() <= STORM_MAX_LOSS
    }

    pub fn to_payload(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }

    /// `None` for anything that is not a report about a known stream kind.
    pub fn from_payload(bytes: &[u8]) -> Option<Self> {
        if bytes.len() > RX_MAX_PAYLOAD {
            return None;
        }
        serde_json::from_slice::<Self>(bytes).ok().filter(|r| r.kind != StreamKind::Other && r.secs.is_finite())
    }

    /// "4 decode errors / 140 decoded in 5.0 s, loss 0.0%" for logs.
    pub fn describe(&self) -> String {
        format!("{} decode errors / {} decoded in {:.1} s, loss {:.1}%", self.decode_errors, self.decoded, self.secs, self.loss() * 100.0)
    }
}

/// Viewer side: only unhealthy windows, at most one report per publisher and stream per
/// [`RX_REPORT_MIN_INTERVAL`].
#[derive(Debug, Default)]
pub struct RxReportGate {
    last: HashMap<(String, StreamKind), Instant>,
}

impl RxReportGate {
    /// `true` = send `r` to `publisher` now.
    pub fn allow(&mut self, publisher: &str, r: &RxHealthReport, now: Instant) -> bool {
        if !r.unhealthy() {
            return false;
        }
        self.last.retain(|_, t| now.duration_since(*t) < SWITCH_MIN_INTERVAL);
        let key = (publisher.to_string(), r.kind);
        if self.last.get(&key).is_some_and(|t| now.duration_since(*t) < RX_REPORT_MIN_INTERVAL) {
            return false;
        }
        self.last.insert(key, now);
        true
    }
}

/// What the viewers said about one of our streams lately.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ViewerEvidence {
    /// A viewer reported decode errors with at most [`STORM_MAX_LOSS`] of its own loss within
    /// [`RX_CONFIRM_WINDOW`].
    pub confirmed: bool,
    /// At least two viewers did so [`RX_SUSTAINED_REPORTS`] times each in that window, never
    /// losing a packet. One viewer alone may just be a PC too slow to decode, not our stream.
    pub sustained: bool,
    /// The reporting viewers' last reports, for the log.
    pub detail: String,
}

/// Publisher side: the viewers' recent reports about our share and camera.
#[derive(Debug, Default)]
pub struct ViewerReports {
    recent: VecDeque<(Instant, String, RxHealthReport)>,
}

impl ViewerReports {
    pub fn add(&mut self, viewer: &str, r: RxHealthReport, now: Instant) {
        self.recent.retain(|(t, _, _)| now.duration_since(*t) <= RX_CONFIRM_WINDOW);
        if self.recent.len() >= RX_KEPT {
            self.recent.pop_front();
        }
        self.recent.push_back((now, viewer.to_string(), r));
    }

    /// The stream restarted on another encoder: what was said is about the old one.
    pub fn clear(&mut self, kind: StreamKind) {
        self.recent.retain(|(_, _, r)| r.kind != kind);
    }

    pub fn evidence(&self, kind: StreamKind, now: Instant) -> ViewerEvidence {
        // viewer -> (unhealthy reports, all without loss, last report)
        let mut per: BTreeMap<&str, (usize, bool, &RxHealthReport)> = BTreeMap::new();
        for (t, who, r) in &self.recent {
            if r.kind != kind || now.duration_since(*t) > RX_CONFIRM_WINDOW || !r.unhealthy() {
                continue;
            }
            let e = per.entry(who.as_str()).or_insert((0, true, r));
            e.0 += 1;
            e.1 &= r.packets_lost == 0;
            e.2 = r;
        }
        ViewerEvidence {
            confirmed: !per.is_empty(),
            sustained: per.values().filter(|(n, lossless, _)| *n >= RX_SUSTAINED_REPORTS && *lossless).count() >= 2,
            detail: per.iter().map(|(who, (n, _, r))| format!("{who} x{n}: {}", r.describe())).collect::<Vec<_>>().join("; "),
        }
    }
}

/// What [`StormDetector::update`] decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Keep,
    /// A keyframe storm on a clean uplink nobody confirms (a viewer's bad network makes the SFU
    /// forward its PLIs to us): the encoder stays. `log` = at most once a minute.
    Unconfirmed { log: bool },
    /// Switch to the next encoder now. `confirmed` = a viewer backs it, so it counts toward the
    /// persisted failures; `false` = only our own frame drops, this session only.
    Switch { confirmed: bool },
}

/// Decides when a stream's encoder must go (see the constants above): a keyframe storm on a
/// clean uplink that a viewer confirms, two viewers decoding badly on a clean network for two
/// windows, or WebRTC dropping our frames for two windows.
#[derive(Debug, Default)]
pub struct StormDetector {
    pli_hot: u32,
    drop_hot: u32,
    last_switch: Option<Instant>,
    last_note: Option<Instant>,
}

impl StormDetector {
    /// Feed one window and what the viewers said lately.
    pub fn update(&mut self, s: &StormSample, ev: &ViewerEvidence, now: Instant) -> Verdict {
        self.pli_hot = if s.pli_storm() { self.pli_hot + 1 } else { 0 };
        self.drop_hot = if s.drop_storm() { self.drop_hot + 1 } else { 0 };
        let plis = self.pli_hot >= STORM_WINDOWS;
        let drops = self.drop_hot >= STORM_WINDOWS;
        let confirmed = (plis && ev.confirmed) || (ev.sustained && s.clean());
        if !confirmed && !drops {
            if !plis {
                return Verdict::Keep;
            }
            let log = self.last_note.is_none_or(|t| now.duration_since(t) >= UNCONFIRMED_LOG_INTERVAL);
            if log {
                self.last_note = Some(now);
            }
            return Verdict::Unconfirmed { log };
        }
        if self.last_switch.is_some_and(|t| now.duration_since(t) < SWITCH_MIN_INTERVAL) {
            return Verdict::Keep;
        }
        self.pli_hot = 0;
        self.drop_hot = 0;
        self.last_switch = Some(now);
        Verdict::Switch { confirmed }
    }

    /// A new encoder took over: its first windows start from scratch (the 60 s rule stays).
    pub fn reset_windows(&mut self) {
        self.pli_hot = 0;
        self.drop_hot = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(plis: u64, lost: u64) -> StormSample {
        StormSample { secs: 5.0, plis, drop_events: 0, packets_sent: 4000, packets_lost: lost, fraction_lost: 0.0 }
    }

    /// A viewer's report over 5 s.
    fn rx(decode_errors: u64, lost: u64) -> RxHealthReport {
        RxHealthReport { kind: StreamKind::Share, secs: 5.0, decoded: 150, decode_errors, packets_lost: lost, packets_received: 2000 }
    }

    /// Feed `d` `windows` 5 s windows of `sample`, a viewer report (if any) arriving with each
    /// one; the verdicts.
    fn run(d: &mut StormDetector, t0: Instant, windows: u64, sample: StormSample, report: Option<RxHealthReport>) -> Vec<Verdict> {
        let mut reports = ViewerReports::default();
        (1..=windows)
            .map(|k| {
                let now = t0 + Duration::from_secs(k * 5);
                if let Some(r) = report {
                    reports.add("Tati", r, now);
                }
                d.update(&sample, &reports.evidence(StreamKind::Share, now), now)
            })
            .collect()
    }

    fn switches(v: &[Verdict]) -> usize {
        v.iter().filter(|x| matches!(x, Verdict::Switch { .. })).count()
    }

    #[test]
    fn plis_without_viewer_reports_never_demote() {
        // A v0.6 viewer, or one on bad Wi-Fi: the SFU forwards its keyframe requests, our uplink
        // is clean, nobody reports decode errors.
        let mut d = StormDetector::default();
        let v = run(&mut d, Instant::now(), 30, s(9, 0), None);
        assert_eq!(switches(&v), 0);
        assert_eq!(v[0], Verdict::Keep);
        assert!(v[1..].iter().all(|x| matches!(x, Verdict::Unconfirmed { .. })));
        // Logged once a minute: windows 2 (10 s), 14 (70 s) and 26 (130 s).
        let logged: Vec<usize> = v.iter().enumerate().filter(|(_, x)| **x == Verdict::Unconfirmed { log: true }).map(|(i, _)| i + 1).collect();
        assert_eq!(logged, vec![2, 14, 26]);
    }

    #[test]
    fn a_lossy_viewer_does_not_demote() {
        // The viewer's own loss explains its trouble: it never sends a report...
        let lossy = rx(20, 200);
        assert!(!lossy.unhealthy());
        assert!(!RxReportGate::default().allow("dono", &lossy, Instant::now()));
        // ...and one that arrives anyway (a modified or future app) confirms nothing.
        let mut d = StormDetector::default();
        assert_eq!(switches(&run(&mut d, Instant::now(), 20, s(9, 0), Some(lossy))), 0);
        // Keyframe requests with loss on our own uplink are the network's fault, whatever the
        // viewers say.
        let mut d = StormDetector::default();
        assert_eq!(switches(&run(&mut d, Instant::now(), 20, s(10, 200), Some(rx(20, 0)))), 0);
        assert!(!StormSample { fraction_lost: 0.1, ..s(10, 0) }.stormy());
    }

    #[test]
    fn plis_confirmed_by_a_clean_viewer_demote_exactly_once() {
        let t0 = Instant::now();
        let at = |k: u64| t0 + Duration::from_secs(k * 5);
        let mut d = StormDetector::default();
        let mut reports = ViewerReports::default();
        // Healthy: a viewer joining asks for one keyframe.
        assert_eq!(d.update(&s(1, 0), &reports.evidence(StreamKind::Share, at(1)), at(1)), Verdict::Keep);
        // Field report: ~1.5 PLI/s at us; one window is not enough...
        assert_eq!(d.update(&s(8, 0), &reports.evidence(StreamKind::Share, at(2)), at(2)), Verdict::Keep);
        // ...two with a viewer dropping ~4 frames per 5 s, nothing lost, are.
        reports.add("Tati", rx(4, 0), at(3));
        assert_eq!(d.update(&s(8, 0), &reports.evidence(StreamKind::Share, at(3)), at(3)), Verdict::Switch { confirmed: true });
        // Still stormy and still reported with the next encoder: no second switch within 60 s.
        let v: Vec<Verdict> = (4..=14)
            .map(|k| {
                reports.add("Tati", rx(4, 0), at(k));
                d.update(&s(9, 0), &reports.evidence(StreamKind::Share, at(k)), at(k))
            })
            .collect();
        assert_eq!(switches(&v), 0);
        // A lasting, reported storm over the first minute: exactly one switch, a counted one.
        let mut d = StormDetector::default();
        let v = run(&mut d, t0, 12, s(8, 0), Some(rx(4, 0)));
        assert_eq!(switches(&v), 1);
        assert!(v.contains(&Verdict::Switch { confirmed: true }));
    }

    #[test]
    fn reports_expire_belong_to_one_stream_and_restart_with_the_encoder() {
        let t0 = Instant::now();
        let mut reports = ViewerReports::default();
        reports.add("Tati", rx(4, 0), t0);
        assert!(reports.evidence(StreamKind::Share, t0 + Duration::from_secs(10)).confirmed);
        assert!(!reports.evidence(StreamKind::Share, t0 + Duration::from_secs(16)).confirmed);
        assert!(!reports.evidence(StreamKind::Camera, t0).confirmed);
        // One report confirms a storm but is not "sustained"; neither is one viewer's lasting
        // trouble (a PC too slow to decode); two viewers with two clean reports each are.
        let ev = reports.evidence(StreamKind::Share, t0);
        assert!(ev.confirmed && !ev.sustained);
        assert!(ev.detail.starts_with("Tati x1: 4 decode errors / 150 decoded"), "{}", ev.detail);
        reports.add("Tati", rx(4, 0), t0 + Duration::from_secs(5));
        assert!(!reports.evidence(StreamKind::Share, t0 + Duration::from_secs(5)).sustained);
        reports.add("Joao", rx(4, 0), t0);
        reports.add("Joao", rx(4, 0), t0 + Duration::from_secs(5));
        assert!(reports.evidence(StreamKind::Share, t0 + Duration::from_secs(5)).sustained);
        // Two viewers with one report each are not one viewer's lasting trouble.
        let mut two = ViewerReports::default();
        two.add("Tati", rx(4, 0), t0);
        two.add("Joao", rx(4, 0), t0);
        assert!(!two.evidence(StreamKind::Share, t0).sustained);
        // A little loss still confirms a storm, but it is not "no loss".
        let mut lossy = ViewerReports::default();
        lossy.add("Tati", rx(4, 10), t0);
        lossy.add("Tati", rx(4, 10), t0 + Duration::from_secs(5));
        let ev = lossy.evidence(StreamKind::Share, t0 + Duration::from_secs(5));
        assert!(ev.confirmed && !ev.sustained);
        reports.clear(StreamKind::Share);
        assert_eq!(reports.evidence(StreamKind::Share, t0 + Duration::from_secs(5)), ViewerEvidence::default());
    }

    #[test]
    fn sustained_viewer_decode_errors_demote_without_plis() {
        // The SFU may answer a viewer's PLIs itself: two windows of clean-network decode errors
        // from two viewers are enough...
        let t0 = Instant::now();
        let both = |d: &mut StormDetector, sample: StormSample| -> Vec<Verdict> {
            let mut reports = ViewerReports::default();
            (1..=12u64)
                .map(|k| {
                    let now = t0 + Duration::from_secs(k * 5);
                    reports.add("Tati", rx(4, 0), now);
                    reports.add("Joao", rx(4, 0), now);
                    d.update(&sample, &reports.evidence(StreamKind::Share, now), now)
                })
                .collect()
        };
        let v = both(&mut StormDetector::default(), s(0, 0));
        assert_eq!(v[0], Verdict::Keep);
        assert_eq!(v[1], Verdict::Switch { confirmed: true });
        assert_eq!(switches(&v), 1);
        // ...one viewer alone is not (its own PC may be too slow to decode).
        let mut d = StormDetector::default();
        assert_eq!(switches(&run(&mut d, t0, 12, s(0, 0), Some(rx(4, 0)))), 0);
        // Not over a lossy uplink.
        assert_eq!(switches(&both(&mut StormDetector::default(), s(0, 400))), 0);
    }

    #[test]
    fn our_own_frame_drops_demote_for_the_session_only() {
        let drops = StormSample { drop_events: 4, ..s(0, 0) };
        assert!(drops.stormy() && drops.drop_storm() && !drops.pli_storm());
        assert!(!StormSample { drop_events: 1, ..s(0, 0) }.stormy());
        let mut d = StormDetector::default();
        let v = run(&mut d, Instant::now(), 12, drops, None);
        assert_eq!(v[1], Verdict::Switch { confirmed: false });
        assert_eq!(switches(&v), 1);
        // Below the threshold rate.
        assert!(!s(4, 0).stormy());
        assert!(s(5, 0).stormy());
        // A window too short to judge.
        assert!(!StormSample { secs: 0.5, ..s(9, 0) }.stormy());
    }

    #[test]
    fn the_report_payload_roundtrips_and_tolerates_other_shapes() {
        let r = RxHealthReport { kind: StreamKind::Camera, secs: 5.02, decoded: 148, decode_errors: 3, packets_lost: 0, packets_received: 1890 };
        let bytes = r.to_payload();
        assert!(bytes.len() < 120, "{}", String::from_utf8_lossy(&bytes));
        assert_eq!(RxHealthReport::from_payload(&bytes), Some(r));
        // A newer app: unknown fields are ignored, missing ones default.
        let newer = br#"{"v":2,"kind":"share","secs":5,"decoded":10,"decode_errors":4,"freezes":1,"codec":"h264"}"#;
        let got = RxHealthReport::from_payload(newer).unwrap();
        assert_eq!((got.kind, got.decode_errors, got.packets_lost, got.packets_received), (StreamKind::Share, 4, 0, 0));
        assert!(got.unhealthy());
        // Not about a stream we know, not ours, too big.
        assert_eq!(RxHealthReport::from_payload(br#"{"kind":"screen-audio","secs":5,"decode_errors":9}"#), None);
        assert_eq!(RxHealthReport::from_payload(br#"{"secs":5,"decode_errors":9}"#), None);
        assert_eq!(RxHealthReport::from_payload(b"hello"), None);
        assert_eq!(RxHealthReport::from_payload(br#"{"kind":"share","secs":"x"}"#), None);
        let mut big = br#"{"kind":"share","pad":""#.to_vec();
        big.extend(std::iter::repeat_n(b'a', RX_MAX_PAYLOAD));
        big.extend(br#""}"#);
        assert_eq!(RxHealthReport::from_payload(&big), None);
    }

    #[test]
    fn viewers_report_only_unhealthy_windows_at_most_every_5_s() {
        let mut g = RxReportGate::default();
        let t0 = Instant::now();
        assert!(!g.allow("dono", &rx(0, 0), t0));
        assert!(!g.allow("dono", &rx(1, 0), t0), "one dropped frame in 5 s is noise");
        assert!(g.allow("dono", &rx(4, 0), t0));
        assert!(!g.allow("dono", &rx(4, 0), t0 + Duration::from_secs(3)));
        assert!(g.allow("dono", &RxHealthReport { kind: StreamKind::Camera, ..rx(4, 0) }, t0 + Duration::from_secs(3)));
        assert!(g.allow("ana", &rx(4, 0), t0 + Duration::from_secs(3)));
        assert!(g.allow("dono", &rx(4, 0), t0 + Duration::from_secs(5)));
        // A short window (subscribed mid-window) needs the rate, not just the count.
        assert!(!RxHealthReport { secs: 0.5, ..rx(4, 0) }.unhealthy());
    }

    #[test]
    fn plans_follow_preference_ranking_and_demotions() {
        // The real machine's list (whatever this host has), but the shape always holds.
        let p = plan(EncoderChoice::Software);
        assert_eq!(p.len(), 1);
        assert!(p[0].is_software());
        assert_eq!(p[0].label, "Processador (sem aceleração)");
        let auto = plan(EncoderChoice::Auto);
        assert!(auto.last().unwrap().is_software());
        assert_eq!(auto.iter().filter(|a| a.is_software()).count(), 1);
        // Hardware acceleration off: only the processor.
        set_preference(EncoderPref { hardware: false, encoder: None });
        assert_eq!(plan(EncoderChoice::Auto).len(), 1);
        // A specific pick goes first.
        if auto.len() > 2 {
            set_preference(EncoderPref { hardware: true, encoder: Some(auto[1].id.clone()) });
            assert_eq!(plan(EncoderChoice::Auto)[0].id, auto[1].id);
        }
        set_preference(EncoderPref::default());
        // Explicit backends only list that backend.
        assert!(plan(EncoderChoice::Nvenc).iter().all(|a| a.is_software() || a.backend == Some(EncoderBackend::Nvenc)));
        assert!(snapshot().options.last().is_some_and(|o| o.kind == EncoderKind::Software));
    }

    #[test]
    fn the_cache_file_roundtrips() {
        let c = CacheFile {
            key: "0.7.0|10de:2d04:x:0@32.0.16.1088".into(),
            results: vec![SelfTestResult::failed("mf:1", "lento")],
            failures: [("nvenc:1".to_string(), 1)].into(),
            gpus: Vec::new(),
        };
        let back: CacheFile = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
        assert_eq!((back.key, back.results, back.failures), (c.key, c.results, c.failures));
        let old: CacheFile = serde_json::from_str(r#"{"key":"k"}"#).unwrap();
        assert!(old.results.is_empty() && old.failures.is_empty());
    }
}
