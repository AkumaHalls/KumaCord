//! Microphone level meter and voice-activity gate ("Sensibilidade de entrada").
//!
//! Both run in the mic callback on the signal that would be sent: after the WebRTC APM (echo
//! cancellation, noise suppression, AGC), in 10 ms chunks. The LiveKit APM wrapper exposes no
//! voice-activity output (see `libwebrtc::native::apm`), so *Automatic* mode is a small adaptive
//! gate: an energy threshold that rides a tracked noise floor. *Manual* mode is a plain
//! threshold. In both, the gate opens at once (a 5 ms fade-in), stays open for a hold time after
//! the last loud chunk so word endings are not cut, then fades out over 10 ms and sends digital
//! silence (which the Opus encoder turns into DTX, almost no bandwidth).
//!
//! Everything shared with the UI is an atomic ([`VoiceGate`]); the processor itself
//! ([`GateProcessor`]) is owned by the callback, allocation-free and lock-free.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::mixer::MIX_RATE;

/// Bottom of the dBFS scale (digital silence and anything quieter reads as this).
pub const MIN_DB: f32 = -100.0;
/// Range of the manual threshold slider.
pub const THRESHOLD_MIN_DB: f32 = -70.0;
pub const THRESHOLD_MAX_DB: f32 = -10.0;
pub const DEFAULT_THRESHOLD_DB: f32 = -45.0;
/// Range and default of the hold time (how long the gate stays open after the last loud chunk).
pub const HOLD_MIN_MS: u32 = 100;
pub const HOLD_MAX_MS: u32 = 1000;
pub const DEFAULT_HOLD_MS: u32 = 350;

/// Fade-in (the attack) and fade-out lengths in samples at 48 kHz: 5 ms and 10 ms.
const FADE_IN_SAMPLES: f32 = 240.0;
const FADE_OUT_SAMPLES: f32 = 480.0;
/// While open, the signal may dip this far below the threshold without counting as quiet.
const HYSTERESIS_DB: f32 = 3.0;
/// The meter falls at this speed (rises instantly), so a 15-20 fps reader sees a smooth bar.
const METER_FALL_DB_PER_S: f32 = 70.0;

// Automatic mode.
/// Threshold = noise floor + margin, kept inside these bounds. The lower bound stops a very
/// quiet room from gating on breath noise; the upper one guarantees normal speech always passes
/// even in a loud room.
const AUTO_MARGIN_DB: f32 = 15.0;
const AUTO_MIN_DB: f32 = -58.0;
const AUTO_MAX_DB: f32 = -30.0;
/// 10 ms chunks used to learn the first noise floor after the mic starts (gate shut meanwhile
/// unless something loud comes in).
const CALIBRATION_CHUNKS: u32 = 25;
/// Noise floor re-estimation window (10 ms chunks): the minimum level over it.
const WINDOW_CHUNKS: u32 = 50;
/// A window whose levels span at most this many dB is steady noise, adopted as the floor.
const STATIONARY_DB: f32 = 10.0;
/// Steady windows in a row needed to adopt a new floor while the gate is open.
const STEADY_WHEN_OPEN: u32 = 3;
/// The floor never goes below this: a mic that reads digital silence while it warms up, or a
/// noise suppressor that mutes a quiet room, must not make breath noise look like speech.
const FLOOR_MIN_DB: f32 = -75.0;
/// Otherwise the floor may only creep up by this much per window (closed / open gate), so
/// speech pauses never drag it up quickly.
const RISE_CLOSED_DB: f32 = 1.5;
const RISE_OPEN_DB: f32 = 0.3;

/// How the gate decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GateMode {
    /// Adaptive threshold that follows the room's noise floor.
    #[default]
    Auto,
    /// The user's fixed threshold.
    Manual,
}

impl GateMode {
    fn to_u8(self) -> u8 {
        match self {
            GateMode::Auto => 0,
            GateMode::Manual => 1,
        }
    }

    fn from_u8(v: u8) -> Self {
        if v == 1 {
            GateMode::Manual
        } else {
            GateMode::Auto
        }
    }
}

/// The user-facing gate settings (what `settings.json` stores).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GateConfig {
    pub mode: GateMode,
    /// Manual threshold in dBFS ([`THRESHOLD_MIN_DB`]..=[`THRESHOLD_MAX_DB`]).
    pub threshold_db: f32,
    /// Hold time in ms ([`HOLD_MIN_MS`]..=[`HOLD_MAX_MS`]).
    pub hold_ms: u32,
}

impl Default for GateConfig {
    fn default() -> Self {
        Self { mode: GateMode::Auto, threshold_db: DEFAULT_THRESHOLD_DB, hold_ms: DEFAULT_HOLD_MS }
    }
}

impl GateConfig {
    /// Clamp everything into its valid range (NaN falls back to the default).
    pub fn sanitized(self) -> Self {
        let threshold_db = if self.threshold_db.is_nan() { DEFAULT_THRESHOLD_DB } else { self.threshold_db };
        Self {
            mode: self.mode,
            threshold_db: threshold_db.clamp(THRESHOLD_MIN_DB, THRESHOLD_MAX_DB),
            hold_ms: self.hold_ms.clamp(HOLD_MIN_MS, HOLD_MAX_MS),
        }
    }
}

/// State shared between the mic callback and the UI: the settings flow in, the meter flows out.
/// All access is a relaxed atomic load/store, so changes take effect on the next chunk.
#[derive(Debug)]
pub struct VoiceGate {
    mode: AtomicU8,
    threshold: AtomicU32,
    hold_ms: AtomicU32,
    /// Push-to-talk is on: the gate is bypassed while the mic is open (the key is held).
    ptt: AtomicBool,
    // Outputs of the processor.
    level: AtomicU32,
    open: AtomicBool,
    active_threshold: AtomicU32,
}

impl Default for VoiceGate {
    fn default() -> Self {
        Self::new(GateConfig::default())
    }
}

impl VoiceGate {
    pub fn new(cfg: GateConfig) -> Self {
        let cfg = cfg.sanitized();
        Self {
            mode: AtomicU8::new(cfg.mode.to_u8()),
            threshold: AtomicU32::new(cfg.threshold_db.to_bits()),
            hold_ms: AtomicU32::new(cfg.hold_ms),
            ptt: AtomicBool::new(false),
            level: AtomicU32::new(MIN_DB.to_bits()),
            open: AtomicBool::new(false),
            active_threshold: AtomicU32::new(cfg.threshold_db.to_bits()),
        }
    }

    pub fn set_config(&self, cfg: GateConfig) {
        let cfg = cfg.sanitized();
        self.mode.store(cfg.mode.to_u8(), Ordering::Relaxed);
        self.threshold.store(cfg.threshold_db.to_bits(), Ordering::Relaxed);
        self.hold_ms.store(cfg.hold_ms, Ordering::Relaxed);
    }

    pub fn config(&self) -> GateConfig {
        GateConfig {
            mode: GateMode::from_u8(self.mode.load(Ordering::Relaxed)),
            threshold_db: f32::from_bits(self.threshold.load(Ordering::Relaxed)),
            hold_ms: self.hold_ms.load(Ordering::Relaxed),
        }
    }

    /// Push-to-talk mode: while on, an open (unmuted) mic always transmits, with no gate.
    pub fn set_ptt(&self, on: bool) {
        self.ptt.store(on, Ordering::Relaxed);
    }

    pub fn ptt(&self) -> bool {
        self.ptt.load(Ordering::Relaxed)
    }

    /// Post-processing mic level in dBFS (RMS of the last 10 ms, with a smooth fall).
    /// [`MIN_DB`] when the mic is muted or not running.
    pub fn level_db(&self) -> f32 {
        f32::from_bits(self.level.load(Ordering::Relaxed))
    }

    /// Whether audio is being sent right now (gate open, or push-to-talk held).
    pub fn is_open(&self) -> bool {
        self.open.load(Ordering::Relaxed)
    }

    /// The threshold in use, in dBFS: the manual one, or the adaptive one in automatic mode.
    pub fn threshold_db(&self) -> f32 {
        f32::from_bits(self.active_threshold.load(Ordering::Relaxed))
    }

    /// Meter reading to "nothing" (mic muted, stopped or never started).
    pub fn clear_outputs(&self) {
        self.level.store(MIN_DB.to_bits(), Ordering::Relaxed);
        self.open.store(false, Ordering::Relaxed);
    }
}

/// RMS level of `samples` in dBFS (0 dBFS = full-scale square wave), [`MIN_DB`] for silence.
pub fn rms_dbfs(samples: &[i16]) -> f32 {
    if samples.is_empty() {
        return MIN_DB;
    }
    let sq: u64 = samples.iter().map(|&s| (s as i64 * s as i64) as u64).sum();
    if sq == 0 {
        return MIN_DB;
    }
    let mean = sq as f64 / samples.len() as f64;
    let db = 10.0 * (mean / (32768.0 * 32768.0)).log10();
    (db as f32).max(MIN_DB)
}

/// The gate itself. One per microphone path; feed it every post-APM chunk in order.
pub struct GateProcessor {
    gate: Arc<VoiceGate>,
    open: bool,
    hold_left_ms: f32,
    /// Fade gain actually applied (0 = shut, 1 = open).
    gain: f32,
    meter_db: f32,
    // Noise-floor tracker (automatic mode; always running so switching modes is instant).
    floor_db: f32,
    calibrated: bool,
    win_chunks: u32,
    win_min: f32,
    win_max: f32,
    /// Consecutive steady windows seen.
    steady: u32,
}

impl GateProcessor {
    pub fn new(gate: Arc<VoiceGate>) -> Self {
        Self {
            gate,
            open: false,
            hold_left_ms: 0.0,
            gain: 0.0,
            meter_db: MIN_DB,
            floor_db: MIN_DB,
            calibrated: false,
            win_chunks: 0,
            win_min: f32::MAX,
            win_max: f32::MIN,
            steady: 0,
        }
    }

    /// The mic stopped (muted): shut the gate, blank the meter. The noise floor is kept.
    pub fn reset(&mut self) {
        self.open = false;
        self.hold_left_ms = 0.0;
        self.gain = 0.0;
        self.meter_db = MIN_DB;
        self.gate.clear_outputs();
    }

    /// The current noise-floor estimate in dBFS (automatic mode).
    pub fn noise_floor_db(&self) -> f32 {
        self.floor_db
    }

    fn track_floor(&mut self, level: f32) {
        let level = level.max(FLOOR_MIN_DB);
        self.win_min = self.win_min.min(level);
        self.win_max = self.win_max.max(level);
        self.win_chunks += 1;
        if !self.calibrated {
            if self.win_chunks >= CALIBRATION_CHUNKS {
                self.floor_db = self.win_min;
                self.calibrated = true;
                self.new_window();
            }
            return;
        }
        if level < self.floor_db {
            self.floor_db = level; // quieter than the floor: it is the floor
        }
        if self.win_chunks >= WINDOW_CHUNKS {
            if self.win_max - self.win_min <= STATIONARY_DB {
                self.steady += 1;
            } else {
                self.steady = 0;
            }
            if self.win_min > self.floor_db {
                let diff = self.win_min - self.floor_db;
                // Steady sound is noise. While the gate is open it must stay steady for a while
                // first (real speech never holds still for 1.5 s), so a fan that just started is
                // learned but a held note is not.
                let step = if self.steady > 0 && (!self.open || self.steady >= STEADY_WHEN_OPEN) {
                    diff
                } else if self.open {
                    diff.min(RISE_OPEN_DB)
                } else {
                    diff.min(RISE_CLOSED_DB)
                };
                self.floor_db += step;
            }
            self.new_window();
        }
    }

    fn new_window(&mut self) {
        self.win_chunks = 0;
        self.win_min = f32::MAX;
        self.win_max = f32::MIN;
    }

    /// Meter, decide and apply the gate to one chunk (in place). Returns whether the chunk is
    /// being sent (gate open or push-to-talk); when it is not, the chunk has become silence.
    pub fn process(&mut self, chunk: &mut [i16]) -> bool {
        let n = chunk.len();
        if n == 0 {
            return self.open;
        }
        let chunk_ms = n as f32 * 1000.0 / MIX_RATE as f32;
        let full = rms_dbfs(chunk);
        let (a, b) = chunk.split_at(n / 2);
        // The gate looks at the louder half (5 ms), so a word onset anywhere in the chunk opens
        // it from the chunk's start.
        let peak = full.max(rms_dbfs(a)).max(rms_dbfs(b));

        self.meter_db = full.max(self.meter_db - METER_FALL_DB_PER_S * chunk_ms / 1000.0);
        self.track_floor(full);

        let cfg = self.gate.config();
        let threshold = match cfg.mode {
            GateMode::Manual => cfg.threshold_db,
            GateMode::Auto if !self.calibrated => AUTO_MAX_DB,
            GateMode::Auto => (self.floor_db + AUTO_MARGIN_DB).clamp(AUTO_MIN_DB, AUTO_MAX_DB),
        };
        let need = if self.open { threshold - HYSTERESIS_DB } else { threshold };
        if peak >= need {
            self.open = true;
            self.hold_left_ms = cfg.hold_ms as f32;
        } else if self.open {
            self.hold_left_ms -= chunk_ms;
            if self.hold_left_ms <= 0.0 {
                self.open = false;
            }
        }
        if self.gate.ptt() {
            // Push-to-talk overrides the gate: the key decides (the caller only gets here while
            // the mic is unmuted), and the first word must not be faded.
            self.open = true;
            self.hold_left_ms = cfg.hold_ms as f32;
            self.gain = 1.0;
        }

        self.apply_fade(chunk);

        self.gate.level.store(self.meter_db.to_bits(), Ordering::Relaxed);
        self.gate.active_threshold.store(threshold.to_bits(), Ordering::Relaxed);
        self.gate.open.store(self.open, Ordering::Relaxed);
        self.open
    }

    fn apply_fade(&mut self, chunk: &mut [i16]) {
        let target = if self.open { 1.0 } else { 0.0 };
        if self.gain == target {
            if target == 0.0 {
                chunk.fill(0);
            }
            return;
        }
        let rising = target > self.gain;
        let step = if rising { 1.0 / FADE_IN_SAMPLES } else { -1.0 / FADE_OUT_SAMPLES };
        for s in chunk.iter_mut() {
            self.gain = (self.gain + step).clamp(0.0, 1.0);
            *s = (*s as f32 * self.gain) as i16;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHUNK: usize = 480;

    /// Deterministic generator for synthetic signals.
    struct Sig {
        phase: f32,
        rng: u32,
    }

    impl Sig {
        fn new() -> Self {
            Self { phase: 0.0, rng: 12345 }
        }

        /// 1 kHz sine whose RMS is `db` dBFS.
        fn tone(&mut self, db: f32) -> Vec<i16> {
            let amp = 32768.0 * 10f32.powf(db / 20.0) * std::f32::consts::SQRT_2;
            (0..CHUNK)
                .map(|_| {
                    self.phase = (self.phase + 1000.0 / 48000.0).fract();
                    ((self.phase * std::f32::consts::TAU).sin() * amp) as i16
                })
                .collect()
        }

        /// Speech-like: the level wanders between about -20 and -40 dBFS from chunk to chunk.
        fn speech(&mut self, i: usize) -> Vec<i16> {
            const SHAPE: [f32; 12] = [-20.0, -22.0, -26.0, -32.0, -38.0, -30.0, -22.0, -20.0, -24.0, -34.0, -40.0, -28.0];
            self.tone(SHAPE[i % SHAPE.len()])
        }

        /// White noise whose RMS is `db` dBFS.
        fn noise(&mut self, db: f32) -> Vec<i16> {
            let amp = 32768.0 * 10f32.powf(db / 20.0) * 3f32.sqrt(); // uniform: rms = amp / sqrt 3
            (0..CHUNK)
                .map(|_| {
                    self.rng = self.rng.wrapping_mul(1664525).wrapping_add(1013904223);
                    let u = (self.rng >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0;
                    (u * amp) as i16
                })
                .collect()
        }
    }

    fn manual(threshold_db: f32, hold_ms: u32) -> (Arc<VoiceGate>, GateProcessor) {
        let gate = Arc::new(VoiceGate::new(GateConfig { mode: GateMode::Manual, threshold_db, hold_ms }));
        (gate.clone(), GateProcessor::new(gate))
    }

    fn auto() -> (Arc<VoiceGate>, GateProcessor) {
        let gate = Arc::new(VoiceGate::default());
        (gate.clone(), GateProcessor::new(gate))
    }

    fn feed(p: &mut GateProcessor, make: &mut dyn FnMut() -> Vec<i16>, chunks: usize) -> Vec<(bool, Vec<i16>)> {
        (0..chunks)
            .map(|_| {
                let mut c = make();
                let open = p.process(&mut c);
                (open, c)
            })
            .collect()
    }

    #[test]
    fn dbfs_of_known_signals() {
        assert_eq!(rms_dbfs(&[]), MIN_DB);
        assert_eq!(rms_dbfs(&[0; 480]), MIN_DB);
        // Full-scale square wave is 0 dBFS (i16::MIN has the full magnitude).
        let sq: Vec<i16> = (0..480).map(|i| if i % 2 == 0 { i16::MIN } else { 32767 }).collect();
        assert!(rms_dbfs(&sq).abs() < 0.01, "{}", rms_dbfs(&sq));
        // Half scale = -6.02 dB.
        assert!((rms_dbfs(&vec![16384; 480]) + 6.02).abs() < 0.01);
        // Sine: 3.01 dB under its peak.
        let mut sig = Sig::new();
        for db in [-10.0, -30.0, -50.0, -60.0] {
            let got = rms_dbfs(&sig.tone(db));
            assert!((got - db).abs() < 0.3, "wanted {db}, measured {got}");
            let got = rms_dbfs(&sig.noise(db));
            assert!((got - db).abs() < 1.0, "noise: wanted {db}, measured {got}");
        }
        // Anything below the scale floor reads as the floor.
        assert!(rms_dbfs(&[1; 480]) >= MIN_DB);
    }

    #[test]
    fn manual_gate_attack_hold_and_release() {
        let (gate, mut p) = manual(-40.0, 350);
        let mut sig = Sig::new();
        // Silence: closed, output stays silent.
        for (open, c) in feed(&mut p, &mut || vec![0; CHUNK], 20) {
            assert!(!open && c.iter().all(|&s| s == 0));
        }
        assert!(!gate.is_open());
        // Tone burst at -20 dBFS: opens in the very first chunk, with a 5 ms fade-in.
        let inputs: Vec<Vec<i16>> = (0..30).map(|_| sig.tone(-20.0)).collect();
        let mut next = inputs.iter().cloned();
        let burst = feed(&mut p, &mut || next.next().unwrap(), 30);
        assert!(burst[0].0 && gate.is_open());
        let first = &burst[0].1;
        assert!(first[0].abs() < 200, "fade starts near zero: {}", first[0]);
        let peak_late = first[240..].iter().map(|s| s.abs()).max().unwrap();
        assert!(peak_late > 4000, "full level after the 5 ms fade: {peak_late}");
        assert!(first[..48].iter().map(|s| s.abs()).max().unwrap() < first[200..].iter().map(|s| s.abs()).max().unwrap());
        // After the 10th chunk the audio is untouched.
        assert_eq!(burst[10].1, inputs[10]);
        // Silence again: stays open for the hold time (350 ms = 35 chunks), then closes.
        let tail = feed(&mut p, &mut || vec![0; CHUNK], 50);
        let open_chunks = tail.iter().take_while(|(open, _)| *open).count();
        assert!((34..=36).contains(&open_chunks), "held for {open_chunks} chunks");
        assert!(!gate.is_open());
        // Everything after the fade is exact digital silence.
        assert!(tail[open_chunks + 2..].iter().all(|(_, c)| c.iter().all(|&s| s == 0)));
        // A second burst opens it again.
        assert!(feed(&mut p, &mut || sig.tone(-20.0), 1)[0].0);
    }

    #[test]
    fn fade_out_has_no_step() {
        let (_gate, mut p) = manual(-40.0, 100);
        let mut dc = || vec![8000i16; CHUNK];
        let loud = feed(&mut p, &mut dc, 5);
        assert!(loud[4].0);
        // Quiet constant input after the hold: the output ramps down to zero within 10 ms.
        let mut quiet = || vec![3i16; CHUNK];
        let out = feed(&mut p, &mut quiet, 20);
        let closed_at = out.iter().position(|(open, _)| !*open).unwrap();
        let chunk = &out[closed_at].1;
        // The hold chunks passed the quiet signal; the closing chunk fades it to exactly zero.
        assert_eq!(chunk[CHUNK - 1], 0);
        assert!(chunk.windows(2).all(|w| (w[0] - w[1]).abs() <= 1));
    }

    #[test]
    fn hold_bridges_short_pauses() {
        let (gate, mut p) = manual(-40.0, 350);
        let mut sig = Sig::new();
        let mut was_closed = false;
        feed(&mut p, &mut || sig.tone(-25.0), 5);
        for _ in 0..4 {
            // 200 ms pause (shorter than the hold), then speech again.
            for (open, _) in feed(&mut p, &mut || vec![0; CHUNK], 20) {
                was_closed |= !open;
            }
            assert!(feed(&mut p, &mut || sig.tone(-25.0), 5).iter().all(|(o, _)| *o));
        }
        assert!(!was_closed, "word gaps must not close the gate");
        assert!(gate.is_open());
    }

    #[test]
    fn noise_below_threshold_stays_shut_and_above_opens() {
        let (gate, mut p) = manual(-40.0, 350);
        let mut sig = Sig::new();
        let quiet = feed(&mut p, &mut || sig.noise(-55.0), 200);
        assert!(quiet.iter().all(|(open, c)| !open && c.iter().all(|&s| s == 0)));
        assert!(!gate.is_open());
        // The meter still shows what the mic hears (post-APM, pre-gate).
        assert!((gate.level_db() + 55.0).abs() < 2.0, "meter {}", gate.level_db());
        let loud = feed(&mut p, &mut || sig.noise(-30.0), 20);
        assert!(loud.iter().all(|(open, _)| *open));
        assert!(gate.is_open());
        // 3 dB under the threshold while open still counts as speech (hysteresis)...
        let (gate, mut p) = manual(-40.0, 100);
        feed(&mut p, &mut || sig.tone(-30.0), 3);
        assert!(feed(&mut p, &mut || sig.tone(-42.0), 30).iter().all(|(o, _)| *o));
        // ...but from closed the same level does not open it.
        let (_g, mut p2) = manual(-40.0, 100);
        assert!(feed(&mut p2, &mut || sig.tone(-42.0), 30).iter().all(|(o, _)| !*o));
        assert!(gate.is_open());
    }

    #[test]
    fn threshold_changes_apply_to_the_next_chunk() {
        let (gate, mut p) = manual(-20.0, 100);
        let mut sig = Sig::new();
        assert!(!feed(&mut p, &mut || sig.tone(-35.0), 1)[0].0);
        gate.set_config(GateConfig { mode: GateMode::Manual, threshold_db: -50.0, hold_ms: 100 });
        assert!(feed(&mut p, &mut || sig.tone(-35.0), 1)[0].0);
        assert_eq!(gate.threshold_db(), -50.0);
    }

    #[test]
    fn push_to_talk_overrides_the_gate() {
        // A threshold nothing can reach: the gate would never open.
        let (gate, mut p) = manual(-10.0, 350);
        let mut sig = Sig::new();
        let tone = sig.tone(-40.0);
        let mut c = tone.clone();
        assert!(!p.process(&mut c));
        assert!(c.iter().all(|&s| s == 0));
        // PTT on: transmits right away, unmodified (no fade), even silence.
        gate.set_ptt(true);
        let mut c = tone.clone();
        assert!(p.process(&mut c) && gate.is_open());
        assert_eq!(c, tone);
        let mut z = vec![0i16; CHUNK];
        assert!(p.process(&mut z));
        // PTT off: back to gating.
        gate.set_ptt(false);
        let mut c = tone.clone();
        let _ = p.process(&mut c);
        for _ in 0..60 {
            let mut c = tone.clone();
            p.process(&mut c);
        }
        assert!(!gate.is_open());
    }

    #[test]
    fn reset_blanks_the_meter_and_shuts_the_gate() {
        let (gate, mut p) = manual(-40.0, 350);
        let mut sig = Sig::new();
        feed(&mut p, &mut || sig.tone(-20.0), 10);
        assert!(gate.is_open() && gate.level_db() > -25.0);
        p.reset();
        assert!(!gate.is_open());
        assert_eq!(gate.level_db(), MIN_DB);
        // Starts closed again (fade-in on the next speech).
        let c = feed(&mut p, &mut || sig.tone(-20.0), 1);
        assert!(c[0].0 && c[0].1[0].abs() < 200);
    }

    #[test]
    fn meter_rises_instantly_and_falls_smoothly() {
        let (gate, mut p) = manual(-40.0, 350);
        let mut sig = Sig::new();
        feed(&mut p, &mut || sig.tone(-20.0), 3);
        assert!((gate.level_db() + 20.0).abs() < 0.5);
        feed(&mut p, &mut || vec![0; CHUNK], 10);
        let after = gate.level_db();
        assert!(after < -20.0 && after > -50.0, "falls gradually, not at once: {after}");
        feed(&mut p, &mut || vec![0; CHUNK], 200);
        assert_eq!(gate.level_db(), MIN_DB);
    }

    #[test]
    fn auto_threshold_follows_the_noise_floor() {
        let (gate, mut p) = auto();
        let mut sig = Sig::new();
        // Steady room noise at -50 dBFS: after calibration the threshold sits 15 dB above it and
        // the noise never opens the gate (apart from the calibration period, gate shut).
        let r = feed(&mut p, &mut || sig.noise(-50.0), 400);
        assert!(r.iter().all(|(open, _)| !open), "steady noise must not open the gate");
        assert!((p.noise_floor_db() + 50.0).abs() < 4.0, "floor {}", p.noise_floor_db());
        let thr = gate.threshold_db();
        assert!((thr + 35.0).abs() < 4.0, "threshold {thr}");
        // Speech well above the threshold opens it, and the floor does not chase speech.
        let mut i = 0;
        let speech = feed(&mut p, &mut || {
            i += 1;
            sig.speech(i)
        }, 300);
        assert!(speech.iter().all(|(open, _)| *open));
        assert!(gate.threshold_db() < -30.0 + 0.1, "threshold stays bounded: {}", gate.threshold_db());
        assert!(p.noise_floor_db() < -45.0, "speech did not drag the floor up: {}", p.noise_floor_db());
        // Back to noise: closes after the hold.
        let after = feed(&mut p, &mut || sig.noise(-50.0), 60);
        assert!(!after.last().unwrap().0);
    }

    #[test]
    fn auto_adapts_when_the_room_gets_louder() {
        let (gate, mut p) = auto();
        let mut sig = Sig::new();
        feed(&mut p, &mut || sig.noise(-60.0), 300);
        assert!(!gate.is_open());
        let quiet_thr = gate.threshold_db();
        assert!((quiet_thr + 45.0).abs() < 4.0, "threshold {quiet_thr}");
        // A fan starts: steady noise 20 dB louder. It trips the gate for a moment, then the floor
        // adopts it and the gate closes again.
        let r = feed(&mut p, &mut || sig.noise(-40.0), 300);
        assert!(r.iter().take(80).any(|(open, _)| *open), "initially the new noise opens the gate");
        assert!(!r.last().unwrap().0, "adapted: steady noise no longer opens the gate");
        assert!(gate.threshold_db() > quiet_thr + 6.0, "threshold rose: {}", gate.threshold_db());
        // Speech on top of the new floor still passes.
        assert!(feed(&mut p, &mut || sig.tone(-22.0), 5).iter().all(|(o, _)| *o));
    }

    #[test]
    fn auto_threshold_never_goes_below_the_minimum_in_a_silent_room() {
        let (gate, mut p) = auto();
        feed(&mut p, &mut || vec![0; CHUNK], 300);
        assert_eq!(gate.threshold_db(), AUTO_MIN_DB);
        // Breath-level noise under that minimum is ignored; something at -45 opens it.
        let mut sig = Sig::new();
        assert!(!feed(&mut p, &mut || sig.noise(-65.0), 1)[0].0);
        assert!(feed(&mut p, &mut || sig.tone(-45.0), 1)[0].0);
    }

    #[test]
    fn auto_lets_loud_speech_through_before_calibration() {
        let (_gate, mut p) = auto();
        let mut sig = Sig::new();
        assert!(feed(&mut p, &mut || sig.tone(-20.0), 1)[0].0);
        let (_gate, mut p) = auto();
        assert!(!feed(&mut p, &mut || sig.tone(-40.0), 1)[0].0, "calibrating: quiet sounds stay out");
    }

    #[test]
    fn config_is_clamped_and_roundtrips() {
        let c = GateConfig { mode: GateMode::Manual, threshold_db: -200.0, hold_ms: 5 }.sanitized();
        assert_eq!((c.threshold_db, c.hold_ms), (THRESHOLD_MIN_DB, HOLD_MIN_MS));
        let c = GateConfig { mode: GateMode::Auto, threshold_db: f32::NAN, hold_ms: 99_999 }.sanitized();
        assert_eq!((c.threshold_db, c.hold_ms), (DEFAULT_THRESHOLD_DB, HOLD_MAX_MS));
        let gate = VoiceGate::default();
        assert_eq!(gate.config(), GateConfig::default());
        let want = GateConfig { mode: GateMode::Manual, threshold_db: -33.0, hold_ms: 500 };
        gate.set_config(want);
        assert_eq!(gate.config(), want);
        let json = serde_json::to_string(&want).unwrap();
        assert_eq!(serde_json::from_str::<GateConfig>(&json).unwrap(), want);
        // Old/partial files fall back to the defaults.
        assert_eq!(serde_json::from_str::<GateConfig>("{}").unwrap(), GateConfig::default());
        assert_eq!(serde_json::from_str::<GateConfig>(r#"{"mode":"manual"}"#).unwrap().mode, GateMode::Manual);
    }
}
