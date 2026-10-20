//! Automatic encoder choice (v0.7): the machine's GPUs, the H.264 encoders they offer, how those
//! rank and what a self-test result means. Pure logic without Windows types, so the ranking and
//! the adapter choice are unit tested with fake adapter lists; `win::adapters` fills [`GpuInfo`]
//! from DXGI and `win::encode::selftest` runs the tests.
//!
//! Ranking: a candidate that passed its self-test beats one that was not tested, which beats one
//! that failed (never chosen automatically). Within a group the static tier decides (dedicated
//! NVIDIA NVENC > dedicated GPU through Media Foundation > Intel iGPU > AMD iGPU), then the
//! measured encode time and bitrate accuracy. The processor (OpenH264 in libwebrtc) is always last.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::EncoderBackend;

pub const VENDOR_NVIDIA: u32 = 0x10DE;
pub const VENDOR_AMD: u32 = 0x1002;
pub const VENDOR_INTEL: u32 = 0x8086;
/// Microsoft Basic Render Driver / remote display adapters: never an encoder.
pub const VENDOR_MICROSOFT: u32 = 0x1414;

/// One hardware display adapter (software adapters are never listed).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuInfo {
    pub name: String,
    pub vendor_id: u32,
    pub device_id: u32,
    pub subsys_id: u32,
    pub revision: u32,
    /// Adapter LUID (`LowPart | HighPart << 32`). Changes on every boot: never stored as identity.
    pub luid: u64,
    pub dedicated_mb: u64,
    pub integrated: bool,
    /// User-mode driver version, "32.0.15.6094" ("" when unknown).
    pub driver: String,
    /// Drives the primary monitor.
    pub primary: bool,
    /// Position among the adapters with the same vendor and device id (two identical cards).
    pub instance: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuVendor {
    Nvidia,
    Amd,
    Intel,
    Other,
}

impl GpuInfo {
    pub fn vendor(&self) -> GpuVendor {
        match self.vendor_id {
            VENDOR_NVIDIA => GpuVendor::Nvidia,
            VENDOR_AMD => GpuVendor::Amd,
            VENDOR_INTEL => GpuVendor::Intel,
            _ => GpuVendor::Other,
        }
    }

    /// Identity that survives reboots (unlike the LUID): vendor, device, subsystem, instance.
    pub fn key(&self) -> String {
        format!("{:04x}:{:04x}:{:08x}:{}", self.vendor_id, self.device_id, self.subsys_id, self.instance)
    }
}

/// Integrated GPU? DXGI has no flag for it, so: little dedicated memory (an iGPU's is a small BIOS
/// carve-out), and for AMD / Intel the name too (an APU with a 2 GB carve-out is still an iGPU:
/// "AMD Radeon(TM) Graphics", "Radeon 780M Graphics"; Intel Arc cards are dedicated).
pub fn is_integrated(vendor_id: u32, name: &str, dedicated_mb: u64) -> bool {
    if dedicated_mb < 1024 {
        return true;
    }
    let n = name.to_ascii_lowercase();
    match vendor_id {
        VENDOR_AMD => n.contains("graphics") && !["radeon rx", " rx ", "radeon pro", "firepro"].iter().any(|k| n.contains(k)),
        VENDOR_INTEL => !n.contains("arc"),
        _ => false,
    }
}

/// Drop adapters that DXGI lists twice (seen: one GPU under two LUIDs, the second without
/// outputs): same name, ids, memory and driver. The one driving the primary monitor is kept,
/// else the first. Two identical real cards collapse too, which only loses a redundant
/// candidate (same encoder kind).
pub fn dedup(gpus: &mut Vec<GpuInfo>) {
    let same = |a: &GpuInfo, b: &GpuInfo| {
        (a.vendor_id, a.device_id, a.subsys_id, a.revision, a.dedicated_mb, &a.driver, &a.name)
            == (b.vendor_id, b.device_id, b.subsys_id, b.revision, b.dedicated_mb, &b.driver, &b.name)
    };
    let mut out: Vec<GpuInfo> = Vec::with_capacity(gpus.len());
    for g in gpus.drain(..) {
        match out.iter_mut().find(|o| same(o, &g)) {
            Some(o) if g.primary && !o.primary => *o = g,
            Some(_) => {}
            None => out.push(g),
        }
    }
    *gpus = out;
}

/// Number the adapters that share vendor + device id (enumeration order), see [`GpuInfo::instance`].
pub fn number_instances(gpus: &mut [GpuInfo]) {
    for i in 0..gpus.len() {
        let n = gpus[..i]
            .iter()
            .filter(|g| g.vendor_id == gpus[i].vendor_id && g.device_id == gpus[i].device_id)
            .count();
        gpus[i].instance = n as u32;
    }
}

/// "32.0.15.6094" from the UMD version DXGI reports (four 16-bit parts).
pub fn driver_version_string(v: u64) -> String {
    format!("{}.{}.{}.{}", v >> 48, (v >> 32) & 0xFFFF, (v >> 16) & 0xFFFF, v & 0xFFFF)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EncoderKind {
    Nvenc,
    #[serde(rename = "mf")]
    MediaFoundation,
    /// OpenH264 inside libwebrtc (the raw-frame path); not a vox-capture encoder.
    Software,
    /// macOS: Apple's hardware encoder, inside libwebrtc on the raw path (see
    /// [`EncoderBackend::VideoToolbox`]).
    #[cfg(target_os = "macos")]
    #[serde(rename = "videotoolbox")]
    VideoToolbox,
}

impl EncoderKind {
    pub fn tag(self) -> &'static str {
        match self {
            EncoderKind::Nvenc => "nvenc",
            EncoderKind::MediaFoundation => "mf",
            EncoderKind::Software => "software",
            #[cfg(target_os = "macos")]
            EncoderKind::VideoToolbox => "videotoolbox",
        }
    }
}

/// Id of the processor candidate.
pub const SOFTWARE_ID: &str = "software";

/// One way to encode: a backend on one adapter, or the processor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderCandidate {
    /// Stable across reboots: `"<kind>:<gpu key>"`, or [`SOFTWARE_ID`].
    pub id: String,
    pub kind: EncoderKind,
    pub gpu: Option<GpuInfo>,
}

impl EncoderCandidate {
    pub fn software() -> Self {
        Self { id: SOFTWARE_ID.into(), kind: EncoderKind::Software, gpu: None }
    }

    pub fn hardware(kind: EncoderKind, gpu: &GpuInfo) -> Self {
        Self { id: format!("{}:{}", kind.tag(), gpu.key()), kind, gpu: Some(gpu.clone()) }
    }

    /// The vox-capture backend (`None` for the processor).
    pub fn backend(&self) -> Option<EncoderBackend> {
        match self.kind {
            EncoderKind::Nvenc => Some(EncoderBackend::Nvenc),
            EncoderKind::MediaFoundation => Some(EncoderBackend::MediaFoundation),
            EncoderKind::Software => None,
            #[cfg(target_os = "macos")]
            EncoderKind::VideoToolbox => Some(EncoderBackend::VideoToolbox),
        }
    }

    pub fn luid(&self) -> Option<u64> {
        self.gpu.as_ref().map(|g| g.luid)
    }

    /// Static preference, lower is better: 0 NVENC on a dedicated NVIDIA, 1 Media Foundation on a
    /// dedicated GPU, 2 Intel (or other) iGPU, 3 AMD iGPU, 4 the processor.
    pub fn tier(&self) -> u8 {
        let Some(g) = &self.gpu else { return 4 };
        match (self.kind, g.integrated, g.vendor()) {
            (EncoderKind::Software, ..) => 4,
            #[cfg(target_os = "macos")]
            (EncoderKind::VideoToolbox, ..) => 0,
            (EncoderKind::Nvenc, false, _) => 0,
            (_, false, _) => 1,
            (_, true, GpuVendor::Amd) => 3,
            (_, true, _) => 2,
        }
    }

    /// Plain Portuguese name for the settings: "NVIDIA GeForce GTX 1650 (NVENC)", "AMD Radeon(TM)
    /// Graphics (integrada)", "Processador (sem aceleração)". `all` disambiguates a GPU that
    /// offers both NVENC and Media Foundation.
    pub fn label_pt(&self, all: &[EncoderCandidate]) -> String {
        let Some(g) = &self.gpu else { return "Processador (sem aceleração)".into() };
        #[cfg(target_os = "macos")]
        if self.kind == EncoderKind::VideoToolbox {
            return "Apple VideoToolbox".into();
        }
        let name = g.name.trim();
        let twin = all.iter().any(|o| o.id != self.id && o.gpu.as_ref().is_some_and(|og| og.key() == g.key()));
        let same_name = all
            .iter()
            .filter(|o| o.kind == self.kind && o.gpu.as_ref().is_some_and(|og| og.name.trim() == name))
            .count()
            > 1;
        let mut s = name.to_string();
        if same_name {
            s.push_str(&format!(" #{}", g.instance + 1));
        }
        match self.kind {
            EncoderKind::Nvenc => s.push_str(" (NVENC)"),
            EncoderKind::MediaFoundation if twin => s.push_str(" (Media Foundation)"),
            _ => {}
        }
        if g.integrated {
            s.push_str(" (integrada)");
        }
        s
    }
}

/// Every candidate of a machine in static order (tier, then enumeration order), the processor
/// last. `nvenc` says whether the NVIDIA encode library loads; `mf(gpu)` whether that adapter
/// lists a hardware H.264 encoder MFT.
pub fn build_candidates(gpus: &[GpuInfo], nvenc: bool, mf: &dyn Fn(&GpuInfo) -> bool) -> Vec<EncoderCandidate> {
    let mut out = Vec::new();
    for g in gpus {
        if g.vendor_id == VENDOR_MICROSOFT {
            continue;
        }
        if nvenc && g.vendor_id == VENDOR_NVIDIA {
            out.push(EncoderCandidate::hardware(EncoderKind::Nvenc, g));
        }
        if mf(g) {
            out.push(EncoderCandidate::hardware(EncoderKind::MediaFoundation, g));
        }
    }
    // Stable: enumeration order breaks tier ties.
    out.sort_by_key(|c| c.tier());
    out.push(EncoderCandidate::software());
    out
}

// ---------------------------------------------------------------------------------------------
// Self-test
// ---------------------------------------------------------------------------------------------

/// Self-test frames (synthetic, with motion and text): 2 s at 30 fps would be thorough but too
/// slow on a weak iGPU; 60 frames at 720p are about 0.5 s per candidate.
pub const SELFTEST_FRAMES: u32 = 60;
pub const SELFTEST_WIDTH: u32 = 1280;
pub const SELFTEST_HEIGHT: u32 = 720;
pub const SELFTEST_FPS: u32 = 30;
/// Start bitrate, then [`SELFTEST_BPS_LOW`] from frame [`SELFTEST_RATE_CHANGE_AT`] on (an
/// encoder that ignores a lower target mid-stream overflows WebRTC's frame dropper).
pub const SELFTEST_BPS_START: u32 = 4_000_000;
pub const SELFTEST_BPS_LOW: u32 = 2_000_000;
pub const SELFTEST_RATE_CHANGE_AT: u32 = 20;
/// This frame is a requested IDR (a viewer's keyframe request).
pub const SELFTEST_FORCED_IDR_AT: u32 = 30;
/// Output bitrate after the change above this multiple of the target: fails.
pub const SELFTEST_MAX_RATE_RATIO: f32 = 2.0;
/// Average submit-to-output time above this (ms, 720p): too slow for 30 fps.
pub const SELFTEST_MAX_MS: f32 = 30.0;
/// Decoded pictures worse than this (Y PSNR against what the encoder got): broken bitstream.
pub const SELFTEST_MIN_PSNR_DB: f32 = 20.0;

/// What one self-test measured.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SelfTestMeasure {
    pub frames: u32,
    /// Access units the encoder produced.
    pub access_units: u32,
    /// Pictures the Microsoft software H.264 decoder produced from them.
    pub decoded: u32,
    pub first_is_idr: bool,
    /// The requested keyframe came out as an IDR.
    pub forced_idr_ok: bool,
    pub ms_per_frame: f32,
    /// Delta-frame bitrate after the rate change, at the nominal frame rate.
    pub kbps: u32,
    pub target_kbps: u32,
    /// Worst Y PSNR over the sampled frames (0 = none sampled).
    pub psnr_db: f32,
}

/// Pass / fail of a measurement (the reason is Portuguese, shown in the settings).
pub fn judge(m: &SelfTestMeasure) -> TestOutcome {
    let fail = |r: String| TestOutcome::Failed { reason: r };
    if m.access_units < m.frames {
        return fail(format!("codificou {} de {} quadros", m.access_units, m.frames));
    }
    if m.decoded < m.access_units {
        return fail(format!("só {} de {} quadros decodificaram", m.decoded, m.access_units));
    }
    if !m.first_is_idr || !m.forced_idr_ok {
        return fail("não gera quadro-chave quando pedido".into());
    }
    if m.psnr_db > 0.0 && m.psnr_db < SELFTEST_MIN_PSNR_DB {
        return fail(format!("imagem corrompida ({:.0} dB)", m.psnr_db));
    }
    if m.target_kbps > 0 && m.kbps as f32 > m.target_kbps as f32 * SELFTEST_MAX_RATE_RATIO {
        return fail(format!("não respeita o bitrate ({} de {} kbps)", m.kbps, m.target_kbps));
    }
    if m.ms_per_frame > SELFTEST_MAX_MS {
        return fail(format!("lento demais ({:.0} ms por quadro)", m.ms_per_frame));
    }
    TestOutcome::Passed
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "lowercase")]
pub enum TestOutcome {
    Passed,
    Failed { reason: String },
    /// Not run (see [`should_test`]); ranks like an untested candidate.
    Skipped { reason: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SelfTestResult {
    pub id: String,
    #[serde(flatten)]
    pub outcome: TestOutcome,
    #[serde(default)]
    pub ms_per_frame: f32,
    #[serde(default)]
    pub kbps: u32,
    #[serde(default)]
    pub target_kbps: u32,
    #[serde(default)]
    pub decoded: u32,
    #[serde(default)]
    pub frames: u32,
    #[serde(default)]
    pub psnr_db: f32,
}

impl SelfTestResult {
    pub fn from_measure(id: &str, m: &SelfTestMeasure) -> Self {
        Self {
            id: id.into(),
            outcome: judge(m),
            ms_per_frame: m.ms_per_frame,
            kbps: m.kbps,
            target_kbps: m.target_kbps,
            decoded: m.decoded,
            frames: m.frames,
            psnr_db: m.psnr_db,
        }
    }

    pub fn failed(id: &str, reason: impl Into<String>) -> Self {
        Self::bare(id, TestOutcome::Failed { reason: reason.into() })
    }

    pub fn skipped(id: &str, reason: impl Into<String>) -> Self {
        Self::bare(id, TestOutcome::Skipped { reason: reason.into() })
    }

    fn bare(id: &str, outcome: TestOutcome) -> Self {
        Self { id: id.into(), outcome, ms_per_frame: 0.0, kbps: 0, target_kbps: 0, decoded: 0, frames: 0, psnr_db: 0.0 }
    }

    pub fn passed(&self) -> bool {
        self.outcome == TestOutcome::Passed
    }
}

/// Whether to run `c`'s self-test given the results so far (candidates are tested in static
/// order, so NVENC comes before the same adapter's MFT). `Err(reason)` = skip.
///
/// NVIDIA driver bug (seen with 610.88, NVENC API 13.1): once the NVIDIA H.264 MFT has been
/// activated in a process, every later NVENC session there fails. So where NVENC works the
/// NVIDIA MFT is listed but never activated by the test (it is still a fallback at run time).
pub fn should_test(c: &EncoderCandidate, done: &[(EncoderCandidate, SelfTestResult)]) -> Result<(), String> {
    match (c.kind, &c.gpu) {
        (EncoderKind::Software, _) => Err("o processador não precisa de teste".into()),
        (EncoderKind::MediaFoundation, Some(g)) if g.vendor_id == VENDOR_NVIDIA => {
            let nvenc_ok = done
                .iter()
                .any(|(o, r)| o.kind == EncoderKind::Nvenc && o.luid() == Some(g.luid) && r.passed());
            if nvenc_ok {
                Err("NVENC funciona nesta placa; o MFT da NVIDIA não é ativado".into())
            } else {
                Ok(())
            }
        }
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------------------------------
// Ranking
// ---------------------------------------------------------------------------------------------

/// Runtime failures (video so unstable that the encoder was switched while live) after which a
/// candidate is no longer chosen automatically, until the drivers or the app change or the user
/// runs the test again.
pub const MAX_RUNTIME_FAILURES: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CandidateStatus {
    Passed,
    Untested,
    Failed,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RankedCandidate {
    pub candidate: EncoderCandidate,
    pub status: CandidateStatus,
    pub result: Option<SelfTestResult>,
    pub failures: u32,
}

/// Lower is better, for passed candidates of the same tier: encode time, inflated by how far the
/// output bitrate was from the target.
fn score(r: Option<&SelfTestResult>) -> f32 {
    let Some(r) = r.filter(|r| r.passed()) else { return f32::MAX };
    let dev = if r.target_kbps > 0 { (r.kbps as f32 / r.target_kbps as f32 - 1.0).abs() } else { 0.0 };
    r.ms_per_frame.max(0.1) * (1.0 + dev)
}

/// Best first; the processor always last. Failed candidates stay in the list (the settings show
/// them) but [`auto_order`] skips them.
pub fn rank(cands: &[EncoderCandidate], results: &[SelfTestResult], failures: &BTreeMap<String, u32>) -> Vec<RankedCandidate> {
    let mut out: Vec<RankedCandidate> = cands
        .iter()
        .map(|c| {
            let result = results.iter().find(|r| r.id == c.id).cloned();
            let fails = failures.get(&c.id).copied().unwrap_or(0);
            let status = match (&result, c.kind) {
                (_, EncoderKind::Software) => CandidateStatus::Passed,
                _ if fails >= MAX_RUNTIME_FAILURES => CandidateStatus::Failed,
                (Some(r), _) => match r.outcome {
                    TestOutcome::Passed => CandidateStatus::Passed,
                    TestOutcome::Failed { .. } => CandidateStatus::Failed,
                    TestOutcome::Skipped { .. } => CandidateStatus::Untested,
                },
                (None, _) => CandidateStatus::Untested,
            };
            RankedCandidate { candidate: c.clone(), status, result, failures: fails }
        })
        .collect();
    out.sort_by(|a, b| {
        let sw = |r: &RankedCandidate| r.candidate.kind == EncoderKind::Software;
        sw(a).cmp(&sw(b))
            .then(a.status.cmp(&b.status))
            .then(a.candidate.tier().cmp(&b.candidate.tier()))
            .then(score(a.result.as_ref()).total_cmp(&score(b.result.as_ref())))
    });
    out
}

/// The automatic order: everything not failed, best first, ending with the processor.
pub fn auto_order(ranked: &[RankedCandidate]) -> Vec<&EncoderCandidate> {
    ranked.iter().filter(|r| r.status != CandidateStatus::Failed).map(|r| &r.candidate).collect()
}

/// Cache key of the self-test results: app version plus every adapter's identity and driver,
/// independent of enumeration order and of the LUIDs (new on every boot).
pub fn selftest_key(app_version: &str, gpus: &[GpuInfo]) -> String {
    let mut parts: Vec<String> = gpus.iter().map(|g| format!("{}@{}", g.key(), g.driver)).collect();
    parts.sort();
    format!("{app_version}|{}", parts.join(";"))
}

// ---------------------------------------------------------------------------------------------
// Capture adapter
// ---------------------------------------------------------------------------------------------

/// Where the capture's D3D11 device goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureAdapter {
    /// `None` = the monitor's adapter (today's behaviour, the default D3D picks if unknown).
    pub luid: Option<u64>,
    /// The device is on another adapter than the one scanning out the captured monitor: Windows
    /// copies every frame across (Windows.Graphics.Capture accepts such a device).
    pub cross: bool,
}

/// The capture device follows the encoder: `requested` (the chosen encoder's adapter) when that
/// adapter still exists, else the monitor's adapter.
pub fn capture_adapter(requested: Option<u64>, monitor: Option<u64>, exists: &dyn Fn(u64) -> bool) -> CaptureAdapter {
    match requested {
        Some(r) if monitor == Some(r) => CaptureAdapter { luid: Some(r), cross: false },
        Some(r) if exists(r) => CaptureAdapter { luid: Some(r), cross: true },
        _ => CaptureAdapter { luid: monitor, cross: false },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gpu(name: &str, vendor_id: u32, device_id: u32, mb: u64, luid: u64, primary: bool) -> GpuInfo {
        GpuInfo {
            name: name.into(),
            vendor_id,
            device_id,
            subsys_id: 0x1234,
            revision: 1,
            luid,
            dedicated_mb: mb,
            integrated: is_integrated(vendor_id, name, mb),
            driver: "31.0.1.1".into(),
            primary,
            instance: 0,
        }
    }

    /// A field-report laptop: Ryzen 5 APU driving the panel + GTX 1650.
    fn hybrid() -> Vec<GpuInfo> {
        vec![
            gpu("AMD Radeon(TM) Graphics", VENDOR_AMD, 0x1638, 512, 0x100, true),
            gpu("NVIDIA GeForce GTX 1650", VENDOR_NVIDIA, 0x1f99, 4096, 0x200, false),
        ]
    }

    fn ids(v: &[&EncoderCandidate]) -> Vec<String> {
        v.iter().map(|c| c.id.clone()).collect()
    }

    #[test]
    fn integrated_or_dedicated() {
        assert!(is_integrated(VENDOR_AMD, "AMD Radeon(TM) Graphics", 512));
        assert!(is_integrated(VENDOR_AMD, "AMD Radeon 780M Graphics", 2048), "APU with a big carve-out");
        assert!(!is_integrated(VENDOR_AMD, "AMD Radeon RX 6600", 8176));
        assert!(!is_integrated(VENDOR_AMD, "AMD Radeon Pro W6600 Graphics", 8176));
        assert!(is_integrated(VENDOR_INTEL, "Intel(R) UHD Graphics 620", 128));
        assert!(is_integrated(VENDOR_INTEL, "Intel(R) Iris(R) Xe Graphics", 2048));
        assert!(!is_integrated(VENDOR_INTEL, "Intel(R) Arc(TM) A770 Graphics", 16_000));
        assert!(!is_integrated(VENDOR_NVIDIA, "NVIDIA GeForce GTX 1650", 4096));
        assert!(is_integrated(VENDOR_NVIDIA, "NVIDIA tiny", 256));
        assert_eq!(driver_version_string((32 << 48) | (15 << 16) | 6094), "32.0.15.6094");
    }

    #[test]
    fn hybrid_laptop_prefers_the_dedicated_nvidia() {
        let gpus = hybrid();
        let c = build_candidates(&gpus, true, &|_| true);
        let order: Vec<String> = c.iter().map(|c| c.id.clone()).collect();
        assert_eq!(
            order,
            vec!["nvenc:10de:1f99:00001234:0", "mf:10de:1f99:00001234:0", "mf:1002:1638:00001234:0", "software"]
        );
        assert_eq!(c[0].luid(), Some(0x200));
        assert_eq!(c[0].label_pt(&c), "NVIDIA GeForce GTX 1650 (NVENC)");
        assert_eq!(c[1].label_pt(&c), "NVIDIA GeForce GTX 1650 (Media Foundation)");
        assert_eq!(c[2].label_pt(&c), "AMD Radeon(TM) Graphics (integrada)");
        assert_eq!(c[3].label_pt(&c), "Processador (sem aceleração)");
        // Without the NVIDIA driver's encode library: Media Foundation only.
        let c = build_candidates(&gpus, false, &|_| true);
        assert_eq!(c[0].kind, EncoderKind::MediaFoundation);
        assert_eq!(c[0].gpu.as_ref().unwrap().vendor(), GpuVendor::Nvidia);
    }

    #[test]
    fn static_tiers_rank_vendors_and_kinds() {
        let gpus = vec![
            gpu("AMD Radeon(TM) Graphics", VENDOR_AMD, 1, 512, 1, true),
            gpu("Intel(R) UHD Graphics 770", VENDOR_INTEL, 2, 128, 2, false),
            gpu("AMD Radeon RX 7600", VENDOR_AMD, 3, 8176, 3, false),
            gpu("Microsoft Basic Render Driver", VENDOR_MICROSOFT, 0x8c, 0, 4, false),
        ];
        let c = build_candidates(&gpus, true, &|_| true);
        let names: Vec<_> = c.iter().map(|x| x.label_pt(&c)).collect();
        assert_eq!(names, vec!["AMD Radeon RX 7600", "Intel(R) UHD Graphics 770 (integrada)", "AMD Radeon(TM) Graphics (integrada)", "Processador (sem aceleração)"]);
        assert_eq!(c.iter().map(|c| c.tier()).collect::<Vec<_>>(), vec![1, 2, 3, 4]);
        // A machine with no GPU encoder at all still has the processor.
        assert_eq!(build_candidates(&[], true, &|_| true), vec![EncoderCandidate::software()]);
    }

    #[test]
    fn duplicate_adapters_collapse_to_the_primary() {
        let mut gpus = vec![
            gpu("NVIDIA GeForce RTX 5070", VENDOR_NVIDIA, 0x2f04, 12_000, 0x1000, false),
            gpu("NVIDIA GeForce RTX 5070", VENDOR_NVIDIA, 0x2f04, 12_000, 0x2000, true),
            gpu("AMD Radeon(TM) Graphics", VENDOR_AMD, 0x1638, 512, 0x100, false),
        ];
        dedup(&mut gpus);
        assert_eq!(gpus.len(), 2);
        assert_eq!(gpus[0].luid, 0x2000);
        assert!(gpus[0].primary);
    }

    #[test]
    fn two_identical_cards_get_distinct_ids_and_names() {
        let mut gpus = vec![
            gpu("NVIDIA GeForce RTX 3060", VENDOR_NVIDIA, 0x2504, 12_000, 1, true),
            gpu("NVIDIA GeForce RTX 3060", VENDOR_NVIDIA, 0x2504, 12_000, 2, false),
        ];
        number_instances(&mut gpus);
        let c = build_candidates(&gpus, true, &|_| false);
        assert_ne!(c[0].id, c[1].id);
        assert_eq!(c[1].label_pt(&c), "NVIDIA GeForce RTX 3060 #2 (NVENC)");
    }

    fn pass(id: &str, ms: f32, kbps: u32) -> SelfTestResult {
        SelfTestResult { ms_per_frame: ms, kbps, target_kbps: 2000, ..SelfTestResult::bare(id, TestOutcome::Passed) }
    }

    #[test]
    fn self_test_results_reorder_and_failures_drop_out() {
        let gpus = hybrid();
        let c = build_candidates(&gpus, true, &|_| true);
        let none = BTreeMap::new();
        // Before any test: the static order.
        let r = rank(&c, &[], &none);
        assert!(r.iter().take(3).all(|x| x.status == CandidateStatus::Untested));
        assert_eq!(ids(&auto_order(&r)), c.iter().map(|c| c.id.clone()).collect::<Vec<_>>());
        // NVENC fails (broken bitstream), the AMD iGPU passes, the NVIDIA MFT was not tested yet:
        // a passed candidate beats an untested one, the failed one is never automatic.
        let results = vec![
            SelfTestResult::failed(&c[0].id, "imagem corrompida"),
            pass(&c[2].id, 6.0, 2100),
        ];
        let r = rank(&c, &results, &none);
        assert_eq!(ids(&auto_order(&r)), vec![c[2].id.clone(), c[1].id.clone(), "software".into()]);
        assert_eq!(r.last().unwrap().candidate.kind, EncoderKind::Software);
        assert_eq!(r.iter().find(|x| x.candidate.id == c[0].id).unwrap().status, CandidateStatus::Failed);
        // Everything passes: tiers decide.
        let results = vec![pass(&c[0].id, 3.0, 2000), pass(&c[1].id, 2.0, 2000), pass(&c[2].id, 1.0, 2000)];
        assert_eq!(ids(&auto_order(&rank(&c, &results, &none)))[0], c[0].id);
        // Runtime failures (PLI storms) in two sessions: no longer automatic.
        let fails: BTreeMap<String, u32> = [(c[0].id.clone(), MAX_RUNTIME_FAILURES)].into();
        assert_eq!(ids(&auto_order(&rank(&c, &results, &fails)))[0], c[1].id);
        let fails: BTreeMap<String, u32> = [(c[0].id.clone(), 1)].into();
        assert_eq!(ids(&auto_order(&rank(&c, &results, &fails)))[0], c[0].id);
    }

    #[test]
    fn same_tier_prefers_faster_and_more_accurate() {
        let gpus = vec![
            gpu("NVIDIA A", VENDOR_NVIDIA, 1, 8000, 1, true),
            gpu("NVIDIA B", VENDOR_NVIDIA, 2, 8000, 2, false),
        ];
        let c = build_candidates(&gpus, true, &|_| false);
        let none = BTreeMap::new();
        let r = rank(&c, &[pass(&c[0].id, 4.0, 2000), pass(&c[1].id, 2.0, 2000)], &none);
        assert_eq!(r[0].candidate.id, c[1].id);
        // B is faster but overshoots by 90%: 2.0 * 1.9 = 3.8 > 4.0 * 1.0? No: A wins only past that.
        let r = rank(&c, &[pass(&c[0].id, 4.0, 2000), pass(&c[1].id, 2.0, 4400)], &none);
        assert_eq!(r[0].candidate.id, c[0].id);
    }

    #[test]
    fn nvidia_mft_is_never_activated_where_nvenc_works() {
        let gpus = hybrid();
        let c = build_candidates(&gpus, true, &|_| true);
        let (nvenc, nv_mf, amd_mf) = (&c[0], &c[1], &c[2]);
        assert!(should_test(nvenc, &[]).is_ok());
        let ok = vec![(nvenc.clone(), pass(&nvenc.id, 2.0, 2000))];
        assert!(should_test(nv_mf, &ok).is_err());
        assert!(should_test(amd_mf, &ok).is_ok());
        let bad = vec![(nvenc.clone(), SelfTestResult::failed(&nvenc.id, "x"))];
        assert!(should_test(nv_mf, &bad).is_ok(), "NVENC is broken anyway: test the MFT");
        assert!(should_test(&EncoderCandidate::software(), &[]).is_err());
        // A skipped MFT ranks as untested, after the passed NVENC.
        let results = vec![ok[0].1.clone(), SelfTestResult::skipped(&nv_mf.id, "nvenc ok")];
        let r = rank(&c, &results, &BTreeMap::new());
        assert_eq!(r[0].candidate.id, nvenc.id);
        assert_eq!(r.iter().find(|x| x.candidate.id == nv_mf.id).unwrap().status, CandidateStatus::Untested);
    }

    #[test]
    fn judging_measurements() {
        let good = SelfTestMeasure {
            frames: 60,
            access_units: 60,
            decoded: 60,
            first_is_idr: true,
            forced_idr_ok: true,
            ms_per_frame: 3.0,
            kbps: 2100,
            target_kbps: 2000,
            psnr_db: 36.0,
        };
        assert_eq!(judge(&good), TestOutcome::Passed);
        let bad = |m: SelfTestMeasure| matches!(judge(&m), TestOutcome::Failed { .. });
        assert!(bad(SelfTestMeasure { decoded: 59, ..good }));
        assert!(bad(SelfTestMeasure { access_units: 50, ..good }));
        assert!(bad(SelfTestMeasure { forced_idr_ok: false, ..good }));
        assert!(bad(SelfTestMeasure { psnr_db: 12.0, ..good }));
        // The field-report symptom: ~140 KB frames whatever the target.
        assert!(bad(SelfTestMeasure { kbps: 4500, ..good }));
        assert!(bad(SelfTestMeasure { ms_per_frame: 45.0, ..good }));
        // Undershooting is fine (easy content).
        assert_eq!(judge(&SelfTestMeasure { kbps: 600, ..good }), TestOutcome::Passed);
    }

    #[test]
    fn cache_key_follows_drivers_and_version_not_luids() {
        let a = hybrid();
        let k = selftest_key("0.7.0", &a);
        let mut b = hybrid();
        b.reverse();
        b[0].luid = 0x999;
        assert_eq!(selftest_key("0.7.0", &b), k, "order and LUIDs do not matter");
        let mut c = hybrid();
        c[1].driver = "32.0.15.7000".into();
        assert_ne!(selftest_key("0.7.0", &c), k, "a driver update re-tests");
        assert_ne!(selftest_key("0.7.1", &a), k, "an app update re-tests");
        assert_ne!(selftest_key("0.7.0", &a[..1]), k, "a GPU added or removed re-tests");
    }

    #[test]
    fn results_roundtrip_as_json() {
        let r = vec![pass("nvenc:x", 2.5, 1900), SelfTestResult::failed("mf:y", "lento"), SelfTestResult::skipped("mf:z", "nvenc ok")];
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains(r#""result":"passed""#) && json.contains(r#""reason":"lento""#), "{json}");
        let back: Vec<SelfTestResult> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
        // An older file without the measurement fields still loads.
        let old: SelfTestResult = serde_json::from_str(r#"{"id":"mf:q","result":"passed"}"#).unwrap();
        assert!(old.passed());
    }

    #[test]
    fn capture_device_follows_the_encoder_adapter() {
        let gpus = hybrid();
        let gpus = |l: u64| gpus.iter().any(|g| g.luid == l);
        let gpus = &gpus;
        // Same adapter as the monitor (desktop, single GPU): exactly today's behaviour.
        assert_eq!(capture_adapter(Some(0x100), Some(0x100), &gpus), CaptureAdapter { luid: Some(0x100), cross: false });
        assert_eq!(capture_adapter(None, Some(0x100), &gpus), CaptureAdapter { luid: Some(0x100), cross: false });
        assert_eq!(capture_adapter(None, None, &gpus), CaptureAdapter { luid: None, cross: false });
        // Hybrid laptop: panel on the iGPU, encoder on the NVIDIA -> capture device on the NVIDIA.
        assert_eq!(capture_adapter(Some(0x200), Some(0x100), &gpus), CaptureAdapter { luid: Some(0x200), cross: true });
        // An adapter that went away (eGPU unplugged, stale LUID): the monitor's.
        assert_eq!(capture_adapter(Some(0x777), Some(0x100), &gpus), CaptureAdapter { luid: Some(0x100), cross: false });
    }
}
