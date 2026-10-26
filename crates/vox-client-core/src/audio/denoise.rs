//! Noise suppression level ("Supressão de ruído") and the AI denoiser of the mic chain.
//!
//! Chain (see `audio::device`): mic -> (resample) -> WebRTC APM (AEC3 + AGC + high-pass, plus its
//! own noise suppressor only at [`NoiseLevel::Standard`]) -> [`Denoiser`] (RNNoise via the
//! pure-Rust `nnnoiseless` port at [`NoiseLevel::AiLight`], DeepFilterNet3 on a worker thread at
//! [`NoiseLevel::AiMax`], see `audio::dfn`) -> level meter + gate -> Opus. Screen-share audio
//! never goes through here.
//!
//! RNNoise works on 10 ms frames at 48 kHz (480 samples), the size of our mic chunks, so the
//! normal path is one frame per chunk with no buffering; its own algorithmic delay is one frame
//! (10 ms). DeepFilterNet3 uses the same frames, with 30 ms of algorithmic delay plus 10 ms of
//! hand-off to its worker. Any other chunk size goes through a fixed-size frame adapter (one more
//! frame of delay). Everything here is allocation-free once built.
//!
//! Switching the level applies on the next chunk, without rejoining: [`NoiseControl::set_level`]
//! rebuilds the APMs it knows (with or without their noise suppressor; that happens on the
//! caller's thread, never in the audio callback), starts or lets go of the DeepFilterNet workers,
//! and the [`Denoiser`] crossfades between the plain, the RNNoise and the DeepFilterNet signal
//! over one chunk. While DeepFilterNet loads (about 0.3 s, each time it is selected) the
//! chain runs RNNoise. On the way in RNNoise is primed with the previous chunk; on the way out
//! (and when the mic is muted) both models are flushed, so no stale audio is ever heard when they
//! are turned back on. If the machine cannot run DeepFilterNet in real time the level drops back
//! to "IA leve" by itself and [`NoiseControl::take_fallback_notice`] tells the UI.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Weak};

use nnnoiseless::DenoiseState;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use super::device::VoiceApm;
use super::dfn::{DfnClient, DfnShared};
use super::voicefx::VoiceFxControl;

/// RNNoise frame: 10 ms at 48 kHz.
pub const FRAME: usize = DenoiseState::FRAME_SIZE;

/// "Supressão de ruído".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum NoiseLevel {
    /// "Padrão": the WebRTC APM noise suppressor (what v0.4.0 and earlier did).
    Standard,
    /// "IA leve": RNNoise, with the APM's own suppressor off.
    #[default]
    AiLight,
    /// "IA máxima": DeepFilterNet3, with the APM's own suppressor off.
    AiMax,
}

const LEVEL_STANDARD: u8 = 0;
const LEVEL_AI_LIGHT: u8 = 1;
const LEVEL_AI_MAX: u8 = 2;

impl NoiseLevel {
    pub const ALL: [NoiseLevel; 3] = [NoiseLevel::Standard, NoiseLevel::AiLight, NoiseLevel::AiMax];

    /// Whether the WebRTC APM runs its own noise suppressor at this level.
    pub fn apm_ns(self) -> bool {
        self == NoiseLevel::Standard
    }

    pub fn is_ai(self) -> bool {
        !self.apm_ns()
    }

    /// The name shown in the UI.
    pub fn label(self) -> &'static str {
        match self {
            NoiseLevel::Standard => "Padrão",
            NoiseLevel::AiLight => "IA leve",
            NoiseLevel::AiMax => "IA máxima",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            NoiseLevel::Standard => "standard",
            NoiseLevel::AiLight => "ai_light",
            NoiseLevel::AiMax => "ai_max",
        }
    }

    fn to_u8(self) -> u8 {
        match self {
            NoiseLevel::Standard => LEVEL_STANDARD,
            NoiseLevel::AiLight => LEVEL_AI_LIGHT,
            NoiseLevel::AiMax => LEVEL_AI_MAX,
        }
    }

    fn from_u8(v: u8) -> Self {
        match v {
            LEVEL_STANDARD => NoiseLevel::Standard,
            LEVEL_AI_MAX => NoiseLevel::AiMax,
            _ => NoiseLevel::AiLight,
        }
    }
}

/// Unknown names (a newer or older settings file) fall back to the default level.
impl From<String> for NoiseLevel {
    fn from(s: String) -> Self {
        match s.as_str() {
            "standard" => NoiseLevel::Standard,
            "ai_light" => NoiseLevel::AiLight,
            "ai_max" => NoiseLevel::AiMax,
            _ => NoiseLevel::default(),
        }
    }
}

impl From<NoiseLevel> for String {
    fn from(l: NoiseLevel) -> Self {
        l.as_str().to_string()
    }
}

/// The level shared by the UI and every mic chain (voice and mic test). The audio callbacks only
/// read an atomic; the APMs registered with [`NoiseControl::attach`] are rebuilt, and the
/// DeepFilterNet workers of the chains started, on the thread that calls
/// [`NoiseControl::set_level`].
///
/// It also carries the voice changer setting ([`NoiseControl::voice_fx`]): both are the user's
/// microphone processing choices, followed live by every mic chain (the call and the mic test).
#[derive(Debug)]
pub struct NoiseControl {
    level: Arc<AtomicU8>,
    apms: Mutex<Vec<Weak<VoiceApm>>>,
    dfn: Mutex<Vec<Weak<DfnShared>>>,
    /// "IA máxima" was too heavy and the level fell back to "IA leve" (for the UI).
    fell_back: AtomicBool,
    /// "Modificador de voz" (see `audio::voicefx`).
    fx: VoiceFxControl,
}

impl Default for NoiseControl {
    fn default() -> Self {
        Self::new(NoiseLevel::default())
    }
}

impl NoiseControl {
    pub fn new(level: NoiseLevel) -> Self {
        Self {
            level: Arc::new(AtomicU8::new(level.to_u8())),
            apms: Mutex::new(Vec::new()),
            dfn: Mutex::new(Vec::new()),
            fell_back: AtomicBool::new(false),
            fx: VoiceFxControl::default(),
        }
    }

    /// The voice changer setting ("Modificador de voz"); changes apply on the next 10 ms chunk.
    pub fn voice_fx(&self) -> &VoiceFxControl {
        &self.fx
    }

    pub fn level(&self) -> NoiseLevel {
        NoiseLevel::from_u8(self.level.load(Ordering::Relaxed))
    }

    /// Takes effect on the next 10 ms chunk of every running mic chain. Selecting "IA máxima"
    /// starts the DeepFilterNet worker of every chain (it loads the model in the background);
    /// selecting another level makes them exit and free it.
    pub fn set_level(&self, level: NoiseLevel) {
        let mut apms = self.apms.lock();
        self.level.store(level.to_u8(), Ordering::Relaxed);
        apms.retain(|w| match w.upgrade() {
            Some(apm) => {
                apm.set_noise_suppression(level.apm_ns());
                true
            }
            None => false,
        });
        drop(apms);
        let mut dfn = self.dfn.lock();
        dfn.retain(|w| match w.upgrade() {
            Some(sh) => {
                if level == NoiseLevel::AiMax {
                    sh.ensure_worker();
                } else {
                    sh.wake_to_exit();
                }
                true
            }
            None => false,
        });
    }

    /// Keep this APM's noise suppressor in step with the level (now and on every change).
    pub fn attach(&self, apm: &Arc<VoiceApm>) {
        let mut apms = self.apms.lock();
        apms.retain(|w| w.strong_count() > 0);
        apm.set_noise_suppression(self.level().apm_ns());
        apms.push(Arc::downgrade(apm));
    }

    /// A new mic chain's DeepFilterNet side (started now if "IA máxima" is selected).
    fn register_dfn(&self) -> Arc<DfnShared> {
        let sh = DfnShared::new(self.level.clone(), LEVEL_AI_MAX);
        let mut dfn = self.dfn.lock();
        dfn.retain(|w| w.strong_count() > 0);
        dfn.push(Arc::downgrade(&sh));
        if self.level() == NoiseLevel::AiMax {
            sh.ensure_worker();
        }
        sh
    }

    /// Whether "IA máxima" fell back to "IA leve" because this machine could not run it in real
    /// time (once per fallback). The level is already "IA leve" when this returns true.
    pub fn take_fallback_notice(&self) -> bool {
        self.fell_back.swap(false, Ordering::Relaxed)
    }

    /// From the audio callback: "IA máxima" is too heavy here. Allocation-free.
    fn fall_back(&self) {
        if self.level.compare_exchange(LEVEL_AI_MAX, LEVEL_AI_LIGHT, Ordering::Relaxed, Ordering::Relaxed).is_ok() {
            self.fell_back.store(true, Ordering::Relaxed);
        }
    }

    /// Mic chains whose DeepFilterNet worker currently holds the model.
    pub fn ai_max_loaded(&self) -> usize {
        self.dfn.lock().iter().filter_map(|w| w.upgrade()).filter(|s| s.loaded()).count()
    }
}

/// Equal-power crossfade weights for sample `i` of a frame: (old, new).
pub(crate) fn fade(i: usize) -> (f32, f32) {
    let t = (i as f32 + 0.5) / FRAME as f32 * std::f32::consts::FRAC_PI_2;
    (t.cos(), t.sin())
}

/// What is in the signal path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Plain,
    Rnn,
    Dfn,
}

/// RNNoise and DeepFilterNet on whole 480-sample frames, with the transitions. Samples are `f32`
/// in the `i16` range (what RNNoise expects).
struct FrameDenoiser {
    state: Box<DenoiseState<'static>>,
    /// RNNoise is running (in the signal, or being faded in or out).
    rnn_on: bool,
    mode: Mode,
    /// The previous plain frame (to prime RNNoise when it is switched on).
    prev: [f32; FRAME],
    wet: [f32; FRAME],
    dfn_out: [f32; FRAME],
    scratch: [f32; FRAME],
    dfn: DfnClient,
}

/// What ran on one frame.
#[derive(Default)]
struct Ran {
    rnn: bool,
    /// DeepFilterNet's output was in the signal.
    dfn: bool,
}

impl FrameDenoiser {
    fn new(dfn: Arc<DfnShared>) -> Self {
        Self {
            state: DenoiseState::new(),
            rnn_on: false,
            mode: Mode::Plain,
            prev: [0.0; FRAME],
            wet: [0.0; FRAME],
            dfn_out: [0.0; FRAME],
            scratch: [0.0; FRAME],
            dfn: DfnClient::new(dfn),
        }
    }

    /// Clear RNNoise's input history and overlap-add memory (two frames of silence).
    fn flush_rnn(&mut self) {
        let zeros = [0.0f32; FRAME];
        for _ in 0..2 {
            self.state.process_frame(&mut self.scratch, &zeros);
        }
    }

    /// Process one frame in place.
    fn frame(&mut self, io: &mut [f32; FRAME], ctl: &NoiseControl) -> Ran {
        let mut level = ctl.level();
        if level == NoiseLevel::AiMax && self.dfn.wait {
            self.dfn.wait_loaded();
        }
        // DeepFilterNet is fed while it is selected and loaded (warming up behind RNNoise, then
        // in the signal), and until it has been faded out.
        if level != NoiseLevel::AiMax && self.mode != Mode::Dfn && self.dfn.engaged {
            self.dfn.release();
        }
        let feed = (level == NoiseLevel::AiMax && self.dfn.loaded()) || self.mode == Mode::Dfn;
        if feed {
            self.dfn.run(io, &mut self.dfn_out);
        }
        if level == NoiseLevel::AiMax && self.dfn.overloaded() {
            ctl.fall_back();
            level = ctl.level();
        }
        let target = match level {
            NoiseLevel::Standard => Mode::Plain,
            NoiseLevel::AiLight => Mode::Rnn,
            NoiseLevel::AiMax if self.mode == Mode::Dfn || (feed && self.dfn.ready()) => Mode::Dfn,
            NoiseLevel::AiMax => Mode::Rnn,
        };
        let mut ran = Ran::default();
        if self.mode == Mode::Rnn || target == Mode::Rnn {
            if !self.rnn_on {
                // Prime with the previous frame (its output has the fade-in artifacts).
                self.state.process_frame(&mut self.scratch, &self.prev);
                self.rnn_on = true;
            }
            self.state.process_frame(&mut self.wet, io);
            ran.rnn = true;
        }
        self.prev.copy_from_slice(io);
        let old = self.mode;
        ran.dfn = old == Mode::Dfn || target == Mode::Dfn;
        if old == target {
            match target {
                Mode::Plain => {}
                Mode::Rnn => io.copy_from_slice(&self.wet),
                Mode::Dfn => io.copy_from_slice(&self.dfn_out),
            }
        } else {
            for (i, s) in io.iter_mut().enumerate() {
                let plain = *s;
                let pick = |m: Mode| match m {
                    Mode::Plain => plain,
                    Mode::Rnn => self.wet[i],
                    Mode::Dfn => self.dfn_out[i],
                };
                let (a, b) = fade(i);
                *s = pick(old) * a + pick(target) * b;
            }
            if old == Mode::Rnn {
                self.rnn_on = false;
                self.flush_rnn();
            }
            if old == Mode::Dfn {
                self.dfn.release();
            }
            self.mode = target;
        }
        if level != NoiseLevel::AiMax && self.dfn.engaged && self.mode != Mode::Dfn {
            self.dfn.release();
        }
        ran
    }

    /// The mic stopped (muted): drop the models' memory so nothing old is heard on unmute.
    fn reset(&mut self) {
        if self.rnn_on {
            self.flush_rnn();
        }
        if self.dfn.engaged {
            // A new epoch: the worker resets DeepFilterNet before the next frame.
            self.dfn.restart();
        }
        self.prev = [0.0; FRAME];
    }
}

/// The AI denoiser of one mic chain. Owned by the mic callback; feed it every post-APM chunk.
pub struct Denoiser {
    ctl: Arc<NoiseControl>,
    inner: FrameDenoiser,
    io: [f32; FRAME],
    /// Frame adapter for chunks that are not whole frames: input being filled, output being
    /// drained (one frame behind). Used only once a chunk of another size shows up.
    adapting: bool,
    in_buf: [f32; FRAME],
    in_len: usize,
    out_buf: [f32; FRAME],
    out_pos: usize,
    /// Frames RNNoise processed (for the CPU stats).
    pub frames: u64,
    /// Frames where DeepFilterNet's output was in the signal.
    pub dfn_frames: u64,
}

fn to_f32(src: &[i16], dst: &mut [f32]) {
    for (d, &s) in dst.iter_mut().zip(src) {
        *d = s as f32;
    }
}

fn to_i16(src: &[f32], dst: &mut [i16]) {
    for (d, &s) in dst.iter_mut().zip(src) {
        *d = s.clamp(i16::MIN as f32, i16::MAX as f32) as i16;
    }
}

impl Denoiser {
    /// Build it off the audio thread (it registers with `ctl`, which may start the
    /// DeepFilterNet worker).
    pub fn new(ctl: Arc<NoiseControl>) -> Self {
        let dfn = ctl.register_dfn();
        Self {
            ctl,
            inner: FrameDenoiser::new(dfn),
            io: [0.0; FRAME],
            adapting: false,
            in_buf: [0.0; FRAME],
            in_len: 0,
            out_buf: [0.0; FRAME],
            out_pos: FRAME,
            frames: 0,
            dfn_frames: 0,
        }
    }

    /// Whether an AI model is in the signal path right now.
    pub fn active(&self) -> bool {
        self.inner.mode != Mode::Plain
    }

    /// Whether DeepFilterNet is in the signal path right now.
    pub fn dfn_active(&self) -> bool {
        self.inner.mode == Mode::Dfn
    }

    /// Offline processing (evaluation, tests): wait for DeepFilterNet's model and for each of
    /// its frames instead of replacing late frames. Never set this on a live mic chain.
    pub fn set_offline(&mut self, offline: bool) {
        self.inner.dfn.wait = offline;
    }

    /// This chain's DeepFilterNet side (worker state, timing, test hooks).
    pub fn dfn(&self) -> &DfnShared {
        &self.inner.dfn.sh
    }

    /// Frames of hand-off to the DeepFilterNet worker (1 normally; grows on late frames).
    pub fn inner_depth(&self) -> u64 {
        self.inner.dfn.depth()
    }

    /// DeepFilterNet frames that were late and replaced by the unprocessed input.
    pub fn dfn_late(&self) -> u64 {
        self.inner.dfn.late
    }

    /// The mic stopped (muted).
    pub fn reset(&mut self) {
        self.inner.reset();
    }

    fn count(&mut self, ran: Ran) {
        self.frames += ran.rnn as u64;
        self.dfn_frames += ran.dfn as u64;
    }

    /// Denoise `chunk` in place (48 kHz mono). At [`NoiseLevel::Standard`] (and no transition
    /// pending) the chunk is left untouched.
    pub fn process(&mut self, chunk: &mut [i16]) {
        if !self.adapting && chunk.len().is_multiple_of(FRAME) {
            for part in chunk.as_chunks_mut::<FRAME>().0 {
                let bypass = self.ctl.level() == NoiseLevel::Standard && self.inner.mode == Mode::Plain && !self.inner.rnn_on;
                if bypass && !self.inner.dfn.engaged {
                    to_f32(part, &mut self.inner.prev); // bypass: the chunk is not touched
                    continue;
                }
                to_f32(part, &mut self.io);
                let ran = self.inner.frame(&mut self.io, &self.ctl);
                self.count(ran);
                to_i16(&self.io, part);
            }
            return;
        }
        // Mismatched sizes: everything goes through the adapter from now on (one frame later).
        if !self.adapting {
            self.adapting = true;
            self.out_buf = [0.0; FRAME];
            self.out_pos = 0;
        }
        for s in chunk.iter_mut() {
            self.in_buf[self.in_len] = *s as f32;
            self.in_len += 1;
            *s = self.out_buf[self.out_pos].clamp(i16::MIN as f32, i16::MAX as f32) as i16;
            self.out_pos += 1;
            if self.in_len == FRAME {
                let ran = self.inner.frame(&mut self.in_buf, &self.ctl);
                self.count(ran);
                self.out_buf.copy_from_slice(&self.in_buf);
                self.in_len = 0;
                self.out_pos = 0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Speech-like test signal: a 150 Hz buzz with harmonics, amplitude-modulated into syllables.
    fn voice(n: usize, offset: usize) -> Vec<i16> {
        (0..n)
            .map(|k| {
                let i = (k + offset) as f32;
                let t = i / 48_000.0;
                let env = (0.5 - 0.5 * (t * 2.0 * std::f32::consts::PI * 4.0).cos()).powi(2);
                let mut s = 0.0;
                for h in 1..=8 {
                    s += (t * 2.0 * std::f32::consts::PI * 150.0 * h as f32).sin() / h as f32;
                }
                (s * env * 6000.0) as i16
            })
            .collect()
    }

    fn noise(n: usize, seed: u32, amp: f32) -> Vec<i16> {
        let mut x = seed.max(1);
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                ((x as f32 / u32::MAX as f32 - 0.5) * 2.0 * amp) as i16
            })
            .collect()
    }

    fn rms(s: &[i16]) -> f32 {
        (s.iter().map(|&v| (v as f32).powi(2)).sum::<f32>() / s.len().max(1) as f32).sqrt()
    }

    #[test]
    fn levels_roundtrip_and_unknown_names_fall_back() {
        assert_eq!(NoiseLevel::default(), NoiseLevel::AiLight);
        for l in NoiseLevel::ALL {
            let j = serde_json::to_string(&l).unwrap();
            assert_eq!(serde_json::from_str::<NoiseLevel>(&j).unwrap(), l);
        }
        assert_eq!(serde_json::to_string(&NoiseLevel::Standard).unwrap(), "\"standard\"");
        assert_eq!(serde_json::to_string(&NoiseLevel::AiMax).unwrap(), "\"ai_max\"");
        assert_eq!(serde_json::from_str::<NoiseLevel>("\"ai_ultra\"").unwrap(), NoiseLevel::AiLight);
        assert!(NoiseLevel::Standard.apm_ns() && !NoiseLevel::AiLight.apm_ns() && !NoiseLevel::AiMax.apm_ns());
        assert_eq!(NoiseLevel::AiMax.label(), "IA máxima");
    }

    #[test]
    fn standard_is_a_bit_exact_bypass() {
        let ctl = Arc::new(NoiseControl::new(NoiseLevel::Standard));
        let mut d = Denoiser::new(ctl);
        let input: Vec<i16> = voice(FRAME * 20, 0).iter().zip(noise(FRAME * 20, 7, 3000.0)).map(|(a, b)| a.saturating_add(b)).collect();
        let mut out = input.clone();
        for c in out.chunks_mut(FRAME) {
            d.process(c);
        }
        assert_eq!(out, input);
        assert_eq!(d.frames, 0, "RNNoise never ran");
        assert!(!d.active());
    }

    #[test]
    fn ai_light_removes_steady_noise_and_keeps_the_voice() {
        let ctl = Arc::new(NoiseControl::new(NoiseLevel::AiLight));
        let mut d = Denoiser::new(ctl);
        // 2 s of noise only: RNNoise should take it down a lot once it has settled.
        let mut n = noise(FRAME * 200, 3, 2000.0);
        for c in n.chunks_mut(FRAME) {
            d.process(c);
        }
        let before = rms(&noise(FRAME * 100, 3, 2000.0)[FRAME * 50..]);
        let after = rms(&n[FRAME * 100..]);
        let reduction_db = 20.0 * (before / after.max(1e-3)).log10();
        assert!(reduction_db > 10.0, "noise reduced by {reduction_db:.1} dB");
        assert_eq!(d.frames, 200);
        assert!(d.active());
    }

    #[test]
    fn switching_levels_is_glitch_free() {
        let ctl = Arc::new(NoiseControl::new(NoiseLevel::Standard));
        let mut d = Denoiser::new(ctl.clone());
        let sig = voice(FRAME * 60, 0);
        let mut out = sig.clone();
        for (k, c) in out.chunks_mut(FRAME).enumerate() {
            match k {
                20 => ctl.set_level(NoiseLevel::AiLight),
                40 => ctl.set_level(NoiseLevel::Standard),
                _ => {}
            }
            d.process(c);
        }
        assert!(!d.active());
        // RNNoise ran from chunk 20 through the fade-out chunk 40.
        assert_eq!(d.frames, 21);
        // No click: sample-to-sample steps stay in the range of the signal's own steps.
        let max_step = |s: &[i16]| s.windows(2).map(|w| (w[1] as i32 - w[0] as i32).abs()).max().unwrap();
        let natural = max_step(&sig);
        assert!(max_step(&out) <= natural * 2, "step {} vs natural {natural}", max_step(&out));
        // After switching back, the output is the plain signal again, bit for bit.
        assert_eq!(&out[FRAME * 41..], &sig[FRAME * 41..]);
    }

    #[test]
    fn muting_flushes_old_audio() {
        let ctl = Arc::new(NoiseControl::new(NoiseLevel::AiLight));
        let mut d = Denoiser::new(ctl);
        let mut loud = voice(FRAME * 30, 0);
        for c in loud.chunks_mut(FRAME) {
            d.process(c);
        }
        d.reset();
        // Silence in after unmute: no old speech comes out, only the decaying tail of RNNoise's
        // input high-pass filter (below -50 dBFS, against a voice peaking near -10 dBFS).
        let mut quiet = vec![0i16; FRAME];
        d.process(&mut quiet);
        let peak = quiet.iter().map(|s| s.unsigned_abs()).max().unwrap();
        assert!(peak < 100, "stale audio after unmute: peak {peak}");
    }

    #[test]
    fn odd_chunk_sizes_are_adapted_with_one_frame_of_delay() {
        let sig: Vec<i16> = voice(FRAME * 30, 0).iter().zip(noise(FRAME * 30, 11, 1500.0)).map(|(a, b)| a.saturating_add(b)).collect();
        // Reference: whole frames.
        let mut want = sig.clone();
        let mut d = Denoiser::new(Arc::new(NoiseControl::new(NoiseLevel::AiLight)));
        for c in want.chunks_mut(FRAME) {
            d.process(c);
        }
        // 441-sample chunks (a 44.1 kHz device's 10 ms) through the adapter.
        let mut got = sig.clone();
        let mut d = Denoiser::new(Arc::new(NoiseControl::new(NoiseLevel::AiLight)));
        for c in got.chunks_mut(441) {
            d.process(c);
        }
        assert!(got[..FRAME].iter().all(|&s| s == 0), "the first frame is the adapter's delay");
        let n = sig.len() - FRAME;
        assert_eq!(&got[FRAME..], &want[..n]);
    }

    // ---------------------------------------------------------------- IA máxima (DeepFilterNet)

    fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
        let t = std::time::Instant::now();
        while !f() {
            assert!(t.elapsed() < std::time::Duration::from_secs(20), "timed out waiting for {what}");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    fn noisy_voice(frames: usize) -> Vec<i16> {
        voice(FRAME * frames, 0).iter().zip(noise(FRAME * frames, 5, 2500.0)).map(|(a, b)| a.saturating_add(b)).collect()
    }

    #[test]
    fn ai_max_loads_lazily_and_unloads_when_switching_away() {
        let ctl = Arc::new(NoiseControl::new(NoiseLevel::AiLight));
        let mut d = Denoiser::new(ctl.clone());
        d.set_offline(true);
        let mut sig = noisy_voice(200);
        let mut chunks = sig.chunks_mut(FRAME);
        for c in chunks.by_ref().take(20) {
            d.process(c);
        }
        // Other levels never start the worker nor load the model.
        assert_eq!(d.dfn().worker_starts(), 0);
        assert!(!d.dfn().worker_running() && !d.dfn().loaded());
        ctl.set_level(NoiseLevel::AiMax);
        assert_eq!(d.dfn().worker_starts(), 1);
        for c in chunks.by_ref().take(60) {
            d.process(c);
        }
        assert!(d.dfn().loaded() && d.dfn_active(), "DeepFilterNet is in the signal");
        assert_eq!(ctl.ai_max_loaded(), 1);
        assert!(d.dfn_frames > 40, "{}", d.dfn_frames);
        // Away from IA máxima: faded out on the next chunk, the worker exits and frees the model.
        ctl.set_level(NoiseLevel::AiLight);
        for c in chunks.by_ref().take(5) {
            d.process(c);
        }
        assert!(!d.dfn_active() && d.active());
        wait_until("the worker to exit", || !d.dfn().worker_running());
        assert!(!d.dfn().loaded());
        assert_eq!(ctl.ai_max_loaded(), 0);
        // Back again: a new worker, a fresh model.
        ctl.set_level(NoiseLevel::AiMax);
        for c in chunks.by_ref().take(30) {
            d.process(c);
        }
        assert!(d.dfn_active());
        assert_eq!(d.dfn().worker_starts(), 2);
        // Dropping the chain stops the worker too.
        let sh = Arc::downgrade(&d.inner.dfn.sh);
        drop(d);
        wait_until("the worker of a dropped chain to exit", || sh.upgrade().is_none_or(|s| !s.worker_running()));
    }

    #[test]
    fn switching_across_all_three_levels_is_glitch_free() {
        let ctl = Arc::new(NoiseControl::new(NoiseLevel::Standard));
        let mut d = Denoiser::new(ctl.clone());
        d.set_offline(true);
        let sig = voice(FRAME * 140, 0);
        let mut out = sig.clone();
        let mut modes = Vec::new();
        for (k, c) in out.chunks_mut(FRAME).enumerate() {
            match k {
                20 => ctl.set_level(NoiseLevel::AiMax),
                50 => ctl.set_level(NoiseLevel::AiLight),
                70 => ctl.set_level(NoiseLevel::AiMax),
                100 => ctl.set_level(NoiseLevel::Standard),
                _ => {}
            }
            d.process(c);
            modes.push(d.inner.mode);
        }
        // Padrão -> RNNoise at once (while DeepFilterNet loads and warms up) -> DeepFilterNet.
        assert_eq!(modes[19], Mode::Plain);
        assert_eq!(modes[20], Mode::Rnn);
        assert_eq!(modes[45], Mode::Dfn);
        assert_eq!(modes[50], Mode::Rnn);
        assert_eq!(modes[95], Mode::Dfn);
        assert_eq!(modes[100], Mode::Plain);
        // No click at any switch: steps stay in the range of the signal's own steps.
        let max_step = |s: &[i16]| s.windows(2).map(|w| (w[1] as i32 - w[0] as i32).abs()).max().unwrap();
        let natural = max_step(&sig);
        assert!(max_step(&out) <= natural * 2, "step {} vs natural {natural}", max_step(&out));
        // Back at Padrão: the plain signal, bit for bit.
        assert_eq!(&out[FRAME * 101..], &sig[FRAME * 101..]);
        assert!(!d.inner.dfn.engaged && !d.inner.rnn_on);
    }

    #[test]
    fn ai_max_muting_flushes_old_audio() {
        let ctl = Arc::new(NoiseControl::new(NoiseLevel::AiMax));
        let mut d = Denoiser::new(ctl);
        d.set_offline(true);
        let mut loud = noisy_voice(60);
        for c in loud.chunks_mut(FRAME) {
            d.process(c);
        }
        assert!(d.dfn_active());
        d.reset();
        // Silence in after unmute: nothing of the old speech comes out of the model's buffers.
        for _ in 0..6 {
            let mut quiet = vec![0i16; FRAME];
            d.process(&mut quiet);
            let peak = quiet.iter().map(|s| s.unsigned_abs()).max().unwrap();
            assert!(peak < 100, "stale audio after unmute: peak {peak}");
        }
        assert!(d.dfn_active());
    }

    #[test]
    fn ai_max_odd_chunk_sizes_are_adapted_with_one_frame_of_delay() {
        let sig = noisy_voice(60);
        let run = |size: usize| {
            let mut d = Denoiser::new(Arc::new(NoiseControl::new(NoiseLevel::AiMax)));
            d.set_offline(true);
            let mut out = sig.clone();
            for c in out.chunks_mut(size) {
                d.process(c);
            }
            assert!(d.dfn_frames > 30);
            out
        };
        let want = run(FRAME);
        let got = run(441);
        assert!(got[..FRAME].iter().all(|&s| s == 0), "the first frame is the adapter's delay");
        let n = sig.len() - FRAME;
        assert_eq!(&got[FRAME..], &want[..n]);
    }

    #[test]
    fn ai_max_falls_back_to_ai_light_on_a_slow_machine() {
        let ctl = Arc::new(NoiseControl::new(NoiseLevel::AiMax));
        let mut d = Denoiser::new(ctl.clone());
        d.set_offline(true);
        // Twice the budget per frame: far from real time.
        d.dfn().set_slowdown(std::time::Duration::from_nanos(2 * super::super::dfn::BUDGET_NS as u64));
        let mut sig = noisy_voice(150);
        let mut fell_at = None;
        for (k, c) in sig.chunks_mut(FRAME).enumerate() {
            d.process(c);
            if fell_at.is_none() && ctl.level() == NoiseLevel::AiLight {
                fell_at = Some(k);
            }
        }
        let k = fell_at.expect("fell back to IA leve");
        assert!(k < 100, "fell back after {k} frames");
        assert!(ctl.take_fallback_notice(), "the UI is told once");
        assert!(!ctl.take_fallback_notice());
        assert!(!d.dfn_active() && d.active(), "RNNoise took over");
        wait_until("the worker to exit", || !d.dfn().worker_running());
        // Choosing it again explicitly is allowed (and starts over).
        d.dfn().set_slowdown(std::time::Duration::ZERO);
        ctl.set_level(NoiseLevel::AiMax);
        let mut more = noisy_voice(30);
        for c in more.chunks_mut(FRAME) {
            d.process(c);
        }
        assert_eq!(ctl.level(), NoiseLevel::AiMax);
        assert!(d.dfn_active());
    }

    #[test]
    fn ai_max_late_frames_are_replaced_and_trigger_the_fallback() {
        // Real time (no waiting) with a worker slower than real time: late frames are replaced
        // by the time-aligned input and, sustained, the level falls back.
        let ctl = Arc::new(NoiseControl::new(NoiseLevel::AiMax));
        let mut d = Denoiser::new(ctl.clone());
        wait_until("the model", || d.dfn().loaded());
        d.dfn().set_slowdown(std::time::Duration::from_millis(30));
        let mut sig = noisy_voice(400);
        for c in sig.chunks_mut(FRAME) {
            d.process(c);
            std::thread::sleep(std::time::Duration::from_millis(2));
            if ctl.level() == NoiseLevel::AiLight {
                break;
            }
        }
        assert!(d.dfn_late() > 0);
        assert_eq!(ctl.level(), NoiseLevel::AiLight);
        assert!(ctl.take_fallback_notice());
    }
}
