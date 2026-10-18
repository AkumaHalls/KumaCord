//! The camera driver's own settings dialog (v0.8.1): the DirectShow property pages OBS opens with
//! "Configure Video". The capture itself is Media Foundation; this only finds the same device
//! among the DirectShow video input devices (`ICreateDevEnum` -> `CLSID_VideoInputDeviceCategory`),
//! binds its filter and asks it for its pages (`ISpecifyPropertyPages`), then shows them with
//! `OleCreatePropertyFrame`. Everything runs on a dedicated STA thread (property pages are
//! apartment-threaded UI); the call blocks until the dialog is closed.

use windows::core::{Interface, GUID, PCWSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::Media::DirectShow::{IBaseFilter, ICreateDevEnum};
use windows::Win32::Media::MediaFoundation::{CLSID_SystemDeviceEnum, CLSID_VideoInputDeviceCategory};
use windows::Win32::System::Com::StructuredStorage::IPropertyBag;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, IEnumMoniker, IMoniker, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::Ole::{OleCreatePropertyFrame, ISpecifyPropertyPages};
use windows::Win32::System::Variant::{VariantClear, VARIANT, VT_BSTR};

use crate::{CameraError, CameraFault, SYNTHETIC_CAMERA_ID};

fn err(fault: CameraFault, detail: impl Into<String>) -> CameraError {
    CameraError { fault, detail: detail.into() }
}

fn com_err(ctx: &str, e: windows_core::Error) -> CameraError {
    err(CameraFault::Other, format!("{ctx} failed: hr=0x{:08X} {}", e.code().0 as u32, e.message()))
}

/// The part of a device interface path that names the device itself. Media Foundation lists a
/// camera under `KSCATEGORY_VIDEO_CAMERA` and DirectShow under `KSCATEGORY_VIDEO`, so the two
/// paths differ only in the interface class GUID after the last `#`:
/// `\\?\usb#vid_046d&pid_082d&mi_00#8&1a2b3c4d&0&0000#{e5323777-...}\global`
/// -> `usb#vid_046d&pid_082d&mi_00#8&1a2b3c4d&0&0000`.
pub(crate) fn device_instance_key(path: &str) -> String {
    let p = path.trim().to_ascii_lowercase();
    let p = p.strip_prefix(r"\\?\").unwrap_or(&p);
    match p.rfind('#') {
        Some(i) if p[i..].starts_with("#{") => p[..i].to_string(),
        _ => p.to_string(),
    }
}

struct Sta;

impl Sta {
    fn init() -> Self {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }
        Sta
    }
}

impl Drop for Sta {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

fn bag_string(bag: &IPropertyBag, name: &str) -> Option<String> {
    let wname: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    unsafe {
        let mut v = VARIANT::default();
        bag.Read(PCWSTR(wname.as_ptr()), &mut v, None).ok()?;
        let out = (v.Anonymous.Anonymous.vt == VT_BSTR).then(|| v.Anonymous.Anonymous.Anonymous.bstrVal.to_string());
        let _ = VariantClear(&mut v);
        out
    }
}

/// The DirectShow filter of the camera: by device path (the Media Foundation symbolic link with
/// its interface class swapped), else by friendly name.
unsafe fn find_filter(device: &str) -> Result<(IBaseFilter, String), CameraError> {
    let devenum: ICreateDevEnum =
        CoCreateInstance(&CLSID_SystemDeviceEnum, None, CLSCTX_INPROC_SERVER).map_err(|e| com_err("CoCreateInstance(SystemDeviceEnum)", e))?;
    let mut list: Option<IEnumMoniker> = None;
    devenum
        .CreateClassEnumerator(&CLSID_VideoInputDeviceCategory, &mut list, 0)
        .map_err(|e| com_err("CreateClassEnumerator(VideoInputDeviceCategory)", e))?;
    // S_FALSE (no devices) leaves the list empty.
    let list = list.ok_or_else(|| err(CameraFault::NotFound, "no DirectShow video input device"))?;
    let want_key = device_instance_key(device);
    let mut by_name: Option<(IMoniker, String)> = None;
    loop {
        let mut m: [Option<IMoniker>; 1] = [None];
        if list.Next(&mut m, None).0 != 0 {
            break;
        }
        let Some(m) = m[0].take() else { break };
        let Ok(bag) = m.BindToStorage::<_, _, IPropertyBag>(None, None) else { continue };
        let name = bag_string(&bag, "FriendlyName").unwrap_or_default();
        let path = bag_string(&bag, "DevicePath").unwrap_or_default();
        tracing::debug!(%name, %path, "DirectShow video input device");
        if !path.is_empty() && device_instance_key(&path) == want_key {
            let f = m.BindToObject::<_, _, IBaseFilter>(None, None).map_err(|e| com_err("IMoniker::BindToObject(filter)", e))?;
            return Ok((f, name));
        }
        if by_name.is_none() && !name.is_empty() && name.eq_ignore_ascii_case(device.trim()) {
            by_name = Some((m, name));
        }
    }
    let (m, name) = by_name.ok_or_else(|| err(CameraFault::NotFound, format!("no DirectShow filter for {device:?}")))?;
    let f = m.BindToObject::<_, _, IBaseFilter>(None, None).map_err(|e| com_err("IMoniker::BindToObject(filter)", e))?;
    Ok((f, name))
}

/// Find the camera's filter and its property pages; show them when `title` is given (blocks until
/// the dialog is closed). Returns how many pages the driver has.
pub fn camera_property_pages(device: &str, title: Option<&str>) -> Result<u32, CameraError> {
    if device == SYNTHETIC_CAMERA_ID {
        return Err(err(CameraFault::Unsupported, "the synthetic camera has no driver dialog"));
    }
    let device = device.to_string();
    let title = title.map(str::to_string);
    std::thread::Builder::new()
        .name("vox-camera-dialog".into())
        .spawn(move || -> Result<u32, CameraError> {
            let _sta = Sta::init();
            unsafe {
                let (filter, name) = find_filter(&device)?;
                let pages: ISpecifyPropertyPages =
                    filter.cast().map_err(|_| err(CameraFault::Unsupported, format!("{name}: no property pages")))?;
                let ca = pages.GetPages().map_err(|e| com_err("ISpecifyPropertyPages::GetPages", e))?;
                let count = ca.cElems;
                let clsids: Vec<GUID> = if ca.pElems.is_null() { Vec::new() } else { std::slice::from_raw_parts(ca.pElems, count as usize).to_vec() };
                if !ca.pElems.is_null() {
                    CoTaskMemFree(Some(ca.pElems as *const _));
                }
                tracing::info!(camera = %name, pages = count, show = title.is_some(), "camera driver property pages");
                if let Some(title) = title {
                    if clsids.is_empty() {
                        return Err(err(CameraFault::Unsupported, format!("{name}: no property pages")));
                    }
                    let wtitle: Vec<u16> = title.encode_utf16().chain(Some(0)).collect();
                    let unk: Option<windows_core::IUnknown> = Some(filter.cast().map_err(|e| com_err("IBaseFilter -> IUnknown", e))?);
                    // No owner window: the app keeps running (and drawing) while the dialog is up.
                    OleCreatePropertyFrame(HWND::default(), 0, 0, PCWSTR(wtitle.as_ptr()), 1, &unk, clsids.len() as u32, clsids.as_ptr(), 0, None, None)
                        .map_err(|e| com_err("OleCreatePropertyFrame", e))?;
                    tracing::info!(camera = %name, "camera driver dialog closed");
                }
                Ok(count)
            }
        })
        .map_err(|e| err(CameraFault::Other, format!("spawn dialog thread: {e}")))?
        .join()
        .map_err(|_| err(CameraFault::Other, "camera dialog thread panicked"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_foundation_and_directshow_paths_name_the_same_device() {
        let mf = r"\\?\usb#vid_046d&pid_082d&mi_00#8&1a2b3c4d&0&0000#{e5323777-f976-4f5b-9b55-b94699c46e44}\global";
        let ds = r"\\?\USB#VID_046D&PID_082D&MI_00#8&1a2b3c4d&0&0000#{65e8773d-8f56-11d0-a3b9-00a0c9223196}\global";
        assert_eq!(device_instance_key(mf), device_instance_key(ds));
        assert_eq!(device_instance_key(mf), "usb#vid_046d&pid_082d&mi_00#8&1a2b3c4d&0&0000");
        // Another port is another device instance.
        let other = r"\\?\usb#vid_046d&pid_082d&mi_00#7&2b3c4d5e&0&0000#{65e8773d-8f56-11d0-a3b9-00a0c9223196}\global";
        assert_ne!(device_instance_key(mf), device_instance_key(other));
        // A friendly name is left as it is (lowercased).
        assert_eq!(device_instance_key("HD Pro Webcam C920"), "hd pro webcam c920");
    }

    #[test]
    fn the_synthetic_camera_has_no_driver_dialog() {
        let e = camera_property_pages(SYNTHETIC_CAMERA_ID, None).unwrap_err();
        assert_eq!(e.fault, CameraFault::Unsupported);
    }
}
