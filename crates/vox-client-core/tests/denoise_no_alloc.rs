//! The AI denoiser and the voice changer run in the mic callback: in steady state (and when
//! switching levels or effects) they must not touch the heap. A counting global allocator (this
//! test binary only) checks that.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;

use vox_client_core::audio::denoise::{Denoiser, FRAME};
use vox_client_core::audio::voicefx::VoiceChanger;
use vox_client_core::{NoiseControl, NoiseLevel, VoiceEffect, VoiceFxConfig};

struct Counting;

thread_local! {
    static COUNT: Cell<usize> = const { Cell::new(0) };
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = COUNT.try_with(|c| c.set(c.get() + 1));
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let _ = COUNT.try_with(|c| c.set(c.get() + 1));
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn allocations() -> usize {
    COUNT.with(|c| c.get())
}

fn fill(c: &mut [i16; FRAME], k: usize, x: &mut u32) {
    for (i, s) in c.iter_mut().enumerate() {
        *x ^= *x << 13;
        *x ^= *x >> 17;
        *x ^= *x << 5;
        let t = (k * FRAME + i) as f32 / 48_000.0;
        *s = ((t * 900.0).sin() * 5000.0 + (*x as f32 / u32::MAX as f32 - 0.5) * 2000.0) as i16;
    }
}

/// Allocations made by `d.process` alone (level switches happen in the UI and may allocate:
/// starting the DeepFilterNet worker spawns a thread; its own allocations are on that thread).
fn process_counted(d: &mut Denoiser, chunk: &mut [i16; FRAME]) -> usize {
    let before = allocations();
    d.process(chunk);
    allocations() - before
}

#[test]
fn denoiser_does_not_allocate_in_steady_state_or_when_switching() {
    let ctl = Arc::new(NoiseControl::new(NoiseLevel::AiLight));
    let mut d = Denoiser::new(ctl.clone());
    let mut chunk = [0i16; FRAME];
    let mut x = 1u32;
    // Warm-up (first calls may set up FFT plans lazily).
    for k in 0..20 {
        fill(&mut chunk, k, &mut x);
        d.process(&mut chunk);
    }
    let before = allocations();
    for k in 20..400 {
        fill(&mut chunk, k, &mut x);
        // Level switches happen in the UI; here only the atomic changes (no APM attached).
        match k {
            100 => ctl.set_level(NoiseLevel::Standard),
            200 => ctl.set_level(NoiseLevel::AiLight),
            300 => d.reset(),
            _ => {}
        }
        d.process(&mut chunk);
    }
    assert_eq!(allocations() - before, 0, "the denoiser allocated in the audio path");
    // The counter itself works.
    let v = std::hint::black_box(vec![1u8; 64]);
    assert!(allocations() > before && v.len() == 64);
}

#[test]
fn ai_max_does_not_allocate_in_the_audio_path() {
    let ctl = Arc::new(NoiseControl::new(NoiseLevel::AiLight));
    let mut d = Denoiser::new(ctl.clone());
    let mut chunk = [0i16; FRAME];
    let mut x = 7u32;
    // Warm-up at IA leve (RNNoise sets up its tables lazily, once per process), then Padrão.
    for k in 0..20 {
        fill(&mut chunk, k, &mut x);
        d.process(&mut chunk);
    }
    ctl.set_level(NoiseLevel::Standard);
    let mut allocated = 0;
    // Offline (waiting for each DeepFilterNet frame) so every path is taken deterministically:
    // loading behind RNNoise, warm-up, fade-in, steady state, mute flush, switches to the other
    // two levels and back.
    d.set_offline(true);
    for k in 20..500 {
        fill(&mut chunk, k, &mut x);
        match k {
            40 => ctl.set_level(NoiseLevel::AiMax),
            150 => d.reset(),
            200 => ctl.set_level(NoiseLevel::AiLight),
            250 => ctl.set_level(NoiseLevel::AiMax),
            350 => ctl.set_level(NoiseLevel::Standard),
            400 => ctl.set_level(NoiseLevel::AiMax),
            _ => {}
        }
        allocated += process_counted(&mut d, &mut chunk);
    }
    assert!(d.dfn_frames > 250, "DeepFilterNet ran ({} frames, level {:?})", d.dfn_frames, ctl.level());
    // Real time (late frames replaced by the input, the hand-off growing): still nothing.
    d.set_offline(false);
    for k in 500..600 {
        fill(&mut chunk, k, &mut x);
        allocated += process_counted(&mut d, &mut chunk);
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert_eq!(allocated, 0, "the denoiser allocated in the audio path at IA máxima");
}

#[test]
fn voice_changer_does_not_allocate_in_the_audio_path() {
    let ctl = Arc::new(NoiseControl::new(NoiseLevel::Standard));
    let mut fx = VoiceChanger::new(ctl.clone());
    let mut chunk = [0i16; FRAME];
    let mut x = 3u32;
    let mut allocated = 0;
    let before = allocations();
    // Every effect in turn (each switch crossfades and primes the pitch shifter), the intensity
    // moving all the time, a mute in the middle of an effect, odd chunk sizes, back to Desligado.
    let effects = VoiceEffect::ALL;
    for k in 0..1_200 {
        fill(&mut chunk, k, &mut x);
        if k % 60 == 0 {
            let effect = effects[(k / 60) % effects.len()];
            ctl.voice_fx().set(VoiceFxConfig { effect, intensity_pct: (k as u32 * 7) % 101 });
        } else if k % 7 == 0 {
            let effect = ctl.voice_fx().effect();
            ctl.voice_fx().set(VoiceFxConfig { effect, intensity_pct: (k as u32 * 13) % 101 });
        }
        if k == 700 {
            fx.reset();
        }
        let b = allocations();
        if k % 50 == 25 {
            let (a, rest) = chunk.split_at_mut(441);
            fx.process(a);
            fx.process(rest);
        } else {
            fx.process(&mut chunk);
        }
        allocated += allocations() - b;
    }
    assert!(fx.blocks > 900, "the effects ran ({} blocks)", fx.blocks);
    assert_eq!(allocated, 0, "the voice changer allocated in the audio path");
    assert_eq!(allocations() - before, 0);
}
