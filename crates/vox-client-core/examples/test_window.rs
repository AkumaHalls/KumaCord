//! A self-animating window to use as a Go Live capture target in tests (no files, no audio).
//!
//! test_window [--fps 60] [--resize-every SECS] [--close-after SECS] [--physical-size WxH]
//!
//! Prints `hwnd=<n> pid=<n>` on startup. With `--resize-every`, it alternates between
//! 1280x720 and 960x600 (logical) to exercise mid-stream resolution changes. With
//! `--close-after`, it closes itself, so the sharer's capture stops.
//!
//! A GUI program (no console window opens for it); the `hwnd=` line still reaches a redirected
//! stdout.

// No console: the release gate starts it unattended and must not open a terminal window.
#![windows_subsystem = "windows"]

use std::num::NonZeroU32;
use std::rc::Rc;
use std::time::{Duration, Instant};

use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::{Window, WindowId};

type Win = (Rc<Window>, softbuffer::Surface<Rc<Window>, Rc<Window>>, softbuffer::Context<Rc<Window>>);

struct App {
    physical: Option<(u32, u32)>,
    fps: u32,
    resize_every: Option<Duration>,
    close_after: Option<Duration>,
    start: Instant,
    last_resize: Instant,
    big: bool,
    frame: u64,
    next: Instant,
    win: Option<Win>,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.win.is_some() {
            return;
        }
        // A test target only: it never takes the user's foreground (shown without activation, then
        // `WS_EX_NOACTIVATE` at the bottom of the z-order; WGC captures it all the same).
        let mut attrs = Window::default_attributes().with_title("vox test window").with_inner_size(LogicalSize::new(1280.0, 720.0)).with_active(false);
        if let Some((w, h)) = self.physical {
            attrs = attrs.with_inner_size(winit::dpi::PhysicalSize::new(w, h));
        }
        let window = Rc::new(el.create_window(attrs).expect("create window"));
        let hwnd = match window.window_handle().map(|h| h.as_raw()) {
            Ok(RawWindowHandle::Win32(h)) => h.hwnd.get(),
            _ => 0,
        };
        #[cfg(windows)]
        vox_client_core::testmode::keep_behind(hwnd, true);
        println!("hwnd={hwnd} pid={}", std::process::id());
        let ctx = softbuffer::Context::new(window.clone()).expect("softbuffer context");
        let surface = softbuffer::Surface::new(&ctx, window.clone()).expect("softbuffer surface");
        self.win = Some((window, surface, ctx));
        self.next = Instant::now();
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::RedrawRequested => self.draw(),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        let now = Instant::now();
        if let Some(c) = self.close_after {
            if now.duration_since(self.start) >= c {
                println!("closing");
                el.exit();
                return;
            }
        }
        if let (Some(every), Some((w, _, _))) = (self.resize_every, &self.win) {
            if now.duration_since(self.last_resize) >= every {
                self.last_resize = now;
                self.big = !self.big;
                let size = if self.big { LogicalSize::new(1280.0, 720.0) } else { LogicalSize::new(960.0, 600.0) };
                let _ = w.request_inner_size(size);
                println!("resized to {}x{} (logical)", size.width, size.height);
            }
        }
        if now >= self.next {
            if let Some((w, _, _)) = &self.win {
                w.request_redraw();
            }
            self.next += Duration::from_secs_f64(1.0 / self.fps as f64);
            if self.next < now {
                self.next = now;
            }
        }
        el.set_control_flow(ControlFlow::WaitUntil(self.next));
    }
}

impl App {
    fn draw(&mut self) {
        let Some((w, surface, _)) = self.win.as_mut() else { return };
        let size = w.inner_size();
        let (Some(ww), Some(wh)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else { return };
        if surface.resize(ww, wh).is_err() {
            return;
        }
        let Ok(mut buf) = surface.buffer_mut() else { return };
        let (ww, wh) = (ww.get() as usize, wh.get() as usize);
        let t = self.frame as usize;
        // Colour gradient that scrolls, plus a moving white bar: cheap and clearly animated.
        for y in 0..wh {
            let row = &mut buf[y * ww..(y + 1) * ww];
            let g = ((y * 255 / wh.max(1)) as u32) << 8;
            for (x, px) in row.iter_mut().enumerate() {
                let r = (((x + t * 4) % ww) * 255 / ww.max(1)) as u32;
                *px = (r << 16) | g | 0x40;
            }
        }
        let bar = (t * 6) % ww.max(1);
        for y in 0..wh {
            for x in bar..(bar + 24).min(ww) {
                buf[y * ww + x] = 0x00FF_FFFF;
            }
        }
        let _ = buf.present();
        self.frame += 1;
    }
}

fn arg(args: &[String], name: &str) -> Option<f64> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let el = EventLoop::new().expect("event loop");
    let mut app = App {
        fps: arg(&args, "--fps").unwrap_or(60.0) as u32,
        physical: args.iter().position(|a| a == "--physical-size").and_then(|i| args.get(i + 1)).and_then(|v| v.split_once('x')).and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?))),
        resize_every: arg(&args, "--resize-every").map(Duration::from_secs_f64),
        close_after: arg(&args, "--close-after").map(Duration::from_secs_f64),
        start: Instant::now(),
        last_resize: Instant::now(),
        big: true,
        frame: 0,
        next: Instant::now(),
        win: None,
    };
    el.run_app(&mut app).expect("run");
}
