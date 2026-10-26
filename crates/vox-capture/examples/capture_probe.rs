//! Manual test/benchmark for vox-capture.
//!
//! ```text
//! capture_probe list
//! capture_probe video (--monitor N | --window HWND) [--secs S] [--fps F] [--max WxH] [--no-cursor] [--png out.png]
//! capture_probe audio (--pid P | --exclude-pid P) [--secs S] [--out file.wav]
//! capture_probe encoders
//! capture_probe encode (--monitor N | --window HWND) [--secs S] [--fps F] [--max WxH]
//!     [--bitrate BPS] [--backend auto|nvenc|mf] [--out f.h264] [--force-key-every N]
//!     [--bitrate-step] [--no-cursor]
//!     [--preview-fps N] [--preview-size WxH] [--preview-png out.png] [--preview-toggle SECS]
//!     [--preview-fps2 N --preview-size2 WxH]   (screen preview tap, item 8a: fps2/size2 apply at half time)
//! capture_probe encode-cycle [--backend auto|nvenc|mf] [--rounds N] [--monitor M]
//! capture_probe testwin [--fps F] [--secs S] [--resize-every S] [--noise PCT] [--topmost] [--size WxH] [--at X,Y]
//! capture_probe cameras
//! capture_probe camera [--device ID|NAME|vox:synthetic-camera] [--secs S] [--size WxH] [--fps F]
//!     [--bitrate BPS] [--backend auto|nvenc|mf] [--mode encoded|raw|preview] [--preview] [--toggle-preview]
//!     (counts frames only; never saves pictures)
//! capture_probe thumbs [--threads N] [--method wgc|print] [--timeout MS] [--only windows|monitors] [--repeat N]
//!     [--max WxH] [--window HWND --png out.png]   (item 8c: time picker thumbnails; prints exe names, never titles)
//! capture_probe bench --work DIR [...]   (synthetic-scene encoder quality benchmark, see common/bench_cmd.rs)
//! ```
//! HWND accepts decimal or 0x-hex. Prints delivered/dropped fps, GPU convert time, process CPU%
//! and memory, and dumps the first frame as PNG / the audio as WAV.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use vox_capture::*;

#[path = "common/bench_cmd.rs"]
mod bench_cmd;
#[path = "common/scenes.rs"]
mod scenes;

type Result<T, E = String> = std::result::Result<T, E>;

fn main() {
    let _log = vox_log::init("capture_probe", None, true);
    let args: Vec<String> = std::env::args().skip(1).collect();
    let r = match args.first().map(String::as_str) {
        Some("list") => cmd_list(),
        Some("video") => cmd_video(&args[1..]),
        Some("audio") => cmd_audio(&args[1..]),
        Some("tone") => cmd_tone(&args[1..]),
        Some("cycle") => cmd_cycle(&args[1..]),
        Some("encoders") => cmd_encoders(),
        Some("encode") => cmd_encode(&args[1..]),
        Some("encode-cycle") => cmd_encode_cycle(&args[1..]),
        Some("testwin") => cmd_testwin(&args[1..]),
        Some("thumbs") => cmd_thumbs(&args[1..]),
        Some("bench") => bench_cmd::cmd_bench(&args[1..]),
        Some("cameras") => cmd_cameras(),
        Some("camera") => cmd_camera(&args[1..]),
        _ => {
            eprintln!(
                "usage:\n  capture_probe list\n  capture_probe video (--monitor N | --window HWND) [--secs S] [--fps F] [--max WxH] [--no-cursor] [--png out.png]\n  capture_probe audio (--pid P | --exclude-pid P) [--secs S] [--out file.wav]\n  capture_probe encoders\n  capture_probe encode (--monitor N | --window HWND) [--secs S] [--fps F] [--max WxH] [--bitrate BPS] [--backend auto|nvenc|mf] [--out f.h264] [--force-key-every N] [--bitrate-step] [--no-cursor]\n  capture_probe encode-cycle [--backend auto|nvenc|mf] [--rounds N] [--monitor M]\n  capture_probe testwin [--fps F] [--secs S] [--resize-every S] [--noise PCT] [--topmost] [--size WxH] [--at X,Y]\n  capture_probe bench --work DIR [--scenes pointcloud,text,game] [--secs S] [--size WxH] [--fps 30,60] [--kbps 4000,8000] [--configs label=mf:v01;label=nv:p5,ll] [--input-fps F] [--idr-every N] [--steps frame:kbps,..] [--vmaf] [--png] [--keep]\n  capture_probe tone --device <endpoint name substring> [--secs S]   (440 Hz tone on that endpoint only, never the default device)"
            );
            std::process::exit(2);
        }
    };
    if let Err(e) = r {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn opt<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

fn parse_int(s: &str) -> Result<i64, String> {
    let r = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(h) => i64::from_str_radix(h, 16),
        None => s.parse(),
    };
    r.map_err(|e| format!("bad number '{s}': {e}"))
}

fn cmd_list() -> Result<(), String> {
    println!("Monitors:");
    for m in list_monitors().map_err(|e| e.to_string())? {
        println!(
            "  [{}] {} {}x{}{}",
            m.index,
            m.name,
            m.width,
            m.height,
            if m.primary { " (primary)" } else { "" }
        );
    }
    println!("Windows:");
    for w in list_windows().map_err(|e| e.to_string())? {
        println!(
            "  0x{:X}  pid={:<6} {:<24} {}",
            w.hwnd, w.process_id, w.exe_name, w.title
        );
    }
    Ok(())
}

struct FirstFrame {
    w: u32,
    h: u32,
    y: Vec<u8>,
    uv: Vec<u8>,
}

fn cmd_video(args: &[String]) -> Result<(), String> {
    let target = if let Some(m) = opt(args, "--monitor") {
        VideoTarget::Monitor(parse_int(m)? as usize)
    } else if let Some(w) = opt(args, "--window") {
        VideoTarget::Window(parse_int(w)? as isize)
    } else {
        return Err("need --monitor N or --window HWND".into());
    };
    let secs = opt(args, "--secs")
        .map(parse_int)
        .transpose()?
        .unwrap_or(10) as u64;
    let fps = opt(args, "--fps").map(parse_int).transpose()?.unwrap_or(30) as u32;
    let (mw, mh) = match opt(args, "--max") {
        Some(s) => {
            let (a, b) = s.split_once('x').ok_or("--max WxH")?;
            (parse_int(a)? as u32, parse_int(b)? as u32)
        }
        None => (1920, 1080),
    };
    let png_path = opt(args, "--png")
        .unwrap_or("capture_probe_frame.png")
        .to_string();
    let cfg = VideoConfig {
        max_fps: fps,
        max_width: mw,
        max_height: mh,
        capture_cursor: !args.iter().any(|a| a == "--no-cursor"),
    };

    let first: Arc<Mutex<Option<FirstFrame>>> = Arc::new(Mutex::new(None));
    let last_size = Arc::new(AtomicU64::new(0));
    let cb_first = first.clone();
    let cb_size = last_size.clone();
    let last: Arc<Mutex<Option<FirstFrame>>> = Arc::new(Mutex::new(None));
    let cb_last = last.clone();
    let cap = start_video(
        target,
        cfg,
        Box::new(move |f: &Nv12Frame<'_>| {
            // Behave like a real consumer: copy every frame out (tightly packed) and return.
            cb_size.store(
                ((f.width as u64) << 32) | f.height as u64,
                Ordering::Relaxed,
            );
            let (w, h) = (f.width as usize, f.height as usize);
            let mut g = cb_last.lock().unwrap();
            let buf = g.get_or_insert_with(|| FirstFrame {
                w: 0,
                h: 0,
                y: Vec::new(),
                uv: Vec::new(),
            });
            buf.w = f.width;
            buf.h = f.height;
            buf.y.clear();
            buf.uv.clear();
            for r in 0..h {
                buf.y
                    .extend_from_slice(&f.y[r * f.y_stride as usize..][..w]);
            }
            for r in 0..h / 2 {
                buf.uv
                    .extend_from_slice(&f.uv[r * f.uv_stride as usize..][..w]);
            }
            let mut first = cb_first.lock().unwrap();
            if first.is_none() {
                *first = Some(FirstFrame {
                    w: buf.w,
                    h: buf.h,
                    y: buf.y.clone(),
                    uv: buf.uv.clone(),
                });
            }
        }),
    )
    .map_err(|e| e.to_string())?;
    // Measure steady state only (exclude D3D device / WGC setup).
    let cpu0 = ProcSample::now();
    println!("capturing {target:?} for {secs}s at max {fps} fps, fit {mw}x{mh}");

    let start = Instant::now();
    let mut prev = cap.stats();
    let mut ended_early = false;
    for s in 1..=secs {
        std::thread::sleep(Duration::from_secs(1));
        let st = cap.stats();
        let sz = last_size.load(Ordering::Relaxed);
        println!(
            "  t={s:>3}s  delivered {:>3}/s  dropped {:>3}/s  size {}x{}  running={}",
            st.frames_delivered - prev.frames_delivered,
            st.frames_dropped - prev.frames_dropped,
            sz >> 32,
            sz & 0xffff_ffff,
            cap.is_running()
        );
        prev = st;
        if !cap.is_running() {
            ended_early = true;
            println!("  capture ended on its own (target closed?)");
            break;
        }
    }
    let elapsed = start.elapsed().as_secs_f64();
    let cpu1 = ProcSample::now();
    let st = cap.stats();
    let t_drop = Instant::now();
    drop(cap);
    let drop_ms = t_drop.elapsed().as_secs_f64() * 1000.0;

    println!("--- video results ---");
    println!(
        "delivered: {} ({:.1} fps)",
        st.frames_delivered,
        st.frames_delivered as f64 / elapsed
    );
    println!(
        "dropped:   {} ({:.1} /s, pre-GPU pacing + superseded)",
        st.frames_dropped,
        st.frames_dropped as f64 / elapsed
    );
    println!("avg GPU convert+readback: {} us", st.avg_gpu_convert_us);
    print_proc(&cpu0, &cpu1);
    println!(
        "drop(VideoCapture) took {drop_ms:.1} ms{}",
        if ended_early { " (already ended)" } else { "" }
    );

    if let Some(f) = first.lock().unwrap().take() {
        save_png(&f, &png_path)?;
        println!("first frame {}x{} saved to {png_path}", f.w, f.h);
    } else {
        println!("no frame captured");
    }
    if let Some(f) = last.lock().unwrap().take() {
        let p = png_path.replace(".png", "_last.png");
        save_png(&f, &p)?;
        println!("last frame {}x{} saved to {p}", f.w, f.h);
    }
    Ok(())
}

fn save_png(f: &FirstFrame, path: &str) -> Result<(), String> {
    let (w, h) = (f.w as usize, f.h as usize);
    let mut rgb = vec![0u8; w * h * 3];
    for y in 0..h {
        for x in 0..w {
            // BT.709 limited range -> RGB.
            let yy = f.y[y * w + x] as f32 - 16.0;
            let u = f.uv[(y / 2) * w + (x & !1)] as f32 - 128.0;
            let v = f.uv[(y / 2) * w + (x & !1) + 1] as f32 - 128.0;
            let c = 1.164 * yy;
            let r = c + 1.793 * v;
            let g = c - 0.213 * u - 0.533 * v;
            let b = c + 2.112 * u;
            let o = (y * w + x) * 3;
            rgb[o] = r.clamp(0.0, 255.0) as u8;
            rgb[o + 1] = g.clamp(0.0, 255.0) as u8;
            rgb[o + 2] = b.clamp(0.0, 255.0) as u8;
        }
    }
    let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), f.w, f.h);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let mut wr = enc.write_header().map_err(|e| e.to_string())?;
    wr.write_image_data(&rgb).map_err(|e| e.to_string())?;
    Ok(())
}

fn cmd_audio(args: &[String]) -> Result<(), String> {
    let target = if let Some(p) = opt(args, "--pid") {
        AudioTarget::ProcessTree {
            pid: parse_int(p)? as u32,
        }
    } else if let Some(p) = opt(args, "--exclude-pid") {
        AudioTarget::SystemExcludingProcessTree {
            pid: parse_int(p)? as u32,
        }
    } else {
        return Err("need --pid P or --exclude-pid P".into());
    };
    let secs = opt(args, "--secs").map(parse_int).transpose()?.unwrap_or(5) as u64;
    let out = opt(args, "--out")
        .unwrap_or("capture_probe.wav")
        .to_string();

    #[derive(Default)]
    struct Acc {
        samples: Vec<i16>,
        stamps: Vec<i64>,
        arrivals: Vec<i64>,
    }
    let acc = Arc::new(Mutex::new(Acc::default()));
    let a2 = acc.clone();
    let cpu0 = ProcSample::now();
    let cap = start_audio(
        target,
        Box::new(move |c: &AudioChunk<'_>| {
            let mut a = a2.lock().unwrap();
            a.samples.extend_from_slice(c.samples);
            a.stamps.push(c.timestamp_us);
            a.arrivals.push(monotonic_now_us());
        }),
    )
    .map_err(|e| e.to_string())?;
    println!("capturing audio {target:?} for {secs}s");
    std::thread::sleep(Duration::from_secs(secs));
    let cpu1 = ProcSample::now();
    drop(cap);

    let a = acc.lock().unwrap();
    let n = a.samples.len();
    let rms = if n == 0 {
        0.0
    } else {
        (a.samples.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / n as f64).sqrt()
    };
    let peak = a
        .samples
        .iter()
        .map(|&s| (s as i32).abs())
        .max()
        .unwrap_or(0);
    let nonzero = a.samples.iter().filter(|&&s| s != 0).count();
    let dbfs = if rms > 0.0 {
        20.0 * (rms / 32768.0).log10()
    } else {
        f64::NEG_INFINITY
    };
    let gaps: Vec<i64> = a.stamps.windows(2).map(|w| w[1] - w[0]).collect();
    let arr_gaps: Vec<i64> = a.arrivals.windows(2).map(|w| w[1] - w[0]).collect();
    println!("--- audio results ---");
    println!(
        "chunks: {} (expected ~{}), samples {}",
        a.stamps.len(),
        secs * 100,
        n
    );
    println!(
        "timestamp step: min {} us, max {} us | callback arrival gap: max {} us",
        gaps.iter().min().unwrap_or(&0),
        gaps.iter().max().unwrap_or(&0),
        arr_gaps.iter().max().unwrap_or(&0)
    );
    println!(
        "RMS {rms:.1} ({dbfs:.1} dBFS), peak {peak}, nonzero samples {:.2}%",
        100.0 * nonzero as f64 / n.max(1) as f64
    );
    if rms > 30.0 {
        // Rough dominant frequency from upward zero crossings of the left channel (fine for a tone).
        let left: Vec<i16> = a.samples.iter().step_by(2).copied().collect();
        let ups = left.windows(2).filter(|w| w[0] < 0 && w[1] >= 0).count();
        println!(
            "est. frequency: {:.0} Hz",
            ups as f64 * AUDIO_SAMPLE_RATE as f64 / left.len().max(1) as f64
        );
    }
    print_proc(&cpu0, &cpu1);

    let spec = hound::WavSpec {
        channels: AUDIO_CHANNELS,
        sample_rate: AUDIO_SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(&out, spec).map_err(|e| e.to_string())?;
    for &s in &a.samples {
        w.write_sample(s).map_err(|e| e.to_string())?;
    }
    w.finalize().map_err(|e| e.to_string())?;
    println!("wrote {out}");
    Ok(())
}

// --- process CPU / memory --------------------------------------------------------------------

struct ProcSample {
    wall: Instant,
    cpu_100ns: u64,
}

impl ProcSample {
    fn now() -> Self {
        use windows::Win32::Foundation::FILETIME;
        use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
        let (mut c, mut e, mut k, mut u) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        unsafe {
            let _ = GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u);
        }
        let ft = |f: FILETIME| ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64;
        Self {
            wall: Instant::now(),
            cpu_100ns: ft(k) + ft(u),
        }
    }
}

fn print_proc(a: &ProcSample, b: &ProcSample) {
    use windows::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
    };
    use windows::Win32::System::Threading::GetCurrentProcess;
    let wall = (b.wall - a.wall).as_secs_f64();
    let cpu = (b.cpu_100ns - a.cpu_100ns) as f64 / 1e7;
    let ncpu = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1) as f64;
    println!(
        "process CPU: {:.2}% of one core, {:.2}% of total ({} logical CPUs)",
        100.0 * cpu / wall,
        100.0 * cpu / wall / ncpu,
        ncpu
    );
    let mut pmc = PROCESS_MEMORY_COUNTERS_EX {
        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ..Default::default()
    };
    unsafe {
        let _ = GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut pmc as *mut _ as *mut PROCESS_MEMORY_COUNTERS,
            pmc.cb,
        );
    }
    println!(
        "memory: working set {:.1} MB (peak {:.1} MB), private bytes {:.1} MB",
        pmc.WorkingSetSize as f64 / 1048576.0,
        pmc.PeakWorkingSetSize as f64 / 1048576.0,
        pmc.PrivateUsage as f64 / 1048576.0
    );
}

// --- test tone on a named (virtual) endpoint ---------------------------------------------------

/// Plays a 440 Hz tone via WASAPI on the ONE active render endpoint whose friendly name contains
/// `--device` (case-insensitive), e.g. "Steam Streaming Speakers", so a real app-audio source can
/// be captured without anything reaching the speakers. Never touches the default device: if no
/// endpoint (or more than one) matches, it errors out instead of falling back.
fn cmd_tone(args: &[String]) -> Result<()> {
    use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
    use windows::Win32::Media::Audio::*;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ,
    };
    fn e(c: &'static str) -> impl Fn(windows::core::Error) -> String {
        move |err| format!("{c}: {err}")
    }
    let want = opt(args, "--device")
        .ok_or("tone requires --device <endpoint name substring>")?
        .to_lowercase();
    let secs = opt(args, "--secs")
        .map(parse_int)
        .transpose()?
        .unwrap_or(30) as u64;
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let en: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(e("enumerator"))?;
        let coll = en
            .EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
            .map_err(e("EnumAudioEndpoints"))?;
        let mut all = Vec::new();
        let mut matches = Vec::new();
        for i in 0..coll.GetCount().map_err(e("GetCount"))? {
            let dev = coll.Item(i).map_err(e("Item"))?;
            let name = dev
                .OpenPropertyStore(STGM_READ)
                .and_then(|ps| ps.GetValue(&PKEY_Device_FriendlyName))
                .map(|v| v.to_string())
                .unwrap_or_default();
            if name.to_lowercase().contains(&want) {
                matches.push((name.clone(), dev));
            }
            all.push(name);
        }
        let (name, dev) = match matches.len() {
            1 => matches.pop().unwrap(),
            0 => {
                return Err(format!(
                    "no active render endpoint matches '{want}'. Available: {all:?}"
                ))
            }
            _ => {
                let names: Vec<_> = matches.iter().map(|m| m.0.clone()).collect();
                return Err(format!("'{want}' is ambiguous: {names:?}"));
            }
        };
        let id = dev.GetId().map_err(e("GetId"))?;
        let id_str = id.to_string().unwrap_or_default();
        windows::Win32::System::Com::CoTaskMemFree(Some(id.0 as *const _));

        let client: IAudioClient = dev.Activate(CLSCTX_ALL, None).map_err(e("activate"))?;
        let wfx = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM as u16,
            nChannels: 2,
            nSamplesPerSec: 48000,
            nAvgBytesPerSec: 192000,
            nBlockAlign: 4,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                1_000_000,
                0,
                &wfx,
                None,
            )
            .map_err(e("initialize"))?;
        let render: IAudioRenderClient = client.GetService().map_err(e("IAudioRenderClient"))?;
        let buf_frames = client.GetBufferSize().map_err(e("GetBufferSize"))?;
        println!(
            "tone pid={} 440 Hz on '{name}' ({id_str}) for {secs}s",
            std::process::id()
        );
        client.Start().map_err(e("start"))?;
        let start = Instant::now();
        let mut phase = 0f64;
        while start.elapsed() < Duration::from_secs(secs) {
            let pad = client.GetCurrentPadding().map_err(e("padding"))?;
            let n = buf_frames - pad;
            if n > 0 {
                let p = render.GetBuffer(n).map_err(e("GetBuffer"))? as *mut i16;
                let s = std::slice::from_raw_parts_mut(p, n as usize * 2);
                for f in 0..n as usize {
                    let v = (phase.sin() * 8000.0) as i16;
                    phase = (phase + 2.0 * std::f64::consts::PI * 440.0 / 48000.0)
                        % (2.0 * std::f64::consts::PI);
                    s[2 * f] = v;
                    s[2 * f + 1] = v;
                }
                render.ReleaseBuffer(n, 0).map_err(e("ReleaseBuffer"))?;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = client.Stop();
    }
    Ok(())
}

// --- start/stop cycling -----------------------------------------------------------------------

/// `cycle [--rounds N] [--monitor M] [--pid P]`: start -> run 1.5 s -> drop, N times, for video
/// (monitor M) and then audio (ProcessTree of P, default: this process, i.e. silent). Logs working
/// set and handle count each round to spot leaks. Silent sources only.
fn cmd_cycle(args: &[String]) -> Result<()> {
    let rounds = opt(args, "--rounds")
        .map(parse_int)
        .transpose()?
        .unwrap_or(5);
    let mon = opt(args, "--monitor")
        .map(parse_int)
        .transpose()?
        .unwrap_or(0) as usize;
    let pid = opt(args, "--pid")
        .map(parse_int)
        .transpose()?
        .map(|p| p as u32)
        .unwrap_or(std::process::id());
    for r in 1..=rounds {
        let n = Arc::new(AtomicU64::new(0));
        let n2 = n.clone();
        let cap = start_video(
            VideoTarget::Monitor(mon),
            VideoConfig::default(),
            Box::new(move |_f: &Nv12Frame<'_>| {
                n2.fetch_add(1, Ordering::Relaxed);
            }),
        )
        .map_err(|e| format!("video round {r}: {e}"))?;
        std::thread::sleep(Duration::from_millis(1500));
        let running = cap.is_running();
        drop(cap);
        println!(
            "video round {r}: frames={} running={running} | {}",
            n.load(Ordering::Relaxed),
            proc_counters()
        );
    }
    for r in 1..=rounds {
        let n = Arc::new(AtomicU64::new(0));
        let n2 = n.clone();
        let cap = start_audio(
            AudioTarget::ProcessTree { pid },
            Box::new(move |_c: &AudioChunk<'_>| {
                n2.fetch_add(1, Ordering::Relaxed);
            }),
        )
        .map_err(|e| format!("audio round {r}: {e}"))?;
        std::thread::sleep(Duration::from_millis(1500));
        let running = cap.is_running();
        drop(cap);
        println!(
            "audio round {r}: chunks={} running={running} | {}",
            n.load(Ordering::Relaxed),
            proc_counters()
        );
    }
    println!("cycle ok");
    Ok(())
}

fn proc_counters() -> String {
    use windows::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};
    let mut pmc = PROCESS_MEMORY_COUNTERS_EX {
        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ..Default::default()
    };
    let mut handles = 0u32;
    unsafe {
        let _ = GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut pmc as *mut _ as *mut PROCESS_MEMORY_COUNTERS,
            pmc.cb,
        );
        let _ = GetProcessHandleCount(GetCurrentProcess(), &mut handles);
    }
    format!(
        "ws {:.1} MB, private {:.1} MB, handles {handles}",
        pmc.WorkingSetSize as f64 / 1048576.0,
        pmc.PrivateUsage as f64 / 1048576.0
    )
}

// --- hardware encode ---------------------------------------------------------------------------

fn parse_backend(args: &[String]) -> Result<EncoderBackend> {
    Ok(match opt(args, "--backend").unwrap_or("auto") {
        "auto" => EncoderBackend::Auto,
        "nvenc" => EncoderBackend::Nvenc,
        "mf" => EncoderBackend::MediaFoundation,
        o => return Err(format!("unknown backend '{o}' (auto|nvenc|mf)")),
    })
}

fn cmd_encoders() -> Result<()> {
    let t = Instant::now();
    let v = available_encoders();
    println!(
        "available_encoders: {v:?} (probe {:.0} ms)",
        t.elapsed().as_secs_f64() * 1000.0
    );
    let t = Instant::now();
    let v2 = available_encoders();
    println!(
        "second call (cached): {v2:?} ({:.3} ms)",
        t.elapsed().as_secs_f64() * 1000.0
    );
    Ok(())
}

/// Samples `nvidia-smi` (encoder %, GPU %) every 500 ms in the background.
struct GpuSampler {
    child: Option<std::process::Child>,
    samples: Arc<Mutex<Vec<(u32, u32)>>>,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl GpuSampler {
    fn start() -> Self {
        use std::io::BufRead;
        let samples = Arc::new(Mutex::new(Vec::new()));
        let child = std::process::Command::new("nvidia-smi")
            .args([
                "--query-gpu=utilization.encoder,utilization.gpu",
                "--format=csv,noheader,nounits",
                "-lms",
                "500",
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok();
        let mut me = Self {
            child,
            samples: samples.clone(),
            reader: None,
        };
        if let Some(out) = me.child.as_mut().and_then(|c| c.stdout.take()) {
            me.reader = Some(std::thread::spawn(move || {
                for line in std::io::BufReader::new(out).lines().map_while(|l| l.ok()) {
                    let mut it = line.split(',').map(|x| x.trim().parse::<u32>());
                    if let (Some(Ok(e)), Some(Ok(g))) = (it.next(), it.next()) {
                        samples.lock().unwrap().push((e, g));
                    }
                }
            }));
        }
        me
    }

    /// Stop and return (avg enc %, max enc %, avg gpu %, n).
    fn stop(mut self) -> Option<(f64, u32, f64, usize)> {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        if let Some(r) = self.reader.take() {
            let _ = r.join();
        }
        let s = self.samples.lock().unwrap();
        // Skip the first sample (covers encoder start-up).
        let s: Vec<_> = s.iter().skip(1).copied().collect();
        if s.is_empty() {
            return None;
        }
        let n = s.len() as f64;
        Some((
            s.iter().map(|x| x.0 as f64).sum::<f64>() / n,
            s.iter().map(|x| x.0).max().unwrap_or(0),
            s.iter().map(|x| x.1 as f64).sum::<f64>() / n,
            s.len(),
        ))
    }
}

impl Drop for GpuSampler {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

#[derive(Default)]
struct EncLog {
    frames: u64,
    bytes: u64,
    keyframes: Vec<u64>,
    key_requests: Vec<u64>,
    /// (frame index, w, h, keyframe) whenever the size changes.
    size_changes: Vec<(u64, u32, u32, bool)>,
    last_size: (u32, u32),
    /// Bytes per bitrate phase (index = phase).
    phase_bytes: [u64; 3],
    phase_frames: [u64; 3],
    first_has_sps: Option<bool>,
    out: Option<std::io::BufWriter<std::fs::File>>,
    write_err: Option<String>,
}

fn cmd_encode(args: &[String]) -> Result<()> {
    use std::io::Write;
    use std::sync::atomic::AtomicUsize;
    let target = if let Some(m) = opt(args, "--monitor") {
        VideoTarget::Monitor(parse_int(m)? as usize)
    } else if let Some(w) = opt(args, "--window") {
        VideoTarget::Window(parse_int(w)? as isize)
    } else {
        return Err("need --monitor N or --window HWND".into());
    };
    let secs = opt(args, "--secs").map(parse_int).transpose()?.unwrap_or(10) as u64;
    let fps = opt(args, "--fps").map(parse_int).transpose()?.unwrap_or(30) as u32;
    let (mw, mh) = match opt(args, "--max") {
        Some(s) => {
            let (a, b) = s.split_once('x').ok_or("--max WxH")?;
            (parse_int(a)? as u32, parse_int(b)? as u32)
        }
        None => (1920, 1080),
    };
    let bitrate = opt(args, "--bitrate")
        .map(parse_int)
        .transpose()?
        .unwrap_or(if fps > 30 { 6_000_000 } else { 4_000_000 }) as u32;
    let backend = parse_backend(args)?;
    let key_every = opt(args, "--force-key-every")
        .map(parse_int)
        .transpose()?
        .unwrap_or(0) as u64;
    let step = args.iter().any(|a| a == "--bitrate-step");
    let out_path = opt(args, "--out").map(str::to_string);

    let log = Arc::new(Mutex::new(EncLog::default()));
    if let Some(p) = &out_path {
        log.lock().unwrap().out = Some(std::io::BufWriter::with_capacity(
            1 << 20,
            std::fs::File::create(p).map_err(|e| format!("{p}: {e}"))?,
        ));
    }
    let phase = Arc::new(AtomicUsize::new(0));
    let control_slot: Arc<std::sync::OnceLock<EncoderControl>> = Arc::default();

    let cfg = EncodeConfig {
        video: VideoConfig {
            max_fps: fps,
            max_width: mw,
            max_height: mh,
            capture_cursor: !args.iter().any(|a| a == "--no-cursor"),
        },
        bitrate_bps: bitrate,
        backend,
        degradation: if args.iter().any(|a| a == "--smooth") {
            DegradationPreference::Smoothness
        } else {
            DegradationPreference::Sharpness
        },
        adapter_luid: None,
    };
    let cb_log = log.clone();
    let cb_phase = phase.clone();
    let cb_ctl = control_slot.clone();

    // Screen preview tap (item 8a).
    let size_arg = |name: &str| -> Result<Option<(u32, u32)>> {
        match opt(args, name) {
            Some(s) => {
                let (a, b) = s.split_once('x').ok_or(format!("{name} WxH"))?;
                Ok(Some((parse_int(a)? as u32, parse_int(b)? as u32)))
            }
            None => Ok(None),
        }
    };
    let preview_fps = opt(args, "--preview-fps").map(parse_int).transpose()?.unwrap_or(0) as u32;
    let (pw, ph) = size_arg("--preview-size")?.unwrap_or((640, 360));
    let preview_png = opt(args, "--preview-png").map(str::to_string);
    let preview_toggle = opt(args, "--preview-toggle").map(parse_int).transpose()?.map(|v| v as u64);
    let preview_fps2 = opt(args, "--preview-fps2").map(parse_int).transpose()?.map(|v| v as u32);
    let (pw2, ph2) = size_arg("--preview-size2")?.unwrap_or((pw, ph));
    let preview_last: Arc<Mutex<Option<FirstFrame>>> = Arc::new(Mutex::new(None));
    let preview_sizes: Arc<Mutex<Vec<(u32, u32)>>> = Arc::new(Mutex::new(Vec::new()));
    let screen_preview = if preview_fps > 0 {
        let sp = ScreenPreview::new().map_err(|e| e.to_string())?;
        sp.set(PreviewConfig { on: true, fps: preview_fps, max_width: pw, max_height: ph });
        Some(sp)
    } else {
        None
    };
    let preview_cb: Option<FrameCallback> = preview_fps.gt(&0).then(|| {
        let keep = preview_png.is_some();
        let last = preview_last.clone();
        let sizes = preview_sizes.clone();
        let mut next_copy = Instant::now();
        let cb: FrameCallback = Box::new(move |f: &Nv12Frame<'_>| {
            let (w, h) = (f.width as usize, f.height as usize);
            {
                let mut s = sizes.lock().unwrap();
                if s.last() != Some(&(f.width, f.height)) {
                    s.push((f.width, f.height));
                }
            }
            if keep && Instant::now() >= next_copy {
                next_copy = Instant::now() + Duration::from_secs(1);
                let mut y = Vec::with_capacity(w * h);
                let mut uv = Vec::with_capacity(w * h / 2);
                for r in 0..h {
                    y.extend_from_slice(&f.y[r * f.y_stride as usize..][..w]);
                }
                for r in 0..h / 2 {
                    uv.extend_from_slice(&f.uv[r * f.uv_stride as usize..][..w]);
                }
                *last.lock().unwrap() = Some(FirstFrame { w: f.width, h: f.height, y, uv });
            }
        });
        cb
    });
    let t_start = Instant::now();
    let cap = start_video_encoded_with_preview(
        target,
        cfg,
        Box::new(move |f: &EncodedFrame<'_>| {
            let mut l = cb_log.lock().unwrap();
            let idx = l.frames;
            l.frames += 1;
            l.bytes += f.data.len() as u64;
            let ph = cb_phase.load(Ordering::Relaxed);
            l.phase_bytes[ph] += f.data.len() as u64;
            l.phase_frames[ph] += 1;
            if f.keyframe {
                l.keyframes.push(idx);
            }
            if l.first_has_sps.is_none() {
                l.first_has_sps = Some(
                    f.data
                        .windows(4)
                        .any(|w| w[..3] == [0, 0, 1] && w[3] & 0x1f == 7),
                );
            }
            if (f.width, f.height) != l.last_size {
                l.last_size = (f.width, f.height);
                l.size_changes.push((idx, f.width, f.height, f.keyframe));
            }
            if let Some(o) = l.out.as_mut() {
                if let Err(e) = o.write_all(f.data) {
                    l.write_err = Some(e.to_string());
                }
            }
            if key_every > 0 && (idx + 1) % key_every == 0 {
                if let Some(c) = cb_ctl.get() {
                    c.request_keyframe();
                    l.key_requests.push(idx + 1);
                }
            }
        }),
        screen_preview.as_ref().zip(preview_cb),
    )
    .map_err(|e| e.to_string())?;
    let start_ms = t_start.elapsed().as_secs_f64() * 1000.0;
    let control = cap.control();
    let _ = control_slot.set(control.clone());
    println!(
        "encoding {target:?} with {:?} for {secs}s at max {fps} fps, fit {mw}x{mh}, {} kbps (start took {start_ms:.0} ms)",
        cap.backend(),
        bitrate / 1000
    );
    let gpu = GpuSampler::start();
    let cpu0 = ProcSample::now();
    let start = Instant::now();
    let mut prev = cap.stats();
    let prev_start_frames = prev.frames_encoded;
    let mut prev_bytes = 0u64;
    let low = (bitrate / 3).max(100_000);
    let mut prev_preview = 0u64;
    for s in 1..=secs {
        std::thread::sleep(Duration::from_secs(1));
        if let Some(sp) = &screen_preview {
            if let Some(every) = preview_toggle {
                if every > 0 && s % every == 0 {
                    let mut c = sp.config();
                    c.on = !c.on;
                    sp.set(c);
                    println!("  -> preview {}", if c.on { "ON" } else { "OFF" });
                }
            }
            if let Some(f2) = preview_fps2 {
                if s == secs / 2 {
                    sp.set(PreviewConfig { on: true, fps: f2, max_width: pw2, max_height: ph2 });
                    println!("  -> preview {f2} fps, fit {pw2}x{ph2}");
                }
            }
        }
        if step {
            if s == secs / 3 {
                control.set_bitrate(low);
                phase.store(1, Ordering::Relaxed);
                println!("  -> set_bitrate({low})");
            } else if s == 2 * secs / 3 {
                control.set_bitrate(bitrate);
                phase.store(2, Ordering::Relaxed);
                println!("  -> set_bitrate({bitrate})");
            }
        }
        let st = cap.stats();
        let (bytes, size) = {
            let l = log.lock().unwrap();
            (l.bytes, l.last_size)
        };
        println!(
            "  t={s:>3}s  encoded {:>3}/s  dropped {:>3}/s  {:>6.0} kbps  key {:>2}  size {}x{}  avg_enc {} us  stats.bitrate {} kbps  running={}",
            st.frames_encoded - prev.frames_encoded,
            st.frames_dropped - prev.frames_dropped,
            (bytes - prev_bytes) as f64 * 8.0 / 1000.0,
            st.keyframes - prev.keyframes,
            size.0,
            size.1,
            st.avg_encode_us,
            st.bitrate_bps / 1000,
            cap.is_running()
        );
        if let Some(sp) = &screen_preview {
            let n = sp.frames();
            println!("          preview: +{} frames ({:?})", n - prev_preview, sp.config());
            prev_preview = n;
        }
        prev = st;
        prev_bytes = bytes;
        if !cap.is_running() {
            println!("  capture ended on its own");
            break;
        }
    }
    let elapsed = start.elapsed().as_secs_f64();
    let cpu1 = ProcSample::now();
    let gpu = gpu.stop();
    let st = cap.stats();
    let backend_used = cap.backend();
    let t_drop = Instant::now();
    drop(cap);
    let drop_ms = t_drop.elapsed().as_secs_f64() * 1000.0;

    let mut l = log.lock().unwrap();
    if let Some(mut o) = l.out.take() {
        let _ = o.flush();
    }
    println!("--- encode results ({backend_used:?}) ---");
    println!(
        "frames: {} ({:.1} fps), dropped {} (pre-GPU pacing/busy), keyframes {}",
        st.frames_encoded,
        st.frames_encoded as f64 / elapsed,
        st.frames_dropped,
        st.keyframes
    );
    println!("avg encode latency (submit->output): {} us", st.avg_encode_us);
    let total_kbps = l.bytes as f64 * 8.0 / elapsed / 1000.0;
    println!(
        "output bitrate: {total_kbps:.0} kbps avg vs target {} kbps ({:.0}%)",
        bitrate / 1000,
        100.0 * total_kbps * 1000.0 / bitrate as f64
    );
    if step {
        let third = elapsed / 3.0;
        for (i, tgt) in [bitrate, low, bitrate].iter().enumerate() {
            println!(
                "  phase {i}: target {} kbps -> measured {:.0} kbps ({} frames)",
                tgt / 1000,
                l.phase_bytes[i] as f64 * 8.0 / third / 1000.0,
                l.phase_frames[i]
            );
        }
    }
    println!(
        "first AU has SPS: {:?}; keyframes at frames {:?}",
        l.first_has_sps,
        &l.keyframes[..l.keyframes.len().min(40)]
    );
    if key_every > 0 {
        let honoured = l
            .key_requests
            .iter()
            .filter(|&&r| l.keyframes.iter().any(|&k| k >= r && k <= r + 2))
            .count();
        println!(
            "keyframe requests: {} at {:?}, honoured within 2 frames: {honoured}",
            l.key_requests.len(),
            &l.key_requests[..l.key_requests.len().min(20)]
        );
    }
    println!("size changes (frame, w, h, key): {:?}", l.size_changes);
    print_proc(&cpu0, &cpu1);
    let enc_frames = st.frames_encoded.saturating_sub(prev_start_frames).max(1);
    println!(
        "process CPU per encoded frame: {:.0} us",
        (cpu1.cpu_100ns - cpu0.cpu_100ns) as f64 / 10.0 / enc_frames as f64
    );
    match gpu {
        Some((e, emax, g, n)) => println!(
            "GPU (nvidia-smi, {n} samples): encoder {e:.1}% avg / {emax}% max, gpu {g:.1}% avg"
        ),
        None => println!("GPU: nvidia-smi not available"),
    }
    println!("drop(EncodedVideoCapture) took {drop_ms:.1} ms");
    if let Some(sp) = &screen_preview {
        println!("preview: {} frames delivered, sizes seen {:?}", sp.frames(), preview_sizes.lock().unwrap());
        if let (Some(p), Some(f)) = (&preview_png, preview_last.lock().unwrap().take()) {
            save_png(&f, p)?;
            println!("last preview frame {}x{} saved to {p}", f.w, f.h);
        }
    }
    if let Some(e) = &l.write_err {
        println!("write error: {e}");
    }
    if let Some(p) = &out_path {
        println!("wrote {p}");
    }
    Ok(())
}

/// `encode-cycle`: start -> 1.5 s -> drop, N times. Logs handles / private bytes per round.
fn cmd_encode_cycle(args: &[String]) -> Result<()> {
    let rounds = opt(args, "--rounds").map(parse_int).transpose()?.unwrap_or(5);
    let mon = opt(args, "--monitor").map(parse_int).transpose()?.unwrap_or(0) as usize;
    let backend = parse_backend(args)?;
    println!("before: {}", proc_counters());
    for r in 1..=rounds {
        let n = Arc::new(AtomicU64::new(0));
        let n2 = n.clone();
        let cap = start_video_encoded(
            VideoTarget::Monitor(mon),
            EncodeConfig {
                video: VideoConfig::default(),
                bitrate_bps: 4_000_000,
                backend,
                ..Default::default()
            },
            Box::new(move |_f: &EncodedFrame<'_>| {
                n2.fetch_add(1, Ordering::Relaxed);
            }),
        )
        .map_err(|e| format!("round {r}: {e}"))?;
        let used = cap.backend();
        std::thread::sleep(Duration::from_millis(1500));
        let running = cap.is_running();
        drop(cap);
        println!(
            "encode round {r} ({used:?}): frames={} running={running} | {}",
            n.load(Ordering::Relaxed),
            proc_counters()
        );
    }
    println!("encode-cycle ok");
    Ok(())
}

// --- self-made animated test window ----------------------------------------------------------

mod testwin {
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
    use std::sync::Mutex;

    use windows::core::w;
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows::Win32::Graphics::Gdi::*;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::*;

    pub static FRAME: AtomicU64 = AtomicU64::new(0);
    pub static NOISE_PCT: AtomicU32 = AtomicU32::new(25);
    static BUF: Mutex<Vec<u32>> = Mutex::new(Vec::new());

    fn xorshift(s: &mut u32) -> u32 {
        *s ^= *s << 13;
        *s ^= *s >> 17;
        *s ^= *s << 5;
        *s
    }

    unsafe fn paint(hwnd: HWND) {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        let (w, h) = (
            (rc.right - rc.left).max(1) as usize,
            (rc.bottom - rc.top).max(1) as usize,
        );
        let t = FRAME.load(Ordering::Relaxed) as usize;
        let mut buf = BUF.lock().unwrap();
        buf.resize(w * h, 0);
        // Scrolling colour gradient + moving bar (easy to encode) ...
        for y in 0..h {
            let g = ((y * 255 / h) as u32) << 8;
            let row = &mut buf[y * w..(y + 1) * w];
            for (x, px) in row.iter_mut().enumerate() {
                let r = (((x + t * 4) % w) * 255 / w) as u32;
                *px = (r << 16) | g | 0x40;
            }
        }
        let bar = (t * 6) % w;
        for y in 0..h {
            for x in bar..(bar + 32).min(w) {
                buf[y * w + x] = 0x00FF_FFFF;
            }
        }
        // ... plus a block of changing noise (hard to encode), like busy real screen content.
        let pct = NOISE_PCT.load(Ordering::Relaxed) as usize;
        let nh = h * pct / 100;
        let mut seed = (t as u32).wrapping_mul(2_654_435_761) | 1;
        for y in h - nh..h {
            for x in 0..w / 2 {
                buf[y * w + x] = xorshift(&mut seed) & 0x00FF_FFFF;
            }
        }
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                biHeight: -(h as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        SetDIBitsToDevice(
            hdc,
            0,
            0,
            w as u32,
            h as u32,
            0,
            0,
            0,
            h as u32,
            buf.as_ptr() as *const _,
            &bmi,
            DIB_RGB_COLORS,
        );
        let _ = EndPaint(hwnd, &ps);
    }

    unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        match msg {
            WM_PAINT => {
                paint(hwnd);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }

    pub fn run(
        fps: u32,
        secs: u64,
        resize_every: Option<f64>,
        topmost: bool,
        size: (i32, i32),
        at: Option<(i32, i32)>,
    ) -> Result<(), String> {
        use std::io::Write;
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::WAIT_OBJECT_0;
        use windows::Win32::System::Threading::{
            CreateWaitableTimerExW, SetWaitableTimer, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
            TIMER_ALL_ACCESS,
        };
        unsafe {
            let hinst = GetModuleHandleW(None).map_err(|e| e.to_string())?;
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: hinst.into(),
                lpszClassName: w!("VoxEncTestWindow"),
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                ..Default::default()
            };
            RegisterClassW(&wc);
            // Default position: bottom-right corner of the primary monitor.
            let (x, y) = at.unwrap_or_else(|| {
                let (sw, sh) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
                (sw - size.0 - 16, sh - size.1 - 64)
            });
            // Topmost + no-activate: always visible for monitor-capture tests, never steals focus.
            let ex = if topmost {
                WS_EX_TOPMOST | WS_EX_NOACTIVATE
            } else {
                WINDOW_EX_STYLE(0)
            };
            let hwnd = CreateWindowExW(
                ex,
                w!("VoxEncTestWindow"),
                w!("vox encoder test window"),
                WS_OVERLAPPEDWINDOW,
                x,
                y,
                size.0,
                size.1,
                None,
                None,
                Some(hinst.into()),
                None,
            )
            .map_err(|e| e.to_string())?;
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            println!("hwnd={} pid={}", hwnd.0 as isize, std::process::id());
            let _ = std::io::stdout().flush();
            let timer = CreateWaitableTimerExW(
                None,
                PCWSTR::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS.0,
            )
            .map_err(|e| e.to_string())?;
            let period_100ns = 10_000_000i64 / fps.max(1) as i64;
            let start = std::time::Instant::now();
            let mut next = start;
            let mut last_resize = start;
            let mut big = true;
            let mut msg = MSG::default();
            'outer: loop {
                next += std::time::Duration::from_nanos(period_100ns as u64 * 100);
                let now = std::time::Instant::now();
                let wait = next.saturating_duration_since(now);
                if wait.is_zero() {
                    next = now;
                }
                let due = -((wait.as_nanos() / 100) as i64).max(1);
                let _ = SetWaitableTimer(timer, &due, 0, None, None, false);
                loop {
                    let r = MsgWaitForMultipleObjects(Some(&[timer]), false, 1000, QS_ALLINPUT);
                    while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                        if msg.message == WM_QUIT {
                            break 'outer;
                        }
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                    if r == WAIT_OBJECT_0 {
                        break;
                    }
                }
                FRAME.fetch_add(1, Ordering::Relaxed);
                let _ = InvalidateRect(Some(hwnd), None, false);
                let _ = UpdateWindow(hwnd);
                if start.elapsed().as_secs() >= secs {
                    let _ = DestroyWindow(hwnd);
                }
                if let Some(every) = resize_every {
                    if last_resize.elapsed().as_secs_f64() >= every {
                        last_resize = std::time::Instant::now();
                        big = !big;
                        let (w, h) = if big { size } else { (size.0 * 3 / 4, size.1 * 5 / 6) };
                        let _ = SetWindowPos(
                            hwnd,
                            None,
                            0,
                            0,
                            w,
                            h,
                            SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
                        );
                        println!("resized to {w}x{h}");
                        let _ = std::io::stdout().flush();
                    }
                }
            }
            let _ = windows::Win32::Foundation::CloseHandle(timer);
        }
        Ok(())
    }
}

fn cmd_testwin(args: &[String]) -> Result<()> {
    let fps = opt(args, "--fps").map(parse_int).transpose()?.unwrap_or(60) as u32;
    let secs = opt(args, "--secs").map(parse_int).transpose()?.unwrap_or(120) as u64;
    let noise = opt(args, "--noise").map(parse_int).transpose()?.unwrap_or(25) as u32;
    testwin::NOISE_PCT.store(noise.min(100), Ordering::Relaxed);
    let resize_every = opt(args, "--resize-every")
        .map(|s| s.parse::<f64>().map_err(|e| e.to_string()))
        .transpose()?;
    let pair = |name: &str, sep: char| -> Result<Option<(i32, i32)>> {
        match opt(args, name) {
            Some(s) => {
                let (a, b) = s.split_once(sep).ok_or(format!("{name} A{sep}B"))?;
                Ok(Some((parse_int(a)? as i32, parse_int(b)? as i32)))
            }
            None => Ok(None),
        }
    };
    let size = pair("--size", 'x')?.unwrap_or((1280, 720));
    let at = pair("--at", ',')?;
    testwin::run(
        fps,
        secs,
        resize_every,
        args.iter().any(|a| a == "--topmost"),
        size,
        at,
    )
}

// --- webcam ------------------------------------------------------------------------------------

/// `capture_probe cameras`: list the video capture devices.
fn cmd_cameras() -> Result<()> {
    let t = Instant::now();
    let cams = list_cameras().map_err(|e| e.to_string())?;
    println!("{} camera(s) ({:.0} ms):", cams.len(), t.elapsed().as_secs_f64() * 1000.0);
    for c in cams {
        println!("  {:<32} {}", c.name, c.id);
    }
    Ok(())
}

/// `capture_probe camera`: run a camera through the real path and print frame counts, rates,
/// bitrate, CPU and memory. Frames are only counted: nothing is written anywhere (privacy).
fn cmd_camera(args: &[String]) -> Result<()> {
    let device = opt(args, "--device").unwrap_or("").to_string();
    let secs = opt(args, "--secs").map(parse_int).transpose()?.unwrap_or(10) as u64;
    let fps = opt(args, "--fps").map(parse_int).transpose()?.unwrap_or(30) as u32;
    let (w, h) = match opt(args, "--size") {
        Some(s) => {
            let (a, b) = s.split_once('x').ok_or("--size WxH")?;
            (parse_int(a)? as u32, parse_int(b)? as u32)
        }
        None => (1280, 720),
    };
    let bitrate = opt(args, "--bitrate").map(parse_int).transpose()?.unwrap_or(1_200_000) as u32;
    let mode = opt(args, "--mode").unwrap_or("encoded");
    let preview = args.iter().any(|a| a == "--preview");
    let cfg = CameraConfig { width: w, height: h, fps, bitrate_bps: bitrate, backend: parse_backend(args)?, ..Default::default() };
    let bytes = Arc::new(AtomicU64::new(0));
    let keys = Arc::new(AtomicU64::new(0));
    let frames = Arc::new(AtomicU64::new(0));
    let previews = Arc::new(AtomicU64::new(0));
    let pv = previews.clone();
    let on_preview: Option<FrameCallback> = preview.then(|| -> FrameCallback {
        Box::new(move |_f: &Nv12Frame<'_>| {
            pv.fetch_add(1, Ordering::Relaxed);
        })
    });
    let before = proc_counters();
    let t0 = Instant::now();
    let cam = match mode {
        "encoded" => {
            let (b, k, f) = (bytes.clone(), keys.clone(), frames.clone());
            start_camera_encoded(
                &device,
                cfg,
                Box::new(move |e: &EncodedFrame<'_>| {
                    b.fetch_add(e.data.len() as u64, Ordering::Relaxed);
                    f.fetch_add(1, Ordering::Relaxed);
                    if e.keyframe {
                        k.fetch_add(1, Ordering::Relaxed);
                    }
                }),
                on_preview,
            )
        }
        "raw" => {
            let f = frames.clone();
            start_camera_raw(
                &device,
                cfg,
                Box::new(move |_n: &Nv12Frame<'_>| {
                    f.fetch_add(1, Ordering::Relaxed);
                }),
                on_preview,
            )
        }
        "preview" => {
            let pv = previews.clone();
            start_camera_preview(
                &device,
                cfg,
                Box::new(move |_n: &Nv12Frame<'_>| {
                    pv.fetch_add(1, Ordering::Relaxed);
                }),
            )
        }
        o => return Err(format!("unknown --mode {o} (encoded|raw|preview)")),
    };
    let cam = cam.map_err(|e| format!("start failed: {:?}: {}", e.fault, e.detail))?;
    println!("started in {:.0} ms: {:?}, backend {:?}", t0.elapsed().as_secs_f64() * 1000.0, cam.format(), cam.backend());
    println!("before: {before}");
    // Skip the first second (start-up), then measure.
    std::thread::sleep(Duration::from_secs(1));
    let (f0, p0, b0) = (frames.load(Ordering::Relaxed), previews.load(Ordering::Relaxed), bytes.load(Ordering::Relaxed));
    let a = ProcSample::now();
    let end = Instant::now() + Duration::from_secs(secs);
    let mut toggled = false;
    while Instant::now() < end && cam.is_running() {
        std::thread::sleep(Duration::from_millis(250));
        if preview && args.iter().any(|a| a == "--toggle-preview") && !toggled && Instant::now() + Duration::from_secs(secs / 2) > end {
            toggled = true;
            cam.set_preview(false);
            println!("preview off (half time)");
        }
    }
    let b = ProcSample::now();
    let wall = (b.wall - a.wall).as_secs_f64();
    let (f1, p1, b1) = (frames.load(Ordering::Relaxed), previews.load(Ordering::Relaxed), bytes.load(Ordering::Relaxed));
    let st = cam.stats();
    println!(
        "{:.1} s: output {:.1} fps, preview {:.1} fps, {:.0} kbps, keyframes {}, captured {}, dropped {}, encoder {:?}",
        wall,
        (f1 - f0) as f64 / wall,
        (p1 - p0) as f64 / wall,
        (b1 - b0) as f64 * 8.0 / wall / 1000.0,
        keys.load(Ordering::Relaxed),
        st.frames_captured,
        st.encoded.frames_dropped,
        cam.backend()
    );
    print_proc(&a, &b);
    if !cam.is_running() {
        println!("camera stopped early: {:?}", cam.failure());
    }
    let t = Instant::now();
    drop(cam);
    println!("released in {:.0} ms; after: {}", t.elapsed().as_secs_f64() * 1000.0, proc_counters());
    Ok(())
}

// --- picker thumbnails (item 8c) ---------------------------------------------------------------

#[link(name = "user32")]
extern "system" {
    fn PrintWindow(hwnd: isize, hdc: isize, flags: u32) -> i32;
}

/// The PrintWindow(PW_RENDERFULLCONTENT) alternative, for the timing comparison only: window DC
/// -> full-size bitmap -> halftone StretchBlt to the thumbnail size. Blocks while the window's
/// thread answers WM_PRINT (no timeout is possible), which is why it is not the production path.
fn grab_printwindow(hwnd: isize, max_w: u32, max_h: u32) -> Result<RgbaImage, String> {
    use windows::Win32::Foundation::{HWND, RECT};
    use windows::Win32::Graphics::Gdi::*;
    use windows::Win32::UI::WindowsAndMessaging::{GetWindowRect, IsIconic};
    unsafe {
        let h = HWND(hwnd as *mut _);
        if IsIconic(h).as_bool() {
            return Err("minimized".into());
        }
        let mut rc = RECT::default();
        GetWindowRect(h, &mut rc).map_err(|e| e.to_string())?;
        let (w, hh) = ((rc.right - rc.left).max(1), (rc.bottom - rc.top).max(1));
        let s = (max_w as f64 / w as f64).min(max_h as f64 / hh as f64).min(1.0);
        let (tw, th) = (((w as f64 * s) as i32).max(1), ((hh as f64 * s) as i32).max(1));
        let screen = GetDC(None);
        let full_dc = CreateCompatibleDC(Some(screen));
        let full_bmp = CreateCompatibleBitmap(screen, w, hh);
        let old_full = SelectObject(full_dc, HGDIOBJ(full_bmp.0));
        let ok = PrintWindow(hwnd, full_dc.0 as isize, 2);
        let small_dc = CreateCompatibleDC(Some(screen));
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: tw,
                biHeight: -th,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let small_bmp = CreateDIBSection(Some(small_dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0).map_err(|e| e.to_string())?;
        let old_small = SelectObject(small_dc, HGDIOBJ(small_bmp.0));
        SetStretchBltMode(small_dc, HALFTONE);
        let _ = SetBrushOrgEx(small_dc, 0, 0, None);
        let _ = StretchBlt(small_dc, 0, 0, tw, th, Some(full_dc), 0, 0, w, hh, SRCCOPY);
        let _ = GdiFlush();
        let mut data = std::slice::from_raw_parts(bits as *const u8, (tw * th * 4) as usize).to_vec();
        for px in data.chunks_mut(4) {
            px.swap(0, 2);
            px[3] = 255;
        }
        SelectObject(small_dc, old_small);
        SelectObject(full_dc, old_full);
        let _ = DeleteObject(HGDIOBJ(small_bmp.0));
        let _ = DeleteObject(HGDIOBJ(full_bmp.0));
        let _ = DeleteDC(small_dc);
        let _ = DeleteDC(full_dc);
        ReleaseDC(None, screen);
        if ok == 0 {
            return Err("PrintWindow failed".into());
        }
        Ok(RgbaImage { width: tw as u32, height: th as u32, data })
    }
}

fn save_rgba_png(img: &RgbaImage, path: &str) -> Result<(), String> {
    let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), img.width, img.height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut wr = enc.write_header().map_err(|e| e.to_string())?;
    wr.write_image_data(&img.data).map_err(|e| e.to_string())
}

fn cmd_thumbs(args: &[String]) -> Result<(), String> {
    let (mw, mh) = match opt(args, "--max") {
        Some(s) => {
            let (a, b) = s.split_once('x').ok_or("--max WxH")?;
            (parse_int(a)? as u32, parse_int(b)? as u32)
        }
        None => (240, 135),
    };
    let method = opt(args, "--method").unwrap_or("wgc").to_string();
    let timeout = Duration::from_millis(opt(args, "--timeout").map(parse_int).transpose()?.unwrap_or(2500) as u64);

    // One window to a PNG (a window of our own, like `testwin`, so no private content is saved).
    if let Some(w) = opt(args, "--window") {
        let hwnd = parse_int(w)? as isize;
        let t = Instant::now();
        let img = if method == "print" {
            grab_printwindow(hwnd, mw, mh)?
        } else {
            let mut g = ThumbnailGrabber::new().map_err(|e| e.to_string())?;
            g.set_timeout(timeout);
            g.grab(VideoTarget::Window(hwnd), mw, mh).map_err(|e| e.to_string())?
        };
        println!("{method}: {}x{} in {:.0} ms", img.width, img.height, t.elapsed().as_secs_f64() * 1000.0);
        if let Some(p) = opt(args, "--png") {
            save_rgba_png(&img, p)?;
            println!("saved {p}");
        }
        return Ok(());
    }

    let threads = opt(args, "--threads").map(parse_int).transpose()?.unwrap_or(3).max(1) as usize;
    let repeat = opt(args, "--repeat").map(parse_int).transpose()?.unwrap_or(1).max(1);
    let only = opt(args, "--only").unwrap_or("all").to_string();
    // (label, target, minimized)
    let mut jobs: Vec<(String, VideoTarget, bool)> = Vec::new();
    if only != "monitors" {
        for (i, w) in list_windows().map_err(|e| e.to_string())?.into_iter().enumerate() {
            jobs.push((format!("window#{i} {}", w.exe_name), VideoTarget::Window(w.hwnd), is_window_minimized(w.hwnd)));
        }
    }
    if only != "windows" {
        for m in list_monitors().map_err(|e| e.to_string())? {
            jobs.push((format!("monitor#{} {}x{}", m.index, m.width, m.height), VideoTarget::Monitor(m.index), false));
        }
    }
    println!("{} targets, {threads} thread(s), method {method}, timeout {} ms, max {mw}x{mh}", jobs.len(), timeout.as_millis());
    for round in 1..=repeat {
        let queue = Arc::new(Mutex::new(jobs.iter().cloned().collect::<std::collections::VecDeque<_>>()));
        type Row = (String, bool, f64, String);
        let results: Arc<Mutex<Vec<Row>>> = Arc::default();
        let cpu0 = ProcSample::now();
        let t0 = Instant::now();
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                let (queue, results, method) = (queue.clone(), results.clone(), method.clone());
                std::thread::spawn(move || {
                    let mut grabber = if method == "print" {
                        None
                    } else {
                        let mut g = ThumbnailGrabber::new().ok();
                        if let Some(g) = g.as_mut() {
                            g.set_timeout(timeout);
                        }
                        g
                    };
                    loop {
                        let Some((label, target, minimized)) = queue.lock().unwrap().pop_front() else { break };
                        let t = Instant::now();
                        let r: Result<RgbaImage, String> = if minimized {
                            Err("minimized".into())
                        } else if method == "print" {
                            match target {
                                VideoTarget::Window(h) => grab_printwindow(h, mw, mh),
                                VideoTarget::Monitor(_) => Err("n/a".into()),
                            }
                        } else {
                            match grabber.as_mut() {
                                Some(g) => g.grab(target, mw, mh).map_err(|e| e.to_string()),
                                None => Err("no grabber".into()),
                            }
                        };
                        let ms = t.elapsed().as_secs_f64() * 1000.0;
                        let (ok, note) = match r {
                            Ok(i) => (true, format!("{}x{}", i.width, i.height)),
                            Err(e) => (false, e),
                        };
                        results.lock().unwrap().push((label, ok, ms, note));
                    }
                })
            })
            .collect();
        for w in workers {
            let _ = w.join();
        }
        let total = t0.elapsed().as_secs_f64() * 1000.0;
        let cpu1 = ProcSample::now();
        let res = results.lock().unwrap();
        println!("--- round {round}: all {} done in {total:.0} ms", res.len());
        if round == 1 {
            for (label, ok, ms, note) in res.iter() {
                println!("  {:>6.0} ms  {:<4} {:<40} {}", ms, if *ok { "ok" } else { "FAIL" }, label, note);
            }
        }
        let oks: Vec<f64> = res.iter().filter(|r| r.1).map(|r| r.2).collect();
        let max = oks.iter().cloned().fold(0.0, f64::max);
        println!(
            "  ok {} / fail {} | avg {:.0} ms, max {max:.0} ms per ok thumbnail",
            oks.len(),
            res.len() - oks.len(),
            oks.iter().sum::<f64>() / oks.len().max(1) as f64
        );
        print_proc(&cpu0, &cpu1);
    }
    println!("after: {}", proc_counters());
    Ok(())
}
