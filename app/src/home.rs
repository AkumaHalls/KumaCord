//! The AkumaLabs home screen: the community's cinematic full-bleed, the brand and the headline
//! over it on the left, and one translucent card on the right with the single server this client
//! joins ([`brand::HOME_URL`]) and the name to enter with.
//!
//! There is no server list and no address field: the server is already the AkumaLabs official one.
//! The server password is asked only when it is actually needed (a failed login says so, or a
//! saved entry lost its password), and an invite (`vox1-`) is one optional link away. A child
//! module of `app`.

use eframe::egui::{self, Margin, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, UiBuilder, Vec2};

use super::App;
use crate::brand;
use crate::theme;
use crate::ui_kit::{self, Kind, Size};

// The launcher's own accents, chosen to sit on the footage: the app's blurple, one step deeper,
// and the violet of the community's art. The rest of the app keeps `theme`'s tokens.
const ACCENT: egui::Color32 = egui::Color32::from_rgb(0x7C, 0x3A, 0xED);
const ACCENT_HOVER: egui::Color32 = egui::Color32::from_rgb(0x8B, 0x5C, 0xF6);
const ACCENT_DEEP: egui::Color32 = egui::Color32::from_rgb(0x66, 0x2C, 0xD0);
const CARD: egui::Color32 = egui::Color32::from_rgba_premultiplied(12, 12, 22, 216);
const HAIRLINE: egui::Color32 = egui::Color32::from_rgba_premultiplied(26, 26, 26, 26);
const STRIP: egui::Color32 = egui::Color32::from_rgba_premultiplied(12, 12, 12, 12);

impl App {
    pub(super) fn connect_screen(&mut self, ui: &mut egui::Ui) {
        let full = ui.max_rect();
        self.intro.paint(ui, full);
        let wide = full.width() >= 1000.0 && full.height() >= 560.0;
        let margin = if full.width() < 900.0 { 26.0 } else { 44.0 };
        let card_w = if wide { 404.0 } else { (full.width() - 2.0 * margin).min(404.0) };
        let card_x = if wide { full.right() - margin - card_w } else { full.center().x - card_w / 2.0 };
        let card_top = if wide { full.top() + (full.height() * 0.13).clamp(56.0, 92.0) } else { full.top() + 74.0 };

        self.brand_block(ui, full, margin, wide);
        self.corner_controls(ui, full, margin);
        if wide {
            self.hero(ui, full, margin, card_x);
        }
        self.entry_card(ui, full, card_x, card_top, card_w, margin);
        if !wide {
            // The card takes the room: a quiet line under it instead of the big headline.
            let y = (card_top + 470.0).min(full.bottom() - 34.0);
            let mut foot = ui.new_child(UiBuilder::new().max_rect(Rect::from_min_size(Pos2::new(margin, y), Vec2::new(full.width() - 2.0 * margin, 24.0))));
            foot.label(RichText::new("Voz, tela e chat da comunidade AkumaLabs.").font(theme::body(12.5)).color(theme::NEVOA.gamma_multiply(0.8)));
        }
    }

    /// Top left: the logo, the wordmark and one quiet line. It sits on the movie's top scrim.
    fn brand_block(&self, ui: &mut egui::Ui, full: Rect, margin: f32, wide: bool) {
        let area = Rect::from_min_size(Pos2::new(full.left() + margin, full.top() + margin - 8.0), Vec2::new(460.0, 64.0));
        let mut ui = ui.new_child(UiBuilder::new().max_rect(area));
        brand::wordmark(&mut ui, if wide { 34.0 } else { 28.0 }, if wide { 27.0 } else { 23.0 });
        ui.add_space(2.0);
        ui.label(RichText::new("A comunidade da AkumaLabs, em voz e tela.").font(theme::body(12.5)).color(theme::NEVOA));
    }

    /// Top right: the movie's sound, and "Pular" while it plays.
    fn corner_controls(&mut self, ui: &mut egui::Ui, full: Rect, margin: f32) {
        if !self.intro.playing() {
            return;
        }
        let area = Rect::from_min_size(Pos2::new(full.right() - margin - 250.0, full.top() + margin - 6.0), Vec2::new(250.0, 34.0));
        let mut ui = ui.new_child(UiBuilder::new().max_rect(area).layout(egui::Layout::right_to_left(egui::Align::Center)));
        egui::Frame::new()
            .fill(egui::Color32::from_rgba_premultiplied(5, 5, 10, 140))
            .corner_radius(theme::radius::PILL)
            .inner_margin(Margin::symmetric(8, 3))
            .show(&mut ui, |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let skip = ui_kit::button_sized(ui, Kind::Ghost, Size::Small, Some(theme::ph::X), "Pular");
                if skip.on_hover_text("Deixar a abertura de lado").clicked() {
                    self.intro.skip();
                }
                let (icon, hint) = if self.intro.sound { (theme::ph::SPEAKER_HIGH, "Silenciar a abertura") } else { (theme::ph::SPEAKER_SLASH, "Ouvir a abertura") };
                let sound = ui_kit::button_sized(ui, Kind::Ghost, Size::Small, Some(icon), "");
                if sound.on_hover_text(hint).clicked() {
                    self.intro.toggle_sound();
                }
            });
    }

    /// Bottom left: the headline that the movie was picked for.
    fn hero(&self, ui: &mut egui::Ui, full: Rect, margin: f32, card_x: f32) {
        let w = (card_x - margin - 56.0).min(560.0).max(320.0);
        let area = Rect::from_min_size(Pos2::new(full.left() + margin, full.bottom() - 236.0), Vec2::new(w, 190.0));
        let mut ui = ui.new_child(UiBuilder::new().max_rect(area));
        ui.spacing_mut().item_spacing.y = 10.0;
        let fade = self.intro_reveal(0.15);
        let eyebrow = egui::Color32::from_rgb(0xC4, 0xB5, 0xFD).gamma_multiply(fade);
        ui.label(RichText::new("VOZ  ·  TELA  ·  CHAT").font(theme::semibold(11.5)).color(eyebrow));
        let big = (full.height() * 0.062).clamp(34.0, 52.0);
        ui.label(RichText::new("Sua comunidade.").font(theme::display(big)).color(theme::GIZ.gamma_multiply(fade)));
        ui.label(RichText::new("Uma só chamada.").font(theme::display(big)).color(ACCENT_HOVER.gamma_multiply(fade)));
        ui.add_space(2.0);
        ui.label(
            RichText::new("Entre no servidor oficial da AkumaLabs e encontre a galera: conversa, voz e tela compartilhada em tempo real.")
                .font(theme::body(14.0))
                .color(theme::TEXTO.gamma_multiply(fade * 0.92)),
        );
    }

    /// The login card, in the place the launcher's login always had.
    fn entry_card(&mut self, ui: &mut egui::Ui, full: Rect, x: f32, top: f32, w: f32, margin: f32) {
        let pad = 24.0;
        let avail_h = (full.bottom() - top - margin).max(240.0);
        let inner = Rect::from_min_size(Pos2::new(x + pad, top + pad), Vec2::new(w - 2.0 * pad, avail_h - 2.0 * pad));
        let painter = ui.painter().clone();
        // Reserved before the content: the panel is painted behind it, sized to what the content
        // actually used (the card grows for an error, a password, an invite).
        let slot = painter.add(egui::Shape::Noop);
        let mut child = ui.new_child(UiBuilder::new().max_rect(inner));
        child.spacing_mut().item_spacing.y = 6.0;
        let used = egui::ScrollArea::vertical()
            .max_height(avail_h - 2.0 * pad)
            .auto_shrink([false, true])
            .show(&mut child, |ui| {
                ui.set_width(w - 2.0 * pad);
                self.card_body(ui, w - 2.0 * pad);
            })
            .inner;
        let _ = used;
        let bottom = (child.min_rect().bottom() + pad).min(full.bottom() - 12.0);
        let rect = Rect::from_min_max(Pos2::new(x, top), Pos2::new(x + w, bottom.max(top + 320.0)));
        painter.set(slot, egui::Shape::rect_filled(rect, 18, CARD));
        painter.rect_stroke(rect, 18, Stroke::new(1.0, HAIRLINE), StrokeKind::Inside);
        // A violet hairline along the top edge: the card's one bit of colour.
        let line = Rect::from_min_max(Pos2::new(rect.left() + 22.0, rect.top()), Pos2::new(rect.right() - 22.0, rect.top() + 2.0));
        painter.rect_filled(line, 1, ACCENT.gamma_multiply(0.85));
    }

    /// Everything inside the card.
    fn card_body(&mut self, ui: &mut egui::Ui, width: f32) {
        let fade = self.intro_reveal(0.05);
        ui.add_space(2.0);
        ui.label(RichText::new("ACESSO AO SERVIDOR").font(theme::semibold(10.5)).color(theme::NEVOA.gamma_multiply(fade)));
        ui.add_space(2.0);
        ui.label(RichText::new("Bem-vindo de volta").font(theme::display(26.0)).color(theme::GIZ.gamma_multiply(fade)));
        ui.label(RichText::new("Use o nome pelo qual a galera te conhece.").font(theme::body(13.0)).color(theme::NEVOA.gamma_multiply(fade)));
        ui.add_space(14.0);
        self.server_strip(ui, width);
        ui.add_space(14.0);

        let mut submit = false;
        let enter = |ui: &egui::Ui, r: &egui::Response| r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));

        ui.label(RichText::new("Seu nome").font(theme::semibold(12.5)).color(theme::NEVOA));
        let name = ui.add(ui_kit::field(&mut self.form.user).hint_text("como os outros vão ver você").desired_width(f32::INFINITY).min_size(Vec2::new(0.0, 40.0)));
        submit |= enter(ui, &name);

        // The password is not part of the flow: it shows when the server asked for it (an error
        // saying so, or a saved entry that lost it) and not while an invite is being used.
        if self.show_password() {
            ui.add_space(8.0);
            ui.label(RichText::new("Senha do servidor").font(theme::semibold(12.5)).color(theme::NEVOA));
            let revealed = self.form.reveal_password;
            let mut pw: Option<egui::Response> = None;
            ui.horizontal(|ui| {
                let mut field = ui_kit::field(&mut self.form.password)
                    .password(!revealed)
                    .desired_width(ui.available_width() - 40.0)
                    .min_size(Vec2::new(0.0, 40.0));
                if self.password_hint.is_some() {
                    field = field.hint_text("a senha que combinaram com você");
                }
                pw = Some(ui.add(field));
                let eye = ui_kit::button_sized(ui, Kind::Ghost, Size::Normal, Some(if revealed { theme::ph::EYE_SLASH } else { theme::ph::EYE }), "");
                if eye.on_hover_text(if revealed { "Esconder a senha" } else { "Mostrar a senha" }).clicked() {
                    self.form.reveal_password = !revealed;
                }
            });
            let pw = pw.expect("the field was added");
            if std::mem::take(&mut self.servers.focus_password) {
                pw.request_focus();
            }
            submit |= enter(ui, &pw);
            ui.add_space(2.0);
            ui.checkbox(&mut self.form.remember, "Lembrar a senha neste computador");
        }
        // Item 12: only once the server says this name is protected.
        if self.form.need_personal {
            ui.add_space(8.0);
            ui.label(RichText::new("Senha pessoal").font(theme::semibold(12.5)).color(theme::NEVOA));
            let personal = ui.add(
                ui_kit::field(&mut self.form.personal)
                    .password(true)
                    .hint_text("a senha do seu nome (ou o código recebido)")
                    .desired_width(f32::INFINITY)
                    .min_size(Vec2::new(0.0, 40.0)),
            );
            submit |= enter(ui, &personal);
            ui.add_space(2.0);
            ui.checkbox(&mut self.form.remember_device, "Lembrar este computador");
        }

        if let Some(err) = self.form.error.clone() {
            ui.add_space(10.0);
            ui_kit::notice_frame(ui_kit::Notice::Error).show(ui, |ui| {
                ui.set_max_width(width - 26.0);
                ui.label(RichText::new(err).font(theme::body(12.5)).color(theme::GIZ));
            });
        }

        ui.add_space(14.0);
        let label = if self.form.busy { "Entrando..." } else { "Entrar no servidor" };
        let clicked = self.cta(ui, label, !self.form.busy);
        if clicked || submit {
            self.connect();
        }

        ui.add_space(10.0);
        if self.form.show_invite {
            ui.label(RichText::new("Convite").font(theme::semibold(12.5)).color(theme::NEVOA));
            let invite = ui.add(
                ui_kit::field(&mut self.form.url)
                    .hint_text("vox1-... (um convite entra sem a senha do servidor)")
                    .desired_width(f32::INFINITY)
                    .min_size(Vec2::new(0.0, 38.0)),
            );
            submit |= enter(ui, &invite);
            if submit {
                self.connect();
            }
            let off = ui_kit::button_sized(ui, Kind::Ghost, Size::Small, Some(theme::ph::X), "Sem convite");
            if off.clicked() {
                self.form.show_invite = false;
                self.form.url.clear();
            }
        } else {
            let have = ui_kit::button_sized(ui, Kind::Ghost, Size::Normal, Some(theme::ph::ENVELOPE_SIMPLE), "Tenho um convite");
            if have.on_hover_text("Convites entram sem a senha do servidor").clicked() {
                self.form.show_invite = true;
                self.form.show_password = false;
            }
        }

        ui.add_space(10.0);
        ui.separator();
        ui.add_space(6.0);
        ui.label(
            RichText::new("Servidor oficial da AkumaLabs. Ao entrar, você concorda com as regras da comunidade.")
                .font(theme::body(11.5))
                .color(theme::NEVOA.gamma_multiply(0.75)),
        );
    }

    /// The one server, already chosen: the card that says this is the official one.
    fn server_strip(&self, ui: &mut egui::Ui, width: f32) {
        let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 66.0), Sense::hover());
        if !ui.is_rect_visible(rect) {
            return;
        }
        let p = ui.painter();
        p.rect_filled(rect, 12, STRIP);
        p.rect_stroke(rect, 12, Stroke::new(1.0, ACCENT.gamma_multiply(0.45)), StrokeKind::Inside);
        // The logo, small, in a rounded square.
        let icon = Rect::from_min_size(Pos2::new(rect.left() + 12.0, rect.top() + 13.0), Vec2::splat(40.0));
        p.rect_filled(icon, 10, egui::Color32::from_rgb(0x1B, 0x18, 0x2E));
        brand::paint_logo(p, ui.ctx(), icon.center(), 30.0);
        let text_x = icon.right() + 12.0;
        p.text(Pos2::new(text_x, rect.top() + 15.0), egui::Align2::LEFT_TOP, "AkumaLabs", theme::semibold(15.5), theme::GIZ);
        let address = brand::HOME_URL.trim_start_matches("http://").trim_end_matches('/');
        p.text(Pos2::new(text_x, rect.top() + 36.0), egui::Align2::LEFT_TOP, address, egui::FontId::monospace(11.5), theme::NEVOA);
        // "OFICIAL" on the right.
        let text = "OFICIAL";
        let g = p.layout_no_wrap(text.to_owned(), theme::semibold(9.5), egui::Color32::from_rgb(0xE7, 0xDE, 0xFF));
        let pill = Rect::from_min_size(Pos2::new(rect.right() - g.size().x - 26.0, rect.top() + 21.0), Vec2::new(g.size().x + 16.0, 20.0));
        p.rect_filled(pill, theme::radius::PILL, ACCENT.gamma_multiply(0.28));
        p.galley(Pos2::new(pill.left() + 8.0, pill.center().y - g.size().y / 2.0), g, egui::Color32::from_rgb(0xE7, 0xDE, 0xFF));
    }

    /// The one main action, as wide as the card.
    fn cta(&self, ui: &mut egui::Ui, text: &str, enabled: bool) -> bool {
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 46.0), Sense::click());
        if ui.is_rect_visible(rect) {
            let pressed = resp.is_pointer_button_down_on();
            let fill = if !enabled {
                ACCENT.gamma_multiply(0.45)
            } else if pressed {
                ACCENT_DEEP
            } else if resp.hovered() {
                ACCENT_HOVER
            } else {
                ACCENT
            };
            let p = ui.painter();
            p.rect_filled(rect, 12, fill);
            if resp.hovered() && enabled {
                p.rect_stroke(rect, 12, Stroke::new(1.0, egui::Color32::from_rgba_premultiplied(255, 255, 255, 60)), StrokeKind::Inside);
            }
            if resp.has_focus() {
                p.rect_stroke(rect, 12, Stroke::new(1.5, egui::Color32::WHITE), StrokeKind::Inside);
            }
            let label = p.layout_no_wrap(text.to_owned(), theme::semibold(14.5), egui::Color32::WHITE);
            let glyph = p.layout_no_wrap(theme::ph::ARROW_RIGHT.to_owned(), theme::body(17.0), egui::Color32::PLACEHOLDER);
            let gap = 8.0;
            let label_size = label.size();
            let content = label_size.x + gap + glyph.size().x;
            let x = rect.center().x - content / 2.0;
            p.galley(Pos2::new(x, rect.center().y - label_size.y / 2.0), label, egui::Color32::WHITE);
            p.galley(Pos2::new(x + label_size.x + gap, rect.center().y - glyph.size().y / 2.0), glyph, egui::Color32::WHITE);
        }
        resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, text));
        resp.clicked() && enabled
    }

    /// Whether the server password field belongs on screen right now.
    fn show_password(&self) -> bool {
        if vox_proto::looks_like_invite(&self.form.url) {
            return false;
        }
        self.form.show_password || self.servers.focus_password || self.password_hint.is_some() || self.form.error.as_deref().is_some_and(password_asked)
    }

    /// The movie fades the text in over its first moments (1 = fully there).
    fn intro_reveal(&self, delay: f32) -> f32 {
        if !self.intro.playing() && self.intro.shown_first() {
            return 1.0;
        }
        let t = self.intro.elapsed() - delay;
        (t / 0.45).clamp(0.0, 1.0)
    }
}

/// Whether a login error is the server asking for its password.
fn password_asked(error: &str) -> bool {
    error.to_lowercase().contains("senha do servidor")
}

/// Kept for the autotests that build the home screen without a window.
#[allow(dead_code)]
pub fn reveal_all() {}
