//! "IA máxima": DeepFilterNet3 (the vendored libDF `deep_filter` crate, inference through
//! `tract`, model embedded in the executable) on a worker thread of its own.
//!
//! The network costs 0.5 to 0.9 ms per 10 ms frame (5 to 9% of one core of an 8-core desktop CPU) and
//! its inference allocates, so it never runs in the mic callback. Instead the callback pushes each
//! 480-sample frame into a fixed-capacity single-producer/single-consumer ring, wakes the worker
//! and takes the frame the worker finished one frame earlier from a second ring: one frame (10 ms)
//! of hand-off on top of the model's own 30 ms (20 ms STFT window + 2 frames of look-ahead), so
//! 40 ms in all. A frame that is not back in time (a scheduling hiccup, a slow machine) is
//! replaced by the unprocessed input, time-aligned, and the hand-off grows by one frame (up to
//! [`MAX_DEPTH`]). Too many late frames, or a worker that needs more than [`BUDGET_NS`] per
//! frame, and the chain falls back to "IA leve" (see [`DfnClient::overloaded`]).
//!
//! The model is loaded lazily: the worker thread is started (by [`super::NoiseControl`]) only
//! while "IA máxima" is selected and a mic chain exists, loads the model (about 0.3 s, during
//! which the chain keeps running RNNoise) and exits, freeing it, as soon as another level is
//! selected or the chain is dropped. Users of the other levels pay nothing but the two rings.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::Thread;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tracing::{info, warn};

use super::denoise::FRAME;

/// Frames of algorithmic delay of DeepFilterNet3 at 48 kHz: the STFT overlap (960 - 480 samples)
/// plus two frames of look-ahead. Used to time-align the unprocessed fallback frame.
pub const MODEL_DELAY_FRAMES: u64 = 3;
/// Most frames of hand-off between the callback and the worker (10 ms each).
pub const MAX_DEPTH: u64 = 3;
/// Frames the worker must have processed before its output is faded in.
const WARM_FRAMES: u64 = 6;
/// Worker time per frame above which the machine is "too slow" (half of real time).
pub const BUDGET_NS: u32 = 5_000_000;
/// Late frames per [`LATE_WINDOW`] frames above which the chain falls back.
const LATE_LIMIT: u32 = 15;
const LATE_WINDOW: u32 = 300;
/// Frames after (re)starting during which the timing is not judged (model warm-up).
const GRACE_FRAMES: u64 = 50;
/// Ring capacity in frames (both directions).
const RING: usize = 16;
/// Recent input frames kept by the callback for the time-aligned fallback.
const DRY: usize = 8;
const _: () = assert!(DRY as u64 > MAX_DEPTH + MODEL_DELAY_FRAMES);

/// One 10 ms frame in flight.
struct Block {
    seq: u64,
    epoch: u32,
    data: [f32; FRAME],
}

/// Fixed-capacity single-producer/single-consumer ring of frames. Lock- and allocation-free.
///
/// Discipline (what makes the `unsafe` sound): exactly one thread pushes and one thread reads at
/// any time. The input ring is pushed by the mic callback (the [`DfnClient`]'s owner) and read by
/// the worker; the output ring the other way round. At most one worker exists per ring pair:
/// [`DfnShared::ensure_worker`] starts a new one only after the previous one has marked itself
/// gone, which it does after its last ring access.
struct BlockRing {
    slots: Box<[UnsafeCell<Block>]>,
    /// Blocks written / read so far (monotonic; the slot is the count modulo the capacity).
    written: AtomicUsize,
    read: AtomicUsize,
}

// SAFETY: see the discipline above; a slot is only written by the producer while it is free
// (`written - read < capacity`) and only read by the consumer while it is filled, with the
// Release/Acquire pairs on the counters ordering the slot accesses.
unsafe impl Sync for BlockRing {}

impl BlockRing {
    fn new() -> Self {
        let slots = (0..RING).map(|_| UnsafeCell::new(Block { seq: 0, epoch: 0, data: [0.0; FRAME] })).collect();
        Self { slots, written: AtomicUsize::new(0), read: AtomicUsize::new(0) }
    }

    /// Producer: append a frame (scaled by `gain`). False when the ring is full.
    fn push(&self, seq: u64, epoch: u32, data: &[f32; FRAME], gain: f32) -> bool {
        let w = self.written.load(Ordering::Relaxed);
        if w - self.read.load(Ordering::Acquire) >= RING {
            return false;
        }
        // SAFETY: the slot is free (checked above) and only this producer writes it.
        let slot = unsafe { &mut *self.slots[w % RING].get() };
        slot.seq = seq;
        slot.epoch = epoch;
        for (d, s) in slot.data.iter_mut().zip(data) {
            *d = s * gain;
        }
        self.written.store(w + 1, Ordering::Release);
        true
    }

    /// Consumer: the oldest frame's `(seq, epoch)`.
    fn peek(&self) -> Option<(u64, u32)> {
        let r = self.read.load(Ordering::Relaxed);
        if r == self.written.load(Ordering::Acquire) {
            return None;
        }
        // SAFETY: the slot is filled (r < written) and the producer does not touch it until
        // `read` moves past it.
        let slot = unsafe { &*self.slots[r % RING].get() };
        Some((slot.seq, slot.epoch))
    }

    /// Consumer: copy the oldest frame out (scaled by `gain`) and drop it. Call after `peek`.
    fn pop_into(&self, out: &mut [f32; FRAME], gain: f32) {
        let r = self.read.load(Ordering::Relaxed);
        debug_assert!(r < self.written.load(Ordering::Acquire));
        // SAFETY: as in `peek`.
        let slot = unsafe { &*self.slots[r % RING].get() };
        for (o, s) in out.iter_mut().zip(&slot.data) {
            *o = s * gain;
        }
        self.read.store(r + 1, Ordering::Release);
    }

    /// Consumer: drop the oldest frame.
    fn skip(&self) {
        let r = self.read.load(Ordering::Relaxed);
        self.read.store(r + 1, Ordering::Release);
    }
}

/// What the callback and the worker share. Created with the mic chain (off the audio thread).
pub struct DfnShared {
    /// The selected level (shared with [`super::NoiseControl`]); the worker exits when it is not
    /// "IA máxima".
    level: Arc<AtomicU8>,
    level_max: u8,
    input: BlockRing,
    output: BlockRing,
    /// Bumped by the callback on every (re)start: the worker resets the model on a new epoch.
    epoch: AtomicU32,
    /// The model is loaded and the worker is serving frames.
    loaded: AtomicBool,
    /// The model could not be loaded (or the thread not started).
    failed: AtomicBool,
    /// The mic chain is gone.
    dropped: AtomicBool,
    /// A worker thread exists. The lock serialises starting one against the old one exiting.
    running: Mutex<bool>,
    thread: Mutex<Option<Thread>>,
    /// Smoothed worker time per frame (ns).
    frame_ns: AtomicU32,
    /// Workers started so far (tests).
    starts: AtomicU32,
    /// Test hook: extra time the worker spends per frame (µs), to simulate a slow machine.
    slowdown_us: AtomicU32,
}

impl DfnShared {
    pub(crate) fn new(level: Arc<AtomicU8>, level_max: u8) -> Arc<Self> {
        Arc::new(Self {
            level,
            level_max,
            input: BlockRing::new(),
            output: BlockRing::new(),
            epoch: AtomicU32::new(0),
            loaded: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            dropped: AtomicBool::new(false),
            running: Mutex::new(false),
            thread: Mutex::new(None),
            frame_ns: AtomicU32::new(0),
            starts: AtomicU32::new(0),
            slowdown_us: AtomicU32::new(0),
        })
    }

    /// Start the worker (which loads the model) unless one is running. Never call this from the
    /// audio callback: it spawns a thread.
    pub(crate) fn ensure_worker(self: &Arc<Self>) {
        let mut running = self.running.lock();
        if *running || self.dropped.load(Ordering::Acquire) {
            return;
        }
        self.failed.store(false, Ordering::Relaxed);
        self.frame_ns.store(0, Ordering::Relaxed);
        let me = self.clone();
        match std::thread::Builder::new().name("vox-dfn".into()).spawn(move || worker(me)) {
            Ok(_) => {
                *running = true;
                self.starts.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => {
                warn!(error = %e, "IA máxima: could not start the worker thread");
                self.failed.store(true, Ordering::Release);
            }
        }
    }

    /// A worker thread exists (loading, serving or about to exit).
    pub fn worker_running(&self) -> bool {
        *self.running.lock()
    }

    pub fn loaded(&self) -> bool {
        self.loaded.load(Ordering::Acquire)
    }

    /// The model could not be loaded (or its thread not started).
    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }

    pub fn worker_starts(&self) -> u32 {
        self.starts.load(Ordering::Relaxed)
    }

    /// Smoothed worker time per 10 ms frame.
    pub fn frame_time(&self) -> Duration {
        Duration::from_nanos(self.frame_ns.load(Ordering::Relaxed) as u64)
    }

    pub fn set_slowdown(&self, extra: Duration) {
        self.slowdown_us.store(extra.as_micros().min(u32::MAX as u128) as u32, Ordering::Relaxed);
    }

    /// Wake the worker. Allocation-free (callback side).
    fn wake(&self) {
        if let Some(t) = self.thread.try_lock() {
            if let Some(t) = t.as_ref() {
                t.unpark();
            }
        }
    }

    /// Let the worker re-check the level now (it exits when "IA máxima" is no longer selected).
    pub(crate) fn wake_to_exit(&self) {
        if let Some(t) = self.thread.lock().as_ref() {
            t.unpark();
        }
    }

    /// The chain is gone: the worker exits (freeing the model).
    pub(crate) fn drop_chain(&self) {
        self.dropped.store(true, Ordering::Release);
        if let Some(t) = self.thread.lock().as_ref() {
            t.unpark();
        }
    }
}

impl std::fmt::Debug for DfnShared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DfnShared").field("loaded", &self.loaded()).finish()
    }
}

#[cfg(windows)]
fn raise_priority() {
    use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST};
    // SAFETY: pseudo-handle of the calling thread.
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) };
}

#[cfg(not(windows))]
fn raise_priority() {}

fn worker(sh: Arc<DfnShared>) {
    use df::tract::{DfParams, DfTract, RuntimeParams, DEFAULT_MODEL};
    use df::ndarray::Array2;

    raise_priority();
    let me = std::thread::current();
    let my_id = me.id();
    *sh.thread.lock() = Some(me);
    let t0 = Instant::now();
    // The thresholds of upstream's `deep-filter` tool (the library defaults run the decoders less
    // often and, in our evaluation, damage clean speech: 20.8 dB SI-SDR against 41 dB).
    let params = RuntimeParams::default_with_ch(1).with_thresholds(-15.0, 35.0, 35.0);
    let model = DfParams::from_bytes(DEFAULT_MODEL).and_then(|p| DfTract::new(p, &params));
    let mut model = match model {
        Ok(m) => {
            info!(load_ms = t0.elapsed().as_millis() as u64, "IA máxima: DeepFilterNet3 loaded");
            Some(m)
        }
        Err(e) => {
            warn!(error = %e, "IA máxima: could not load DeepFilterNet3");
            sh.failed.store(true, Ordering::Release);
            None
        }
    };
    let mut noisy = Array2::<f32>::zeros((1, FRAME));
    let mut enh = Array2::<f32>::zeros((1, FRAME));
    let mut frame = [0.0f32; FRAME];
    let mut epoch = None;
    let mut errors = 0u32;
    if model.is_some() {
        sh.loaded.store(true, Ordering::Release);
    }
    loop {
        if let Some(m) = model.as_mut() {
            while let Some((seq, ep)) = sh.input.peek() {
                sh.input.pop_into(&mut frame, 1.0);
                let t = Instant::now();
                if epoch != Some(ep) {
                    epoch = Some(ep);
                    if let Err(e) = m.reset() {
                        warn!(error = %e, "IA máxima: model reset failed");
                    }
                }
                noisy.as_slice_mut().expect("contiguous").copy_from_slice(&frame);
                if let Err(e) = m.process(noisy.view(), enh.view_mut()) {
                    errors += 1;
                    if errors == 1 {
                        warn!(error = %e, "IA máxima: inference failed (passing the frame through)");
                    }
                    enh.assign(&noisy);
                }
                let extra = sh.slowdown_us.load(Ordering::Relaxed);
                if extra > 0 {
                    std::thread::sleep(Duration::from_micros(extra as u64));
                }
                let ns = t.elapsed().as_nanos().min(u32::MAX as u128) as u32;
                let avg = sh.frame_ns.load(Ordering::Relaxed);
                let avg = if avg == 0 { ns } else { (avg as u64 * 19 / 20 + ns as u64 / 20) as u32 };
                sh.frame_ns.store(avg, Ordering::Relaxed);
                frame.copy_from_slice(enh.as_slice().expect("contiguous"));
                // A full output ring means the callback stopped taking frames: drop this one.
                let _ = sh.output.push(seq, ep, &frame, 1.0);
            }
        }
        {
            let mut running = sh.running.lock();
            let selected = sh.level.load(Ordering::Relaxed) == sh.level_max;
            if model.is_none() || sh.dropped.load(Ordering::Acquire) || !selected {
                sh.loaded.store(false, Ordering::Release);
                *running = false;
                break;
            }
        }
        std::thread::park_timeout(Duration::from_millis(50));
    }
    {
        let mut t = sh.thread.lock();
        if t.as_ref().is_some_and(|t| t.id() == my_id) {
            *t = None;
        }
    }
    drop(model);
    info!("IA máxima: DeepFilterNet3 unloaded");
}

/// The callback side of "IA máxima" for one mic chain. Allocation-free.
pub(crate) struct DfnClient {
    pub(crate) sh: Arc<DfnShared>,
    /// Frames are being fed to the worker.
    pub(crate) engaged: bool,
    epoch: u32,
    /// Next frame number to push.
    seq: u64,
    /// Hand-off in frames.
    depth: u64,
    dry: [[f32; FRAME]; DRY],
    /// The last output was the unprocessed fallback.
    last_dry: bool,
    last_wet: bool,
    win_frames: u32,
    win_late: u32,
    overload: bool,
    /// Block until the worker has each frame (offline processing and tests).
    pub(crate) wait: bool,
    /// Frames that were late (replaced by the unprocessed input).
    pub(crate) late: u64,
}

impl DfnClient {
    pub(crate) fn new(sh: Arc<DfnShared>) -> Self {
        Self {
            sh,
            engaged: false,
            epoch: 0,
            seq: 0,
            depth: 1,
            dry: [[0.0; FRAME]; DRY],
            last_dry: false,
            last_wet: false,
            win_frames: 0,
            win_late: 0,
            overload: false,
            wait: false,
            late: 0,
        }
    }

    pub(crate) fn loaded(&self) -> bool {
        self.sh.loaded()
    }

    pub(crate) fn depth(&self) -> u64 {
        self.depth
    }

    /// Offline: wait (at most 10 s) for the model to load.
    pub(crate) fn wait_loaded(&self) {
        let t = Instant::now();
        while !self.sh.loaded() && !self.sh.failed.load(Ordering::Acquire) && t.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Start feeding from scratch: new epoch (the worker resets the model), empty hand-off.
    pub(crate) fn restart(&mut self) {
        self.epoch = self.sh.epoch.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        self.engaged = true;
        self.seq = 0;
        self.depth = 1;
        self.last_dry = false;
        self.last_wet = false;
        self.win_frames = 0;
        self.win_late = 0;
        self.overload = false;
    }

    /// Stop feeding (the worker's pending output is discarded on the next start).
    pub(crate) fn release(&mut self) {
        self.engaged = false;
        self.last_wet = false;
    }

    /// Its output has settled and can be faded in.
    pub(crate) fn ready(&self) -> bool {
        self.engaged && self.seq >= WARM_FRAMES + self.depth && self.last_wet
    }

    /// The machine cannot keep up (or the model failed): fall back to "IA leve".
    pub(crate) fn overloaded(&self) -> bool {
        self.sh.failed.load(Ordering::Acquire)
            || self.overload
            || (self.engaged && self.seq > GRACE_FRAMES && self.sh.frame_ns.load(Ordering::Relaxed) > BUDGET_NS)
    }

    /// Feed one frame (i16-range samples) and get the denoised frame from `depth` frames ago
    /// (the time-aligned input if it is late). Returns whether `out` is the model's output.
    pub(crate) fn run(&mut self, io: &[f32; FRAME], out: &mut [f32; FRAME]) -> bool {
        if !self.engaged {
            self.restart();
        }
        let s = self.seq;
        self.dry[(s % DRY as u64) as usize] = *io;
        let _ = self.sh.input.push(s, self.epoch, io, 1.0 / 32768.0);
        self.seq += 1;
        self.sh.wake();
        if s < self.depth {
            // Start-up: nothing can be back yet.
            out.fill(0.0);
            self.last_wet = false;
            return false;
        }
        let target = s - self.depth;
        if self.wait {
            let t = Instant::now();
            while !self.has(target) && t.elapsed() < Duration::from_secs(5) && !self.sh.failed.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
        }
        // Drop what is stale (an old epoch, or frames already replaced by the fallback).
        while let Some((seq, ep)) = self.sh.output.peek() {
            if ep != self.epoch || seq < target {
                self.sh.output.skip();
            } else {
                break;
            }
        }
        let wet = matches!(self.sh.output.peek(), Some((seq, _)) if seq == target);
        if wet {
            self.sh.output.pop_into(out, 32768.0);
            if self.last_dry {
                // Back from the fallback: fade the aligned input into the model output.
                let dry = self.aligned(target);
                for (i, o) in out.iter_mut().enumerate() {
                    let (a, b) = super::denoise::fade(i);
                    *o = dry[i] * a + *o * b;
                }
            }
            self.last_dry = false;
        } else {
            *out = *self.aligned(target);
            self.last_dry = true;
            self.late += 1;
            self.win_late += 1;
            if self.depth < MAX_DEPTH {
                // Give the worker one more frame from now on (the next output repeats this
                // frame's time slot, processed).
                self.depth += 1;
            }
        }
        self.last_wet = wet;
        if s > GRACE_FRAMES {
            self.win_frames += 1;
            if self.win_frames >= LATE_WINDOW {
                if self.win_late > LATE_LIMIT {
                    self.overload = true;
                }
                self.win_frames = 0;
                self.win_late = 0;
            }
        } else if s == GRACE_FRAMES {
            self.win_late = 0;
        }
        wet
    }

    fn has(&self, target: u64) -> bool {
        // Only `peek` (the consumer side): is a frame of this epoch at or past `target` queued?
        // Stale frames in front are skipped by the caller right after.
        let mut found = false;
        while let Some((seq, ep)) = self.sh.output.peek() {
            if ep != self.epoch || seq < target {
                self.sh.output.skip();
                continue;
            }
            found = true;
            break;
        }
        found
    }

    /// The input frame that lines up with the model's output for frame `target`.
    fn aligned(&self, target: u64) -> &[f32; FRAME] {
        const ZERO: [f32; FRAME] = [0.0; FRAME];
        match target.checked_sub(MODEL_DELAY_FRAMES) {
            Some(t) => &self.dry[(t % DRY as u64) as usize],
            None => &ZERO,
        }
    }
}

impl Drop for DfnClient {
    fn drop(&mut self) {
        self.sh.drop_chain();
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_is_a_bounded_fifo() {
        let r = BlockRing::new();
        let mut f = [0.0f32; FRAME];
        for k in 0..RING as u64 {
            f[0] = k as f32;
            assert!(r.push(k, 7, &f, 2.0));
        }
        assert!(!r.push(99, 7, &f, 1.0), "full");
        let mut out = [0.0f32; FRAME];
        for k in 0..RING as u64 {
            assert_eq!(r.peek(), Some((k, 7)));
            r.pop_into(&mut out, 0.5);
            assert_eq!(out[0], k as f32, "scaled in and out");
        }
        assert_eq!(r.peek(), None);
        // Wraps around.
        assert!(r.push(100, 8, &f, 1.0));
        assert_eq!(r.peek(), Some((100, 8)));
        r.skip();
        assert_eq!(r.peek(), None);
    }

    /// Real-time behaviour of "IA máxima" on this machine: frames fed every 10 ms like a mic,
    /// with the CPU, lateness, latency and memory it costs. Run by hand:
    /// `cargo test -p vox-client-core --release --lib ai_max_realtime -- --ignored --nocapture`.
    #[test]
    #[ignore = "benchmark (10 s, real time)"]
    fn ai_max_realtime() {
        use crate::audio::denoise::Denoiser;
        use crate::audio::{NoiseControl, NoiseLevel};
        let mem = || crate::stats::process_memory().1 as f64 / 1048576.0;
        let ctl = Arc::new(NoiseControl::new(NoiseLevel::AiLight));
        let mut d = Denoiser::new(ctl.clone());
        let base = mem();
        ctl.set_level(NoiseLevel::AiMax);
        let t = Instant::now();
        while !d.dfn().loaded() {
            std::thread::sleep(Duration::from_millis(1));
        }
        println!("model load: {:?}, private memory +{:.1} MB", t.elapsed(), mem() - base);
        let mut x = 1u32;
        let mut chunk = [0i16; FRAME];
        let mut cb = Duration::ZERO;
        let cpu0 = crate::stats::process_cpu_100ns();
        let start = Instant::now();
        let frames = 1000u32;
        for k in 0..frames {
            for (i, s) in chunk.iter_mut().enumerate() {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                let t = (k as usize * FRAME + i) as f32 / 48_000.0;
                let env = (0.5 - 0.5 * (t * 2.0 * std::f32::consts::PI * 3.0).cos()).powi(2);
                *s = ((t * 2.0 * std::f32::consts::PI * 180.0).sin() * 6000.0 * env + (x as f32 / u32::MAX as f32 - 0.5) * 2000.0) as i16;
            }
            let t = Instant::now();
            d.process(&mut chunk);
            cb += t.elapsed();
            // Pace like a 10 ms mic callback.
            let due = start + Duration::from_millis(10 * (k as u64 + 1));
            while Instant::now() < due {
                std::thread::sleep(Duration::from_micros(500));
            }
        }
        let cpu = (crate::stats::process_cpu_100ns() - cpu0) as f64 * 1e-7;
        println!(
            "worker: {:?} per frame, wall ({:.1}% of a core); process CPU {:.1}% of a core; callback: {:?} per chunk",
            d.dfn().frame_time(),
            d.dfn().frame_time().as_secs_f64() / 0.010 * 100.0,
            cpu / start.elapsed().as_secs_f64() * 100.0,
            cb / frames
        );
        println!(
            "late frames: {} of {frames}, hand-off {} frame(s): latency {} ms (model 30 ms + hand-off)",
            d.dfn_late(),
            d.inner_depth(),
            (MODEL_DELAY_FRAMES + d.inner_depth()) * 10
        );
        println!("in use: private memory +{:.1} MB, level still {:?}", mem() - base, ctl.level());
        ctl.set_level(NoiseLevel::AiLight);
        d.process(&mut chunk);
        while d.dfn().worker_running() {
            std::thread::sleep(Duration::from_millis(1));
        }
        crate::stats::trim_memory(false);
        println!("after switching away: private memory +{:.1} MB", mem() - base);
    }
}
