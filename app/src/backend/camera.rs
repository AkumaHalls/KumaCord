//! Webcam configuration in the backend (v0.6): the saved per-camera setup is remembered and
//! applied to the call camera / Settings preview **only when it changes** (image controls,
//! mirror, rotation, zoom), used when a camera starts, and a once-a-second look at the running
//! capture feeds the Settings section (control values, effective frame rate, low-light tip).

use vox_capture::CameraControl;
use vox_client_core::golive::camera::{apply_delta, setup_delta, CameraSetup};

use super::*;

/// Camera choice as the settings hold it: (device id, device name).
pub(super) type CamDev = (Option<String>, Option<String>);

/// What the UI shows about the camera that is running (call camera first, else the preview).
#[derive(Debug, Clone, PartialEq)]
pub struct CameraLiveInfo {
    /// Supported controls with their current values.
    pub controls: Vec<CameraControl>,
    /// Frames per second the device delivers now, and what its mode promises.
    pub fps: f32,
    pub requested: f32,
    pub low_light: bool,
    /// The GPU can mirror and rotate.
    pub transform_ok: bool,
    /// The call camera (not the Settings preview).
    pub from_call: bool,
}

/// A running camera and the setup it was last given.
struct Running {
    dev: CamDev,
    setup: CameraSetup,
}

#[derive(Default)]
pub(super) struct CamBackend {
    /// The latest setup the UI sent, and the camera it is for.
    setup: CameraSetup,
    dev: CamDev,
    call: Option<Running>,
    preview: Option<Running>,
    last_live: Option<CameraLiveInfo>,
    /// "Configurar câmera" is open: controls are read back from the driver every tick.
    pub(super) watch: bool,
}

impl CamBackend {
    /// The setup to start `dev` with: the UI's, when it is for this camera.
    pub(super) fn setup_for(&self, dev: &CamDev) -> CameraSetup {
        if &self.dev == dev { self.setup.clone() } else { CameraSetup::default() }
    }

    pub(super) fn call_started(&mut self, dev: CamDev, setup: CameraSetup) {
        self.call = Some(Running { dev, setup });
    }

    pub(super) fn preview_started(&mut self, dev: CamDev, setup: CameraSetup) {
        self.preview = Some(Running { dev, setup });
    }

    pub(super) fn preview_stopped(&mut self) {
        self.preview = None;
    }
}

impl Controller {
    /// The UI's setup for a camera changed: remember it, and apply the difference to the camera
    /// that is running with that device.
    pub(super) async fn camera_setup_cmd(&mut self, dev: CamDev, setup: CameraSetup) {
        self.cam.dev = dev.clone();
        self.cam.setup = setup.clone();
        // The call camera.
        if let Some(run) = self.cam.call.as_mut().filter(|r| r.dev == dev) {
            let d = setup_delta(&run.setup, &setup);
            // (A new native mode only applies the next time the camera starts.)
            if d.transform.is_some() || !d.set.is_empty() || !d.reset.is_empty() {
                if let Some(v) = &self.voice {
                    v.with_camera(|c| apply_delta(c, &d)).await;
                }
                run.setup = CameraSetup { mode: run.setup.mode, ..setup.clone() };
            }
        }
        // The Settings preview.
        if let Some(run) = self.cam.preview.as_mut().filter(|r| r.dev == dev) {
            let d = setup_delta(&run.setup, &setup);
            if d.transform.is_some() || !d.set.is_empty() || !d.reset.is_empty() {
                if let Some(c) = self.camera_preview.as_ref().and_then(|p| p.capture()) {
                    apply_delta(c, &d);
                }
                run.setup = CameraSetup { mode: run.setup.mode, ..setup.clone() };
            }
        }
        // Show the result sooner than the next tick.
        self.camera_tick().await;
    }

    /// "Restaurar padrão": every control of the running camera back to its default.
    pub(super) async fn camera_reset_controls(&mut self, dev: CamDev) {
        if let Some(run) = self.cam.call.as_mut().filter(|r| r.dev == dev) {
            run.setup.controls.clear();
            if let Some(v) = &self.voice {
                v.with_camera(|c| c.reset_controls()).await;
            }
        }
        if let Some(run) = self.cam.preview.as_mut().filter(|r| r.dev == dev) {
            run.setup.controls.clear();
            if let Some(c) = self.camera_preview.as_ref().and_then(|p| p.capture()) {
                c.reset_controls();
            }
        }
        self.cam.setup.controls.clear();
    }

    /// The capture running with `dev` (call camera first, else the preview).
    async fn with_running<R>(&self, dev: &CamDev, f: impl Fn(&vox_capture::CameraCapture) -> R) -> Option<R> {
        if self.cam.call.as_ref().is_some_and(|r| &r.dev == dev) {
            if let Some(v) = &self.voice {
                if let Some(r) = v.with_camera(&f).await {
                    return Some(r);
                }
            }
        }
        if self.cam.preview.as_ref().is_some_and(|r| &r.dev == dev) {
            return self.camera_preview.as_ref().and_then(|p| p.capture()).map(f);
        }
        None
    }

    /// Native modes of a camera (the "Formato" list). A camera we are running already knows
    /// them: opening the device a second time would get it in shared mode, where Windows lists
    /// only the format that is streaming.
    pub(super) async fn camera_formats(&self, id: Option<String>, name: Option<String>, key: String) {
        let dev = (id.clone(), name.clone());
        if let Some(list) = self.with_running(&dev, |c| c.native_modes()).await.filter(|l| !l.is_empty()) {
            self.out.send(Ev::CameraFormats { key, list });
            return;
        }
        let out = self.out.clone();
        tokio::task::spawn_blocking(move || {
            let device = Self::resolve_camera(id, name);
            match vox_capture::list_camera_formats(&device) {
                Ok(list) => out.send(Ev::CameraFormats { key, list }),
                Err(e) => {
                    warn!(error = %e, "cannot list camera formats");
                    out.send(Ev::CameraFormats { key, list: Vec::new() })
                }
            }
        });
    }

    /// Every running camera reads its controls back from the driver.
    pub(super) async fn camera_refresh_controls(&mut self) {
        if let Some(v) = &self.voice {
            v.with_camera(|c| c.refresh_controls()).await;
        }
        if let Some(c) = self.camera_preview.as_ref().and_then(|p| p.capture()) {
            c.refresh_controls();
        }
    }

    /// The driver's own settings dialog, on its own thread (it blocks until closed).
    pub(super) fn camera_native_dialog(&self, id: Option<String>, name: Option<String>) {
        let out = self.out.clone();
        let spawned = std::thread::Builder::new().name("vox-camera-native".into()).spawn(move || {
            let mut device = Self::resolve_camera(id, name);
            if device.is_empty() {
                // "Padrão": the first camera.
                device = vox_capture::list_cameras().unwrap_or_default().into_iter().next().map(|c| c.id).unwrap_or_default();
            }
            let res = if device.is_empty() {
                Err("Nenhuma câmera encontrada.".to_string())
            } else {
                vox_capture::show_camera_settings_dialog(&device, "Configurações da câmera").map_err(|e| {
                    warn!(error = %e, "camera driver dialog failed");
                    match e.fault {
                        vox_capture::CameraFault::NotFound => "O Windows não achou o driver desta câmera para abrir as configurações do fabricante.".to_string(),
                        vox_capture::CameraFault::Unsupported => "Esta câmera não tem configurações do fabricante.".to_string(),
                        _ => "Não foi possível abrir as configurações do fabricante.".to_string(),
                    }
                })
            };
            out.send(Ev::CameraNativeClosed(res));
        });
        if spawned.is_err() {
            self.out.send(Ev::CameraNativeClosed(Err("Não foi possível abrir as configurações do fabricante.".into())));
        }
    }

    /// Once a second: what the running camera reports, sent when it changed.
    pub(super) async fn camera_tick(&mut self) {
        if self.cam.watch {
            // Read back for the next tick (automatic values move by themselves).
            self.camera_refresh_controls().await;
        }
        let snap = |c: &vox_capture::CameraCapture| {
            let st = c.stats();
            (c.controls(), st.fps, c.format().native_fps, st.low_light, c.transform_supported())
        };
        let mut got = match &self.voice {
            Some(v) => v.with_camera(snap).await,
            None => None,
        };
        let from_call = got.is_some();
        if !from_call {
            // The call camera is off (or stopped by itself): no longer ours to configure.
            self.cam.call = None;
            got = self.camera_preview.as_ref().and_then(|p| p.capture()).map(snap);
        }
        let live = got.map(|(controls, fps, requested, low_light, transform_ok)| CameraLiveInfo {
            controls,
            // One decimal is all the UI shows.
            fps: (fps * 10.0).round() / 10.0,
            requested,
            low_light,
            transform_ok,
            from_call,
        });
        if live != self.cam.last_live {
            if let Some(l) = &live {
                self.out.send(Ev::CameraLive(l.clone()));
            } else {
                self.out.send(Ev::CameraLiveOff);
            }
            self.cam.last_live = live;
        }
    }
}
