//! Lists a camera's native formats and image controls, changes a few, reads them back and puts
//! everything back to the defaults. Opens the camera briefly (preview only; no frame is kept or
//! written anywhere).
//!
//! `cargo run -p vox-capture --example camera_controls -- [name-or-id] [--format WxH@FPS]`
//!
//! The camera's own settings are read first and **put back as found** at the end (values first,
//! automatic modes last); nothing is reset to factory defaults. `--set NAME=VALUE|auto` (repeatable,
//! NAME is a control key such as `gain`, `white_balance`) applies those settings and keeps them
//! instead of running the test sequence, then reads everything back.
//! `--list` is read-only: it prints every native media type, the controls and the menu of modes,
//! and checks that the driver's settings dialog can be found (without showing it), all without
//! streaming. (`VOX_FAKE_CAMERA=1` for the synthetic camera).

use std::time::{Duration, Instant};

use vox_capture::{CameraConfig, CameraControl, CameraControlId, CameraMode};

fn show(c: &CameraControl) -> String {
    format!(
        "{:<18} {:>6}..{:<6} step {:<3} default {:<6} now {:<8}{}{}",
        c.id.label_pt(),
        c.min,
        c.max,
        c.step,
        c.format_value(c.default),
        c.format_value(c.value),
        if c.can_auto { " [auto]" } else { "" },
        if c.auto { " (auto on)" } else { "" },
    )
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut want = String::new();
    let mut format: Option<(u32, u32, u32)> = None;
    let mut sets: Vec<(CameraControlId, Option<i32>)> = Vec::new();
    let mut list_only = false;
    while let Some(a) = args.next() {
        if a == "--list" {
            list_only = true;
        } else if a == "--set" {
            let kv = args.next().unwrap_or_default();
            let (k, v) = kv.split_once('=').unwrap_or((&kv, ""));
            let id = CameraControlId::from_key(k).expect("unknown control key");
            sets.push((id, if v == "auto" { None } else { Some(v.parse().expect("value")) }));
        } else if a == "--format" {
            let f = args.next().unwrap_or_default();
            let (wh, fps) = f.split_once('@').unwrap_or((&f, "30"));
            let (w, h) = wh.split_once('x').unwrap_or(("1280", "720"));
            format = Some((w.parse().unwrap_or(1280), h.parse().unwrap_or(720), fps.parse().unwrap_or(30)));
        } else {
            want = a;
        }
    }
    let cams = vox_capture::list_cameras().expect("list cameras");
    for c in &cams {
        println!("camera: {} ({})", c.name, c.id);
    }
    let cam = cams
        .iter()
        .find(|c| want.is_empty() || c.name.to_lowercase().contains(&want.to_lowercase()) || c.id == want)
        .expect("no such camera");
    println!("using: {}", cam.name);
    if list_only {
        match vox_capture::camera_diagnostics(&cam.id) {
            Ok(text) => print!("{text}"),
            Err(e) => println!("diagnostics failed: {e}"),
        }
        match vox_capture::list_camera_formats(&cam.id) {
            Ok(modes) => {
                let menu = vox_capture::camera_mode_menu(&modes);
                println!("{} usable modes, {} in the menu:", modes.len(), menu.len());
                for m in &menu {
                    println!("  {}", m.label());
                }
            }
            Err(e) => println!("formats failed: {e}"),
        }
        match vox_capture::camera_settings_dialog_pages(&cam.id) {
            Ok(n) => println!("driver settings dialog: {n} property pages (not shown)"),
            Err(e) => println!("driver settings dialog: unavailable ({e})"),
        }
        return;
    }

    let t0 = Instant::now();
    let modes = vox_capture::list_camera_formats(&cam.id).expect("formats");
    println!("{} native modes (listed in {} ms, camera not streaming):", modes.len(), t0.elapsed().as_millis());
    for m in &modes {
        println!("  {}", m.label());
    }

    let mut cfg = CameraConfig { width: 1280, height: 720, fps: 30, ..Default::default() };
    if let Some((w, h, fps)) = format {
        cfg.mode = modes.iter().find(|m| m.width == w && m.height == h && (m.fps() - fps as f32).abs() < 1.0).copied();
        println!("chosen format: {:?}", cfg.mode.map(|m: CameraMode| m.label()));
    }
    let t0 = Instant::now();
    let cap = vox_capture::start_camera_preview(&cam.id, cfg, Box::new(|_| {})).expect("start camera");
    println!("opened in {} ms: {:?}", t0.elapsed().as_millis(), cap.format());
    std::thread::sleep(Duration::from_millis(800));

    let controls = cap.controls();
    println!("{} controls:", controls.len());
    for c in &controls {
        println!("  {}", show(c));
    }
    let found = controls.clone();
    let get = |id| cap.controls().into_iter().find(|c| c.id == id);
    // Put the camera back as it was found: manual values first, automatic modes last.
    let restore = |cap: &vox_capture::CameraCapture| {
        for c in found.iter().filter(|c| !c.auto).chain(found.iter().filter(|c| c.auto)) {
            cap.set_control(c.id, c.value, c.auto);
        }
        std::thread::sleep(Duration::from_millis(600));
    };
    if !sets.is_empty() {
        for (id, v) in &sets {
            let cur = get(*id).expect("this camera has no such control");
            cap.set_control(*id, v.unwrap_or(cur.value), v.is_none());
        }
        std::thread::sleep(Duration::from_millis(600));
        println!("after --set:");
        for c in cap.controls() {
            println!("  {}", show(&c));
        }
        drop(cap);
        println!("released");
        return;
    }
    let wait_for = |what: &str, ok: &dyn Fn() -> bool| {
        let t0 = Instant::now();
        while !ok() && t0.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(20));
        }
        println!("  {what}: {} after {} ms", if ok() { "ok" } else { "NOT APPLIED" }, t0.elapsed().as_millis());
    };

    // Brightness: move off the default, read back, restore.
    if let Some(b) = get(CameraControlId::Brightness) {
        let target = b.snap(if b.value + 20 <= b.max { b.value + 20 } else { b.value - 20 });
        println!("brightness {} -> {target}", b.value);
        cap.set_control(CameraControlId::Brightness, target, false);
        wait_for("brightness set", &|| get(CameraControlId::Brightness).is_some_and(|c| c.value == target));
        cap.set_control(CameraControlId::Brightness, b.value, b.auto);
        wait_for("brightness restored", &|| get(CameraControlId::Brightness).is_some_and(|c| c.value == b.value));
    }
    // Exposure: switch to manual at a value, then back to automatic.
    if let Some(e) = get(CameraControlId::Exposure) {
        let was_auto = e.auto;
        let target = e.snap(e.max - 2);
        println!("exposure (auto={}) -> manual {} ({})", e.auto, target, e.format_value(target));
        cap.set_control(CameraControlId::Exposure, target, false);
        wait_for("exposure manual", &|| get(CameraControlId::Exposure).is_some_and(|c| !c.auto && c.value == target));
        std::thread::sleep(Duration::from_millis(1500));
        println!("  measured {:.1} fps with manual exposure {}", cap.stats().fps, e.format_value(target));
        cap.set_control(CameraControlId::Exposure, e.value, e.auto);
        wait_for("exposure restored", &|| get(CameraControlId::Exposure).is_some_and(|c| c.auto == was_auto));
    }
    // Everything back as found (never factory defaults).
    restore(&cap);
    println!("after restore (found values above):");
    for c in cap.controls() {
        println!("  {}", show(&c));
    }
    println!("stats: {:?}", cap.stats());
    let t0 = Instant::now();
    drop(cap);
    println!("released in {} ms", t0.elapsed().as_millis());
}
