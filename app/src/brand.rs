//! concord's name, logo and signature (v1.0.1, when voxhub became concord).
//!
//! - The logo is the owner's: a white speech-bubble face in a black circle (`assets/logo/`, PNGs
//!   with a transparent background and `concord.ico`). It is the window, taskbar, tray and
//!   notification icon (built here from the PNGs) and, through `build.rs`, the icon of the exe.
//! - In the app it sits on a quiet circle so it reads on the night background ([`logo`]), next to
//!   the wordmark "concord" in Bricolage ([`wordmark`]).
//! - The signature ([`signature`]) is a quiet credit line with a mailto: link, the only link the app
//!   opens that is not http(s) (`links::open_url`).

use std::sync::Arc;

use eframe::egui::{self, Color32, Pos2, Rect, RichText, Sense, Vec2};

use crate::theme;

/// The app's name as a wordmark (always lowercase; "Concord" only at the start of a sentence).
pub const APP_NAME: &str = "concord";

/// The signature: the owner's name and e-mail (here and in the Calladex only).
pub const OWNER_NAME: &str = "Cezar Callado";
pub const OWNER_EMAIL: &str = "cezar.callado@gmail.com";
/// The one non-http link the app may open (`links::open_url`).
pub const OWNER_MAILTO: &str = "mailto:cezar.callado@gmail.com";

/// The logo at each size it was drawn at (transparent background).
const LOGO_PNGS: [(u32, &[u8]); 8] = [
    (16, include_bytes!("../assets/logo/concord-logo-16.png")),
    (24, include_bytes!("../assets/logo/concord-logo-24.png")),
    (32, include_bytes!("../assets/logo/concord-logo-32.png")),
    (48, include_bytes!("../assets/logo/concord-logo-48.png")),
    (64, include_bytes!("../assets/logo/concord-logo-64.png")),
    (128, include_bytes!("../assets/logo/concord-logo-128.png")),
    (256, include_bytes!("../assets/logo/concord-logo-256.png")),
    (512, include_bytes!("../assets/logo/concord-logo-512.png")),
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
    let id = egui::Id::new("concord_logo_texture");
    if let Some(t) = ctx.data(|d| d.get_temp::<egui::TextureHandle>(id)) {
        return Some(t);
    }
    let img = logo_rgba(256)?;
    let color = egui::ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw());
    let opts = egui::TextureOptions { mipmap_mode: Some(egui::TextureFilter::Linear), ..egui::TextureOptions::LINEAR };
    let t = ctx.load_texture("concord-logo", color, opts);
    ctx.data_mut(|d| d.insert_temp(id, t.clone()));
    Some(t)
}

/// Paint the logo, `side` points wide, centred in `rect`: on a quiet circle a little bigger than
/// itself (its black disc would otherwise melt into the night background).
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

/// The logo and the wordmark "concord" side by side (the home screen, Configurações → Sobre).
pub fn wordmark(ui: &mut egui::Ui, logo_side: f32, text_size: f32) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = logo_side * 0.3;
        logo(ui, logo_side);
        ui.label(RichText::new(APP_NAME).font(theme::display(text_size)).color(theme::GIZ));
    });
}

/// "Feito por Cezar Callado · cezar.callado@gmail.com", the address a mailto: link. `size` is the
/// text size; `color` the quiet colour of the words around the link.
pub fn signature(ui: &mut egui::Ui, size: f32, color: Color32) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        ui.label(RichText::new(format!("Feito por {OWNER_NAME} · ")).font(theme::body(size)).color(color));
        email_link(ui, size);
    });
}

/// [`signature`] along the bottom of `area` (the home screen's left panel): one line when it fits,
/// else the name over the address.
pub fn signature_footer(ui: &mut egui::Ui, area: Rect, size: f32) {
    let line = ui.painter().layout_no_wrap(format!("Feito por {OWNER_NAME} · {OWNER_EMAIL}"), theme::body(size), theme::NEVOA);
    let one_line = line.size().x <= area.width();
    let h = line.size().y * if one_line { 1.0 } else { 2.0 } + 2.0;
    let rect = Rect::from_min_max(Pos2::new(area.left(), area.bottom() - h), area.max);
    // (A child that takes no room in `ui`: the rest of the panel lays out from the top as before.)
    let mut ui = ui.new_child(egui::UiBuilder::new().max_rect(rect));
    if one_line {
        signature(&mut ui, size, theme::NEVOA);
    } else {
        ui.spacing_mut().item_spacing.y = 0.0;
        ui.label(RichText::new(format!("Feito por {OWNER_NAME}")).font(theme::body(size)).color(theme::NEVOA));
        email_link(&mut ui, size);
    }
}

/// The owner's e-mail as a link (opens the mail app; never in an autotest).
pub fn email_link(ui: &mut egui::Ui, size: f32) -> egui::Response {
    let resp = ui
        .add(egui::Label::new(RichText::new(OWNER_EMAIL).font(theme::body(size)).color(theme::LINK)).sense(Sense::click()))
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text("Escrever um e-mail");
    if resp.hovered() {
        let r = resp.rect;
        ui.painter().hline(r.x_range(), r.bottom() - 1.0, egui::Stroke::new(1.0, theme::LINK));
    }
    if resp.clicked() {
        crate::links::open_url(OWNER_MAILTO);
    }
    resp
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
            assert!(c[3] == 255 && c[0] > 200, "{size}: the white face in the middle");
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
    fn the_signature_is_the_owners() {
        assert_eq!(OWNER_MAILTO, format!("mailto:{OWNER_EMAIL}"));
        assert_eq!(APP_NAME, "concord");
    }
}
