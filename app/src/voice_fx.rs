//! "Modificador de voz": the settings section and the voice bar's quick menu.
//!
//! The audio side is `vox_client_core::audio::voicefx`. Both places here write
//! `settings.voice_fx` and apply it at once through the shared `NoiseControl`: the call and the
//! mic test follow it on their next 10 ms of audio (a short crossfade, no rejoin). Only the
//! microphone is changed: Go Live audio, the soundboard and the alert sounds never are.

use eframe::egui::{self, Align2, Pos2, RichText, Sense, Vec2};
use vox_client_core::{VoiceEffect, VoiceFxConfig};

use crate::app::{App, MicTestUi};
use crate::forms;
use crate::theme::{self, ph, radius, size, space};
use crate::ui_kit::{self, Kind, Size};

/// Height of an effect row in the popup.
const FX_ROW_H: f32 = 30.0;
/// Width of the voice changer popup's content.
const POPUP_W: f32 = 260.0;

impl App {
    /// Apply a new voice changer setting now; save it unless a slider is still being dragged.
    pub(crate) fn set_voice_fx(&mut self, cfg: VoiceFxConfig, save: bool) {
        let cfg = cfg.sanitized();
        self.settings.voice_fx = cfg;
        self.noise.voice_fx().set(cfg);
        if save {
            self.save_settings();
        }
    }

    /// The effect list and the intensity slider (shared by the settings and the voice dock's
    /// popup). `vertical`: the popup (one row per effect, the current one checked); otherwise
    /// the settings page (pill choices in the form's control column).
    pub(crate) fn voice_fx_controls(&mut self, ui: &mut egui::Ui, vertical: bool) {
        let before = self.settings.voice_fx;
        let mut cfg = before;
        let (mut dragging, mut released) = (false, false);
        if vertical {
            ui.spacing_mut().item_spacing.y = 1.0;
            for e in VoiceEffect::ALL {
                if effect_row(ui, e, cfg.effect == e).on_hover_text(e.hint()).clicked() {
                    cfg.effect = e;
                }
            }
            ui.add_space(space::M);
            ui.add_enabled_ui(cfg.effect.has_intensity(), |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Intensidade").color(theme::NEVOA).size(size::SMALL + 0.5));
                    ui.spacing_mut().slider_width = POPUP_W - 140.0;
                    let r = ui.add(egui::Slider::new(&mut cfg.intensity_pct, 0..=100).suffix("%"));
                    (dragging, released) = (r.dragged(), r.drag_stopped());
                });
            });
        } else {
            forms::row(ui, "Efeito", |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = Vec2::splat(space::S);
                    for e in VoiceEffect::ALL {
                        if forms::chip(ui, cfg.effect == e, e.label()).on_hover_text(e.hint()).clicked() {
                            cfg.effect = e;
                        }
                    }
                });
            });
            ui.add_enabled_ui(cfg.effect.has_intensity(), |ui| {
                forms::row(ui, "Intensidade", |ui| {
                    let r = ui.add(egui::Slider::new(&mut cfg.intensity_pct, 0..=100).suffix("%"));
                    (dragging, released) = (r.dragged(), r.drag_stopped());
                });
            });
        }
        if cfg != before {
            // Heard at once; saved when the slider is let go.
            self.set_voice_fx(cfg, !dragging);
        }
        if released {
            self.save_settings();
        }
    }

    /// Configurações → Voz → "Modificador de voz". `toggle_test` starts or stops the mic test
    /// (the same one as "Testar microfone": you hear yourself with the effect, nothing is sent).
    pub(crate) fn voice_fx_section(&mut self, ui: &mut egui::Ui, toggle_test: &mut bool) {
        forms::group(ui, "Modificador de voz");
        self.voice_fx_controls(ui, false);
        let e = self.settings.voice_fx.effect;
        forms::note(ui, e.hint(), true);
        let testing = self.settings_ui.mic_test != MicTestUi::Off;
        forms::row(ui, "Testar", |ui| {
            let (icon, label) = if testing { (ph::STOP, "Parar teste") } else { (ph::MASK_HAPPY, "Ouvir minha voz") };
            if ui_kit::button_sized(ui, Kind::Secondary, Size::Small, Some(icon), label).on_hover_text("Ouça sua voz com o efeito (use fones para não dar eco). Nada é enviado.").clicked() {
                *toggle_test = true;
            }
            if testing {
                ui.label(RichText::new("Fale: você se ouve com o efeito.").color(theme::NEVOA));
            }
        });
        forms::note(ui, "Muda só o microfone: transmissões (Go Live), soundboard e alertas continuam como são.", true);
    }

    /// The content of the voice changer popup (the voice dock's mask button).
    pub(crate) fn voice_fx_popup(&mut self, ui: &mut egui::Ui) {
        ui.set_width(POPUP_W);
        ui.horizontal(|ui| {
            ui.add_space(space::S);
            ui_kit::title(ui, "Modificador de voz");
        });
        ui.add_space(space::S);
        self.voice_fx_controls(ui, true);
        ui.add_space(space::XS);
        let text = "Só o microfone muda. Para se ouvir com o efeito: Configurações, Voz.";
        ui.horizontal(|ui| {
            ui.add_space(space::S);
            ui.add(egui::Label::new(RichText::new(text).size(size::SMALL).color(theme::NEVOA)).wrap());
        });
    }
}

/// One effect of the popup: its name, and a check when it is the one on. (No emoji: the UI font
/// lacks several of the effects' ones.)
fn effect_row(ui: &mut egui::Ui, e: VoiceEffect, current: bool) -> egui::Response {
    let w = ui.available_width().max(200.0);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, FX_ROW_H), Sense::click());
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        if resp.hovered() {
            p.rect_filled(rect, radius::ROW, theme::MOVEL_HOVER);
        }
        let fg = if current || resp.hovered() { theme::GIZ } else { theme::TEXTO };
        let font = if current { theme::semibold(13.5) } else { theme::body(13.5) };
        p.text(Pos2::new(rect.left() + 10.0, rect.center().y), Align2::LEFT_CENTER, e.label(), font, fg);
        if current {
            p.text(Pos2::new(rect.right() - 10.0, rect.center().y), Align2::RIGHT_CENTER, ph::CHECK, theme::body(15.0), theme::GIZ);
        }
    }
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, current, e.label()));
    resp
}
