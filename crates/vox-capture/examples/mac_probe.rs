//! Manual check of the macOS backend (run from Terminal; Screen Recording / Camera are asked for
//! the app that runs it, i.e. Terminal).
//!
//! ```text
//! mac_probe perm                          permissions as the app sees them
//! mac_probe list                          screens and windows (app names only, never titles)
//! mac_probe video (--monitor N | --window ID) [--secs S] [--fps F] [--max WxH]
//! mac_probe audio (--pid P | --all) [--secs S]
//! mac_probe cameras
//! mac_probe camera [--device ID|NAME|vox:synthetic-camera] [--secs S] [--size WxH] [--fps F]
//! ```
//! Prints frames per second, how many frames carried their IOSurface pixel buffer, and the
//! process CPU time per second. Never saves pictures or sound.

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("mac_probe is for macOS (use capture_probe on Windows)");
}

#[cfg(target_os = "macos")]
fn main() {
    if let Err(e) = mac::run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use vox_capture::*;

    fn arg(args: &[String], name: &str) -> Option<String> {
        args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
    }

    fn num(args: &[String], name: &str, default: u32) -> u32 {
        arg(args, name).and_then(|v| v.parse().ok()).unwrap_or(default)
    }

    fn size(args: &[String], name: &str, default: (u32, u32)) -> (u32, u32) {
        arg(args, name)
            .and_then(|v| v.split_once('x').and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?))))
            .unwrap_or(default)
    }

    /// Process CPU time (user + system) in seconds.
    fn cpu_secs() -> f64 {
        let t = std::process::Command::new("ps").args(["-o", "time=", "-p", &std::process::id().to_string()]).output();
        let s = t.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
        // "M:SS.ss"
        let (m, sec) = s.split_once(':').unwrap_or(("0", &s));
        m.trim().parse::<f64>().unwrap_or(0.0) * 60.0 + sec.trim().parse::<f64>().unwrap_or(0.0)
    }

    fn watch(secs: u32, frames: &AtomicU64, native: Option<&AtomicU64>) {
        let (t0, c0) = (Instant::now(), cpu_secs());
        let mut last = 0;
        for _ in 0..secs {
            std::thread::sleep(Duration::from_secs(1));
            let n = frames.load(Ordering::Relaxed);
            let nat = native.map(|x| x.load(Ordering::Relaxed));
            println!("  {:>3} per s  (total {n}{})", n - last, nat.map(|v| format!(", with pixel buffer {v}")).unwrap_or_default());
            last = n;
        }
        let cpu = (cpu_secs() - c0) / t0.elapsed().as_secs_f64() * 100.0;
        println!("process CPU: {cpu:.1}% of one core");
    }

    fn counting(frames: Arc<AtomicU64>, native: Arc<AtomicU64>) -> FrameCallback {
        Box::new(move |f| {
            frames.fetch_add(1, Ordering::Relaxed);
            if let Some(pb) = retained_pixel_buffer(f) {
                native.fetch_add(1, Ordering::Relaxed);
                // Give the reference back (libwebrtc would take it over).
                extern "C" {
                    fn CVPixelBufferRelease(pb: *mut std::ffi::c_void);
                }
                // SAFETY: the reference retained_pixel_buffer gave us.
                unsafe { CVPixelBufferRelease(pb.as_ptr()) };
            }
        })
    }

    pub fn run() -> std::result::Result<(), String> {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let cmd = args.first().map(String::as_str).unwrap_or("perm");
        let secs = num(&args, "--secs", 5);
        match cmd {
            "perm" => {
                println!("screen recording: {:?}", screen_capture_permission());
                println!("camera: {:?}", camera_permission());
            }
            "list" => {
                for m in list_monitors().map_err(|e| e.to_string())? {
                    println!("monitor {} {}x{} {}{}", m.index, m.width, m.height, m.name, if m.primary { " (main)" } else { "" });
                }
                for w in list_windows().map_err(|e| e.to_string())? {
                    println!("window {} pid {} {}", w.hwnd, w.process_id, w.exe_name);
                }
            }
            "video" => {
                let target = match (arg(&args, "--monitor"), arg(&args, "--window")) {
                    (Some(m), _) => VideoTarget::Monitor(m.parse().map_err(|_| "bad --monitor")?),
                    (_, Some(w)) => VideoTarget::Window(w.parse().map_err(|_| "bad --window")?),
                    _ => VideoTarget::Monitor(0),
                };
                let (w, h) = size(&args, "--max", (1920, 1080));
                let cfg = VideoConfig { max_fps: num(&args, "--fps", 30), max_width: w, max_height: h, capture_cursor: true };
                let (frames, native) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
                let cap = start_video(target, cfg, counting(frames.clone(), native.clone())).map_err(|e| e.to_string())?;
                println!("capturing {target:?} at up to {w}x{h}@{}", cfg.max_fps);
                watch(secs, &frames, Some(&native));
                println!("stats {:?}, running {}", cap.stats(), cap.is_running());
            }
            "audio" => {
                let target = match arg(&args, "--pid") {
                    Some(p) => AudioTarget::ProcessTree { pid: p.parse().map_err(|_| "bad --pid")? },
                    None => AudioTarget::SystemExcludingProcessTree { pid: std::process::id() },
                };
                let chunks = Arc::new(AtomicU64::new(0));
                let peak = Arc::new(AtomicU64::new(0));
                let (c, p) = (chunks.clone(), peak.clone());
                let cap = start_audio(
                    target,
                    Box::new(move |a| {
                        c.fetch_add(1, Ordering::Relaxed);
                        let m = a.samples.iter().map(|s| s.unsigned_abs() as u64).max().unwrap_or(0);
                        p.fetch_max(m, Ordering::Relaxed);
                    }),
                )
                .map_err(|e| e.to_string())?;
                println!("capturing sound of {target:?} (10 ms chunks)");
                watch(secs, &chunks, None);
                println!("peak sample {} of 32767, running {}", peak.load(Ordering::Relaxed), cap.is_running());
            }
            "cameras" => {
                for c in list_cameras().map_err(|e| e.to_string())? {
                    println!("camera {} | {}", c.id, c.name);
                    for m in list_camera_formats(&c.id).unwrap_or_default() {
                        println!("    {}", m.label());
                    }
                }
            }
            "camera" => {
                let device = arg(&args, "--device").unwrap_or_default();
                let (w, h) = size(&args, "--size", (1280, 720));
                let cfg = CameraConfig { width: w, height: h, fps: num(&args, "--fps", 30), ..Default::default() };
                let (frames, native) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
                let cap = start_camera_raw(&device, cfg, counting(frames.clone(), native.clone()), Some(Box::new(|_| {}))).map_err(|e| e.to_string())?;
                println!("camera {:?}", cap.format());
                watch(secs, &frames, Some(&native));
                println!("stats {:?}", cap.stats());
            }
            other => return Err(format!("unknown command {other:?} (perm|list|video|audio|cameras|camera)")),
        }
        Ok(())
    }
}
