//! Tiny streaming linear resampler. Only used when an audio device does not run at 48 kHz
//! (WASAPI shared mode almost always does, so this is normally bypassed).

/// Streaming linear-interpolation resampler for interleaved `f32` audio.
pub struct LinearResampler {
    channels: usize,
    /// Input frames consumed per output frame.
    step: f64,
    /// Fractional read position, relative to `prev` (index 0 = `prev`, 1 = first new frame).
    pos: f64,
    /// Last input frame of the previous block.
    prev: Vec<f32>,
}

impl LinearResampler {
    pub fn new(from_rate: u32, to_rate: u32, channels: usize) -> Self {
        assert!(from_rate > 0 && to_rate > 0 && channels > 0);
        Self { channels, step: from_rate as f64 / to_rate as f64, pos: 0.0, prev: vec![0.0; channels] }
    }

    /// Resample `input` (interleaved) and append the result to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let ch = self.channels;
        let n = input.len() / ch;
        if n == 0 {
            return;
        }
        // Virtual array y = [prev, input frames...] of n + 1 frames; interpolate y[i]..y[i+1].
        let frame = |i: usize, c: usize| -> f32 { if i == 0 { self.prev[c] } else { input[(i - 1) * ch + c] } };
        while self.pos < n as f64 {
            let i = self.pos as usize;
            let f = (self.pos - i as f64) as f32;
            for c in 0..ch {
                let a = frame(i, c);
                let b = frame(i + 1, c);
                out.push(a + (b - a) * f);
            }
            self.pos += self.step;
        }
        self.pos -= n as f64;
        self.prev.copy_from_slice(&input[(n - 1) * ch..n * ch]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_length_tracks_rate_ratio() {
        let mut r = LinearResampler::new(44_100, 48_000, 1);
        let mut out = Vec::new();
        let block = vec![0.25f32; 441];
        for _ in 0..100 {
            r.process(&block, &mut out);
        }
        // 1 s of 44.1k input -> ~48000 output samples.
        assert!((out.len() as i64 - 48_000).abs() <= 2, "got {}", out.len());
    }

    #[test]
    fn constant_signal_stays_constant() {
        let mut r = LinearResampler::new(48_000, 44_100, 2);
        let mut out = Vec::new();
        let block: Vec<f32> = (0..960).map(|i| if i % 2 == 0 { 0.5 } else { -0.5 }).collect();
        r.process(&block, &mut out); // warm-up (first sample interpolates from silence)
        out.clear();
        r.process(&block, &mut out);
        assert!(!out.is_empty());
        for (i, s) in out.iter().enumerate() {
            let want = if i % 2 == 0 { 0.5 } else { -0.5 };
            assert!((s - want).abs() < 1e-6);
        }
    }
}
