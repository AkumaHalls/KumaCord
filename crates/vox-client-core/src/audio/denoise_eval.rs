//! Offline quality check of the noise suppression levels (an ignored test; run by hand):
//!
//! ```text
//! set VOX_DENOISE_EVAL_DIR=<folder with speech*.wav, 48 kHz 16-bit mono>
//! cargo test -p vox-client-core --release --lib denoise_eval -- --ignored --nocapture
//! ```
//!
//! `speech0.wav` and `speech3.wav` are the clean talker; the others become babble. Fan, keyboard,
//! babble (background conversation) and street noise are synthesised / mixed here at 5 dB SNR,
//! each level runs over the mix (the APM without AGC, so its time-varying gain does not blur the
//! numbers) and the table reports SI-SDR improvement, noise attenuation in speech pauses, the
//! SI-SDR of clean speech run through the same chain (speech preservation) and the denoiser's
//! delay. "IA máxima" runs offline (each DeepFilterNet frame is waited for, never replaced).
//! Nothing is played or recorded.

use std::path::Path;
use std::sync::Arc;

use livekit::webrtc::native::apm::AudioProcessingModule;

use super::denoise::{Denoiser, NoiseControl, NoiseLevel, FRAME};

fn read_wav(path: &Path) -> Vec<f32> {
    let b = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    assert_eq!(&b[0..4], b"RIFF");
    let mut i = 12;
    let mut fmt_ok = false;
    while i + 8 <= b.len() {
        let id = &b[i..i + 4];
        let len = u32::from_le_bytes(b[i + 4..i + 8].try_into().unwrap()) as usize;
        let body = &b[i + 8..(i + 8 + len).min(b.len())];
        if id == b"fmt " {
            let ch = u16::from_le_bytes([body[2], body[3]]);
            let rate = u32::from_le_bytes(body[4..8].try_into().unwrap());
            let bits = u16::from_le_bytes([body[14], body[15]]);
            assert_eq!((ch, rate, bits), (1, 48_000, 16), "{}", path.display());
            fmt_ok = true;
        } else if id == b"data" {
            assert!(fmt_ok);
            return body.as_chunks::<2>().0.iter().map(|c| i16::from_le_bytes([c[0], c[1]]) as f32).collect();
        }
        i += 8 + len + (len & 1);
    }
    panic!("no data chunk in {}", path.display());
}

struct Rng(u32);
impl Rng {
    fn next(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        self.0 as f32 / u32::MAX as f32
    }
    fn white(&mut self) -> f32 {
        self.next() * 2.0 - 1.0
    }
}

fn rms(x: &[f32]) -> f32 {
    (x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32).sqrt()
}

/// Desk fan / AC: low-passed noise (pink-ish), mains hum harmonics and a slow wobble.
fn fan(n: usize) -> Vec<f32> {
    let mut r = Rng(12345);
    let (mut lp1, mut lp2) = (0.0f32, 0.0f32);
    (0..n)
        .map(|i| {
            let t = i as f32 / 48_000.0;
            lp1 += 0.05 * (r.white() - lp1);
            lp2 += 0.2 * (r.white() - lp2);
            let hum: f32 = (1..=4).map(|h| (t * 2.0 * std::f32::consts::PI * 120.0 * h as f32).sin() / h as f32).sum();
            (lp1 * 3.0 + lp2 * 0.6 + hum * 0.08) * (1.0 + 0.15 * (t * 2.0 * std::f32::consts::PI * 0.7).sin())
        })
        .collect()
}

/// Mechanical keyboard: random clicks (3-8 ms high-passed noise bursts with a fast decay).
fn keyboard(n: usize) -> Vec<f32> {
    let mut r = Rng(777);
    let mut out = vec![0.0f32; n];
    let mut at = 2_000usize;
    while at < n {
        let len = (150.0 + r.next() * 250.0) as usize;
        let amp = 0.5 + r.next();
        let mut prev = 0.0;
        for k in 0..len.min(n - at) {
            let w = r.white();
            let hp = w - prev;
            prev = w;
            out[at + k] += hp * amp * (-(k as f32) / (len as f32 * 0.25)).exp();
        }
        at += (4_000.0 + r.next() * 9_000.0) as usize; // 80-270 ms apart
    }
    out
}

/// Street: traffic rumble (brown noise), cars passing every few seconds (band-limited noise
/// swelling and fading over ~3 s) and an occasional two-tone horn.
fn street(n: usize) -> Vec<f32> {
    let mut r = Rng(4242);
    let (mut brown, mut car_lp, mut car_lp2) = (0.0f32, 0.0f32, 0.0f32);
    let mut out = vec![0.0f32; n];
    for (i, o) in out.iter_mut().enumerate() {
        let t = i as f32 / 48_000.0;
        brown = (brown + 0.02 * r.white()) * 0.998;
        car_lp += 0.08 * (r.white() - car_lp);
        car_lp2 += 0.3 * (car_lp - car_lp2);
        // A car every 5 s, loudest in the middle of its 3 s pass.
        let phase = (t % 5.0) / 3.0;
        let car = if phase < 1.0 { (phase * std::f32::consts::PI).sin().powi(2) } else { 0.0 };
        *o = brown * 4.0 + (car_lp - car_lp2) * 2.5 * (0.2 + car);
        // A horn (0.4 s, 410 + 520 Hz with harmonics) every 11 s.
        let h = t % 11.0 - 7.0;
        if (0.0..0.4).contains(&h) {
            let env = (h / 0.4 * std::f32::consts::PI).sin().min(0.3) / 0.3;
            let tone: f32 = [410.0f32, 520.0]
                .iter()
                .flat_map(|f| (1..=4).map(move |k| (t * 2.0 * std::f32::consts::PI * f * k as f32).sin() / k as f32))
                .sum();
            *o += tone * 0.25 * env;
        }
    }
    out
}

/// Crowd: the other recordings looped at offsets and summed.
fn babble(n: usize, talkers: &[Vec<f32>]) -> Vec<f32> {
    let mut out = vec![0.0f32; n];
    for (j, t) in talkers.iter().enumerate() {
        let off = 7_919 * (j + 1);
        for (i, o) in out.iter_mut().enumerate() {
            *o += t[(i + off) % t.len()];
        }
    }
    out
}

fn mix(clean: &[f32], noise: &[f32], snr_db: f32) -> Vec<f32> {
    let g = rms(clean) / rms(noise) / 10f32.powf(snr_db / 20.0);
    clean.iter().zip(noise).map(|(s, n)| s + n * g).collect()
}

/// Run one level's chain over `x`: APM (AEC + HPF, NS only at Padrão; no AGC) then the denoiser.
/// `None` = no suppression at all (the APM with NS off and no denoiser): the reference chain.
fn chain(x: &[f32], level: Option<NoiseLevel>) -> (Vec<f32>, std::time::Duration) {
    let mut apm = AudioProcessingModule::new(true, false, true, level.is_some_and(|l| l.apm_ns()));
    let mut d = Denoiser::new(Arc::new(NoiseControl::new(level.unwrap_or(NoiseLevel::Standard))));
    d.set_offline(true);
    let mut out = Vec::with_capacity(x.len());
    let mut chunk = [0i16; FRAME];
    let mut spent = std::time::Duration::ZERO;
    for c in x.as_chunks::<FRAME>().0 {
        for (o, &s) in chunk.iter_mut().zip(c) {
            *o = s.clamp(-32768.0, 32767.0) as i16;
        }
        let _ = apm.set_stream_delay_ms(40);
        apm.process_stream(&mut chunk, 48_000, 1).unwrap();
        let t = std::time::Instant::now();
        d.process(&mut chunk);
        spent += t.elapsed();
        out.extend(chunk.iter().map(|&s| s as f32));
    }
    (out, spent)
}

/// Delay (samples) of `y` behind `x`, by cross-correlation over 0..=3000.
fn delay(x: &[f32], y: &[f32]) -> usize {
    let n = x.len().min(y.len()) - 3_000;
    (0..=3_000)
        .map(|d| (d, (0..n).step_by(3).map(|i| x[i] * y[i + d]).sum::<f32>()))
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .unwrap()
        .0
}

/// Scale-invariant SDR of `est` against `reference` (same length), in dB.
fn si_sdr(reference: &[f32], est: &[f32]) -> f32 {
    let dot: f64 = reference.iter().zip(est).map(|(a, b)| *a as f64 * *b as f64).sum();
    let rr: f64 = reference.iter().map(|a| (*a as f64).powi(2)).sum();
    let a = dot / rr;
    let (mut s, mut e) = (0.0f64, 0.0f64);
    for (r, y) in reference.iter().zip(est) {
        let t = a * *r as f64;
        s += t * t;
        e += (*y as f64 - t).powi(2);
    }
    (10.0 * (s / e).log10()) as f32
}

/// 10 ms frames of `clean` that are speech pauses (with 100 ms of margin around speech).
fn pauses(clean: &[f32]) -> Vec<bool> {
    let frames: Vec<bool> = clean.as_chunks::<FRAME>().0.iter().map(|c| rms(c) < 32768.0 * 10f32.powf(-50.0 / 20.0)).collect();
    (0..frames.len()).map(|i| (i.saturating_sub(10)..(i + 11).min(frames.len())).all(|j| frames[j])).collect()
}

fn pause_rms(x: &[f32], quiet: &[bool]) -> f32 {
    let mut acc = Vec::new();
    for (i, c) in x.as_chunks::<FRAME>().0.iter().enumerate() {
        if quiet.get(i).copied().unwrap_or(false) {
            acc.extend_from_slice(c);
        }
    }
    rms(&acc)
}

#[test]
#[ignore = "offline evaluation; needs VOX_DENOISE_EVAL_DIR"]
fn denoise_eval() {
    let dir = std::env::var("VOX_DENOISE_EVAL_DIR").expect("VOX_DENOISE_EVAL_DIR");
    let dir = Path::new(&dir);
    let mut clean = read_wav(&dir.join("speech0.wav"));
    clean.extend(vec![0.0; 24_000]);
    clean.extend(read_wav(&dir.join("speech3.wav")));
    clean.truncate(clean.len() / FRAME * FRAME);
    // Normalise the talker to about -26 dBFS RMS (a typical mic level).
    let g = 32768.0 * 10f32.powf(-26.0 / 20.0) / rms(&clean);
    clean.iter_mut().for_each(|s| *s *= g);
    let talkers: Vec<Vec<f32>> = ["speech1.wav", "speech2.wav", "speech4.wav"].iter().map(|f| read_wav(&dir.join(f))).collect();
    let n = clean.len();
    let quiet = pauses(&clean);
    let noises = [("ventilador", fan(n)), ("teclado", keyboard(n)), ("multidão", babble(n, &talkers)), ("rua", street(n))];

    // The reference is the clean speech through the APM with no suppression (its band-split
    // filter bank and high-pass change the phase; that is not what is measured here).
    let (reference, _) = chain(&clean, None);
    let apm_delay = delay(&clean, &reference);
    println!(
        "\n{} s of speech, noise mixed at 5 dB SNR, {} pause frames, APM delay {:.1} ms",
        n / 48_000,
        quiet.iter().filter(|q| **q).count(),
        apm_delay as f32 / 48.0
    );
    println!("| ruído | nível | SI-SDR sem supressão | SI-SDR com | melhora | ruído nas pausas | fala limpa (SI-SDR) | atraso extra |");
    println!("|---|---|---|---|---|---|---|---|");
    let mut ai_time = [std::time::Duration::ZERO; 2];
    let mut ai_frames = 0u32;
    for (name, noise) in &noises {
        let noisy = mix(&clean, noise, 5.0);
        let (plain, _) = chain(&noisy, None);
        let in_sdr = si_sdr(&reference, &plain);
        for level in NoiseLevel::ALL {
            let (out, spent) = chain(&noisy, Some(level));
            let (clean_out, _) = chain(&clean, Some(level));
            let d = delay(&reference, &out);
            let m = n - d;
            let out_sdr = si_sdr(&reference[..m], &out[d..d + m]);
            let keep = si_sdr(&reference[..m], &clean_out[d..d + m]);
            let in_noise = pause_rms(&plain, &quiet);
            let out_noise = pause_rms(&out[d..], &quiet);
            let att = 20.0 * (in_noise / out_noise.max(1e-3)).log10();
            println!(
                "| {name} | {} | {in_sdr:.1} dB | {out_sdr:.1} dB | {:+.1} dB | -{att:.1} dB | {keep:.1} dB | {:.1} ms |",
                level.label(),
                out_sdr - in_sdr,
                d as f32 / 48.0
            );
            match level {
                NoiseLevel::AiLight => {
                    ai_time[0] += spent;
                    ai_frames += (n / FRAME) as u32;
                }
                NoiseLevel::AiMax => ai_time[1] += spent,
                NoiseLevel::Standard => {}
            }
        }
    }
    for (name, t) in [("RNNoise", ai_time[0]), ("DeepFilterNet3 (callback + worker, wall time)", ai_time[1])] {
        let per = t / ai_frames;
        println!("\n{name}: {per:?} per 10 ms frame = {:.2}% of one core", per.as_secs_f64() / 0.010 * 100.0);
    }
}
