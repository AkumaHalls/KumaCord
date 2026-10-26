//! Capture-only lifecycle probe (no WebRTC): start/stop vox-capture video repeatedly and print
//! memory + handle count after each stop. Used to tell vox-capture leaks from encoder/publish ones.
//!
//! capture_restart [monitor-index] [rounds] [secs-per-round] [--copy]
//!   --copy  also copy every frame into a fresh NV12 buffer (what the publisher does per frame)

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use vox_client_core::stats::ProcessMeter;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mon: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let rounds: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(3);
    let secs: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(2);
    let copy = args.iter().any(|a| a == "--copy");
    let mut meter = ProcessMeter::new();
    let s = meter.sample();
    println!("start: ws={:.1}MB private={:.1}MB handles={}", mb(s.working_set_bytes), mb(s.private_bytes), s.handles);
    for round in 1..=rounds {
        let frames = Arc::new(AtomicU64::new(0));
        let f = frames.clone();
        let cfg = vox_capture::VideoConfig { max_fps: 30, ..Default::default() };
        let cap = vox_capture::start_video(
            vox_capture::VideoTarget::Monitor(mon),
            cfg,
            Box::new(move |fr| {
                f.fetch_add(1, Ordering::Relaxed);
                if copy {
                    let mut buf = vec![0u8; fr.y.len() + fr.uv.len()];
                    buf[..fr.y.len()].copy_from_slice(fr.y);
                    std::hint::black_box(&buf);
                }
            }),
        )
        .expect("start_video");
        std::thread::sleep(Duration::from_secs(secs));
        let during = meter.sample();
        drop(cap);
        std::thread::sleep(Duration::from_millis(500));
        let s = meter.sample();
        println!(
            "round {round}: frames={} during: ws={:.1} private={:.1} | after stop: ws={:.1}MB private={:.1}MB handles={}",
            frames.load(Ordering::Relaxed),
            mb(during.working_set_bytes),
            mb(during.private_bytes),
            mb(s.working_set_bytes),
            mb(s.private_bytes),
            s.handles
        );
    }
}

fn mb(b: u64) -> f64 {
    b as f64 / 1048576.0
}
