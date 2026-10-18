//! Webcam image controls (v0.6): brightness, contrast, white balance, exposure, focus, zoom...
//!
//! The Media Foundation capture source answers `IAMVideoProcAmp` (image quality) and
//! `IAMCameraControl` (exposure, focus, zoom) through `QueryInterface`, and `IKsControl` for the
//! properties those two do not cover (auto-exposure priority, v0.8.1). Only controls the driver
//! reports (with their real range, step and default) are listed. Everything here runs on the
//! camera thread; other threads queue requests (`CtlQueue`) and the thread applies them, so a
//! control is only touched when it changes and never per frame.
//!
//! The synthetic camera has a fake set (kept in memory) so the settings and the live apply can be
//! exercised without hardware.

use windows::core::Interface;
use windows::Win32::Media::DirectShow::{IAMCameraControl, IAMVideoProcAmp};
use windows::Win32::Media::KernelStreaming::{
    IKsControl, KSIDENTIFIER, KSPROPERTY_CAMERACONTROL_AUTO_EXPOSURE_PRIORITY, KSPROPERTY_CAMERACONTROL_S,
    KSPROPERTY_TYPE_GET, KSPROPERTY_TYPE_SET, PROPSETID_VIDCAP_CAMERACONTROL,
};
use windows::Win32::Media::MediaFoundation::IMFMediaSource;

use crate::{CameraAdjust, CameraControl, CameraControlId};

const FLAG_AUTO: i32 = 1;
const FLAG_MANUAL: i32 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// `VideoProcAmpProperty`
    Amp(i32),
    /// `CameraControlProperty`
    Cam(i32),
    /// A `PROPSETID_VIDCAP_CAMERACONTROL` property through `IKsControl` (an on / off value).
    Ks(i32),
}

fn kind_of(id: CameraControlId) -> Kind {
    match id {
        CameraControlId::Brightness => Kind::Amp(0),
        CameraControlId::Contrast => Kind::Amp(1),
        CameraControlId::Saturation => Kind::Amp(3),
        CameraControlId::Sharpness => Kind::Amp(4),
        CameraControlId::Gamma => Kind::Amp(5),
        CameraControlId::WhiteBalance => Kind::Amp(7),
        CameraControlId::Backlight => Kind::Amp(8),
        CameraControlId::Gain => Kind::Amp(9),
        CameraControlId::Zoom => Kind::Cam(3),
        CameraControlId::Exposure => Kind::Cam(4),
        CameraControlId::Focus => Kind::Cam(6),
        CameraControlId::ExposurePriority => Kind::Ks(KSPROPERTY_CAMERACONTROL_AUTO_EXPOSURE_PRIORITY.0),
    }
}

enum Backend {
    None,
    Mf { amp: Option<IAMVideoProcAmp>, cam: Option<IAMCameraControl>, ks: Option<IKsControl> },
    Fake,
}

/// The camera's supported controls and the handle to change them.
pub(crate) struct Controls {
    backend: Backend,
    list: Vec<CameraControl>,
}

impl Controls {
    pub(crate) fn none() -> Self {
        Self { backend: Backend::None, list: Vec::new() }
    }

    /// Ask the media source which controls it has.
    pub(crate) fn query(source: &IMFMediaSource) -> Self {
        let amp = source.cast::<IAMVideoProcAmp>().ok();
        let cam = source.cast::<IAMCameraControl>().ok();
        let ks = source.cast::<IKsControl>().ok();
        let mut c = Self { backend: Backend::Mf { amp, cam, ks }, list: Vec::new() };
        for id in CameraControlId::ALL {
            let Some((min, max, step, default, caps)) = c.hw_range(id) else { continue };
            if max <= min {
                continue;
            }
            let (value, auto) = c.hw_get(id).unwrap_or((default, false));
            let can_auto = caps & FLAG_AUTO != 0;
            let can_manual = caps & FLAG_MANUAL != 0 || caps == 0;
            c.list.push(CameraControl {
                id,
                min,
                max,
                step: step.max(1),
                default,
                value,
                auto: auto && can_auto,
                can_auto,
                can_manual,
            });
        }
        c
    }

    /// A believable set of controls kept in memory (synthetic camera).
    pub(crate) fn fake() -> Self {
        let mk = |id, min, max, step, default, can_auto: bool| CameraControl {
            id,
            min,
            max,
            step,
            default,
            value: default,
            auto: can_auto,
            can_auto,
            can_manual: true,
        };
        use CameraControlId::*;
        Self {
            backend: Backend::Fake,
            list: vec![
                mk(Brightness, 0, 255, 1, 128, false),
                mk(Contrast, 0, 255, 1, 128, false),
                mk(Saturation, 0, 255, 1, 128, false),
                mk(Sharpness, 0, 255, 1, 128, false),
                mk(Gamma, 72, 500, 1, 100, false),
                mk(WhiteBalance, 2000, 6500, 10, 4000, true),
                mk(Backlight, 0, 1, 1, 0, false),
                mk(Exposure, -11, -2, 1, -6, true),
                mk(Gain, 0, 255, 1, 0, false),
                mk(Focus, 0, 250, 5, 0, true),
                mk(Zoom, 100, 500, 1, 100, false),
                mk(ExposurePriority, 0, 1, 1, 1, false),
            ],
        }
    }

    pub(crate) fn list(&self) -> &[CameraControl] {
        &self.list
    }

    /// Automatic exposure is on. `None` when the camera has no exposure control (then a slow
    /// camera cannot be blamed on the light).
    pub(crate) fn exposure_auto(&self) -> Option<bool> {
        self.list.iter().find(|c| c.id == CameraControlId::Exposure).map(|c| c.auto)
    }

    /// Automatic exposure may lower the frame rate in a dim room: exposure is automatic and the
    /// camera's auto-exposure priority (when it has one) allows it.
    pub(crate) fn may_slow_down(&self) -> Option<bool> {
        let auto = self.exposure_auto()?;
        let priority = self.list.iter().find(|c| c.id == CameraControlId::ExposurePriority);
        Some(auto && priority.is_none_or(|p| p.value > p.min))
    }

    /// `KSPROPERTY_CAMERACONTROL_S` get / set through `IKsControl`.
    fn ks(ks: &IKsControl, id: i32, set: Option<i32>) -> windows_core::Result<i32> {
        let mut p = KSPROPERTY_CAMERACONTROL_S::default();
        p.Property.Anonymous.Anonymous.Set = PROPSETID_VIDCAP_CAMERACONTROL;
        p.Property.Anonymous.Anonymous.Id = id as u32;
        p.Property.Anonymous.Anonymous.Flags = if set.is_some() { KSPROPERTY_TYPE_SET } else { KSPROPERTY_TYPE_GET };
        if let Some(v) = set {
            p.Value = v;
            p.Flags = FLAG_MANUAL as u32;
        }
        let size = std::mem::size_of::<KSPROPERTY_CAMERACONTROL_S>() as u32;
        let mut out = p;
        let mut got = 0u32;
        // SAFETY: both buffers are a whole KSPROPERTY_CAMERACONTROL_S, as the property expects.
        unsafe { ks.KsProperty(&p.Property as *const KSIDENTIFIER, size, &mut out as *mut _ as *mut _, size, &mut got)? };
        Ok(if set.is_some() { p.Value } else { out.Value })
    }

    fn hw_range(&self, id: CameraControlId) -> Option<(i32, i32, i32, i32, i32)> {
        let Backend::Mf { amp, cam, ks } = &self.backend else { return None };
        let (mut min, mut max, mut step, mut def, mut caps) = (0, 0, 0, 0, 0);
        let ok = unsafe {
            match kind_of(id) {
                Kind::Amp(p) => amp.as_ref()?.GetRange(p, &mut min, &mut max, &mut step, &mut def, &mut caps).is_ok(),
                Kind::Cam(p) => cam.as_ref()?.GetRange(p, &mut min, &mut max, &mut step, &mut def, &mut caps).is_ok(),
                // An on / off property: present when it can be read. Its "default" is what the
                // camera had when it was opened (restoring it puts the camera back as found).
                Kind::Ks(p) => {
                    let v = Self::ks(ks.as_ref()?, p, None).ok()?;
                    (min, max, step, def, caps) = (0, 1, 1, v.clamp(0, 1), FLAG_MANUAL);
                    true
                }
            }
        };
        ok.then_some((min, max, step, def, caps))
    }

    fn hw_get(&self, id: CameraControlId) -> Option<(i32, bool)> {
        let Backend::Mf { amp, cam, ks } = &self.backend else { return None };
        let (mut v, mut flags) = (0, 0);
        let ok = unsafe {
            match kind_of(id) {
                Kind::Amp(p) => amp.as_ref()?.Get(p, &mut v, &mut flags).is_ok(),
                Kind::Cam(p) => cam.as_ref()?.Get(p, &mut v, &mut flags).is_ok(),
                Kind::Ks(p) => {
                    v = Self::ks(ks.as_ref()?, p, None).ok()?;
                    true
                }
            }
        };
        ok.then_some((v, flags & FLAG_AUTO != 0))
    }

    fn hw_set(&self, id: CameraControlId, value: i32, flags: i32) -> windows_core::Result<()> {
        let Backend::Mf { amp, cam, ks } = &self.backend else { return Ok(()) };
        unsafe {
            match kind_of(id) {
                Kind::Amp(p) => amp.as_ref().map_or(Ok(()), |a| a.Set(p, value, flags)),
                Kind::Cam(p) => cam.as_ref().map_or(Ok(()), |a| a.Set(p, value, flags)),
                Kind::Ks(p) => ks.as_ref().map_or(Ok(()), |k| Self::ks(k, p, Some(value)).map(|_| ())),
            }
        }
    }

    /// Read every value back from the driver (something else may have changed them).
    pub(crate) fn refresh(&mut self) {
        if !matches!(self.backend, Backend::Mf { .. }) {
            return;
        }
        for i in 0..self.list.len() {
            if let Some((v, a)) = self.hw_get(self.list[i].id) {
                let c = &mut self.list[i];
                c.value = v;
                c.auto = a && c.can_auto;
            }
        }
    }

    /// Apply one change and read the control back (the driver may round or refuse).
    pub(crate) fn set(&mut self, adj: CameraAdjust) {
        let Some(i) = self.list.iter().position(|c| c.id == adj.id) else { return };
        let c = self.list[i];
        // Automatic only where the driver has it; manual only where it allows it.
        let auto = if adj.auto { c.can_auto } else { !c.can_manual && c.can_auto };
        let value = c.snap(adj.value);
        let flags = if auto { FLAG_AUTO } else { FLAG_MANUAL };
        match self.backend {
            Backend::Fake => {
                let c = &mut self.list[i];
                c.auto = auto;
                if !auto {
                    c.value = value;
                }
            }
            Backend::None => {}
            Backend::Mf { .. } => {
                // In automatic mode the value is the driver's; hand it the current one.
                let send = if auto { c.value } else { value };
                if let Err(e) = self.hw_set(adj.id, send, flags) {
                    tracing::debug!(control = adj.id.key(), value, auto, error = %e, "camera control set failed");
                }
                if let Some((v, a)) = self.hw_get(adj.id) {
                    let c = &mut self.list[i];
                    c.value = v;
                    c.auto = a && c.can_auto;
                }
            }
        }
    }

    /// Defaults for one control or all of them (automatic where the control has it).
    pub(crate) fn reset(&mut self, id: Option<CameraControlId>) {
        let todo: Vec<CameraAdjust> = self
            .list
            .iter()
            .filter(|c| id.is_none_or(|i| i == c.id))
            .map(|c| {
                let (value, auto) = c.default_setting();
                CameraAdjust { id: c.id, value, auto }
            })
            .collect();
        // Manual values first, automatic modes last: an automatic control ignores its value, and
        // some drivers refuse a manual value while another control is automatic.
        for a in todo.iter().filter(|a| !a.auto).chain(todo.iter().filter(|a| a.auto)) {
            self.set(*a);
        }
    }
}

/// Requests from other threads, applied by the camera thread. The newest request per control
/// wins, so a slider drag costs one driver call per frame at most.
#[derive(Default)]
pub(crate) struct CtlQueue {
    /// In order; a reset applies after the sets queued before it.
    ops: Vec<Op>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Op {
    Set(CameraAdjust),
    Reset(Option<CameraControlId>),
    /// Read everything back from the driver.
    Refresh,
}

impl CtlQueue {
    pub(crate) fn push_set(&mut self, a: CameraAdjust) {
        // A later set of the same control replaces the earlier one (and anything before a
        // reset of everything is moot).
        self.ops.retain(|o| !matches!(o, Op::Set(x) if x.id == a.id));
        self.ops.push(Op::Set(a));
    }

    pub(crate) fn push_reset(&mut self, id: Option<CameraControlId>) {
        match id {
            None => self.ops.clear(),
            Some(i) => self.ops.retain(|o| !matches!(o, Op::Set(x) if x.id == i)),
        }
        self.ops.push(Op::Reset(id));
    }

    pub(crate) fn push_refresh(&mut self) {
        if !self.ops.contains(&Op::Refresh) {
            self.ops.push(Op::Refresh);
        }
    }

    pub(crate) fn take(&mut self) -> Vec<Op> {
        std::mem::take(&mut self.ops)
    }
}

impl Controls {
    pub(crate) fn apply(&mut self, ops: &[Op]) {
        for op in ops {
            match *op {
                Op::Set(a) => self.set(a),
                Op::Reset(id) => self.reset(id),
                Op::Refresh => self.refresh(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_keeps_the_newest_set_per_control() {
        let mut q = CtlQueue::default();
        let b = |v| CameraAdjust { id: CameraControlId::Brightness, value: v, auto: false };
        let c = |v| CameraAdjust { id: CameraControlId::Contrast, value: v, auto: false };
        q.push_set(b(1));
        q.push_set(c(5));
        q.push_set(b(2));
        q.push_set(b(3));
        assert_eq!(q.take(), vec![Op::Set(c(5)), Op::Set(b(3))]);
        assert!(q.take().is_empty());
    }

    #[test]
    fn reset_all_drops_older_requests() {
        let mut q = CtlQueue::default();
        let b = CameraAdjust { id: CameraControlId::Brightness, value: 9, auto: false };
        q.push_set(b);
        q.push_reset(None);
        q.push_set(b);
        assert_eq!(q.take(), vec![Op::Reset(None), Op::Set(b)]);
        q.push_set(b);
        q.push_reset(Some(CameraControlId::Brightness));
        assert_eq!(q.take(), vec![Op::Reset(Some(CameraControlId::Brightness))]);
    }

    #[test]
    fn fake_controls_follow_range_step_and_auto() {
        let mut c = Controls::fake();
        assert_eq!(c.exposure_auto(), Some(true));
        // Manual exposure snaps into the range.
        c.set(CameraAdjust { id: CameraControlId::Exposure, value: -50, auto: false });
        let e = c.list().iter().find(|x| x.id == CameraControlId::Exposure).copied().unwrap();
        assert_eq!((e.value, e.auto), (-11, false));
        assert_eq!(c.exposure_auto(), Some(false));
        // White balance 4567 K lands on the 10 K grid.
        c.set(CameraAdjust { id: CameraControlId::WhiteBalance, value: 4567, auto: false });
        let w = c.list().iter().find(|x| x.id == CameraControlId::WhiteBalance).copied().unwrap();
        assert_eq!(w.value, 4570);
        // A control without automatic mode ignores `auto`.
        c.set(CameraAdjust { id: CameraControlId::Brightness, value: 10, auto: true });
        let b = c.list().iter().find(|x| x.id == CameraControlId::Brightness).copied().unwrap();
        assert_eq!((b.value, b.auto), (10, false));
        // Reset: everything back to default, automatic where there is one.
        c.reset(None);
        for x in c.list() {
            let (v, a) = x.default_setting();
            assert_eq!(x.auto, a, "{:?}", x.id);
            if !a {
                assert_eq!(x.value, v, "{:?}", x.id);
            }
        }
        assert_eq!(c.exposure_auto(), Some(true));
    }

    #[test]
    fn exposure_priority_decides_whether_the_camera_may_slow_down() {
        let mut c = Controls::fake();
        // Automatic exposure, priority on (the camera's default): dim light lowers the rate.
        assert_eq!(c.may_slow_down(), Some(true));
        c.set(CameraAdjust { id: CameraControlId::ExposurePriority, value: 0, auto: false });
        assert_eq!(c.may_slow_down(), Some(false), "frame rate kept");
        c.set(CameraAdjust { id: CameraControlId::ExposurePriority, value: 1, auto: false });
        c.set(CameraAdjust { id: CameraControlId::Exposure, value: -5, auto: false });
        assert_eq!(c.may_slow_down(), Some(false), "manual exposure never slows down");
        // A camera without the priority control: automatic exposure alone decides.
        c.list.retain(|x| x.id != CameraControlId::ExposurePriority);
        c.set(CameraAdjust { id: CameraControlId::Exposure, value: 0, auto: true });
        assert_eq!(c.may_slow_down(), Some(true));
        c.list.retain(|x| x.id != CameraControlId::Exposure);
        assert_eq!(c.may_slow_down(), None);
        // Refreshing a fake set changes nothing; a refresh request is queued once.
        let before = c.list().to_vec();
        c.refresh();
        assert_eq!(c.list(), &before[..]);
        let mut q = CtlQueue::default();
        q.push_refresh();
        q.push_refresh();
        assert_eq!(q.take(), vec![Op::Refresh]);
    }
}
