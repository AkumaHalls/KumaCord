//! Viewer renderer probe (no WebRTC): opens a window, renders synthetic I420 frames through the
//! D3D11 renderer and prints CPU / private bytes after each stage. Used to size the renderer's
//! own cost separately from decode.
//!
//! d3d_mem [WxH] [fps] [secs]

#[cfg(windows)]
fn main() {
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    use livekit::webrtc::video_frame::I420Buffer;
    use vox_client_core::golive::d3d::D3dRenderer;
    use vox_client_core::stats::ProcessMeter;
    use winit::application::ApplicationHandler;
    use winit::dpi::PhysicalSize;
    use winit::event::WindowEvent;
    use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use winit::window::{Window, WindowId};

    let args: Vec<String> = std::env::args().collect();
    let (w, h) = args
        .get(1)
        .and_then(|s| s.split_once('x'))
        .and_then(|(a, b)| Some((a.parse::<u32>().ok()?, b.parse::<u32>().ok()?)))
        .unwrap_or((1920, 1080));
    let fps: f64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(60.0);
    let secs: f64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(10.0);

    fn mb(b: u64) -> f64 {
        b as f64 / 1048576.0
    }

    struct App {
        w: u32,
        h: u32,
        fps: f64,
        secs: f64,
        meter: ProcessMeter,
        win: Option<Rc<Window>>,
        r: Option<D3dRenderer>,
        frames: Vec<I420Buffer>,
        n: u64,
        start: Instant,
        next: Instant,
    }

    impl App {
        fn report(&mut self, stage: &str) {
            let s = self.meter.sample();
            println!("{stage:<28} cpu={:.2}% ({:.1}% core) ws={:.1}MB private={:.1}MB handles={}", s.cpu_pct, s.cpu_pct_core, mb(s.working_set_bytes), mb(s.private_bytes), s.handles);
        }
    }

    impl ApplicationHandler for App {
        fn resumed(&mut self, el: &ActiveEventLoop) {
            if self.win.is_some() {
                return;
            }
            self.report("start");
            let attrs = Window::default_attributes().with_title("vox d3d probe").with_inner_size(PhysicalSize::new(1600, 900));
            let window = Rc::new(el.create_window(attrs).expect("window"));
            self.report("window");
            let hwnd = match window.window_handle().unwrap().as_raw() {
                RawWindowHandle::Win32(h) => h.hwnd.get(),
                _ => unreachable!(),
            };
            let size = window.inner_size();
            self.r = Some(D3dRenderer::new(hwnd, size.width, size.height).expect("renderer"));
            self.report("renderer (device+swapchain)");
            // Two pre-made frames so the loop only measures upload + draw.
            for k in 0..2u8 {
                let mut b = I420Buffer::new(self.w, self.h);
                let (y, u, v) = b.data_mut();
                for (i, p) in y.iter_mut().enumerate() {
                    *p = 16 + ((i as u32 / 7 + k as u32 * 40) % 219) as u8;
                }
                u.fill(90 + k * 40);
                v.fill(160 - k * 30);
                self.frames.push(b);
            }
            self.report("frames allocated");
            self.win = Some(window);
            self.start = Instant::now();
            self.next = self.start;
        }

        fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
            if let WindowEvent::CloseRequested = event {
                el.exit();
            }
        }

        fn about_to_wait(&mut self, el: &ActiveEventLoop) {
            let now = Instant::now();
            if self.win.is_none() {
                return;
            }
            if now >= self.next {
                let f = &self.frames[(self.n / 15 % 2) as usize];
                let r = self.r.as_mut().unwrap();
                r.upload(f).expect("upload");
                r.draw().expect("draw");
                self.n += 1;
                if self.n == 1 {
                    self.report("first frame");
                    self.meter = ProcessMeter::new();
                }
                self.next += Duration::from_secs_f64(1.0 / self.fps);
            }
            if now.duration_since(self.start).as_secs_f64() >= self.secs {
                self.report(&format!("after {} frames", self.n));
                el.exit();
                return;
            }
            el.set_control_flow(ControlFlow::WaitUntil(self.next));
        }
    }

    let el = EventLoop::new().expect("event loop");
    let mut app = App {
        w,
        h,
        fps,
        secs,
        meter: ProcessMeter::new(),
        win: None,
        r: None,
        frames: Vec::new(),
        n: 0,
        start: Instant::now(),
        next: Instant::now(),
    };
    el.run_app(&mut app).expect("run");
}

#[cfg(not(windows))]
fn main() {}
