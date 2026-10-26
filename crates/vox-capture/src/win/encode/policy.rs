//! Frame-path policies of the hardware-encoder sink, kept free of Windows types so they can be
//! unit tested: input-rate compensation of the encoder bitrate, the fps-first degradation policy
//! and the keyframe-request gate. Everything here is allocation-free and O(1) per frame.

use crate::DegradationPreference;

// ---------------------------------------------------------------------------------------------
// Input-rate compensation
// ---------------------------------------------------------------------------------------------

/// Hardware encoders budget bits per *frame* from their configured frame rate (CBR, ~1 frame of
/// VBV): `bitrate / nominal_fps`. Window capture only delivers frames on change (the v0.1 field
/// log showed 11-15 fps into a 30 fps encoder), and the fps-first degradation lowers the rate on
/// purpose, so the encoder spent only a fraction of the target and starved every frame. This
/// tracks the real input rate and scales the bitrate handed to the encoder so each frame gets
/// `target / real_fps`.
pub(crate) struct RateAdapter {
    nominal_fps: u32,
    last_ts_us: Option<i64>,
    /// Smoothed frame interval (µs); 0 = no estimate yet (assume nominal).
    interval_us: f64,
}

/// Never budget a frame more than this many nominal frames' worth (bounds the burst when motion
/// resumes after a static period, before the estimate catches up).
pub(crate) const MAX_RATE_COMPENSATION: f64 = 3.0;

impl RateAdapter {
    pub fn new(nominal_fps: u32) -> Self {
        Self {
            nominal_fps: nominal_fps.max(1),
            last_ts_us: None,
            interval_us: 0.0,
        }
    }

    /// Account one submitted frame (capture timestamp, µs).
    pub fn on_frame(&mut self, ts_us: i64) {
        if let Some(last) = self.last_ts_us {
            let dt = (ts_us - last).clamp(1_000, 1_000_000) as f64;
            if self.interval_us <= 0.0 {
                self.interval_us = dt;
            } else if dt < self.interval_us {
                // Rate going up: follow fast, so a burst after a pause isn't over-budgeted for long.
                self.interval_us += 0.5 * (dt - self.interval_us);
            } else {
                // Rate going down (or a pause): follow slowly.
                self.interval_us += 0.1 * (dt - self.interval_us);
            }
        }
        self.last_ts_us = Some(ts_us);
    }

    /// The pacer now delivers at most `fps`: jump the estimate there instead of waiting for the
    /// slow decay, so the per-frame budget follows a degradation step at once.
    pub fn limit_fps(&mut self, fps: u32) {
        let min_interval = 1e6 / fps.max(1) as f64;
        if self.interval_us > 0.0 && self.interval_us < min_interval {
            self.interval_us = min_interval;
        }
    }

    /// Current input rate estimate, clamped to `[nominal / MAX_RATE_COMPENSATION, nominal]`.
    pub fn effective_fps(&self) -> f64 {
        let nominal = self.nominal_fps as f64;
        if self.interval_us <= 0.0 {
            return nominal;
        }
        (1e6 / self.interval_us).clamp(nominal / MAX_RATE_COMPENSATION, nominal)
    }

    /// Bitrate to configure on an encoder that runs at `nominal_fps` so that frames arriving at
    /// the measured rate average `target_bps`.
    pub fn encoder_bitrate(&self, target_bps: u32) -> u32 {
        let k = self.nominal_fps as f64 / self.effective_fps();
        (target_bps as f64 * k).min(u32::MAX as f64) as u32
    }
}

/// Reconfigure the encoder only for changes of at least ~8% (NVENC reconfigure is not free).
pub(crate) fn bitrate_change_significant(current: u32, new: u32) -> bool {
    current == 0 || (new.abs_diff(current) as u64) * 100 >= current as u64 * 8
}

// ---------------------------------------------------------------------------------------------
// fps-first degradation
// ---------------------------------------------------------------------------------------------

/// Bits per pixel per frame below which "Priorizar nitidez" drops the frame rate a step.
/// Chosen from `docs/quality-bench.md`: on the hard synthetic scenes, worst-frame SSIM falls off
/// a cliff below ~0.05 bpp at 1080p (H.264, low-latency CBR), while halving the frame rate
/// doubles the per-frame budget.
pub(crate) const DEGRADE_BPP: f64 = 0.05;
/// A step back up needs this much at the higher rate (hysteresis: 1.5x the degrade threshold).
pub(crate) const RESTORE_BPP: f64 = 0.075;
/// ... sustained for this long.
pub(crate) const RESTORE_HOLD_US: i64 = 3_000_000;
/// Rates the policy steps through (never above the configured rate, never below the last one).
const LADDER: [u32; 4] = [60, 30, 20, 15];

pub(crate) struct FpsPolicy {
    pref: DegradationPreference,
    nominal: u32,
    cap: u32,
    up_since_us: Option<i64>,
}

impl FpsPolicy {
    pub fn new(pref: DegradationPreference, nominal_fps: u32) -> Self {
        let nominal = nominal_fps.max(1);
        Self {
            pref,
            nominal,
            cap: nominal,
            up_since_us: None,
        }
    }

    pub fn cap(&self) -> u32 {
        self.cap
    }

    pub fn pref(&self) -> DegradationPreference {
        self.pref
    }

    /// Candidate rates, highest first: the configured rate, then the ladder below it.
    fn candidates(&self) -> impl Iterator<Item = u32> + '_ {
        std::iter::once(self.nominal).chain(LADDER.iter().copied().filter(move |&f| f < self.nominal))
    }

    fn bpp(target_bps: u32, w: u32, h: u32, fps: u32) -> f64 {
        let px = (w.max(1) as f64) * (h.max(1) as f64);
        target_bps as f64 / (px * fps.max(1) as f64)
    }

    /// Re-evaluate with the current network target and frame size. Returns the new fps cap.
    pub fn update(&mut self, target_bps: u32, w: u32, h: u32, now_us: i64) -> u32 {
        if self.pref == DegradationPreference::Smoothness {
            self.cap = self.nominal;
            self.up_since_us = None;
            return self.cap;
        }
        // Highest rate that still gets a decent per-frame budget (or the lowest one).
        let mut best = self.nominal;
        for f in self.candidates() {
            best = f;
            if Self::bpp(target_bps, w, h, f) >= DEGRADE_BPP {
                break;
            }
        }
        if best < self.cap {
            // Degrade at once: every frame at the old rate is already starved.
            self.cap = best;
            self.up_since_us = None;
        } else if best > self.cap {
            // One step up at a time, only with headroom, only after it held for a while.
            let next = self
                .candidates()
                .filter(|&f| f > self.cap)
                .last()
                .unwrap_or(self.nominal);
            if Self::bpp(target_bps, w, h, next) >= RESTORE_BPP {
                let since = *self.up_since_us.get_or_insert(now_us);
                if now_us - since >= RESTORE_HOLD_US {
                    self.cap = next;
                    self.up_since_us = None;
                }
            } else {
                self.up_since_us = None;
            }
        } else {
            self.up_since_us = None;
        }
        self.cap
    }
}

// ---------------------------------------------------------------------------------------------
// Keyframe requests
// ---------------------------------------------------------------------------------------------

/// Minimum spacing of requested IDRs. Several viewers joining (or a burst of PLIs after loss)
/// would otherwise make back-to-back IDRs: each one is a quality dip at a 1-frame VBV, and the
/// later ones are useless because the first one already restarted every decoder.
pub(crate) const MIN_IDR_INTERVAL_US: i64 = 250_000;

#[derive(Default)]
pub(crate) struct KeyframeGate {
    pending: bool,
    last_idr_us: Option<i64>,
    /// Requests folded into an earlier/later IDR (for stats).
    pub coalesced: u64,
}

impl KeyframeGate {
    pub fn request(&mut self) {
        if self.pending {
            self.coalesced += 1;
        }
        self.pending = true;
    }

    /// Should the frame at `now_us` be an IDR? (`forced`: resize/encoder restart, never delayed.)
    pub fn take(&mut self, now_us: i64, forced: bool) -> bool {
        let due = self.pending
            && self
                .last_idr_us
                .is_none_or(|t| now_us - t >= MIN_IDR_INTERVAL_US);
        if forced || due {
            if forced && self.pending && !due {
                self.coalesced += 1;
            }
            self.pending = false;
            self.last_idr_us = Some(now_us);
            return true;
        }
        false
    }

    /// The encoder produced an IDR on its own (first frame): requests before it are satisfied.
    pub fn saw_idr(&mut self, now_us: i64) {
        self.last_idr_us = Some(now_us);
    }

    /// A requested IDR not sent yet.
    pub fn pending(&self) -> bool {
        self.pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_adapter_compensates_slow_input() {
        let mut r = RateAdapter::new(30);
        assert_eq!(r.encoder_bitrate(4_000_000), 4_000_000); // no estimate yet
        let mut ts = 0;
        for _ in 0..60 {
            r.on_frame(ts);
            ts += 83_333; // 12 fps
        }
        let fps = r.effective_fps();
        assert!((11.5..12.5).contains(&fps), "{fps}");
        let b = r.encoder_bitrate(4_000_000);
        assert!((9_600_000..10_400_000).contains(&b), "{b}");
    }

    #[test]
    fn rate_adapter_is_bounded_and_attacks_fast() {
        let mut r = RateAdapter::new(30);
        let mut ts = 0;
        for _ in 0..10 {
            r.on_frame(ts);
            ts += 1_000_000; // 1 fps (static window)
        }
        assert_eq!(r.encoder_bitrate(1_000_000), 3_000_000); // capped at 3x
        // Motion resumes at 30 fps: within a handful of frames the budget is back near nominal.
        for _ in 0..8 {
            r.on_frame(ts);
            ts += 33_333;
        }
        assert!(r.encoder_bitrate(1_000_000) < 1_300_000, "{}", r.encoder_bitrate(1_000_000));
        // Never below the target (input faster than nominal is the pacer's problem).
        for _ in 0..30 {
            r.on_frame(ts);
            ts += 5_000;
        }
        assert_eq!(r.encoder_bitrate(1_000_000), 1_000_000);
    }

    #[test]
    fn significant_change() {
        assert!(bitrate_change_significant(0, 1));
        assert!(!bitrate_change_significant(1_000_000, 1_070_000));
        assert!(bitrate_change_significant(1_000_000, 1_080_000));
        assert!(bitrate_change_significant(1_000_000, 900_000));
    }

    const W: u32 = 1920;
    const H: u32 = 1080;

    fn bps_for(bpp: f64, fps: u32) -> u32 {
        (bpp * W as f64 * H as f64 * fps as f64) as u32
    }

    #[test]
    fn sharpness_drops_fps_first_and_restores_with_hysteresis() {
        let mut p = FpsPolicy::new(DegradationPreference::Sharpness, 60);
        // Plenty of bandwidth: full rate.
        assert_eq!(p.update(12_000_000, W, H, 0), 60);
        // Congestion: 4 Mbps at 1080p60 is 0.032 bpp -> 30 fps gives 0.064 bpp.
        assert_eq!(p.update(4_000_000, W, H, 1_000_000), 30);
        // Worse: 2 Mbps -> 30 fps is 0.032, 20 fps 0.048, 15 fps 0.064.
        assert_eq!(p.update(2_000_000, W, H, 2_000_000), 15);
        // Terrible: stays at the floor.
        assert_eq!(p.update(300_000, W, H, 3_000_000), 15);
        // Recovering to just above the degrade threshold at 20 fps is not enough (hysteresis).
        let t = bps_for(DEGRADE_BPP * 1.1, 20);
        for i in 0..10 {
            assert_eq!(p.update(t, W, H, 4_000_000 + i * 1_000_000), 15);
        }
        // Real headroom at 20 fps: one step up after the hold time, not before.
        let t = bps_for(RESTORE_BPP * 1.05, 20);
        assert_eq!(p.update(t, W, H, 20_000_000), 15);
        assert_eq!(p.update(t, W, H, 20_000_000 + RESTORE_HOLD_US - 1), 15);
        assert_eq!(p.update(t, W, H, 20_000_000 + RESTORE_HOLD_US), 20);
        // Full bandwidth back: climbs one step per hold period up to the configured rate.
        let mut now = 30_000_000;
        let mut seen = vec![];
        for _ in 0..20 {
            seen.push(p.update(12_000_000, W, H, now));
            now += 1_000_000;
        }
        assert_eq!(*seen.last().unwrap(), 60);
        let steps: Vec<u32> = seen.windows(2).filter(|w| w[0] != w[1]).map(|w| w[1]).collect();
        assert_eq!(steps, vec![30, 60]);
    }

    #[test]
    fn a_dip_during_the_hold_resets_it() {
        let mut p = FpsPolicy::new(DegradationPreference::Sharpness, 30);
        assert_eq!(p.update(1_000_000, W, H, 0), 15);
        let good = bps_for(RESTORE_BPP * 1.2, 20);
        assert_eq!(p.update(good, W, H, 1_000_000), 15);
        assert_eq!(p.update(bps_for(DEGRADE_BPP * 1.2, 20), W, H, 2_500_000), 15); // dip
        assert_eq!(p.update(good, W, H, 4_200_000), 15); // hold restarted at 4.2 s
        assert_eq!(p.update(good, W, H, 7_100_000), 15);
        assert_eq!(p.update(good, W, H, 7_200_000), 20);
    }

    #[test]
    fn smoothness_never_changes_fps() {
        let mut p = FpsPolicy::new(DegradationPreference::Smoothness, 60);
        assert_eq!(p.update(300_000, W, H, 0), 60);
        assert_eq!(p.cap(), 60);
    }

    #[test]
    fn small_frames_and_low_nominal_rates() {
        // 720p30 at 1 Mbps is 0.036 bpp -> 20 fps (0.054).
        let mut p = FpsPolicy::new(DegradationPreference::Sharpness, 30);
        assert_eq!(p.update(1_000_000, 1280, 720, 0), 20);
        // A 10 fps share has no lower step.
        let mut p = FpsPolicy::new(DegradationPreference::Sharpness, 10);
        assert_eq!(p.update(100_000, W, H, 0), 10);
    }

    #[test]
    fn keyframe_gate_coalesces_bursts() {
        let mut g = KeyframeGate::default();
        g.saw_idr(0);
        assert!(!g.take(10_000, false));
        g.request();
        assert!(!g.take(100_000, false)); // too soon after the first IDR
        g.request(); // folded into the pending one
        assert!(g.pending());
        assert!(g.take(250_000, false));
        assert!(!g.pending());
        assert_eq!(g.coalesced, 1);
        // A resize forces an IDR regardless and satisfies a pending request.
        g.request();
        assert!(g.take(260_000, true));
        assert!(!g.take(600_000, false));
        assert_eq!(g.coalesced, 2);
        // Spaced requests are each honoured immediately.
        g.request();
        assert!(g.take(900_000, false));
    }
}
