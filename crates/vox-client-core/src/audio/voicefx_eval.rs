//! Offline quality check of the voice changer (an ignored test; run by hand, silent):
//!
//! ```text
//! cargo test -p vox-client-core --release --lib voicefx_eval -- --ignored --nocapture
//! ```
//!
//! Runs every effect over synthesized speech (a low and a high talker: glottal source, three
//! formants, intonation, fricatives and pauses; see `voicefx::synth`) and, when
//! `VOX_DENOISE_EVAL_DIR` points at the denoiser's recordings, over `speech0.wav` too. For each it
//! reports the CPU per 10 ms chunk, the f0 ratio achieved against the one asked for (an
//! autocorrelation pitch estimator independent of the effect's own tracker), the formant scale
//! (the long-term spectral envelope matched against a stretched copy of the input's), the
//! measured latency (envelope cross-correlation) against the declared one, the level change and
//! the peak (no clipping), and checks that "Desligado" is bit-exact. Set `VOX_VOICEFX_OUT_DIR` to
//! also write each result as a WAV file (nothing is ever played).

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::denoise::NoiseControl;
use super::voicefx::synth::{median_f0, rms, speech, to_f32, to_i16};
use super::voicefx::{pitch_params, VoiceChanger, VoiceEffect, VoiceFxConfig, BLOCK};
use super::NoiseLevel;

fn read_wav(path: &Path) -> Option<Vec<f32>> {
    let b = std::fs::read(path).ok()?;
    let mut i = 12;
    while i + 8 <= b.len() {
        let id = &b[i..i + 4];
        let len = u32::from_le_bytes(b[i + 4..i + 8].try_into().ok()?) as usize;
        if id == b"data" {
            let body = &b[i + 8..(i + 8 + len).min(b.len())];
            return Some(body.as_chunks::<2>().0.iter().map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0).collect());
        }
        i += 8 + len + (len & 1);
    }
    None
}

fn write_wav(path: &Path, x: &[i16]) {
    let mut b = Vec::with_capacity(44 + x.len() * 2);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + x.len() as u32 * 2).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&48_000u32.to_le_bytes());
    b.extend_from_slice(&96_000u32.to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&(x.len() as u32 * 2).to_le_bytes());
    for s in x {
        b.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write(path, b).unwrap();
}

/// Run `x` through a fresh chain set to `effect` at `pct`: (output, time per chunk, worst chunk).
fn process(x: &[f32], effect: VoiceEffect, pct: u32) -> (Vec<i16>, Duration, Duration) {
    let ctl = Arc::new(NoiseControl::new(NoiseLevel::Standard));
    ctl.voice_fx().set(VoiceFxConfig { effect, intensity_pct: pct });
    let mut fx = VoiceChanger::new(ctl);
    let mut pcm = to_i16(x);
    let mut total = Duration::ZERO;
    let mut worst = Duration::ZERO;
    for (k, c) in pcm.chunks_mut(BLOCK).enumerate() {
        let t = Instant::now();
        fx.process(c);
        let e = t.elapsed();
        // The first chunk primes the effect (a one-off, reported separately as the worst).
        if k > 0 {
            total += e;
        }
        worst = worst.max(e);
    }
    assert_eq!(fx.late_grains(), 0, "{effect:?}: pitch grains later than the declared delay");
    let n = (pcm.len() / BLOCK).saturating_sub(1).max(1) as u32;
    (pcm, total / n, worst)
}

/// 1 ms RMS envelope.
fn envelope(x: &[f32]) -> Vec<f32> {
    x.chunks(48).map(rms).collect()
}

/// Lag (ms) of `y`'s envelope behind `x`'s, by cross-correlation of the mean-removed envelopes.
fn latency_ms(x: &[f32], y: &[f32]) -> f32 {
    let (ex, ey) = (envelope(x), envelope(y));
    let mx = ex.iter().sum::<f32>() / ex.len() as f32;
    let my = ey.iter().sum::<f32>() / ey.len() as f32;
    let n = ex.len().min(ey.len()) - 100;
    let corr = |d: usize| (0..n).map(|i| (ex[i] - mx) * (ey[i + d] - my)).sum::<f32>();
    let best = (0..100).max_by(|&a, &b| corr(a).total_cmp(&corr(b))).unwrap();
    // Parabolic refinement.
    if best > 0 && best < 99 {
        let (a, b, c) = (corr(best - 1), corr(best), corr(best + 1));
        let den = a - 2.0 * b + c;
        if den.abs() > 1e-12 {
            return best as f32 + 0.5 * (a - c) / den;
        }
    }
    best as f32
}

/// Long-term log spectrum (dB) of the loud frames at 25 Hz steps from 100 Hz to 5 kHz, smoothed
/// over about 350 Hz (the envelope: formants, not harmonics).
fn ltas(x: &[f32]) -> Vec<f32> {
    const N: usize = 2048;
    let freqs: Vec<f32> = (4..=200).map(|k| k as f32 * 25.0).collect();
    let win: Vec<f32> = (0..N).map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / N as f32).cos()).collect();
    let mut acc = vec![0.0f64; freqs.len()];
    let loud = rms(x) * 0.5;
    let mut i = 0;
    while i + N < x.len() {
        let fr = &x[i..i + N];
        if rms(fr) > loud {
            for (a, &f) in acc.iter_mut().zip(&freqs) {
                let w = 2.0 * std::f32::consts::PI * f / 48_000.0;
                let (mut re, mut im) = (0.0f32, 0.0f32);
                for (k, (&v, &h)) in fr.iter().zip(&win).enumerate() {
                    let (s, c) = (w * k as f32).sin_cos();
                    re += v * h * c;
                    im -= v * h * s;
                }
                *a += (re * re + im * im) as f64;
            }
        }
        i += N / 2;
    }
    let db: Vec<f32> = acc.iter().map(|&p| 10.0 * (p.max(1e-20) as f32).log10()).collect();
    let r = 7; // +-175 Hz
    (0..db.len()).map(|k| {
        let (lo, hi) = (k.saturating_sub(r), (k + r + 1).min(db.len()));
        db[lo..hi].iter().sum::<f32>() / (hi - lo) as f32
    }).collect()
}

/// The scale `a` for which `out(f)` best matches `inp(f / a)` between 300 Hz and 3.5 kHz.
fn formant_scale(inp: &[f32], out: &[f32]) -> f32 {
    let (ei, eo) = (ltas(inp), ltas(out));
    let at = |e: &[f32], f: f32| {
        let k = (f / 25.0 - 4.0).clamp(0.0, (e.len() - 1) as f32);
        let i = k.floor() as usize;
        let j = (i + 1).min(e.len() - 1);
        e[i] + (e[j] - e[i]) * (k - i as f32)
    };
    let mut best = (1.0, f32::MIN);
    let mut a = 0.7;
    while a <= 1.5 {
        let fs: Vec<f32> = (12..=140).map(|k| k as f32 * 25.0).collect();
        let p: Vec<f32> = fs.iter().map(|&f| at(&eo, f)).collect();
        let q: Vec<f32> = fs.iter().map(|&f| at(&ei, f / a)).collect();
        let (mp, mq) = (p.iter().sum::<f32>() / p.len() as f32, q.iter().sum::<f32>() / q.len() as f32);
        let num: f32 = p.iter().zip(&q).map(|(x, y)| (x - mp) * (y - mq)).sum();
        let den = (p.iter().map(|x| (x - mp).powi(2)).sum::<f32>() * q.iter().map(|y| (y - mq).powi(2)).sum::<f32>()).sqrt();
        let c = num / den.max(1e-9);
        if c > best.1 {
            best = (a, c);
        }
        a += 0.005;
    }
    best.0
}

#[test]
#[ignore = "offline evaluation, run by hand in release"]
fn voicefx_eval() {
    let out_dir = std::env::var_os("VOX_VOICEFX_OUT_DIR").map(std::path::PathBuf::from);
    let mut inputs: Vec<(String, Vec<f32>)> = vec![("voz grave sintética (110 Hz)".into(), speech(8.0, 110.0, 1)), ("voz aguda sintética (210 Hz)".into(), speech(8.0, 210.0, 2))];
    if let Some(dir) = std::env::var_os("VOX_DENOISE_EVAL_DIR") {
        if let Some(mut x) = read_wav(&Path::new(&dir).join("speech0.wav")) {
            let g = 0.1 / rms(&x).max(1e-9);
            x.iter_mut().for_each(|v| *v *= g);
            inputs.push(("speech0.wav".into(), x));
        }
    }
    let cases: Vec<(VoiceEffect, u32)> = vec![
        (VoiceEffect::Deep, 0),
        (VoiceEffect::Deep, 50),
        (VoiceEffect::Deep, 100),
        (VoiceEffect::High, 0),
        (VoiceEffect::High, 50),
        (VoiceEffect::High, 100),
        (VoiceEffect::Alien, 50),
        (VoiceEffect::Robot, 50),
        (VoiceEffect::Radio, 50),
        (VoiceEffect::Echo, 50),
        (VoiceEffect::Cave, 50),
    ];
    for (name, x) in &inputs {
        // Bypass: bit-exact.
        let (y, t_off, _) = process(x, VoiceEffect::Off, 50);
        assert_eq!(y, to_i16(x), "Desligado is not bit-exact");
        let f_in = median_f0(x);
        println!("\n### {name}: {:.1} s, f0 mediana {f_in:.1} Hz; Desligado: idêntico bit a bit, {t_off:?} por bloco\n", x.len() as f32 / 48_000.0);
        println!("| efeito | intensidade | CPU / 10 ms | % de 1 núcleo | pior bloco | f0 pedido | f0 obtido | formantes | atraso declarado | atraso medido | nível | pico |");
        println!("|---|---|---|---|---|---|---|---|---|---|---|---|");
        for &(e, pct) in &cases {
            let (y, per, worst) = process(x, e, pct);
            let yf = to_f32(&y);
            let clipped = y.iter().filter(|&&s| s == i16::MAX || s == i16::MIN).count();
            assert_eq!(clipped, 0, "{e:?}: clipped");
            let peak_db = 20.0 * (y.iter().map(|s| s.unsigned_abs()).max().unwrap() as f32 / 32768.0).log10();
            let level_db = 20.0 * (rms(&yf) / rms(x)).log10();
            let lat = latency_ms(x, &yf);
            let (want, got, formant) = if matches!(e, VoiceEffect::Deep | VoiceEffect::High | VoiceEffect::Alien) {
                let p = pitch_params(e, pct as f32 / 100.0);
                let want = 2f32.powf(p.semitones / 12.0);
                let got = median_f0(&yf) / f_in;
                // The formant scale is measured without the vibrato/chorus (Alienígena's would blur it).
                (format!("×{want:.3} ({:+.1} st)", p.semitones), format!("×{got:.3} ({:+.2} st)", 12.0 * got.log2()), format!("×{:.3} (alvo ×{:.3})", formant_scale(x, &yf), p.formant))
            } else {
                ("-".into(), "-".into(), "-".into())
            };
            println!(
                "| {} | {pct}% | {per:?} | {:.3}% | {worst:?} | {want} | {got} | {formant} | {:.0} ms | {lat:.1} ms | {level_db:+.1} dB | {peak_db:.1} dBFS |",
                e.label(),
                per.as_secs_f64() / 0.010 * 100.0,
                e.latency_ms()
            );
            if let Some(dir) = &out_dir {
                let file = format!("{}-{}-{pct}.wav", name.split_whitespace().last().unwrap_or("x").trim_matches(|c: char| !c.is_alphanumeric()), e.as_str());
                write_wav(&dir.join(file), &y);
            }
        }
    }
}
