//! Voice changer ("Modificador de voz"): one effect at a time on the microphone, with an
//! intensity.
//!
//! Chain (see `audio::device`): mic -> (resample) -> WebRTC APM -> AI denoiser -> **voice
//! changer** -> level meter + gate -> Opus. The denoiser cleans the real voice first; the meter
//! and the gate see the changed voice (what is actually sent). Only the microphone goes through
//! here: Go Live audio (screen share or "Só áudio"), the soundboard and the alert sounds never do.
//!
//! The effects:
//! - **Desligado**: a bit-exact bypass (the chunk is not touched).
//! - **Grave**, **Agudo (esquilo)**, **Alienígena**: pitch shifting by TD-PSOLA ([`Psola`]): a
//!   YIN pitch tracker, pitch marks on the glottal peaks, and two-period grains laid out again at
//!   the new period. The grains keep the spectral envelope, so the formants stay where they are
//!   (a lower voice, not slow motion); each effect then moves them on purpose by resampling the
//!   grains a little (slightly down for Grave, up for the chipmunk). Unvoiced sounds (s, f, breath,
//!   noise) go through unshifted. Fixed added latency [`PITCH_DELAY`] (30 ms). Alienígena adds a
//!   6 Hz vibrato to the shift and a two-voice chorus.
//! - **Robô**: ring modulation, a short metallic comb and a light bit crush (no latency).
//! - **Rádio / telefone**: 300-3400 Hz band-pass, soft saturation, a hiss that rides the voice and
//!   a short squelch burst when a phrase ends (no latency).
//! - **Eco**: a 280 ms feedback delay that gets darker on each repeat.
//! - **Caverna**: a Freeverb-style reverb (8 damped combs, 4 all-passes) after a 30 ms pre-delay.
//!
//! Switching effects applies at the next 10 ms chunk with a 10 ms equal-power crossfade between
//! the old and the new effect (both run during the fade); a pitch effect that comes in is first
//! primed with the last 43 ms of voice, so it has no gap. The intensity is read on every chunk and
//! smoothed inside the effects. Muting (and push-to-talk release, which mutes) flushes every
//! buffer so no old voice or echo tail comes out later. Nothing here allocates once built.

use std::f32::consts::PI;
use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::denoise::{fade, NoiseControl};

const FS: f32 = 48_000.0;
/// Processing block: 10 ms at 48 kHz (the mic chunk).
pub const BLOCK: usize = 480;
/// Crossfade between effects: 10 ms (the same equal-power curve as the denoiser's).
const FADE: usize = 480;
/// Dry voice kept to prime a pitch effect when it is switched on.
const HIST: usize = 2048;

// ---------------------------------------------------------------- settings

/// "Modificador de voz".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum VoiceEffect {
    /// "Desligado": bit-exact bypass.
    #[default]
    Off,
    /// "Grave": pitch down 3-7 semitones.
    Deep,
    /// "Agudo (esquilo)": pitch up 4-8 semitones, formants up too.
    High,
    /// "Robô".
    Robot,
    /// "Rádio / telefone".
    Radio,
    /// "Eco".
    Echo,
    /// "Caverna" (reverb).
    Cave,
    /// "Alienígena": pitch up with vibrato and chorus.
    Alien,
}

impl VoiceEffect {
    pub const ALL: [VoiceEffect; 8] = [
        VoiceEffect::Off,
        VoiceEffect::Deep,
        VoiceEffect::High,
        VoiceEffect::Robot,
        VoiceEffect::Radio,
        VoiceEffect::Echo,
        VoiceEffect::Cave,
        VoiceEffect::Alien,
    ];

    /// The name shown in the UI.
    pub fn label(self) -> &'static str {
        match self {
            VoiceEffect::Off => "Desligado",
            VoiceEffect::Deep => "Grave",
            VoiceEffect::High => "Agudo (esquilo)",
            VoiceEffect::Robot => "Robô",
            VoiceEffect::Radio => "Rádio / telefone",
            VoiceEffect::Echo => "Eco",
            VoiceEffect::Cave => "Caverna",
            VoiceEffect::Alien => "Alienígena",
        }
    }

    /// A small emoji for menus.
    pub fn icon(self) -> &'static str {
        match self {
            VoiceEffect::Off => "🎤",
            VoiceEffect::Deep => "🐻",
            VoiceEffect::High => "🐿",
            VoiceEffect::Robot => "🤖",
            VoiceEffect::Radio => "📻",
            VoiceEffect::Echo => "⛰",
            VoiceEffect::Cave => "🦇",
            VoiceEffect::Alien => "👽",
        }
    }

    /// One line for the settings dialog.
    pub fn hint(self) -> &'static str {
        match self {
            VoiceEffect::Off => "Sua voz como ela é.",
            VoiceEffect::Deep => "Voz mais grossa (3 a 7 semitons abaixo), sem soar como câmera lenta. Soma 30 ms de atraso.",
            VoiceEffect::High => "Voz fina de esquilo (4 a 8 semitons acima). Soma 30 ms de atraso.",
            VoiceEffect::Robot => "Voz metálica de robô (modulação em anel e bit crush).",
            VoiceEffect::Radio => "Rádio comunicador ou telefone: banda estreita, saturação e chiado.",
            VoiceEffect::Echo => "Eco que se repete (280 ms).",
            VoiceEffect::Cave => "Reverberação de caverna ou salão grande.",
            VoiceEffect::Alien => "Voz aguda tremida com coro, de outro planeta. Soma 30 ms de atraso.",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            VoiceEffect::Off => "off",
            VoiceEffect::Deep => "deep",
            VoiceEffect::High => "high",
            VoiceEffect::Robot => "robot",
            VoiceEffect::Radio => "radio",
            VoiceEffect::Echo => "echo",
            VoiceEffect::Cave => "cave",
            VoiceEffect::Alien => "alien",
        }
    }

    /// Whether the intensity slider does something for this effect.
    pub fn has_intensity(self) -> bool {
        self != VoiceEffect::Off
    }

    /// Latency this effect adds to the voice, in ms.
    pub fn latency_ms(self) -> f32 {
        if self.is_pitch() {
            PITCH_DELAY as f32 / FS * 1000.0
        } else {
            0.0
        }
    }

    fn is_pitch(self) -> bool {
        matches!(self, VoiceEffect::Deep | VoiceEffect::High | VoiceEffect::Alien)
    }

    fn to_u8(self) -> u8 {
        Self::ALL.iter().position(|&e| e == self).unwrap_or(0) as u8
    }

    fn from_u8(v: u8) -> Self {
        Self::ALL.get(v as usize).copied().unwrap_or_default()
    }
}

/// Unknown names (a newer or older settings file) fall back to "Desligado".
impl From<String> for VoiceEffect {
    fn from(s: String) -> Self {
        Self::ALL.into_iter().find(|e| e.as_str() == s).unwrap_or_default()
    }
}

impl From<VoiceEffect> for String {
    fn from(e: VoiceEffect) -> Self {
        e.as_str().to_string()
    }
}

/// What `settings.json` stores.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceFxConfig {
    pub effect: VoiceEffect,
    /// 0..=100 (50 = the middle of each effect's range).
    pub intensity_pct: u32,
}

impl Default for VoiceFxConfig {
    fn default() -> Self {
        Self { effect: VoiceEffect::Off, intensity_pct: 50 }
    }
}

impl VoiceFxConfig {
    pub fn sanitized(self) -> Self {
        Self { effect: self.effect, intensity_pct: self.intensity_pct.min(100) }
    }
}

/// The setting shared by the UI and every mic chain (it lives in [`NoiseControl`], which every
/// chain already follows). The audio callbacks only read two atomics.
#[derive(Debug)]
pub struct VoiceFxControl {
    effect: AtomicU8,
    intensity_pct: AtomicU32,
}

impl Default for VoiceFxControl {
    fn default() -> Self {
        Self::new(VoiceFxConfig::default())
    }
}

impl VoiceFxControl {
    pub fn new(cfg: VoiceFxConfig) -> Self {
        let cfg = cfg.sanitized();
        Self { effect: AtomicU8::new(cfg.effect.to_u8()), intensity_pct: AtomicU32::new(cfg.intensity_pct) }
    }

    /// Takes effect on the next 10 ms chunk of every running mic chain (call and mic test).
    pub fn set(&self, cfg: VoiceFxConfig) {
        let cfg = cfg.sanitized();
        self.intensity_pct.store(cfg.intensity_pct, Ordering::Relaxed);
        self.effect.store(cfg.effect.to_u8(), Ordering::Relaxed);
    }

    pub fn config(&self) -> VoiceFxConfig {
        VoiceFxConfig { effect: self.effect(), intensity_pct: self.intensity_pct.load(Ordering::Relaxed) }
    }

    pub fn effect(&self) -> VoiceEffect {
        VoiceEffect::from_u8(self.effect.load(Ordering::Relaxed))
    }

    /// Intensity as 0.0..=1.0.
    fn amount(&self) -> f32 {
        self.intensity_pct.load(Ordering::Relaxed).min(100) as f32 / 100.0
    }
}

// ---------------------------------------------------------------- the processor

/// The voice changer of one mic chain. Owned by the mic callback; feed it every denoised chunk
/// (48 kHz mono, any length; whole 10 ms chunks normally).
pub struct VoiceChanger {
    ctl: Arc<NoiseControl>,
    cur: VoiceEffect,
    /// The effect being faded out, if a switch is in progress.
    old: Option<VoiceEffect>,
    fade_pos: usize,
    fx: Box<Effects>,
    /// The last [`HIST`] dry samples (ring), to prime a pitch effect.
    hist: Box<[f32; HIST]>,
    hist_pos: usize,
    prime_in: Box<[f32; HIST]>,
    prime_out: Box<[f32; HIST]>,
    dry: [f32; BLOCK],
    wet: [f32; BLOCK],
    wet_old: [f32; BLOCK],
    /// Blocks that went through an effect (not the bypass).
    pub blocks: u64,
}

impl VoiceChanger {
    /// Build it off the audio thread (it allocates its buffers here, about 0.4 MB).
    pub fn new(ctl: Arc<NoiseControl>) -> Self {
        Self {
            ctl,
            cur: VoiceEffect::Off,
            old: None,
            fade_pos: 0,
            fx: Box::new(Effects::new()),
            hist: zeros(),
            hist_pos: 0,
            prime_in: zeros(),
            prime_out: zeros(),
            dry: [0.0; BLOCK],
            wet: [0.0; BLOCK],
            wet_old: [0.0; BLOCK],
            blocks: 0,
        }
    }

    /// The effect in the signal now (the one faded in, during a switch).
    pub fn effect(&self) -> VoiceEffect {
        self.cur
    }

    /// Late pitch grains so far (always 0 unless [`PITCH_DELAY`] is too short; for tests).
    pub fn late_grains(&self) -> u64 {
        self.fx.deep.late + self.fx.high.late + self.fx.alien.late
    }

    /// The mic stopped (muted, push-to-talk released): forget all audio, tails included.
    pub fn reset(&mut self) {
        if let Some(old) = self.old.take() {
            self.fx.reset(old);
        }
        self.fx.reset(self.cur);
        self.hist.fill(0.0);
        self.fade_pos = 0;
    }

    fn push_hist(&mut self, x: &[f32]) {
        for &v in x {
            self.hist[self.hist_pos] = v;
            self.hist_pos = (self.hist_pos + 1) % HIST;
        }
    }

    fn switch_to(&mut self, target: VoiceEffect, amt: f32) {
        // The recent voice in order, oldest first.
        let (a, b) = self.hist.split_at(self.hist_pos);
        self.prime_in[..b.len()].copy_from_slice(b);
        self.prime_in[b.len()..].copy_from_slice(a);
        self.fx.start(target, amt, &self.prime_in[..], &mut self.prime_out[..]);
        self.old = Some(self.cur);
        self.cur = target;
        self.fade_pos = 0;
    }

    /// Process `chunk` in place. With "Desligado" (and no fade pending) it is left untouched.
    pub fn process(&mut self, chunk: &mut [i16]) {
        let target = self.ctl.voice_fx().effect();
        let amt = self.ctl.voice_fx().amount();
        for part in chunk.chunks_mut(BLOCK) {
            let n = part.len();
            if self.old.is_none() && target != self.cur {
                self.switch_to(target, amt);
            }
            if self.cur == VoiceEffect::Off && self.old.is_none() {
                // Bypass: only remember the voice (for priming), the chunk is not touched.
                for &s in part.iter() {
                    self.hist[self.hist_pos] = s as f32 / 32768.0;
                    self.hist_pos = (self.hist_pos + 1) % HIST;
                }
                continue;
            }
            for (d, &s) in self.dry[..n].iter_mut().zip(part.iter()) {
                *d = s as f32 / 32768.0;
            }
            self.fx.run(self.cur, amt, &self.dry[..n], &mut self.wet[..n]);
            if let Some(old) = self.old {
                self.fx.run(old, amt, &self.dry[..n], &mut self.wet_old[..n]);
                for i in 0..n {
                    let k = self.fade_pos + i;
                    let (wo, wn) = if k < FADE { fade(k) } else { (0.0, 1.0) };
                    self.wet[i] = self.wet_old[i] * wo + self.wet[i] * wn;
                }
                self.fade_pos += n;
                if self.fade_pos >= FADE {
                    self.old = None;
                    // Its state is cleared when it is switched on again.
                }
            }
            let dry = self.dry;
            self.push_hist(&dry[..n]);
            for (s, &v) in part.iter_mut().zip(&self.wet[..n]) {
                *s = (v * 32768.0).round().clamp(i16::MIN as f32, i16::MAX as f32) as i16;
            }
            self.blocks += 1;
        }
    }
}

fn zeros<const N: usize>() -> Box<[f32; N]> {
    vec![0.0f32; N].into_boxed_slice().try_into().expect("length")
}

/// Unity below 0.8 (-1.9 dBFS), then a smooth knee up to 0.98 (-0.2 dBFS): an effect output
/// never clips hard.
fn soft_clip(x: f32) -> f32 {
    const K: f32 = 0.8;
    const CEIL: f32 = 0.98;
    let a = x.abs();
    if a <= K {
        x
    } else {
        (K + (CEIL - K) * ((a - K) / (CEIL - K)).tanh()).copysign(x)
    }
}

/// Every effect's state, all allocated up front.
struct Effects {
    deep: Psola,
    high: Psola,
    alien: Psola,
    chorus: Chorus,
    robot: Robot,
    radio: Radio,
    echo: Echo,
    cave: Reverb,
}

/// Pitch shift settings of a pitch effect at intensity `amt` (0..=1).
pub(crate) fn pitch_params(e: VoiceEffect, amt: f32) -> PitchParams {
    match e {
        VoiceEffect::Deep => {
            let st = -(3.0 + 4.0 * amt);
            PitchParams { semitones: st, formant: 2f32.powf(0.2 * st / 12.0), vib_st: 0.0, vib_hz: 0.0 }
        }
        VoiceEffect::High => {
            let st = 4.0 + 4.0 * amt;
            PitchParams { semitones: st, formant: 2f32.powf(0.6 * st / 12.0), vib_st: 0.0, vib_hz: 0.0 }
        }
        _ => {
            let st = 3.0 + 3.0 * amt;
            PitchParams { semitones: st, formant: 2f32.powf(0.4 * st / 12.0), vib_st: 0.3 + 0.9 * amt, vib_hz: 6.0 }
        }
    }
}

impl Effects {
    fn new() -> Self {
        Self {
            deep: Psola::new(),
            high: Psola::new(),
            alien: Psola::new(),
            chorus: Chorus::new(),
            robot: Robot::new(),
            radio: Radio::new(),
            echo: Echo::new(),
            cave: Reverb::new(),
        }
    }

    fn reset(&mut self, e: VoiceEffect) {
        match e {
            VoiceEffect::Off => {}
            VoiceEffect::Deep => self.deep.reset(),
            VoiceEffect::High => self.high.reset(),
            VoiceEffect::Alien => {
                self.alien.reset();
                self.chorus.reset();
            }
            VoiceEffect::Robot => self.robot.reset(),
            VoiceEffect::Radio => self.radio.reset(),
            VoiceEffect::Echo => self.echo.reset(),
            VoiceEffect::Cave => self.cave.reset(),
        }
    }

    /// Switch `e` on: fresh state; a pitch effect is primed with `recent` (output dropped) so
    /// its delayed output starts with voice instead of 30 ms of silence.
    fn start(&mut self, e: VoiceEffect, amt: f32, recent: &[f32], scratch: &mut [f32]) {
        self.reset(e);
        let p = pitch_params(e, amt);
        match e {
            VoiceEffect::Deep => self.deep.process(recent, scratch, &p),
            VoiceEffect::High => self.high.process(recent, scratch, &p),
            VoiceEffect::Alien => self.alien.process(recent, scratch, &p),
            _ => {}
        }
    }

    fn run(&mut self, e: VoiceEffect, amt: f32, x: &[f32], y: &mut [f32]) {
        match e {
            VoiceEffect::Off => {
                y.copy_from_slice(x);
                return;
            }
            VoiceEffect::Deep => self.deep.process(x, y, &pitch_params(e, amt)),
            VoiceEffect::High => self.high.process(x, y, &pitch_params(e, amt)),
            VoiceEffect::Alien => {
                self.alien.process(x, y, &pitch_params(e, amt));
                self.chorus.process(y, amt);
            }
            VoiceEffect::Robot => self.robot.process(x, y, amt),
            VoiceEffect::Radio => self.radio.process(x, y, amt),
            VoiceEffect::Echo => self.echo.process(x, y, amt),
            VoiceEffect::Cave => self.cave.process(x, y, amt),
        }
        for v in y.iter_mut() {
            *v = soft_clip(*v);
        }
    }
}

// ---------------------------------------------------------------- building blocks

/// Biquad (RBJ cookbook), transposed direct form II.
#[derive(Debug, Clone, Copy, Default)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

/// Q of the two sections of a 4th-order Butterworth filter.
const BUTTER4_Q: [f32; 2] = [0.541_196_1, 1.306_563];

impl Biquad {
    fn new(b: [f32; 3], a: [f32; 3]) -> Self {
        Self { b0: b[0] / a[0], b1: b[1] / a[0], b2: b[2] / a[0], a1: a[1] / a[0], a2: a[2] / a[0], z1: 0.0, z2: 0.0 }
    }

    fn lowpass(f: f32, q: f32) -> Self {
        let w = 2.0 * PI * f / FS;
        let (s, c) = w.sin_cos();
        let al = s / (2.0 * q);
        Self::new([(1.0 - c) / 2.0, 1.0 - c, (1.0 - c) / 2.0], [1.0 + al, -2.0 * c, 1.0 - al])
    }

    fn highpass(f: f32, q: f32) -> Self {
        let w = 2.0 * PI * f / FS;
        let (s, c) = w.sin_cos();
        let al = s / (2.0 * q);
        Self::new([(1.0 + c) / 2.0, -(1.0 + c), (1.0 + c) / 2.0], [1.0 + al, -2.0 * c, 1.0 - al])
    }

    #[inline]
    fn run(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }

    fn clear(&mut self) {
        self.z1 = 0.0;
        self.z2 = 0.0;
    }
}

/// xorshift32 white noise in -1..1.
#[derive(Debug, Clone, Copy)]
struct Rng(u32);

impl Rng {
    #[inline]
    fn white(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        self.0 as f32 / u32::MAX as f32 * 2.0 - 1.0
    }
}

/// One-pole parameter smoother (about 10 ms), so intensity changes never click.
#[derive(Debug, Clone, Copy)]
struct Smooth(Option<f32>);

impl Smooth {
    #[inline]
    fn next(&mut self, target: f32) -> f32 {
        let v = match self.0 {
            Some(v) => v + (target - v) * 0.002,
            None => target,
        };
        self.0 = Some(v);
        v
    }
}

// ---------------------------------------------------------------- pitch: TD-PSOLA

const RING: usize = 8192;
const MASK: usize = RING - 1;
/// The pitch tracker runs on the 900 Hz low-passed voice decimated by 4 (12 kHz).
const DEC: usize = 4;
const DRING: usize = 2048;
const DMASK: usize = DRING - 1;
/// Lag range at 12 kHz: 500 Hz .. 75 Hz.
const T_MIN_D: usize = 24;
const T_MAX_D: usize = 160;
/// YIN integration window (10 ms) and the whole analysis span (23.3 ms).
const W_D: usize = 120;
const SPAN_D: usize = W_D + T_MAX_D;
/// One pitch estimate every 2.5 ms.
const PHOP_D: i64 = 30;
/// An estimate is assigned to this many samples before the end of its span.
const EST_BACK: i64 = 400;
/// Grains are at most two periods, and at most 2 x 10 ms (voices under 100 Hz).
const H_MAX: f32 = 480.0;
/// Unvoiced sounds: identity overlap-add of 2 x 5 ms Hann grains.
const PU: f64 = 240.0;
const F_MIN: f32 = 0.85;
const F_MAX: f32 = 1.4;
/// Fixed latency of the pitch effects, in samples (30 ms). Every grain is complete by then (worst
/// case: a 75 Hz voice with the formants lowered needs about 1370 samples: half a period to the
/// nearest mark, a 10 ms half grain of input after it, and a 11.3 ms half grain of output before
/// the next one); `late` counts it if not.
pub const PITCH_DELAY: usize = 1440;
const MARKS: usize = 64;
const ESTS: usize = 64;
/// YIN thresholds (cumulative mean normalised difference): to become voiced, to stay voiced.
const VOICED_ON: f32 = 0.15;
const VOICED_KEEP: f32 = 0.25;
/// Below -50 dBFS RMS nothing is voiced.
const MIN_RMS: f32 = 0.003;

/// How a pitch effect shifts.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct PitchParams {
    pub semitones: f32,
    /// Formant (spectral envelope) scale: 1.0 keeps them.
    pub formant: f32,
    /// Vibrato depth (semitones) and rate.
    pub vib_st: f32,
    pub vib_hz: f32,
}

/// Streaming TD-PSOLA pitch shifter with a fixed latency of [`PITCH_DELAY`]. Positions are
/// absolute sample indices; all buffers are rings.
pub(crate) struct Psola {
    n_in: i64,
    x: Box<[f32; RING]>,
    lp: Box<[f32; RING]>,
    out: Box<[f32; RING]>,
    lp_a: Biquad,
    lp_b: Biquad,
    xd: Box<[f32; DRING]>,
    n_d: i64,
    /// Pitch estimates: centre position and period in samples (0 = unvoiced).
    est_pos: [i64; ESTS],
    est_p: [f32; ESTS],
    n_est: usize,
    next_est_end: i64,
    voiced: bool,
    /// Analysis pitch marks (on the low-passed glottal peaks) and their period.
    mark_pos: [i64; MARKS],
    mark_p: [f32; MARKS],
    n_marks: usize,
    /// Where the next mark is searched (or the scan position while unvoiced).
    cursor: i64,
    prev_mark: Option<i64>,
    /// Next synthesis mark.
    s_next: f64,
    /// First output sample not emitted yet.
    n_out: i64,
    win: [f32; SPAN_D],
    diff: [f32; T_MAX_D + 2],
    /// Grain samples that arrived after their output was emitted (must stay 0).
    pub late: u64,
}

impl Psola {
    pub fn new() -> Self {
        Self {
            n_in: 0,
            x: zeros(),
            lp: zeros(),
            out: zeros(),
            lp_a: Biquad::lowpass(900.0, BUTTER4_Q[0]),
            lp_b: Biquad::lowpass(900.0, BUTTER4_Q[1]),
            xd: zeros(),
            n_d: 0,
            est_pos: [0; ESTS],
            est_p: [0.0; ESTS],
            n_est: 0,
            next_est_end: SPAN_D as i64,
            voiced: false,
            mark_pos: [0; MARKS],
            mark_p: [0.0; MARKS],
            n_marks: 0,
            cursor: 0,
            prev_mark: None,
            s_next: 0.0,
            n_out: -(PITCH_DELAY as i64),
            win: [0.0; SPAN_D],
            diff: [0.0; T_MAX_D + 2],
            late: 0,
        }
    }

    pub fn reset(&mut self) {
        let late = self.late;
        self.x.fill(0.0);
        self.lp.fill(0.0);
        self.out.fill(0.0);
        self.xd.fill(0.0);
        self.lp_a.clear();
        self.lp_b.clear();
        self.n_in = 0;
        self.n_d = 0;
        self.n_est = 0;
        self.next_est_end = SPAN_D as i64;
        self.voiced = false;
        self.n_marks = 0;
        self.cursor = 0;
        self.prev_mark = None;
        self.s_next = 0.0;
        self.n_out = -(PITCH_DELAY as i64);
        self.late = late;
    }

    /// `y` = `x` pitch shifted, [`PITCH_DELAY`] samples later. Same lengths.
    pub fn process(&mut self, x: &[f32], y: &mut [f32], p: &PitchParams) {
        for (xi, yi) in x.chunks(BLOCK).zip(y.chunks_mut(BLOCK)) {
            self.push(xi);
            self.estimate();
            self.marks();
            self.synth(p);
            self.emit(yi);
        }
    }

    fn push(&mut self, x: &[f32]) {
        for &v in x {
            let i = self.n_in as usize & MASK;
            self.x[i] = v;
            let l = self.lp_b.run(self.lp_a.run(v));
            self.lp[i] = l;
            if self.n_in % DEC as i64 == 0 {
                self.xd[self.n_d as usize & DMASK] = l;
                self.n_d += 1;
            }
            self.n_in += 1;
        }
    }

    fn estimate(&mut self) {
        while self.next_est_end <= self.n_d {
            let end = self.next_est_end;
            self.next_est_end += PHOP_D;
            let p = self.yin(end);
            self.voiced = p > 0.0;
            let i = self.n_est % ESTS;
            self.est_pos[i] = end * DEC as i64 - EST_BACK;
            self.est_p[i] = p;
            self.n_est += 1;
        }
    }

    /// YIN on the decimated span ending at `end`: the period in 48 kHz samples, or 0 (unvoiced).
    fn yin(&mut self, end: i64) -> f32 {
        let start = end - SPAN_D as i64;
        for (j, w) in self.win.iter_mut().enumerate() {
            *w = self.xd[(start + j as i64) as usize & DMASK];
        }
        let energy = self.win.iter().map(|v| v * v).sum::<f32>() / SPAN_D as f32;
        if energy < MIN_RMS * MIN_RMS {
            return 0.0;
        }
        let win = &self.win;
        let d = &mut self.diff;
        d[0] = 0.0;
        for tau in 1..=T_MAX_D {
            let mut s = 0.0;
            for (a, b) in win[..W_D].iter().zip(&win[tau..tau + W_D]) {
                let e = a - b;
                s += e * e;
            }
            d[tau] = s;
        }
        // Cumulative mean normalised difference.
        let mut run = 0.0;
        for (tau, v) in d.iter_mut().enumerate().take(T_MAX_D + 1).skip(1) {
            run += *v;
            *v = if run > 0.0 { *v * tau as f32 / run } else { 1.0 };
        }
        let thr = if self.voiced { VOICED_KEEP } else { VOICED_ON };
        let mut tau = T_MIN_D;
        while tau <= T_MAX_D && d[tau] >= thr {
            tau += 1;
        }
        if tau > T_MAX_D {
            return 0.0;
        }
        while tau < T_MAX_D && d[tau + 1] < d[tau] {
            tau += 1;
        }
        // The first dip can belong to a strong harmonic sitting on a formant (a dip at 2/3 of
        // the period, say): a clearly deeper later dip that is not a multiple of it is the period.
        let mut t = tau + 1;
        while t < T_MAX_D {
            if d[t] < d[t - 1] && d[t] <= d[t + 1] && d[t] < d[tau] * 0.6 {
                let m = t as f32 / tau as f32;
                if (m - m.round()).abs() >= 0.06 {
                    tau = t;
                }
            }
            t += 1;
        }
        let t = if tau > 1 && tau < T_MAX_D {
            let (a, b, c) = (d[tau - 1], d[tau], d[tau + 1]);
            let den = a - 2.0 * b + c;
            if den.abs() > 1e-9 {
                tau as f32 + (0.5 * (a - c) / den).clamp(-0.5, 0.5)
            } else {
                tau as f32
            }
        } else {
            tau as f32
        };
        t * DEC as f32
    }

    fn latest_center(&self) -> Option<i64> {
        (self.n_est > 0).then(|| self.est_pos[(self.n_est - 1) % ESTS])
    }

    /// Period at `pos` (interpolated between voiced estimates); 0 = unvoiced. Needs an estimate.
    fn period_at(&self, pos: i64) -> f32 {
        let count = self.n_est.min(ESTS);
        let first = self.n_est - count;
        let mut k = self.n_est - 1;
        loop {
            let i = k % ESTS;
            if self.est_pos[i] <= pos || k == first {
                if k + 1 < self.n_est && self.est_pos[i] <= pos {
                    let j = (k + 1) % ESTS;
                    let (pa, pb) = (self.est_p[i], self.est_p[j]);
                    let (ca, cb) = (self.est_pos[i], self.est_pos[j]);
                    if pa > 0.0 && pb > 0.0 {
                        return pa + (pb - pa) * (pos - ca) as f32 / (cb - ca) as f32;
                    }
                    return if pos - ca <= cb - pos { pa } else { pb };
                }
                return self.est_p[i];
            }
            k -= 1;
        }
    }

    /// Place analysis marks on the low-passed signal's peaks, one period apart.
    /// While voiced, marks run up to one period past the latest estimate with its period (the
    /// peaks are there already; only the voicing beyond is not known yet).
    fn marks(&mut self) {
        let Some(latest) = self.latest_center() else { return };
        let p_latest = self.est_p[(self.n_est - 1) % ESTS];
        loop {
            let c = self.cursor;
            let p = if c <= latest {
                self.period_at(c)
            } else if p_latest > 0.0 && c <= latest + p_latest as i64 {
                p_latest
            } else {
                break;
            };
            if p <= 0.0 {
                self.prev_mark = None;
                self.cursor = c + 60;
                continue;
            }
            let (lo, hi, prev) = match self.prev_mark {
                Some(prev) => {
                    let pred = prev + p.round() as i64;
                    (pred - (p * 0.25) as i64, pred + (p * 0.25) as i64, Some((prev, pred)))
                }
                None => (c - (p * 0.5) as i64, c + (p * 0.5) as i64, None),
            };
            if hi >= self.n_in {
                break;
            }
            let lo = lo.max(self.n_in - RING as i64 + 1).max(0);
            let mut pos = lo;
            let mut best = f32::MIN;
            for t in lo..=hi {
                let v = self.lp[t as usize & MASK];
                if v > best {
                    best = v;
                    pos = t;
                }
            }
            if let Some((prev, pred)) = prev {
                if pos <= prev + (p * 0.5) as i64 {
                    pos = pred;
                }
            }
            let i = self.n_marks % MARKS;
            self.mark_pos[i] = pos;
            self.mark_p[i] = p;
            self.n_marks += 1;
            self.prev_mark = Some(pos);
            self.cursor = pos + p.round() as i64;
        }
    }

    /// Nearest known analysis mark to `si` that is at most `fwd` after it: (index, distance).
    fn nearest_mark(&self, si: i64, fwd: i64) -> Option<(usize, i64)> {
        let count = self.n_marks.min(MARKS);
        let mut best: Option<(usize, i64)> = None;
        for k in (self.n_marks - count..self.n_marks).rev() {
            let i = k % MARKS;
            if self.mark_pos[i] > si + fwd {
                continue;
            }
            let d = (self.mark_pos[i] - si).abs();
            match best {
                Some((_, bd)) if d >= bd => {
                    if self.mark_pos[i] < si {
                        break;
                    }
                }
                _ => best = Some((i, d)),
            }
        }
        best
    }

    /// Lay out synthesis grains as far as the input allows.
    fn synth(&mut self, p: &PitchParams) {
        let Some(latest) = self.latest_center() else { return };
        let f = p.formant.clamp(F_MIN, F_MAX);
        loop {
            let s = self.s_next;
            let si = s.round() as i64;
            if si > latest {
                break;
            }
            let period = self.period_at(si);
            let mut chosen = None;
            if period > 0.0 {
                // Marks up to half a period ahead are used (so the grain's input is there
                // early enough), anything up to 1.25 periods behind.
                let fwd = (period * 0.5) as i64 + 2;
                // While the marks run on, one that is not placed yet could still be nearer.
                if self.prev_mark.is_some() && self.cursor - (period * 0.5) as i64 <= si + fwd {
                    break;
                }
                if let Some((i, d)) = self.nearest_mark(si, fwd) {
                    if d as f32 <= period * 1.25 {
                        chosen = Some(i);
                    }
                }
            }
            match chosen {
                Some(i) => {
                    let (a, pa) = (self.mark_pos[i], self.mark_p[i]);
                    let h = pa.min(H_MAX);
                    if a + h as i64 + 3 >= self.n_in {
                        break;
                    }
                    let vib = if p.vib_st != 0.0 { p.vib_st * (2.0 * std::f64::consts::PI * p.vib_hz as f64 * s / FS as f64).sin() as f32 } else { 0.0 };
                    let ratio = 2f32.powf((p.semitones + vib) / 12.0);
                    let spacing = pa / ratio;
                    // Keep the loudness: PSOLA changes the power by about ratio * (h/pa)^2 / f.
                    let g = (spacing * f / pa).sqrt() * (pa / h);
                    self.grain(s, a as f64, h / f, f, g);
                    self.s_next += spacing as f64;
                }
                None => {
                    if si + PU as i64 + 3 >= self.n_in {
                        break;
                    }
                    self.grain(s, s, PU as f32, 1.0, 1.0);
                    self.s_next += PU;
                }
            }
        }
    }

    /// Overlap-add one Hann grain centred on output `s`, read around input `src` at rate `f`.
    fn grain(&mut self, s: f64, src: f64, half: f32, f: f32, g: f32) {
        let t0 = (s - half as f64).floor() as i64 + 1;
        let t1 = (s + half as f64).ceil() as i64 - 1;
        // Hann window by rotation: w = 0.5 + 0.5 cos(theta), theta from (t0 - s) * pi / half.
        let dth = (PI / half) as f64;
        let th0 = (t0 as f64 - s) * dth;
        let (mut sn, mut cs) = th0.sin_cos();
        let (ds, dc) = dth.sin_cos();
        for t in t0..=t1 {
            if t >= 0 {
                if t < self.n_out {
                    self.late += 1;
                } else {
                    let u = t as f64 - s;
                    let v = self.read(src + u * f as f64);
                    self.out[t as usize & MASK] += g * (0.5 + 0.5 * cs as f32) * v;
                }
            }
            let c2 = cs * dc - sn * ds;
            sn = sn * dc + cs * ds;
            cs = c2;
        }
    }

    /// Input at a fractional position (cubic Hermite).
    #[inline]
    fn read(&self, pos: f64) -> f32 {
        let i = pos.floor();
        let fr = (pos - i) as f32;
        let i = i as i64;
        let at = |k: i64| self.x[k as usize & MASK];
        let (xm, x0, x1, x2) = (at(i - 1), at(i), at(i + 1), at(i + 2));
        if fr == 0.0 {
            return x0;
        }
        let c1 = 0.5 * (x1 - xm);
        let c2 = xm - 2.5 * x0 + 2.0 * x1 - 0.5 * x2;
        let c3 = 0.5 * (x2 - xm) + 1.5 * (x0 - x1);
        ((c3 * fr + c2) * fr + c1) * fr + x0
    }

    fn emit(&mut self, y: &mut [f32]) {
        let start = self.n_in - y.len() as i64 - PITCH_DELAY as i64;
        for (j, o) in y.iter_mut().enumerate() {
            let t = start + j as i64;
            if t < 0 {
                *o = 0.0;
            } else {
                let i = t as usize & MASK;
                *o = self.out[i];
                self.out[i] = 0.0;
            }
        }
        self.n_out = start + y.len() as i64;
    }
}

/// Two slowly modulated delay taps (15 and 20 ms) on top of the voice.
struct Chorus {
    buf: Box<[f32; 2048]>,
    pos: usize,
    ph: [f32; 2],
    mix: Smooth,
}

impl Chorus {
    fn new() -> Self {
        Self { buf: zeros(), pos: 0, ph: [0.0, 1.7], mix: Smooth(None) }
    }

    fn reset(&mut self) {
        self.buf.fill(0.0);
        self.mix = Smooth(None);
    }

    fn process(&mut self, y: &mut [f32], amt: f32) {
        const BASE: [f32; 2] = [720.0, 960.0];
        const DEPTH: [f32; 2] = [144.0, 192.0];
        const RATE: [f32; 2] = [0.9, 1.4];
        for v in y.iter_mut() {
            self.buf[self.pos] = *v;
            let m = self.mix.next(0.25 + 0.35 * amt);
            let mut taps = 0.0;
            for k in 0..2 {
                self.ph[k] += 2.0 * PI * RATE[k] / FS;
                if self.ph[k] > 2.0 * PI {
                    self.ph[k] -= 2.0 * PI;
                }
                let d = BASE[k] + DEPTH[k] * self.ph[k].sin();
                let rp = self.pos as f32 + 2048.0 - d;
                let i = rp.floor();
                let fr = rp - i;
                let i = i as usize;
                taps += self.buf[i & 2047] * (1.0 - fr) + self.buf[(i + 1) & 2047] * fr;
            }
            *v = (*v + m * 0.5 * taps) / (1.0 + 0.5 * m);
            self.pos = (self.pos + 1) & 2047;
        }
    }
}

// ---------------------------------------------------------------- robot

/// Ring modulation (70 Hz), a 5 ms metallic comb and a light bit crush.
struct Robot {
    phase: f32,
    comb: Box<[f32; 512]>,
    pos: usize,
    hold: f32,
    hold_k: u32,
    mix: Smooth,
    fb: Smooth,
}

impl Robot {
    const CARRIER_HZ: f32 = 70.0;
    const COMB: usize = 240;

    fn new() -> Self {
        Self { phase: 0.0, comb: zeros(), pos: 0, hold: 0.0, hold_k: 0, mix: Smooth(None), fb: Smooth(None) }
    }

    fn reset(&mut self) {
        self.comb.fill(0.0);
        self.hold = 0.0;
        self.hold_k = 0;
        self.mix = Smooth(None);
        self.fb = Smooth(None);
    }

    fn process(&mut self, x: &[f32], y: &mut [f32], amt: f32) {
        let dph = 2.0 * PI * Self::CARRIER_HZ / FS;
        let q = 2f32.powf(11.0 - 4.0 * amt); // 12 .. 8 bits
        let hold_n = if amt > 0.6 { 3 } else { 2 };
        for (o, &v) in y.iter_mut().zip(x) {
            let m = self.mix.next(0.6 + 0.4 * amt);
            let fb = self.fb.next(0.45 + 0.35 * amt);
            self.phase += dph;
            if self.phase > 2.0 * PI {
                self.phase -= 2.0 * PI;
            }
            let rm = v * self.phase.sin() * std::f32::consts::SQRT_2;
            let r = v * (1.0 - m) + rm * m;
            let d = self.comb[(self.pos + 512 - Self::COMB) & 511];
            let c = r + fb * d;
            self.comb[self.pos] = c;
            self.pos = (self.pos + 1) & 511;
            let mixed = 0.5 * r + 0.5 * c * (1.0 - fb * fb).sqrt();
            if self.hold_k == 0 {
                self.hold = (mixed * q).round() / q;
            }
            self.hold_k = (self.hold_k + 1) % hold_n;
            *o = self.hold * 1.3;
        }
    }
}

// ---------------------------------------------------------------- radio

/// 300-3400 Hz band-pass, hiss riding the voice, a squelch burst after each phrase, soft
/// saturation.
struct Radio {
    hp: [Biquad; 2],
    lp: [Biquad; 2],
    env: f32,
    rng: Rng,
    talk: u32,
    burst: u32,
    drive: Smooth,
    hiss: Smooth,
}

impl Radio {
    /// Squelch burst: 150 ms, after at least 400 ms of talking.
    const BURST: u32 = 7_200;
    const TALK_MIN: u32 = 19_200;

    fn new() -> Self {
        Self {
            hp: BUTTER4_Q.map(|q| Biquad::highpass(300.0, q)),
            lp: BUTTER4_Q.map(|q| Biquad::lowpass(3_400.0, q)),
            env: 0.0,
            rng: Rng(0x9e37_79b9),
            talk: 0,
            burst: 0,
            drive: Smooth(None),
            hiss: Smooth(None),
        }
    }

    fn reset(&mut self) {
        for b in self.hp.iter_mut().chain(self.lp.iter_mut()) {
            b.clear();
        }
        self.env = 0.0;
        self.talk = 0;
        self.burst = 0;
        self.drive = Smooth(None);
        self.hiss = Smooth(None);
    }

    fn process(&mut self, x: &[f32], y: &mut [f32], amt: f32) {
        for (o, &v) in y.iter_mut().zip(x) {
            let k = self.drive.next(2.0 + 4.0 * amt);
            let h = self.hiss.next(0.12 + 0.2 * amt);
            let a = v.abs();
            self.env += (a - self.env) * if a > self.env { 0.004 } else { 0.0004 };
            if self.env > 0.01 {
                self.talk = self.talk.saturating_add(1);
            } else if self.env < 0.004 {
                if self.talk > Self::TALK_MIN && self.burst == 0 {
                    self.burst = Self::BURST;
                }
                self.talk = 0;
            }
            let mut n = self.rng.white() * self.env * h;
            if self.burst > 0 {
                let left = self.burst as f32 / Self::BURST as f32;
                n += self.rng.white() * 0.05 * (0.5 + amt) * left.sqrt();
                self.burst -= 1;
            }
            let mut b = v + n;
            for f in self.hp.iter_mut().chain(self.lp.iter_mut()) {
                b = f.run(b);
            }
            *o = (k * b).tanh() / k * 1.6;
        }
    }
}

// ---------------------------------------------------------------- echo

const ECHO_LEN: usize = 16_384;
/// 280 ms.
const ECHO_DELAY: usize = 13_440;

struct Echo {
    buf: Box<[f32; ECHO_LEN]>,
    pos: usize,
    lp: f32,
    fb: Smooth,
    wet: Smooth,
}

impl Echo {
    fn new() -> Self {
        Self { buf: zeros(), pos: 0, lp: 0.0, fb: Smooth(None), wet: Smooth(None) }
    }

    fn reset(&mut self) {
        self.buf.fill(0.0);
        self.lp = 0.0;
        self.fb = Smooth(None);
        self.wet = Smooth(None);
    }

    fn process(&mut self, x: &[f32], y: &mut [f32], amt: f32) {
        for (o, &v) in y.iter_mut().zip(x) {
            let fb = self.fb.next(0.3 + 0.3 * amt);
            let wet = self.wet.next(0.4 + 0.25 * amt);
            let d = self.buf[(self.pos + ECHO_LEN - ECHO_DELAY) & (ECHO_LEN - 1)];
            self.lp += 0.33 * (d - self.lp); // ~3 kHz: every repeat a bit darker
            *o = v + wet * self.lp;
            self.buf[self.pos] = v + fb * self.lp;
            self.pos = (self.pos + 1) & (ECHO_LEN - 1);
        }
    }
}

// ---------------------------------------------------------------- reverb

/// Freeverb's comb and all-pass lengths, scaled from 44.1 to 48 kHz.
const COMB_LEN: [usize; 8] = [1215, 1293, 1390, 1476, 1548, 1623, 1695, 1760];
const ALLPASS_LEN: [usize; 4] = [605, 480, 371, 245];
/// 30 ms pre-delay: a big space.
const PREDELAY: usize = 1440;
/// Input gain into the combs: the wet signal is about as loud as the dry one at full wet.
const REVERB_IN: f32 = 0.06;

struct Reverb {
    combs: Box<[[f32; 2048]; 8]>,
    comb_idx: [usize; 8],
    comb_store: [f32; 8],
    aps: Box<[[f32; 1024]; 4]>,
    ap_idx: [usize; 4],
    pre: Box<[f32; 2048]>,
    pre_pos: usize,
    fb: Smooth,
    wet: Smooth,
}

impl Reverb {
    fn new() -> Self {
        Self {
            combs: vec![[0.0f32; 2048]; 8].into_boxed_slice().try_into().expect("len"),
            comb_idx: [0; 8],
            comb_store: [0.0; 8],
            aps: vec![[0.0f32; 1024]; 4].into_boxed_slice().try_into().expect("len"),
            ap_idx: [0; 4],
            pre: zeros(),
            pre_pos: 0,
            fb: Smooth(None),
            wet: Smooth(None),
        }
    }

    fn reset(&mut self) {
        for c in self.combs.iter_mut() {
            c.fill(0.0);
        }
        for a in self.aps.iter_mut() {
            a.fill(0.0);
        }
        self.pre.fill(0.0);
        self.comb_store = [0.0; 8];
        self.fb = Smooth(None);
        self.wet = Smooth(None);
    }

    fn process(&mut self, x: &[f32], y: &mut [f32], amt: f32) {
        const DAMP: f32 = 0.2;
        for (o, &v) in y.iter_mut().zip(x) {
            let fb = self.fb.next(0.8 + 0.16 * amt);
            let wet = self.wet.next(0.25 + 0.45 * amt);
            let pd = self.pre[(self.pre_pos + 2048 - PREDELAY) & 2047];
            self.pre[self.pre_pos] = v;
            self.pre_pos = (self.pre_pos + 1) & 2047;
            let inp = pd * REVERB_IN;
            let mut s = 0.0;
            let combs = self.combs.iter_mut().zip(self.comb_idx.iter_mut()).zip(self.comb_store.iter_mut().zip(COMB_LEN));
            for ((buf, idx), (store, len)) in combs {
                let out = buf[*idx];
                *store = out * (1.0 - DAMP) + *store * DAMP;
                buf[*idx] = inp + *store * fb;
                *idx = if *idx + 1 == len { 0 } else { *idx + 1 };
                s += out;
            }
            for ((buf, idx), len) in self.aps.iter_mut().zip(self.ap_idx.iter_mut()).zip(ALLPASS_LEN) {
                let bo = buf[*idx];
                buf[*idx] = s + bo * 0.5;
                s = bo - s;
                *idx = if *idx + 1 == len { 0 } else { *idx + 1 };
            }
            *o = v * (1.0 - 0.35 * amt) + s * wet;
        }
    }
}

// ---------------------------------------------------------------- test helpers

/// Synthetic speech and a pitch estimator for the tests and the offline evaluation
/// (`voicefx_eval`). Independent of the effect's own pitch tracker.
#[cfg(test)]
pub(crate) mod synth {
    use super::*;

    /// Vowel formants (F1, F2, F3) in Hz.
    const VOWELS: [[f32; 3]; 5] = [[730.0, 1090.0, 2440.0], [530.0, 1840.0, 2480.0], [270.0, 2290.0, 3010.0], [570.0, 840.0, 2410.0], [300.0, 870.0, 2240.0]];

    /// Klatt formant resonator (unity gain at DC), to be cascaded like a vocal tract.
    fn formant(f: f32, bw: f32) -> Biquad {
        let c = -(-2.0 * PI * bw / FS).exp();
        let b = 2.0 * (-PI * bw / FS).exp() * (2.0 * PI * f / FS).cos();
        let a = 1.0 - b - c;
        Biquad::new([a, 0.0, 0.0], [1.0, -b, -c])
    }

    /// Speech-like signal: syllables of voiced vowels (band-limited glottal source through three
    /// formant resonators, f0 gliding +-12 % around `f0` with intonation), fricatives (high-passed
    /// noise) and pauses, at about -20 dBFS RMS while talking. Deterministic per `seed`.
    pub fn speech(seconds: f32, f0: f32, seed: u32) -> Vec<f32> {
        let n = (seconds * FS) as usize;
        let mut out = vec![0.0f32; n];
        let mut rng = Rng(seed.max(1));
        let mut rnd = move || rng.white() * 0.5 + 0.5;
        let mut t = 2_400usize; // starts after 50 ms of silence
        let mut phase = 0.0f32;
        let mut leak = 0.0f32;
        let mut syll = 0usize;
        while t < n {
            // A fricative now and then.
            if rnd() < 0.35 {
                let len = (3_000.0 + rnd() * 2_500.0) as usize;
                let mut hp = Biquad::highpass(3_000.0, 0.7);
                for k in 0..len.min(n - t) {
                    let env = (k as f32 / len as f32 * PI).sin();
                    out[t + k] += hp.run(rnd() * 2.0 - 1.0) * 0.05 * env;
                }
                t += len;
            }
            // A voiced syllable.
            let len = (8_500.0 + rnd() * 4_000.0) as usize;
            let v = VOWELS[syll % VOWELS.len()];
            syll += 1;
            let mut res = [formant(v[0], 90.0), formant(v[1], 110.0), formant(v[2], 160.0)];
            for k in 0..len.min(n.saturating_sub(t)) {
                let time = (t + k) as f32 / FS;
                let f = f0 * (1.0 + 0.12 * (2.0 * PI * 0.6 * time).sin()) * (1.0 + 0.01 * (2.0 * PI * 5.0 * time).sin());
                phase += 2.0 * PI * f / FS;
                if phase > 2.0 * PI {
                    phase -= 2.0 * PI;
                }
                // Band-limited impulse train (harmonics up to 5 kHz), leaky-integrated: -6 dB/oct.
                let h = (5_000.0 / f).floor().max(1.0);
                let half = phase * 0.5;
                let den = half.sin();
                let blit = if den.abs() < 1e-5 { h } else { ((h + 0.5) * phase).sin() / (2.0 * den) - 0.5 };
                leak = leak * 0.995 + blit * (f / FS) * 2.0;
                let env = ((k as f32 / 960.0).min(1.0)) * (((len - k) as f32 / 1_920.0).min(1.0));
                let mut s = leak;
                for r in res.iter_mut() {
                    s = r.run(s);
                }
                out[t + k] += s * env;
            }
            t += len;
            // Pause: short between syllables, long between phrases.
            t += if syll.is_multiple_of(5) { 19_200 } else { (1_500.0 + rnd() * 4_000.0) as usize };
        }
        let talking: Vec<f32> = out.iter().copied().filter(|v| v.abs() > 1e-4).collect();
        let rms = (talking.iter().map(|v| v * v).sum::<f32>() / talking.len().max(1) as f32).sqrt();
        let g = 0.1 / rms.max(1e-9);
        out.iter_mut().for_each(|v| *v *= g);
        out
    }

    /// Median f0 (Hz) over the clearly voiced 40 ms frames of `x`, by normalised autocorrelation
    /// (45-700 Hz) on the 2 kHz low-passed signal.
    pub fn median_f0(x: &[f32]) -> f32 {
        let mut lp = BUTTER4_Q.map(|q| Biquad::lowpass(2_000.0, q));
        let y: Vec<f32> = x
            .iter()
            .step_by(1)
            .map(|&v| {
                let mut s = v;
                for f in lp.iter_mut() {
                    s = f.run(s);
                }
                s
            })
            .collect();
        // Decimate by 2 (24 kHz).
        let y: Vec<f32> = y.iter().step_by(2).copied().collect();
        let fs = FS / 2.0;
        let (lag_min, lag_max) = ((fs / 700.0) as usize, (fs / 45.0) as usize);
        let w = 960; // 40 ms
        let mut f0s = Vec::new();
        let mut i = 0;
        while i + w + lag_max < y.len() {
            let a = &y[i..i + w];
            let e0: f32 = a.iter().map(|v| v * v).sum();
            if e0 / w as f32 > 1e-4 {
                let mut best = (0usize, 0.0f32);
                let mut ncc = vec![0.0f32; lag_max + 2];
                for (lag, nv) in ncc.iter_mut().enumerate().take(lag_max + 1).skip(lag_min) {
                    let b = &y[i + lag..i + lag + w];
                    let num: f32 = a.iter().zip(b).map(|(p, q)| p * q).sum();
                    let e1: f32 = b.iter().map(|v| v * v).sum();
                    *nv = num / (e0 * e1).sqrt().max(1e-12);
                }
                // The highest peak, or its shortest sub-multiple that is nearly as high (so a
                // peak at twice the period does not win).
                for lag in lag_min + 1..lag_max {
                    if ncc[lag] > best.1 && ncc[lag] >= ncc[lag - 1] && ncc[lag] >= ncc[lag + 1] {
                        best = (lag, ncc[lag]);
                    }
                }
                for div in [3usize, 2] {
                    let l = best.0 / div;
                    if l > lag_min + 1 {
                        let (lo, hi) = (l.saturating_sub(3).max(lag_min + 1), (l + 3).min(lag_max - 1));
                        if let Some(c) = (lo..=hi).max_by(|&a, &b| ncc[a].total_cmp(&ncc[b])) {
                            if ncc[c] > 0.9 * best.1 && ncc[c] >= ncc[c - 1] && ncc[c] >= ncc[c + 1] {
                                best = (c, ncc[c]);
                                break;
                            }
                        }
                    }
                }
                if best.1 > 0.75 {
                    let l = best.0;
                    let (a0, b0, c0) = (ncc[l - 1], ncc[l], ncc[l + 1]);
                    let den = a0 - 2.0 * b0 + c0;
                    let lf = if den.abs() > 1e-9 { l as f32 + 0.5 * (a0 - c0) / den } else { l as f32 };
                    f0s.push(fs / lf);
                }
            }
            i += w / 2;
        }
        if f0s.is_empty() {
            return 0.0;
        }
        f0s.sort_by(f32::total_cmp);
        f0s[f0s.len() / 2]
    }

    pub fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32).sqrt()
    }

    pub fn to_i16(x: &[f32]) -> Vec<i16> {
        x.iter().map(|v| (v * 32768.0).round().clamp(-32768.0, 32767.0) as i16).collect()
    }

    pub fn to_f32(x: &[i16]) -> Vec<f32> {
        x.iter().map(|&v| v as f32 / 32768.0).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::synth::*;
    use super::*;
    use crate::audio::NoiseLevel;

    fn chain(effect: VoiceEffect, pct: u32) -> (Arc<NoiseControl>, VoiceChanger) {
        let ctl = Arc::new(NoiseControl::new(NoiseLevel::Standard));
        ctl.voice_fx().set(VoiceFxConfig { effect, intensity_pct: pct });
        let fx = VoiceChanger::new(ctl.clone());
        (ctl, fx)
    }

    fn run(fx: &mut VoiceChanger, x: &[f32]) -> Vec<f32> {
        let mut pcm = to_i16(x);
        for c in pcm.chunks_mut(BLOCK) {
            fx.process(c);
        }
        to_f32(&pcm)
    }

    #[test]
    fn effects_roundtrip_and_unknown_names_fall_back() {
        for e in VoiceEffect::ALL {
            let j = serde_json::to_string(&e).unwrap();
            assert_eq!(serde_json::from_str::<VoiceEffect>(&j).unwrap(), e);
            assert_eq!(VoiceEffect::from_u8(e.to_u8()), e);
        }
        assert_eq!(serde_json::to_string(&VoiceEffect::Cave).unwrap(), "\"cave\"");
        assert_eq!(serde_json::from_str::<VoiceEffect>("\"vocoder\"").unwrap(), VoiceEffect::Off);
        let cfg: VoiceFxConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg, VoiceFxConfig::default());
        let c = VoiceFxControl::new(VoiceFxConfig { effect: VoiceEffect::Robot, intensity_pct: 250 });
        assert_eq!(c.config(), VoiceFxConfig { effect: VoiceEffect::Robot, intensity_pct: 100 });
    }

    #[test]
    fn off_is_a_bit_exact_bypass() {
        let (_ctl, mut fx) = chain(VoiceEffect::Off, 100);
        let x = speech(1.0, 120.0, 3);
        let mut pcm = to_i16(&x);
        let want = pcm.clone();
        // Odd chunk sizes too.
        for c in pcm.chunks_mut(441) {
            fx.process(c);
        }
        assert_eq!(pcm, want);
        assert_eq!(fx.blocks, 0);
    }

    fn check_shift(effect: VoiceEffect, pct: u32, f0: f32) {
        let (_ctl, mut fx) = chain(effect, pct);
        let x = speech(3.0, f0, 11);
        let y = run(&mut fx, &x);
        let want = 2f32.powf(pitch_params(effect, pct as f32 / 100.0).semitones / 12.0);
        let (fi, fo) = (median_f0(&x), median_f0(&y));
        let got = fo / fi;
        assert!((got / want - 1.0).abs() < 0.03, "{effect:?} at {f0} Hz: ratio {got:.3}, wanted {want:.3} ({fi:.1} -> {fo:.1} Hz)");
        assert_eq!(fx.late_grains(), 0, "{effect:?}: grains later than the fixed delay");
        // Loudness kept within 3 dB, nothing at full scale.
        let db = 20.0 * (rms(&y) / rms(&x)).log10();
        assert!(db.abs() < 3.0, "{effect:?} at {f0} Hz: level changed by {db:.1} dB");
        assert!(y.iter().all(|v| v.abs() < 0.99));
    }

    #[test]
    fn deep_lowers_the_pitch_by_the_requested_ratio() {
        check_shift(VoiceEffect::Deep, 50, 120.0);
        check_shift(VoiceEffect::Deep, 100, 210.0);
    }

    #[test]
    fn high_raises_the_pitch_by_the_requested_ratio() {
        check_shift(VoiceEffect::High, 50, 120.0);
        check_shift(VoiceEffect::High, 0, 210.0);
    }

    #[test]
    fn the_fixed_delay_holds_for_a_very_low_voice() {
        // A 79-101 Hz talker (the tracker follows down to 75 Hz), formants lowered the most.
        check_shift(VoiceEffect::Deep, 100, 90.0);
    }

    #[test]
    fn unvoiced_sound_goes_through_the_pitch_shifter_unchanged_after_the_delay() {
        let (_ctl, mut fx) = chain(VoiceEffect::Deep, 50);
        let mut r = Rng(5);
        let x: Vec<f32> = (0..48_000).map(|_| r.white() * 0.1).collect();
        // Straight to the engine (no crossfade from the bypass, no soft clip).
        let mut y = vec![0.0; x.len()];
        fx.fx.deep.process(&x, &mut y, &pitch_params(VoiceEffect::Deep, 0.5));
        let d = PITCH_DELAY;
        let err = x[..x.len() - d].iter().zip(&y[d..]).skip(4_800).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
        assert!(err < 1e-4, "noise is delayed by exactly {d} samples (max error {err})");
    }

    #[test]
    fn switching_effects_is_click_free_and_back_to_off_is_bit_exact() {
        let (ctl, mut fx) = chain(VoiceEffect::Off, 50);
        let x = speech(4.0, 140.0, 21);
        let mut pcm = to_i16(&x);
        let natural = pcm.windows(2).map(|w| (w[1] as i32 - w[0] as i32).abs()).max().unwrap();
        let mut seen = Vec::new();
        let steps = [VoiceEffect::Deep, VoiceEffect::Robot, VoiceEffect::High, VoiceEffect::Echo, VoiceEffect::Alien, VoiceEffect::Radio, VoiceEffect::Cave, VoiceEffect::Off];
        for (k, c) in pcm.chunks_mut(BLOCK).enumerate() {
            if k % 40 == 10 && k / 40 < steps.len() {
                ctl.voice_fx().set(VoiceFxConfig { effect: steps[k / 40], intensity_pct: 30 + (k as u32 % 70) });
            }
            fx.process(c);
            seen.push(fx.effect());
        }
        // Each switch is in effect on the same chunk (fading) and complete one chunk later.
        for (i, e) in steps.iter().enumerate() {
            assert_eq!(seen[i * 40 + 10], *e);
        }
        let max_step = pcm.windows(2).map(|w| (w[1] as i32 - w[0] as i32).abs()).max().unwrap();
        assert!(max_step <= natural * 2, "step {max_step} vs natural {natural}");
        // Back to Desligado (chunk 290, faded by 291): bit for bit from chunk 291 on.
        let orig = to_i16(&x);
        assert_eq!(&pcm[BLOCK * 291..], &orig[BLOCK * 291..]);
        assert_eq!(fx.late_grains(), 0);
    }

    #[test]
    fn intensity_changes_mid_stream_are_smooth() {
        let (ctl, mut fx) = chain(VoiceEffect::Echo, 0);
        let x = speech(2.0, 150.0, 4);
        let mut pcm = to_i16(&x);
        for (k, c) in pcm.chunks_mut(BLOCK).enumerate() {
            ctl.voice_fx().set(VoiceFxConfig { effect: VoiceEffect::Echo, intensity_pct: if (k / 10) % 2 == 0 { 0 } else { 100 } });
            fx.process(c);
        }
        let natural = to_i16(&x).windows(2).map(|w| (w[1] as i32 - w[0] as i32).abs()).max().unwrap();
        let max_step = pcm.windows(2).map(|w| (w[1] as i32 - w[0] as i32).abs()).max().unwrap();
        assert!(max_step <= natural * 2, "step {max_step} vs natural {natural}");
    }

    #[test]
    fn reset_forgets_old_voice_and_tails() {
        for e in VoiceEffect::ALL.into_iter().skip(1) {
            let (_ctl, mut fx) = chain(e, 100);
            let x = speech(1.0, 130.0, 8);
            run(&mut fx, &x);
            fx.reset();
            // Silence after unmute: nothing of the old voice, no echo or reverb tail.
            let y = run(&mut fx, &vec![0.0; 48_000]);
            let peak = y.iter().fold(0.0f32, |m, v| m.max(v.abs()));
            assert!(peak < 1e-3, "{e:?}: stale audio after reset, peak {peak}");
        }
    }

    #[test]
    fn every_effect_changes_the_voice_without_clipping() {
        let x = speech(2.0, 130.0, 9);
        // Loud input too (peaks near full scale).
        let loud: Vec<f32> = x.iter().map(|v| v * 4.0).collect();
        for e in VoiceEffect::ALL.into_iter().skip(1) {
            for (input, name) in [(&x, "normal"), (&loud, "loud")] {
                let (_ctl, mut fx) = chain(e, 100);
                let y = run(&mut fx, input);
                assert!(y.iter().all(|v| v.abs() < 0.995), "{e:?} {name}: clipped");
                let diff = rms(&y.iter().zip(input.iter()).map(|(a, b)| a - b).collect::<Vec<_>>());
                assert!(diff > rms(input) * 0.1, "{e:?} {name}: barely changed the voice");
                assert!(rms(&y) > rms(input) * 0.3, "{e:?} {name}: too quiet");
            }
        }
    }

    #[test]
    fn radio_is_band_limited() {
        let (_ctl, mut fx) = chain(VoiceEffect::Radio, 0);
        // 100 Hz and 8 kHz tones are cut, 1 kHz passes.
        let tone = |f: f32| (0..24_000).map(|i| (2.0 * PI * f * i as f32 / FS).sin() * 0.05).collect::<Vec<f32>>();
        let level = |fx: &mut VoiceChanger, f: f32| {
            let y = run(fx, &tone(f));
            20.0 * (rms(&y[9_600..]) / (0.05 / 2f32.sqrt())).log10()
        };
        let (lo, mid, hi) = (level(&mut fx, 100.0), level(&mut fx, 1_000.0), level(&mut fx, 8_000.0));
        assert!(mid > -3.0 && lo < mid - 20.0 && hi < mid - 20.0, "100 Hz {lo:.1} dB, 1 kHz {mid:.1} dB, 8 kHz {hi:.1} dB");
    }

    #[test]
    fn echo_repeats_after_280_ms() {
        let (_ctl, mut fx) = chain(VoiceEffect::Echo, 50);
        let mut x = vec![0.0f32; 48_000];
        for v in x[4_800..5_280].iter_mut() {
            *v = 0.3;
        }
        let y = run(&mut fx, &x);
        let after = &y[4_800 + ECHO_DELAY..4_800 + ECHO_DELAY + 480];
        assert!(rms(after) > 0.05, "the echo comes back 280 ms later");
        assert!(rms(&y[6_000..4_800 + ECHO_DELAY - 200]) < 1e-3, "and nothing in between");
    }
}
