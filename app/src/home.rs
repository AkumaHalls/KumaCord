//! The v1.0 home screen (the mockup's "Tela inicial"): on the left the logo (v1.0.1: concord's
//! bubble and wordmark), the tagline, the two tabs and the signature at the bottom; on the right
//! the saved servers (one click to join, `servers.rs`) with the connect form under them, or
//! "Criar ou abrir servidor" (`host.rs`). A child module of `app`.

use eframe::egui::{self, Margin, RichText};

use super::App;
use crate::brand;
use crate::host::HomeTab;
use crate::theme;
use crate::ui_kit::{self, Kind};

/// Width of the right column's content.
const FORM_W: f32 = 420.0;

impl App {
    pub(super) fn connect_screen(&mut self, ui: &mut egui::Ui) {
        // "calladex" typed here, with no field focused, opens the easter egg's credits.
        self.calladex_home_keys(ui.ctx());
        let w = (ui.available_width() * 0.4).clamp(280.0, 440.0);
        egui::Panel::left("home_left")
            .exact_size(w)
            .resizable(false)
            .show_separator_line(false)
            .frame(egui::Frame::new().fill(theme::PAREDE).inner_margin(Margin::symmetric(36, 36)))
            .show(ui, |ui| self.home_left(ui));
        egui::CentralPanel::default().frame(egui::Frame::new().fill(theme::NOITE)).show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                let few = self.settings.servers.servers.len() < 4;
                ui.add_space(if few { 64.0 } else { 32.0 });
                let side = ((ui.available_width() - FORM_W) / 2.0).max(28.0);
                ui.horizontal(|ui| {
                    ui.add_space(side);
                    ui.vertical(|ui| {
                        ui.set_max_width(FORM_W);
                        self.home_right(ui);
                    });
                });
                ui.add_space(32.0);
            });
        });
    }

    fn home_left(&mut self, ui: &mut egui::Ui) {
        let h = ui.available_height();
        // The signature, small and quiet at the bottom of the panel (two lines when narrow).
        brand::signature_footer(ui, ui.max_rect(), 11.5);
        ui.add_space((h * 0.30 - 60.0).max(0.0));
        brand::wordmark(ui, 40.0, 40.0);
        ui.add_space(10.0);
        ui.add(egui::Label::new(RichText::new("Voz, tela e chat para o seu grupo, rodando no PC de um de vocês.").font(theme::body(15.0)).color(theme::NEVOA)).wrap());
        ui.add_space(28.0);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
            for (tab, label) in [(HomeTab::Connect, "Conectar"), (HomeTab::Host, "Criar ou abrir servidor")] {
                if tab_button(ui, label, self.host.tab == tab).clicked() {
                    self.host.tab = tab;
                    self.host.error = None;
                }
            }
        });
    }

    fn home_right(&mut self, ui: &mut egui::Ui) {
        if self.host.tab == HomeTab::Host {
            if !self.host.listed {
                self.refresh_servers();
            }
            ui_kit::title(ui, "Criar ou abrir servidor");
            ui.add_space(12.0);
            self.host_home(ui);
            return;
        }
        // v0.9.1: the saved servers first; the form below is "Adicionar servidor".
        if !self.saved_servers(ui) {
            return;
        }
        if self.settings.servers.servers.is_empty() {
            ui_kit::title(ui, "Entrar em um servidor");
            ui.add_space(12.0);
        }
        self.connect_form(ui);
    }

    /// The connect form: address (or invite), name, the server password (not with an invite), the
    /// personal password once the server asks for it, the switches and "Conectar".
    fn connect_form(&mut self, ui: &mut egui::Ui) {
        let invite = vox_proto::looks_like_invite(&self.form.url);
        let mut submit = false;
        let label = |ui: &mut egui::Ui, text: &str| {
            ui.label(RichText::new(text).font(theme::semibold(12.5)).color(theme::NEVOA));
        };
        let enter = |ui: &egui::Ui, r: &egui::Response| r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        ui.spacing_mut().item_spacing.y = 6.0;
        label(ui, "Servidor ou convite");
        let r = ui.add(ui_kit::field(&mut self.form.url).hint_text("192.168.0.10 ou vox1-...").desired_width(FORM_W));
        submit |= enter(ui, &r);
        ui.add_space(4.0);
        label(ui, "Seu nome");
        let r = ui.add(ui_kit::field(&mut self.form.user).hint_text("como os outros vão ver você").desired_width(FORM_W));
        submit |= enter(ui, &r);
        // An invite replaces the server password.
        if !invite {
            ui.add_space(4.0);
            label(ui, "Senha do servidor");
            let r = ui.add(ui_kit::field(&mut self.form.password).password(true).desired_width(FORM_W));
            if std::mem::take(&mut self.servers.focus_password) {
                // A saved server whose password is not kept: the cursor waits here.
                r.request_focus();
            }
            submit |= enter(ui, &r);
        }
        // Item 12: only once the server says this name is protected.
        if self.form.need_personal {
            ui.add_space(4.0);
            label(ui, "Senha pessoal");
            let r = ui.add(ui_kit::field(&mut self.form.personal).password(true).hint_text("a senha do seu nome (ou o código recebido)").desired_width(FORM_W));
            submit |= enter(ui, &r);
        }
        ui.add_space(8.0);
        ui.checkbox(&mut self.form.remember, "Lembrar a senha neste computador (protegida pelo Windows)");
        if self.form.need_personal {
            ui.checkbox(&mut self.form.remember_device, "Lembrar este computador (não pedir a senha pessoal de novo)");
        }
        ui.checkbox(&mut self.form.save_server, "Salvar na lista de servidores")
            .on_hover_text("Depois de entrar, o servidor aparece em \"Seus servidores\": da próxima vez é só clicar.");
        ui.add_space(14.0);
        let text = if self.form.busy { "Conectando..." } else { "Conectar" };
        let clicked = ui.add_enabled_ui(!self.form.busy, |ui| ui_kit::button_sized(ui, Kind::Primary, ui_kit::Size::Large, None, text)).inner.clicked();
        if clicked || submit {
            self.connect();
        }
        if let Some(err) = &self.form.error {
            ui.add_space(12.0);
            ui_kit::notice_frame(ui_kit::Notice::Error).show(ui, |ui| {
                ui.set_max_width(FORM_W - 28.0);
                ui.label(RichText::new(err).color(theme::GIZ));
            });
        }
    }
}

/// A home tab: raised when on, quiet otherwise.
fn tab_button(ui: &mut egui::Ui, text: &str, on: bool) -> egui::Response {
    let g = ui.painter().layout_no_wrap(text.to_owned(), theme::semibold(13.5), theme::GIZ);
    let size = egui::vec2(g.size().x + 28.0, 36.0);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let fill = if on { theme::MOVEL } else if resp.hovered() { theme::ROW_HOVER } else { egui::Color32::TRANSPARENT };
        let fg = if on || resp.hovered() { theme::GIZ } else { theme::NEVOA };
        ui.painter().rect_filled(rect, theme::radius::BUTTON, fill);
        ui.painter().galley(rect.center() - g.size() / 2.0, g, fg);
    }
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, on, text));
    resp
}
