//! `capture_probe bench`: objective encoder quality benchmark on synthetic scenes.
//!
//! For every scene x fps x config x bitrate: render the scene on the CPU, push it through the
//! real video-processor -> encoder path (`vox_capture::bench::run`), dump the NV12 the encoder
//! received (reference) and the H.264 it produced, then score with ffmpeg (PSNR, SSIM per frame,
//! optionally VMAF). One CSV row per run.
//!
//! ```text
//! capture_probe bench --work DIR [--scenes pointcloud,text,game] [--secs 4] [--size 1920x1080]
//!     [--fps 30,60] [--kbps 4000,6000,8000] [--configs "label=mf:v01;label2=nv:p5,ll,aq8"]
//!     [--input-fps F] [--idr-every N] [--steps "frame:kbps,..."] [--csv out.csv] [--vmaf]
//!     [--keep] [--png]
//! ```
//! Config tokens besides the backend tuning ones: `nocomp` (v0.1: no input-rate compensation),
//! `smooth` / `sharp` (degradation preference), `cap` (honour the fps cap like the pacer).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use vox_capture::bench::{BenchSpec, BenchResult};
use vox_capture::{DegradationPreference, EncoderBackend, Nv12Frame};

use super::scenes::{Scene, SceneGen};
use super::{opt, parse_int};

type Result<T, E = String> = std::result::Result<T, E>;

struct Config {
    label: String,
    backend: EncoderBackend,
    nv: String,
    mf: String,
    comp: bool,
    degradation: DegradationPreference,
    cap: bool,
}

fn parse_configs(s: &str) -> Result<Vec<Config>> {
    let mut out = Vec::new();
    for item in s.split(';').map(str::trim).filter(|x| !x.is_empty()) {
        let (label, rest) = item.split_once('=').ok_or(format!("config {item:?}: label=backend:spec"))?;
        let (backend, spec) = rest.split_once(':').unwrap_or((rest, ""));
        let backend = match backend {
            "mf" => EncoderBackend::MediaFoundation,
            "nv" | "nvenc" => EncoderBackend::Nvenc,
            "auto" => EncoderBackend::Auto,
            o => return Err(format!("config {label}: unknown backend {o:?}")),
        };
        let mut c = Config {
            label: label.to_string(),
            backend,
            nv: String::new(),
            mf: String::new(),
            comp: true,
            degradation: DegradationPreference::Sharpness,
            cap: false,
        };
        let mut tuning = Vec::new();
        for tok in spec.split(',').map(str::trim).filter(|t| !t.is_empty()) {
            match tok {
                "nocomp" => c.comp = false,
                "smooth" => c.degradation = DegradationPreference::Smoothness,
                "sharp" => c.degradation = DegradationPreference::Sharpness,
                "cap" => c.cap = true,
                t => tuning.push(t),
            }
        }
        let t = tuning.join(",");
        match backend {
            EncoderBackend::MediaFoundation => c.mf = t,
            _ => c.nv = t,
        }
        out.push(c);
    }
    Ok(out)
}

fn list<T: std::str::FromStr>(s: &str) -> Result<Vec<T>> {
    s.split(',')
        .map(|x| x.trim().parse::<T>().map_err(|_| format!("bad list item {x:?}")))
        .collect()
}

#[derive(Default, Debug)]
struct Scores {
    psnr: f64,
    psnr_min: f64,
    ssim: Vec<f64>,
    vmaf: Option<(f64, f64)>,
}

fn ffmpeg_score(out: &Path, reference: &Path, w: u32, h: u32, fps: f64, vmaf: bool, work: &Path) -> Result<Scores> {
    let ssim_log = work.join("ssim.log");
    let psnr_log = work.join("psnr.log");
    let vmaf_log = work.join("vmaf.json");
    let esc = |p: &Path| p.display().to_string().replace('\\', "/").replace(':', "\\\\:");
    let mut graph = format!(
        "[0:v]settb=AVTB,setpts=N/({fps})/TB,format=yuv420p[d];[1:v]settb=AVTB,setpts=N/({fps})/TB,format=yuv420p[r];\
         [d]split={n}[d1][d2]{d3};[r]split={n}[r1][r2]{r3};\
         [d1][r1]ssim=stats_file='{}';[d2][r2]psnr=stats_file='{}'",
        esc(&ssim_log),
        esc(&psnr_log),
        n = if vmaf { 3 } else { 2 },
        d3 = if vmaf { "[d3]" } else { "" },
        r3 = if vmaf { "[r3]" } else { "" },
    );
    if vmaf {
        graph.push_str(&format!(
            ";[d3][r3]libvmaf=model=version=vmaf_v0.6.1:n_threads=12:log_fmt=json:log_path='{}'",
            esc(&vmaf_log)
        ));
    }
    let status = Command::new("ffmpeg")
        .args(["-hide_banner", "-nostats", "-loglevel", "error", "-y"])
        .args(["-f", "h264", "-i"])
        .arg(out)
        .args(["-f", "rawvideo", "-pix_fmt", "nv12", "-s", &format!("{w}x{h}"), "-i"])
        .arg(reference)
        .args(["-lavfi", &graph, "-f", "null", "-"])
        .status()
        .map_err(|e| format!("ffmpeg: {e}"))?;
    if !status.success() {
        return Err(format!("ffmpeg failed ({status})"));
    }
    let mut s = Scores::default();
    let txt = std::fs::read_to_string(&ssim_log).map_err(|e| e.to_string())?;
    for line in txt.lines() {
        if let Some(v) = field(line, "All:") {
            s.ssim.push(v);
        }
    }
    let txt = std::fs::read_to_string(&psnr_log).map_err(|e| e.to_string())?;
    let mut ps = Vec::new();
    for line in txt.lines() {
        if let Some(v) = field(line, "psnr_avg:") {
            ps.push(v.min(100.0));
        }
    }
    s.psnr = ps.iter().sum::<f64>() / ps.len().max(1) as f64;
    s.psnr_min = ps.iter().copied().fold(f64::INFINITY, f64::min);
    if vmaf {
        let j = std::fs::read_to_string(&vmaf_log).map_err(|e| e.to_string())?;
        if let Some(i) = j.find("\"pooled_metrics\"") {
            let tail = &j[i..];
            if let Some(k) = tail.find("\"vmaf\"") {
                let t = &tail[k..];
                let num = |key: &str| {
                    t.find(key).and_then(|p| {
                        let r = &t[p + key.len()..];
                        let r = r.trim_start_matches([':', ' ']);
                        let end = r.find([',', '}', '\n']).unwrap_or(r.len());
                        r[..end].trim().parse::<f64>().ok()
                    })
                };
                if let (Some(min), Some(mean)) = (num("\"min\""), num("\"mean\"")) {
                    s.vmaf = Some((mean, min));
                }
            }
        }
    }
    Ok(s)
}

fn field(line: &str, key: &str) -> Option<f64> {
    let i = line.find(key)? + key.len();
    let r = &line[i..];
    let end = r.find(' ').unwrap_or(r.len());
    let v = &r[..end];
    if v == "inf" {
        return Some(100.0);
    }
    v.parse().ok()
}

fn pct(v: &[f64], p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    s[((s.len() - 1) as f64 * p).round() as usize]
}

pub fn cmd_bench(args: &[String]) -> Result<()> {
    let work = PathBuf::from(opt(args, "--work").ok_or("need --work DIR")?);
    std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
    let scenes: Vec<Scene> = opt(args, "--scenes")
        .unwrap_or("pointcloud,text,game")
        .split(',')
        .map(|s| Scene::parse(s.trim()).ok_or(format!("unknown scene {s:?}")))
        .collect::<Result<_>>()?;
    let secs: f64 = opt(args, "--secs").map(|s| s.parse().map_err(|_| "--secs")).transpose()?.unwrap_or(4.0);
    let (w, h) = match opt(args, "--size") {
        Some(s) => {
            let (a, b) = s.split_once('x').ok_or("--size WxH")?;
            (parse_int(a)? as u32, parse_int(b)? as u32)
        }
        None => (1920, 1080),
    };
    let fps_list: Vec<u32> = list(opt(args, "--fps").unwrap_or("30"))?;
    let kbps_list: Vec<u32> = list(opt(args, "--kbps").unwrap_or("6000"))?;
    let configs = parse_configs(opt(args, "--configs").unwrap_or("v01-mf=mf:v01,nocomp;v02-mf=mf:def"))?;
    let input_fps: Option<f64> = opt(args, "--input-fps").map(|s| s.parse().map_err(|_| "--input-fps")).transpose()?;
    let idr_every = opt(args, "--idr-every").map(parse_int).transpose()?.unwrap_or(0) as u32;
    let steps: Vec<(u32, u32)> = match opt(args, "--steps") {
        Some(s) => s
            .split(',')
            .map(|p| {
                let (f, k) = p.split_once(':').ok_or("--steps frame:kbps,...")?;
                Ok::<_, String>((parse_int(f)? as u32, parse_int(k)? as u32 * 1000))
            })
            .collect::<Result<_>>()?,
        None => Vec::new(),
    };
    let vmaf = args.iter().any(|a| a == "--vmaf");
    let keep = args.iter().any(|a| a == "--keep");
    let png = args.iter().any(|a| a == "--png");
    let csv_path = opt(args, "--csv").map(PathBuf::from).unwrap_or_else(|| work.join("bench.csv"));
    let new_csv = !csv_path.exists();
    let mut csv = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&csv_path)
        .map_err(|e| format!("{}: {e}", csv_path.display()))?;
    if new_csv {
        writeln!(
            csv,
            "scene,size,fps,input_fps,config,backend,encoder,target_kbps,frames,out_kbps,psnr,psnr_min,ssim,ssim_p5,ssim_min,vmaf,vmaf_min,\
             idr_n,idr_size_ratio,idr_ssim_drop,max_frame_ms,lat_avg_ms,lat_p95_ms,lat_max_ms,cpu_us_frame,cpu_pct,fps_caps,coalesced"
        )
        .map_err(|e| e.to_string())?;
    }
    let ncpu = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) as f64;
    println!(
        "{:<10} {:>3} {:<18} {:>6} | {:>6} {:>6} {:>6} {:>6} {:>6} | {:>5} {:>5} {:>6} | {:>5} {:>5}",
        "scene", "fps", "config", "kbps", "out", "PSNR", "SSIM", "p5", "min", "VMAF", "vmin", "lat", "cpu", "idr"
    );

    for &scene in &scenes {
        let mut gen = SceneGen::new(scene, w, h);
        if png {
            let mut buf = vec![0u8; (w * h * 4) as usize];
            gen.render(1.0, &mut buf, w as usize * 4);
            let p = work.join(format!("{}.png", scene.name()));
            save_png_bgra(&p, w, h, &buf)?;
            println!("wrote {}", p.display());
        }
        for &fps in &fps_list {
            let in_fps = input_fps.unwrap_or(fps as f64);
            let frames = (secs * in_fps).round() as u32;
            let shared_ref = work.join(format!("ref_{}_{}.yuv", scene.name(), in_fps));
            let mut shared_ref_ok = false;
            for c in &configs {
                for &kbps in &kbps_list {
                    let spec = BenchSpec {
                        width: w,
                        height: h,
                        fps,
                        input_fps: in_fps,
                        frames,
                        bitrate_bps: kbps * 1000,
                        backend: c.backend,
                        nv: c.nv.clone(),
                        mf: c.mf.clone(),
                        rate_compensation: c.comp,
                        idr_every,
                        bitrate_steps: steps.clone(),
                        degradation: c.degradation,
                        apply_fps_cap: c.cap,
                    };
                    let per_run_ref = c.cap;
                    let ref_path = if per_run_ref { work.join("ref_run.yuv") } else { shared_ref.clone() };
                    let write_ref = per_run_ref || !shared_ref_ok;
                    let out_path = work.join("out.h264");
                    let r = run_one(&spec, &mut gen, write_ref.then_some(ref_path.as_path()), &out_path);
                    let res = match r {
                        Ok(v) => v,
                        Err(e) => {
                            println!("{:<10} {:>3} {:<18} {:>6} | error: {e}", scene.name(), fps, c.label, kbps);
                            continue;
                        }
                    };
                    if !per_run_ref {
                        shared_ref_ok = true;
                    }
                    let sc = match ffmpeg_score(&out_path, &ref_path, w & !1, h & !1, in_fps, vmaf, &work) {
                        Ok(s) => s,
                        Err(e) => {
                            println!("{:<10} {:>3} {:<18} {:>6} | scoring error: {e}", scene.name(), fps, c.label, kbps);
                            continue;
                        }
                    };
                    if sc.ssim.len() != res.frames.len() {
                        println!("  warning: {} decoded frames vs {} encoded", sc.ssim.len(), res.frames.len());
                    }
                    let n = res.frames.len().max(1) as f64;
                    let bytes: u64 = res.frames.iter().map(|f| f.bytes as u64).sum();
                    let dur = frames as f64 / in_fps;
                    let out_kbps = bytes as f64 * 8.0 / dur / 1000.0;
                    let ssim_avg = sc.ssim.iter().sum::<f64>() / sc.ssim.len().max(1) as f64;
                    let ssim_min = sc.ssim.iter().copied().fold(f64::INFINITY, f64::min);
                    let ssim_p5 = pct(&sc.ssim, 0.05);
                    // Keyframes after the first frame (requested IDRs).
                    let idr: Vec<usize> = res.frames.iter().enumerate().skip(1).filter(|(_, f)| f.keyframe).map(|(i, _)| i).collect();
                    let p_bytes: Vec<f64> = res.frames.iter().skip(1).filter(|f| !f.keyframe).map(|f| f.bytes as f64).collect();
                    let p_avg = p_bytes.iter().sum::<f64>() / p_bytes.len().max(1) as f64;
                    let idr_ratio = if idr.is_empty() { 0.0 } else { idr.iter().map(|&i| res.frames[i].bytes as f64).sum::<f64>() / idr.len() as f64 / p_avg.max(1.0) };
                    let idr_drop = if idr.is_empty() {
                        0.0
                    } else {
                        idr.iter().filter_map(|&i| sc.ssim.get(i)).map(|v| ssim_avg - v).sum::<f64>() / idr.len() as f64
                    };
                    let max_frame_ms = res.frames.iter().skip(1).map(|f| f.bytes as f64 * 8.0 / (kbps as f64)).fold(0.0, f64::max);
                    let lats: Vec<f64> = res.frames.iter().map(|f| f.latency_us as f64 / 1000.0).collect();
                    let lat_avg = lats.iter().sum::<f64>() / n;
                    let cpu_pct = res.cpu_us_per_frame * in_fps.min(fps as f64) / 1e6 / ncpu * 100.0;
                    let caps = {
                        let mut v: Vec<u32> = res.frames.iter().map(|f| f.fps_cap).collect();
                        v.dedup();
                        v.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(">")
                    };
                    let (vm, vmin) = sc.vmaf.unwrap_or((f64::NAN, f64::NAN));
                    println!(
                        "{:<10} {:>3} {:<18} {:>6} | {:>6.0} {:>6.2} {:>6.4} {:>6.4} {:>6.4} | {:>5.1} {:>5.1} {:>6.2} | {:>5.2} {:>5.2}",
                        scene.name(), fps, c.label, kbps, out_kbps, sc.psnr, ssim_avg, ssim_p5, ssim_min, vm, vmin, lat_avg, cpu_pct, idr_ratio
                    );
                    writeln!(
                        csv,
                        "{},{}x{},{},{},{},{:?},\"{}\",{},{},{:.0},{:.3},{:.3},{:.5},{:.5},{:.5},{:.2},{:.2},{},{:.2},{:.5},{:.1},{:.3},{:.3},{:.3},{:.0},{:.3},{},{}",
                        scene.name(), w, h, fps, in_fps, c.label, res.backend, res.encoder.replace('"', "'"), kbps, res.frames.len(), out_kbps,
                        sc.psnr, sc.psnr_min, ssim_avg, ssim_p5, ssim_min, vm, vmin, idr.len(), idr_ratio, idr_drop, max_frame_ms,
                        lat_avg, pct(&lats, 0.95), lats.iter().copied().fold(0.0, f64::max), res.cpu_us_per_frame, cpu_pct, caps,
                        res.keyframe_requests_coalesced
                    )
                    .map_err(|e| e.to_string())?;
                    csv.flush().map_err(|e| e.to_string())?;
                    if per_run_ref && !keep {
                        let _ = std::fs::remove_file(&ref_path);
                    }
                }
            }
            if !keep {
                let _ = std::fs::remove_file(&shared_ref);
            }
        }
    }
    if !keep {
        for f in ["out.h264", "ssim.log", "psnr.log", "vmaf.json"] {
            let _ = std::fs::remove_file(work.join(f));
        }
    }
    println!("csv: {}", csv_path.display());
    Ok(())
}

fn run_one(spec: &BenchSpec, gen: &mut SceneGen, reference: Option<&Path>, out: &Path) -> Result<BenchResult> {
    let mut out_w = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(out).map_err(|e| e.to_string())?);
    let mut ref_w = match reference {
        Some(p) => Some(std::io::BufWriter::with_capacity(8 << 20, std::fs::File::create(p).map_err(|e| e.to_string())?)),
        None => None,
    };
    let mut io_err: Option<String> = None;
    let input_fps = spec.input_fps;
    let mut gen_fn = |i: u32, buf: &mut [u8], stride: usize| gen.render(i as f64 / input_fps, buf, stride);
    let mut src_fn = |f: &Nv12Frame<'_>| {
        if let Some(wr) = ref_w.as_mut() {
            let (w, h) = (f.width as usize, f.height as usize);
            for y in 0..h {
                let _ = wr.write_all(&f.y[y * f.y_stride as usize..y * f.y_stride as usize + w]);
            }
            for y in 0..h / 2 {
                let _ = wr.write_all(&f.uv[y * f.uv_stride as usize..y * f.uv_stride as usize + w]);
            }
        }
    };
    let mut out_fn = |d: &[u8]| {
        if let Err(e) = out_w.write_all(d) {
            io_err.get_or_insert(e.to_string());
        }
    };
    let src_opt: Option<&mut dyn FnMut(&Nv12Frame<'_>)> = if reference.is_some() { Some(&mut src_fn) } else { None };
    let r = vox_capture::bench::run(spec, &mut gen_fn, src_opt, &mut out_fn).map_err(|e| e.to_string())?;
    out_w.flush().map_err(|e| e.to_string())?;
    if let Some(mut wr) = ref_w {
        wr.flush().map_err(|e| e.to_string())?;
    }
    if let Some(e) = io_err {
        return Err(e);
    }
    Ok(r)
}

fn save_png_bgra(path: &Path, w: u32, h: u32, bgra: &[u8]) -> Result<()> {
    let f = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(f), w, h);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let mut wr = enc.write_header().map_err(|e| e.to_string())?;
    let mut rgb = Vec::with_capacity((w * h * 3) as usize);
    for p in bgra.as_chunks::<4>().0 {
        rgb.extend_from_slice(&[p[2], p[1], p[0]]);
    }
    wr.write_image_data(&rgb).map_err(|e| e.to_string())
}
