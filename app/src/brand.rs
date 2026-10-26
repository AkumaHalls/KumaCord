//! AkumaLabs' name, logo and signature.
//!
//! - The logo (`assets/logo/`, PNGs with a transparent background and `akumalabs.ico`) is the
//!   window, taskbar, tray and notification icon (built here from the PNGs) and, through
//!   `build.rs`, the icon of the exe.
//! - In the app it sits on a quiet circle so it reads on the night background ([`logo`]), next to
//!   the wordmark "AkumaLabs" in Bricolage ([`wordmark`]).
//! - [`HOME_URL`] is the one server this client can ever talk to: the connect form and every
//!   invite land there (`app.rs`, `host.rs`).
//! - The signature ([`signature`]) is a quiet brand line at the bottom of the home screen.

use std::sync::Arc;

use eframe::egui::{self, Color32, Pos2, Rect, RichText, Sense, Vec2};

use crate::theme;

/// The app's name as a wordmark.
pub const APP_NAME: &str = "AkumaLabs";

/// The one server this client ever connects to (the brand's own hub). An invite typed into the
/// connect form carries only its token: whatever addresses the code embeds, the client goes to
/// this URL and this URL only (`app.rs::connect`, `host.rs::connect_invite`).
pub const HOME_URL: &str = "http://147.15.124.229:7890";

/// The logo at each size it was drawn at (transparent background).
const LOGO_PNGS: [(u32, &[u8]); 8] = [
    (16, include_bytes!("../assets/logo/akumalabs-logo-16.png")),
    (24, include_bytes!("../assets/logo/akumalabs-logo-24.png")),
    (32, include_bytes!("../assets/logo/akumalabs-logo-32.png")),
    (48, include_bytes!("../assets/logo/akumalabs-logo-48.png")),
    (64, include_bytes!("../assets/logo/akumalabs-logo-64.png")),
    (128, include_bytes!("../assets/logo/akumalabs-logo-128.png")),
    (256, include_bytes!("../assets/logo/akumalabs-logo-256.png")),
    (512, include_bytes!("../assets/logo/akumalabs-logo-512.png")),
];

/// The PNG drawn for `px` pixels: the smallest one at least that big (else the biggest).
pub fn logo_png(px: u32) -> (u32, &'static [u8]) {
    LOGO_PNGS.iter().copied().find(|(s, _)| *s >= px).unwrap_or(LOGO_PNGS[LOGO_PNGS.len() - 1])
}

/// The logo as `px` × `px` RGBA pixels (resized from the nearest bigger drawing when no PNG has
/// exactly that size, e.g. 20 px for the tray at 125%).
pub fn logo_rgba(px: u32) -> Option<image::RgbaImage> {
    let px = px.clamp(8, 512);
    let (size, bytes) = logo_png(px);
    let img = image::load_from_memory(bytes).ok()?.into_rgba8();
    Some(if size == px { img } else { image::imageops::resize(&img, px, px, image::imageops::FilterType::Lanczos3) })
}

/// The window / taskbar icon for eframe.
pub fn window_icon() -> Option<Arc<egui::IconData>> {
    let img = logo_rgba(256)?;
    Some(Arc::new(egui::IconData { width: img.width(), height: img.height(), rgba: img.into_raw() }))
}

// ---------------------------------------------------------------- in the app

/// The logo's texture (256 px with mipmaps, so it stays smooth at any size and zoom), loaded once.
fn logo_texture(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    let id = egui::Id::new("akumalabs_logo_texture");
    if let Some(t) = ctx.data(|d| d.get_temp::<egui::TextureHandle>(id)) {
        return Some(t);
    }
    let img = logo_rgba(256)?;
    let color = egui::ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw());
    let opts = egui::TextureOptions { mipmap_mode: Some(egui::TextureFilter::Linear), ..egui::TextureOptions::LINEAR };
    let t = ctx.load_texture("akumalabs-logo", color, opts);
    ctx.data_mut(|d| d.insert_temp(id, t.clone()));
    Some(t)
}

/// Paint the logo, `side` points wide, centred in `rect`: on a quiet circle a little bigger than
/// itself (its dark art would otherwise melt into the night background).
pub fn paint_logo(painter: &egui::Painter, ctx: &egui::Context, center: Pos2, side: f32) {
    painter.circle_filled(center, side / 2.0 + side * 0.07, theme::RISCO);
    if let Some(t) = logo_texture(ctx) {
        let rect = Rect::from_center_size(center, Vec2::splat(side));
        painter.image(t.id(), rect, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
    }
}

/// The logo as a widget, `side` points wide (plus its circle).
pub fn logo(ui: &mut egui::Ui, side: f32) -> egui::Response {
    let outer = side * 1.14;
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(outer), Sense::hover());
    if ui.is_rect_visible(rect) {
        paint_logo(ui.painter(), ui.ctx(), rect.center(), side);
    }
    resp
}

/// The logo and the wordmark "AkumaLabs" side by side (the home screen, Configurações → Sobre).
pub fn wordmark(ui: &mut egui::Ui, logo_side: f32, text_size: f32) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = logo_side * 0.3;
        logo(ui, logo_side);
        ui.label(RichText::new(APP_NAME).font(theme::display(text_size)).color(theme::GIZ));
    });
}

/// The brand line: "AkumaLabs", quiet, no link. `size` is the text size; `color` its colour.
pub fn signature(ui: &mut egui::Ui, size: f32, color: Color32) {
    ui.label(RichText::new(APP_NAME).font(theme::body(size)).color(color));
}

/// [`signature`] along the bottom of `area` (the home screen's left panel).
pub fn signature_footer(ui: &mut egui::Ui, area: Rect, size: f32) {
    let line = ui.painter().layout_no_wrap(APP_NAME.to_string(), theme::body(size), theme::NEVOA);
    let h = line.size().y + 2.0;
    let rect = Rect::from_min_max(Pos2::new(area.left(), area.bottom() - h), area.max);
    // (A child that takes no room in `ui`: the rest of the panel lays out from the top as before.)
    let mut ui = ui.new_child(egui::UiBuilder::new().max_rect(rect));
    signature(&mut ui, size, theme::NEVOA);
}

// ---------------------------------------------------------------- Windows icons

/// An icon handle of the logo, `px` pixels square (the tray, the balloon, the window). Freed by
/// Windows when the process ends; made a handful of times per run.
#[cfg(windows)]
pub fn hicon(px: u32) -> Option<isize> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{CreateIconFromResourceEx, LR_DEFAULTCOLOR};
    let img = logo_rgba(px)?;
    // CreateIconFromResourceEx reads PNG data as an icon image: the simplest way to a transparent
    // icon (the same trick eframe uses).
    let mut png = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).ok()?;
    // SAFETY: reads `png` (valid for the call) and returns a new icon handle or null.
    let h = unsafe { CreateIconFromResourceEx(png.as_ptr(), png.len() as u32, 1, 0x0003_0000, px as i32, px as i32, LR_DEFAULTCOLOR) };
    (!h.is_null()).then_some(h as isize)
}

/// Give the main window the logo at the sizes Windows asks for (title bar / Alt+Tab and taskbar).
/// eframe does it too, but only once the window is active, which a background autotest never is.
#[cfg(windows)]
pub fn set_window_icons(hwnd: isize) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SendMessageW, ICON_BIG, ICON_SMALL, SM_CXICON, SM_CXSMICON, WM_SETICON};
    // SAFETY: plain queries and messages to a window of this process.
    unsafe {
        let big = GetSystemMetrics(SM_CXICON).max(16) as u32;
        let small = GetSystemMetrics(SM_CXSMICON).max(16) as u32;
        if let Some(h) = hicon(big) {
            SendMessageW(hwnd as _, WM_SETICON, ICON_BIG as usize, h);
        }
        if let Some(h) = hicon(small) {
            SendMessageW(hwnd as _, WM_SETICON, ICON_SMALL as usize, h);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_logo_png_decodes_at_its_size_with_a_transparent_corner() {
        for (size, bytes) in LOGO_PNGS {
            let img = image::load_from_memory(bytes).unwrap().into_rgba8();
            assert_eq!((img.width(), img.height()), (size, size));
            assert_eq!(img.get_pixel(0, 0)[3], 0, "{size}: transparent background");
            let c = img.get_pixel(size / 2, size / 2);
            assert!(c[3] == 255 && c[0] > 200, "{size}: the bright mark in the middle");
        }
    }

    #[test]
    fn the_nearest_bigger_drawing_is_used() {
        assert_eq!(logo_png(16).0, 16);
        assert_eq!(logo_png(20).0, 24);
        assert_eq!(logo_png(40).0, 48);
        assert_eq!(logo_png(4000).0, 512);
        let img = logo_rgba(20).unwrap();
        assert_eq!((img.width(), img.height()), (20, 20));
        assert!(window_icon().is_some_and(|i| i.width == 256 && i.rgba.len() == 256 * 256 * 4));
    }

    #[test]
    fn the_app_points_at_one_well_formed_home() {
        assert_eq!(APP_NAME, "AkumaLabs");
        assert!(HOME_URL.starts_with("http://"), "the hub speaks plain http");
        let host_port = HOME_URL.trim_start_matches("http://");
        let (host, port) = host_port.rsplit_once(':').expect("host:port");
        assert!(!host.is_empty());
        assert_eq!(port.parse::<u16>().unwrap(), 7890);
    }

    /// The icon assets that ship with the binaries must be structurally valid: the .icns per the
    /// Apple format (big-endian lengths, walk ends at EOF) and the .ico directory within bounds.
    /// The PNGs are checked by the tests above; these two had no gate until a generator once
    /// wrote an icns with little-endian lengths that no conformant reader accepts.
    #[test]
    fn the_icns_and_ico_are_well_formed() {
        let icns = include_bytes!("../assets/logo/akumalabs.icns");
        assert_eq!(&icns[0..4], b"icns", "icns magic");
        let total = u32::from_be_bytes([icns[4], icns[5], icns[6], icns[7]]) as usize;
        assert_eq!(total, icns.len(), "icns total length must equal the file size (big-endian)");
        let mut off = 8;
        let mut entries = 0;
        while off < icns.len() {
            assert!(off + 8 <= icns.len(), "icns entry header out of bounds");
            let len = u32::from_be_bytes([icns[off + 4], icns[off + 5], icns[off + 6], icns[off + 7]]) as usize;
            assert!(len >= 8 && off + len <= icns.len(), "icns entry length out of bounds at {off}");
            off += len;
            entries += 1;
        }
        assert_eq!(off, icns.len(), "icns must end exactly at the last entry");
        assert!(entries > 0);

        let ico = include_bytes!("../assets/logo/akumalabs.ico");
        assert_eq!(u16::from_le_bytes([ico[0], ico[1]]), 0, "ico reserved");
        assert_eq!(u16::from_le_bytes([ico[2], ico[3]]), 1, "ico type = icon");
        let count = u16::from_le_bytes([ico[4], ico[5]]) as usize;
        assert!(count > 0 && count <= 16);
        let mut end = 6 + count * 16;
        for i in 0..count {
            let e = 6 + i * 16;
            let bytes = u32::from_le_bytes([ico[e + 8], ico[e + 9], ico[e + 10], ico[e + 11]]) as usize;
            let start = u32::from_le_bytes([ico[e + 12], ico[e + 13], ico[e + 14], ico[e + 15]]) as usize;
            assert!(start >= end && start + bytes <= ico.len(), "ico entry {i} out of bounds");
            end = start + bytes;
        }
        assert_eq!(end, ico.len(), "ico must end exactly at the last entry");
    }
}
