//! Sine-wave generator used as the test screenshare-audio source.

/// Generates interleaved stereo i16 chunks of a sine tone (left and right slightly detuned so
/// stereo survives the round trip in a recognisable way).
pub struct SineGenerator {
    sample_rate: f64,
    freq_l: f64,
    freq_r: f64,
    phase_l: f64,
    phase_r: f64,
    amplitude: f64,
}

impl SineGenerator {
    pub fn new(sample_rate: u32, freq_l: f64, freq_r: f64, amplitude: f64) -> Self {
        Self { sample_rate: sample_rate as f64, freq_l, freq_r, phase_l: 0.0, phase_r: 0.0, amplitude }
    }

    /// Fill `out` (interleaved stereo) with the next `out.len() / 2` frames.
    pub fn fill_stereo(&mut self, out: &mut [i16]) {
        use std::f64::consts::TAU;
        let (dl, dr) = (TAU * self.freq_l / self.sample_rate, TAU * self.freq_r / self.sample_rate);
        let amp = self.amplitude * i16::MAX as f64;
        for frame in out.as_chunks_mut::<2>().0 {
            frame[0] = (self.phase_l.sin() * amp) as i16;
            frame[1] = (self.phase_r.sin() * amp) as i16;
            self.phase_l = (self.phase_l + dl) % TAU;
            self.phase_r = (self.phase_r + dr) % TAU;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn produces_bounded_periodic_tone() {
        let mut g = SineGenerator::new(48_000, 480.0, 480.0, 0.5);
        let mut buf = vec![0i16; 960 * 2]; // 20 ms
        g.fill_stereo(&mut buf);
        let max = buf.iter().map(|s| s.unsigned_abs()).max().unwrap();
        assert!(max > 16_000 && max <= 16_384, "max {max}");
        // 480 Hz at 48 kHz => period of 100 frames: frame 0 and frame 100 match.
        assert!((buf[0] as i32 - buf[200] as i32).abs() <= 2);
        // Continuity across chunks: the next chunk starts where a 40 ms buffer would continue.
        let mut g2 = SineGenerator::new(48_000, 480.0, 480.0, 0.5);
        let mut long = vec![0i16; 1920 * 2];
        g2.fill_stereo(&mut long);
        let mut next = vec![0i16; 960 * 2];
        g.fill_stereo(&mut next);
        assert!((next[0] as i32 - long[1920] as i32).abs() <= 2);
    }
}
