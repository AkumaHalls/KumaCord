//! The trim editor inside the "Adicionar som" dialog: a long recording (up to 10 minutes) is
//! shown as a waveform and the person marks the part, at most 5 s, that becomes the sound.
//!
//! The recording is read once on a background thread into a small overview (see
//! `vox_client_core::soundboard::trim`: no decoded audio is kept, so there is nothing to free but
//! the overview, which goes with the dialog). Everything on screen is drawn from that overview;
//! the selection's audio is decoded again when it is previewed ("Ouvir trecho", through the
//! backend, which plays the very clip that would be uploaded) and when it is uploaded.
//!
//! The pure parts (what a drag does to the selection, how the view zooms) are functions with unit
//! tests; the widget only maps the pointer to seconds.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use eframe::egui::{self, Align2, Color32, CursorIcon, FontId, Pos2, Rect, RichText, Sense, Stroke, Vec2};
use vox_client_core::soundboard::{analyze, format_time, Overview, Selection, Trim, DEFAULT_FADE_MS, MAX_EDIT_SECS, MAX_SELECTION};

use crate::app::App;
use crate::backend::{Cmd, Repaint};
use crate::soundboard::SoundCmd;
use crate::theme::{self, ph, radius, size, space};
use crate::ui_kit::{self, IconState, Kind, Size};

/// How close (px) the pointer must be to a handle to take it.
const GRAB_PX: f32 = 8.0;
/// The view never zooms in past this many seconds.
const MIN_VIEW: f64 = 0.25;
const WAVE_HEIGHT: f32 = 92.0;

/// What a drag on the waveform is doing to the selection.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Drag {
    Start,
    End,
    /// Moving the whole selection; the number is where inside it (seconds from its start) it was
    /// grabbed.
    Move(f64),
    /// A new selection started at this time.
    New(f64),
}

pub struct TrimEditor {
    pub path: PathBuf,
    cancel: Arc<AtomicBool>,
    progress: Arc<AtomicU32>,
    rx: Option<mpsc::Receiver<Result<Overview, String>>>,
    pub overview: Option<Overview>,
    /// Why the file cannot be used (Portuguese, ready to show).
    pub error: Option<String>,
    pub sel: Selection,
    pub fade_in: bool,
    pub fade_out: bool,
    pub trim_silence: bool,
    /// The seconds on screen.
    view: (f64, f64),
    drag: Option<Drag>,
    /// "Ouvir trecho" is waiting for the clip or playing until then.
    preview_until: Option<Instant>,
    pub preview_error: Option<String>,
    /// How many previews the backend confirmed (the autotest's probe) and the last one's length.
    pub previews: u32,
    pub last_preview_ms: u32,
}

impl Drop for TrimEditor {
    /// Closing the dialog (or choosing another file) stops a read still in progress.
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl TrimEditor {
    /// Start reading `path` on its own thread.
    pub fn start(path: PathBuf, repaint: &Repaint) -> Self {
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(AtomicU32::new(0f32.to_bits()));
        let (tx, rx) = mpsc::channel();
        {
            let (path, cancel, progress, repaint) = (path.clone(), cancel.clone(), progress.clone(), repaint.clone());
            std::thread::Builder::new()
                .name("vox-sound-trim".into())
                .spawn(move || {
                    let last_pct = std::cell::Cell::new(u32::MAX);
                    let report = |p: f32| {
                        progress.store(p.to_bits(), Ordering::Relaxed);
                        let pct = (p * 100.0) as u32;
                        if last_pct.replace(pct) != pct {
                            repaint.wake();
                        }
                    };
                    let result = analyze(&path, &report, &cancel).map_err(|e| e.to_string());
                    if !cancel.load(Ordering::Relaxed) {
                        let _ = tx.send(result);
                        repaint.wake();
                    }
                })
                .ok();
        }
        Self {
            path,
            cancel,
            progress,
            rx: Some(rx),
            overview: None,
            error: None,
            sel: Selection { start: 0.0, end: 0.0 },
            fade_in: true,
            fade_out: true,
            trim_silence: true,
            view: (0.0, 1.0),
            drag: None,
            preview_until: None,
            preview_error: None,
            previews: 0,
            last_preview_ms: 0,
        }
    }

    /// Pick up the finished read, if it is finished.
    pub fn poll(&mut self) {
        let Some(rx) = &self.rx else { return };
        let result = match rx.try_recv() {
            Ok(r) => r,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => Err("Não consegui ler esse arquivo.".to_string()),
        };
        self.rx = None;
        match result {
            Ok(ov) => {
                let dur = ov.duration();
                // The loudest 5 s (or the whole recording when it is shorter).
                let at = ov.loudest_window(MAX_SELECTION);
                self.sel = Selection::new(at, at + MAX_SELECTION, dur);
                self.view = (0.0, dur.max(MIN_VIEW));
                self.overview = Some(ov);
            }
            Err(e) => self.error = Some(e),
        }
    }

    pub fn loading(&self) -> bool {
        self.rx.is_some()
    }

    pub fn ready(&self) -> bool {
        self.overview.is_some()
    }

    pub fn duration(&self) -> f64 {
        self.overview.as_ref().map_or(0.0, Overview::duration)
    }

    fn progress(&self) -> f32 {
        f32::from_bits(self.progress.load(Ordering::Relaxed))
    }

    /// What to cut, with the edge options as they stand.
    pub fn trim(&self) -> Trim {
        self.sel.to_trim(self.fade_in, self.fade_out, self.trim_silence)
    }

    pub fn set_selection(&mut self, start: f64, end: f64) {
        self.sel = Selection::new(start, end, self.duration());
    }

    /// Back to the loudest 5 s.
    pub fn best_part(&mut self) {
        if let Some(ov) = &self.overview {
            let at = ov.loudest_window(MAX_SELECTION);
            self.sel = Selection::new(at, at + MAX_SELECTION, ov.duration());
            self.show_selection();
        }
    }

    /// Zoom to the selection with some room around it.
    fn show_selection(&mut self) {
        let dur = self.duration();
        let room = (MAX_SELECTION - self.sel.len()).max(0.0) / 2.0 + 1.0;
        self.view = fit_view(self.sel.start - room, self.sel.end + room, dur);
    }

    pub fn previewing(&self) -> bool {
        self.preview_until.is_some_and(|t| Instant::now() < t)
    }

    /// The clip is being made; the backend will say how long it plays.
    fn preview_requested(&mut self) {
        self.preview_until = Some(Instant::now() + Duration::from_secs(15));
        self.preview_error = None;
    }

    /// The backend started (or refused) the preview.
    pub fn preview_result(&mut self, r: Result<u32, String>) {
        match r {
            Ok(ms) => {
                self.previews += 1;
                self.last_preview_ms = ms;
                self.preview_until = Some(Instant::now() + Duration::from_millis(ms as u64));
            }
            Err(e) => {
                self.preview_until = None;
                self.preview_error = Some(e);
            }
        }
    }
}

// ---------------------------------------------------------------- pure helpers

/// A view of `a..b` seconds made to fit a recording of `dur`: at least [`MIN_VIEW`] (or all of a
/// shorter recording) and inside it.
fn fit_view(a: f64, b: f64, dur: f64) -> (f64, f64) {
    let min = MIN_VIEW.min(dur);
    let span = (b - a).clamp(min, dur.max(min));
    let start = a.clamp(0.0, (dur - span).max(0.0));
    (start, start + span)
}

/// Zoom `view` by `factor` (>1 zooms in) keeping the time under `pivot` where it is on screen.
fn zoom_view(view: (f64, f64), factor: f64, pivot: f64, dur: f64) -> (f64, f64) {
    let span = view.1 - view.0;
    let new_span = (span / factor).clamp(MIN_VIEW.min(dur), dur.max(MIN_VIEW.min(dur)));
    let ratio = if span > 0.0 { ((pivot - view.0) / span).clamp(0.0, 1.0) } else { 0.5 };
    fit_view(pivot - ratio * new_span, pivot - ratio * new_span + new_span, dur)
}

/// What the pointer at time `t` does to `sel` while `drag` is on.
fn apply_drag(drag: Drag, sel: Selection, t: f64, dur: f64) -> Selection {
    match drag {
        Drag::Start => sel.with_start(t, dur),
        Drag::End => sel.with_end(t, dur),
        Drag::Move(grab) => sel.shifted(t - grab - sel.start, dur),
        Drag::New(anchor) => {
            if t >= anchor {
                Selection::new(anchor, t.min(anchor + MAX_SELECTION), dur)
            } else {
                Selection::new(t.max(anchor - MAX_SELECTION), anchor, dur)
            }
        }
    }
}

/// What a press at x on the waveform grabs: a handle, the inside of the selection, or nothing
/// (then it starts a new selection).
fn grab_at(x: f32, x_start: f32, x_end: f32, t: f64, sel: Selection) -> Drag {
    let (ds, de) = ((x - x_start).abs(), (x - x_end).abs());
    if ds <= GRAB_PX && ds <= de {
        Drag::Start
    } else if de <= GRAB_PX {
        Drag::End
    } else if x > x_start && x < x_end {
        Drag::Move(t - sel.start)
    } else {
        Drag::New(t)
    }
}

// ---------------------------------------------------------------- widget

/// The waveform outside the selection (quiet), inside it (chalk), the selection's band and its
/// handles. The lamp stays out of it: it is kept for who speaks and the main action.
const WAVE_OUT: Color32 = theme::DESLIGADO;
const WAVE_IN: Color32 = theme::GIZ;
const BAND: Color32 = Color32::from_rgba_premultiplied(23, 24, 24, 24);
const HANDLE: Color32 = theme::GIZ;

impl TrimEditor {
    /// The editor's widgets. Returns true when "Ouvir trecho" was pressed.
    pub fn ui(&mut self, ui: &mut egui::Ui, enabled: bool) -> bool {
        let dur = self.duration();
        let (rate, channels) = self.overview.as_ref().map_or((0, 0), |o| (o.rate, o.channels));
        ui.horizontal(|ui| {
            ui.label(RichText::new(ph::WAVEFORM).font(theme::body(15.0)).color(theme::NEVOA));
            ui.add(
                egui::Label::new(
                    RichText::new(format!(
                        "{} de áudio · {} · {}. Arraste as alças (ou a faixa) para escolher o trecho; a roda do mouse dá zoom.",
                        format_time(dur),
                        if rate % 1000 == 0 { format!("{} kHz", rate / 1000) } else { format!("{:.1} kHz", rate as f32 / 1000.0) },
                        if channels >= 2 { "estéreo" } else { "mono" }
                    ))
                    .size(size::SMALL + 0.5)
                    .color(theme::NEVOA),
                )
                .wrap(),
            );
        });
        self.waveform(ui, enabled);
        self.view_controls(ui, dur);
        let mut preview = false;
        ui.add_enabled_ui(enabled, |ui| {
            ui.add_space(space::S);
            ui.horizontal(|ui| {
                ui.spacing_mut().interact_size.y = 28.0;
                ui.label(RichText::new("Início").color(theme::NEVOA));
                let mut start = self.sel.start;
                if ui.add(egui::DragValue::new(&mut start).speed(0.01).range(0.0..=dur).fixed_decimals(2).suffix(" s")).changed() {
                    self.sel = self.sel.with_start(start, dur);
                }
                ui.add_space(space::S);
                ui.label(RichText::new("Fim").color(theme::NEVOA));
                let mut end = self.sel.end;
                if ui.add(egui::DragValue::new(&mut end).speed(0.01).range(0.0..=dur).fixed_decimals(2).suffix(" s")).changed() {
                    self.sel = self.sel.with_end(end, dur);
                }
                ui.add_space(space::S);
                let len = self.sel.len();
                let text = format!("Trecho de {len:.2} s (máx. {} s)", MAX_SELECTION as u32);
                let color = if len >= MAX_SELECTION - 0.005 { theme::GIZ } else { theme::NEVOA };
                ui.label(RichText::new(text).color(color));
            });
            ui.horizontal(|ui| {
                let busy = self.previewing();
                let r = ui.add_enabled_ui(!busy, |ui| ui_kit::button_sized(ui, Kind::Secondary, Size::Small, Some(ph::PLAY), "Ouvir trecho")).inner;
                if r.on_hover_text("Toca só para você, do jeito que vai ser enviado").clicked() {
                    preview = true;
                }
                if busy {
                    ui.spinner();
                }
                if ui_kit::button_sized(ui, Kind::Ghost, Size::Small, Some(ph::SCISSORS), "Trecho mais alto").on_hover_text("Escolhe os 5 s mais fortes do áudio").clicked() {
                    self.best_part();
                }
            });
            ui.horizontal_wrapped(|ui| {
                ui.checkbox(&mut self.fade_in, format!("Suavizar o começo ({DEFAULT_FADE_MS} ms)")).on_hover_text("Evita um estalo no início do corte");
                ui.checkbox(&mut self.fade_out, format!("Suavizar o fim ({DEFAULT_FADE_MS} ms)")).on_hover_text("Evita um estalo no fim do corte");
                ui.checkbox(&mut self.trim_silence, "Cortar silêncio das pontas");
            });
        });
        if let Some(e) = &self.preview_error {
            ui.label(RichText::new(format!("{} {e}", ph::WARNING)).color(theme::AO_VIVO));
        }
        if let Some(t) = self.preview_until {
            if Instant::now() < t {
                ui.ctx().request_repaint_after(t - Instant::now());
            }
        }
        if preview {
            self.preview_requested();
        }
        preview
    }

    fn view_controls(&mut self, ui: &mut egui::Ui, dur: f64) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = space::XS;
            let mid = (self.view.0 + self.view.1) / 2.0;
            if ui_kit::icon_button(ui, ph::MINUS, "Afastar", IconState::Normal).clicked() {
                self.view = zoom_view(self.view, 1.0 / 1.6, mid, dur);
            }
            if ui_kit::icon_button(ui, ph::PLUS, "Aproximar", IconState::Normal).clicked() {
                self.view = zoom_view(self.view, 1.6, mid, dur);
            }
            ui.add_space(space::S);
            if ui_kit::button_sized(ui, Kind::Ghost, Size::Small, None, "Tudo").on_hover_text("Ver o áudio inteiro").clicked() {
                self.view = (0.0, dur.max(MIN_VIEW));
            }
            if ui_kit::button_sized(ui, Kind::Ghost, Size::Small, None, "Ver o trecho").on_hover_text("Aproximar no trecho escolhido").clicked() {
                self.show_selection();
            }
            ui.add_space(space::S);
            let span = self.view.1 - self.view.0;
            if span < dur - 1e-6 {
                let mut start = self.view.0;
                ui.spacing_mut().slider_width = ui.available_width().max(60.0) - 8.0;
                if ui.add(egui::Slider::new(&mut start, 0.0..=(dur - span)).show_value(false)).changed() {
                    self.view = (start, start + span);
                }
            }
        });
    }

    fn waveform(&mut self, ui: &mut egui::Ui, enabled: bool) {
        let dur = self.duration();
        let Some(ov) = self.overview.as_ref() else { return };
        let width = ui.available_width().max(240.0);
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, WAVE_HEIGHT), Sense::click_and_drag());
        let (v0, v1) = self.view;
        let span = (v1 - v0).max(1e-6);
        let x_of = |t: f64| rect.left() + ((t - v0) / span) as f32 * rect.width();
        let t_of = |x: f32| v0 + ((x - rect.left()) / rect.width()) as f64 * span;

        if enabled {
            // Wheel / pinch: zoom around the pointer.
            if resp.hovered() {
                let (scroll, pinch) = ui.input(|i| (i.smooth_scroll_delta().y, i.zoom_delta()));
                let factor = pinch as f64 * (1.0 + scroll as f64 * 0.004).clamp(0.5, 2.0);
                if (factor - 1.0).abs() > 1e-4 {
                    if let Some(p) = resp.hover_pos() {
                        self.view = zoom_view(self.view, factor, t_of(p.x), dur);
                    }
                }
            }
            let (xs, xe) = (x_of(self.sel.start), x_of(self.sel.end));
            if resp.drag_started() {
                if let Some(p) = resp.interact_pointer_pos() {
                    self.drag = Some(grab_at(p.x, xs, xe, t_of(p.x), self.sel));
                }
            }
            if let (Some(d), true, Some(p)) = (self.drag, resp.dragged(), resp.interact_pointer_pos()) {
                self.sel = apply_drag(d, self.sel, t_of(p.x), dur);
            }
            if resp.drag_stopped() {
                self.drag = None;
            }
            if resp.clicked() {
                // A click outside the selection brings the selection there; on it, nothing.
                if let Some(p) = resp.interact_pointer_pos().filter(|p| matches!(grab_at(p.x, xs, xe, t_of(p.x), self.sel), Drag::New(_))) {
                    self.sel = self.sel.centered_on(t_of(p.x), dur);
                }
            }
            if let Some(p) = resp.hover_pos().filter(|_| self.drag.is_none()) {
                let icon = match grab_at(p.x, xs, xe, t_of(p.x), self.sel) {
                    Drag::Start | Drag::End => CursorIcon::ResizeHorizontal,
                    Drag::Move(_) => CursorIcon::Grab,
                    Drag::New(_) => CursorIcon::Crosshair,
                };
                ui.ctx().set_cursor_icon(icon);
            } else if self.drag.is_some() {
                ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
            }
        }

        // Paint (after the interaction, so the handles show where they are now).
        let (v0, v1) = self.view;
        let span = (v1 - v0).max(1e-6);
        let x_of = |t: f64| rect.left() + ((t - v0) / span) as f32 * rect.width();
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, radius::BUTTON, theme::FUNDO);
        let (xs, xe) = (x_of(self.sel.start), x_of(self.sel.end));
        let band = Rect::from_min_max(Pos2::new(xs.max(rect.left()), rect.top()), Pos2::new(xe.min(rect.right()), rect.bottom()));
        if band.width() > 0.0 {
            painter.rect_filled(band, 0.0, BAND);
        }
        let n = rect.width().floor().max(1.0) as usize;
        let cols = ov.columns(v0, v1, n);
        let scale = ov.peak().max(0.05);
        let (mid, half) = (rect.center().y, rect.height() / 2.0 - 5.0);
        let cw = rect.width() / n as f32;
        painter.line_segment([Pos2::new(rect.left(), mid), Pos2::new(rect.right(), mid)], Stroke::new(1.0, theme::RISCO));
        for (i, &(lo, hi)) in cols.iter().enumerate() {
            let x = rect.left() + (i as f32 + 0.5) * cw;
            let (top, bottom) = (mid - hi / scale * half, mid - lo / scale * half);
            let bottom = bottom.max(top + 1.0);
            let color = if x >= xs && x <= xe { WAVE_IN } else { WAVE_OUT };
            painter.line_segment([Pos2::new(x, top), Pos2::new(x, bottom)], Stroke::new(cw.max(1.0), color));
        }
        for (x, label_right) in [(xs, false), (xe, true)] {
            if x >= rect.left() - 1.0 && x <= rect.right() + 1.0 {
                let x = x.clamp(rect.left() + 1.0, rect.right() - 1.0);
                painter.line_segment([Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())], Stroke::new(2.0, HANDLE));
                let grip = Rect::from_center_size(Pos2::new(x, rect.top() + 9.0), Vec2::new(8.0, 16.0));
                painter.rect_filled(grip, 2.0, HANDLE);
                let _ = label_right;
            }
        }
        let small = FontId::proportional(10.5);
        painter.text(rect.left_bottom() + Vec2::new(6.0, -3.0), Align2::LEFT_BOTTOM, format_time(v0), small.clone(), theme::NEVOA);
        painter.text(rect.right_bottom() + Vec2::new(-6.0, -3.0), Align2::RIGHT_BOTTOM, format_time(v1), small, theme::NEVOA);
    }
}

// ---------------------------------------------------------------- App glue

impl App {
    /// A file was chosen (picker, drop, typed path, or the autotest): read it in the background.
    pub fn trim_choose(&mut self, path: PathBuf) {
        if self.sb.add.is_none() {
            self.open_add_sound();
        }
        let repaint = self.repaint.clone();
        if let Some(form) = self.sb.add.as_mut() {
            crate::soundboard::set_path(form, path.clone());
            form.analyzed = form.path.clone();
            form.want_analyze = false;
            form.trim = Some(TrimEditor::start(path, &repaint));
        }
    }

    /// The editor of the open dialog.
    pub fn trim_editor(&mut self) -> Option<&mut TrimEditor> {
        self.sb.add.as_mut().and_then(|f| f.trim.as_mut())
    }

    /// "Ouvir trecho": the backend cuts the clip as it would be uploaded and plays it here only.
    pub fn trim_preview(&mut self) {
        let device = self.settings.output_device.clone();
        let Some(ed) = self.trim_editor() else { return };
        let cmd = SoundCmd::PreviewClip { path: ed.path.clone(), trim: ed.trim(), output_device: device };
        self.backend.send(Cmd::Sound(cmd));
    }

    /// The dialog's "Adicionar" for a file with the editor: upload the selection.
    pub fn trim_submit(&mut self, name: &str, emoji: &str) -> Result<(), String> {
        let Some(ed) = self.trim_editor() else { return Err("Escolha um arquivo primeiro.".into()) };
        if !ed.ready() {
            return Err("Espere o app terminar de ler o áudio.".into());
        }
        let (path, trim) = (ed.path.clone(), ed.trim());
        self.add_sound_trimmed(path, name, emoji, Some(trim))
    }
}

/// How the dialog shows a read in progress.
pub fn loading_ui(ui: &mut egui::Ui, ed: &TrimEditor) {
    ui.horizontal(|ui| {
        ui.spinner();
        ui.label(RichText::new(format!("Lendo o áudio... {}%", (ed.progress() * 100.0) as u32)).color(theme::TEXTO));
    });
    // A thin chalk bar on the night rail (like the level meter).
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width().min(420.0), 6.0), Sense::hover());
    ui.painter().rect_filled(rect, 3.0, theme::FUNDO);
    let done = Rect::from_min_size(rect.min, Vec2::new(rect.width() * ed.progress().clamp(0.0, 1.0), rect.height()));
    if done.width() >= 1.0 {
        ui.painter().rect_filled(done, 3.0, theme::GIZ);
    }
    ui.label(RichText::new(format!("Aceita até {} minutos de áudio.", MAX_EDIT_SECS / 60)).size(size::SMALL + 0.5).color(theme::NEVOA));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dragging_a_handle_keeps_the_other_end_and_the_five_second_limit() {
        let dur = 60.0;
        let sel = Selection::new(20.0, 23.0, dur);
        let s = apply_drag(Drag::Start, sel, 10.0, dur);
        assert_eq!((s.start, s.end), (18.0, 23.0));
        let s = apply_drag(Drag::End, sel, 40.0, dur);
        assert_eq!((s.start, s.end), (20.0, 25.0));
        let s = apply_drag(Drag::End, sel, -4.0, dur);
        assert!((s.len() - vox_client_core::soundboard::MIN_SELECTION).abs() < 1e-9, "never inverted: {s:?}");
    }

    #[test]
    fn dragging_the_band_moves_it_by_where_it_was_grabbed() {
        let dur = 60.0;
        let sel = Selection::new(20.0, 23.0, dur);
        let s = apply_drag(Drag::Move(1.0), sel, 31.0, dur); // grabbed 1 s in, pointer now at 31 s
        assert_eq!((s.start, s.end), (30.0, 33.0));
        let s = apply_drag(Drag::Move(1.0), sel, 200.0, dur);
        assert_eq!((s.len(), s.end), (3.0, dur), "stops at the end");
        let s = apply_drag(Drag::Move(1.0), sel, -50.0, dur);
        assert_eq!(s.start, 0.0);
    }

    #[test]
    fn dragging_out_a_new_selection_works_both_ways_and_stops_at_five_seconds() {
        let dur = 60.0;
        let sel = Selection::new(1.0, 2.0, dur);
        let s = apply_drag(Drag::New(30.0), sel, 32.5, dur);
        assert_eq!((s.start, s.end), (30.0, 32.5));
        let s = apply_drag(Drag::New(30.0), sel, 20.0, dur);
        assert_eq!((s.start, s.end), (25.0, 30.0), "to the left, the dragged side stops 5 s away");
        let s = apply_drag(Drag::New(30.0), sel, 29.9, dur);
        assert!(s.len() >= 0.05 - 1e-9 && s.end <= 30.0 + 1e-9);
    }

    #[test]
    fn a_press_takes_the_nearest_handle_then_the_band_then_starts_a_new_selection() {
        let sel = Selection::new(10.0, 12.0, 60.0);
        // 10 px per second, the view starting at 0: handles at x = 100 and 120.
        let at = |x: f32| grab_at(x, 100.0, 120.0, x as f64 / 10.0, sel);
        assert_eq!(at(98.0), Drag::Start);
        assert_eq!(at(123.0), Drag::End);
        assert_eq!(at(110.0), Drag::Move(1.0));
        assert_eq!(at(60.0), Drag::New(6.0));
        // A selection thinner than the grab zones: the nearer handle wins.
        assert_eq!(grab_at(101.0, 100.0, 104.0, 10.1, sel), Drag::Start);
        assert_eq!(grab_at(103.0, 100.0, 104.0, 10.3, sel), Drag::End);
    }

    #[test]
    fn zooming_keeps_the_pivot_in_place_and_stays_inside_the_recording() {
        let dur = 600.0;
        let v = zoom_view((0.0, 600.0), 10.0, 300.0, dur);
        assert!((v.1 - v.0 - 60.0).abs() < 1e-9);
        assert!(((300.0 - v.0) / (v.1 - v.0) - 0.5).abs() < 1e-9, "the pivot stays at the same spot: {v:?}");
        // Zoom further around a point at 25 % of the view.
        let w = zoom_view(v, 2.0, v.0 + 15.0, dur);
        assert!(((v.0 + 15.0 - w.0) / (w.1 - w.0) - 0.25).abs() < 1e-9, "{w:?}");
        // Never out of bounds, never narrower than the minimum, never wider than the recording.
        let tiny = zoom_view(v, 1e9, 10.0, dur);
        assert!((tiny.1 - tiny.0 - MIN_VIEW).abs() < 1e-9);
        let wide = zoom_view(v, 1e-9, 599.0, dur);
        assert_eq!(wide, (0.0, 600.0));
        let edge = zoom_view((590.0, 600.0), 2.0, 600.0, dur);
        assert!(edge.1 <= dur && edge.0 >= 0.0, "{edge:?}");
        // A recording shorter than the minimum view shows all of itself.
        assert_eq!(fit_view(-1.0, 5.0, 0.1), (0.0, 0.1));
        assert_eq!(fit_view(10.0, 11.0, 600.0), (10.0, 11.0));
        assert_eq!(fit_view(599.5, 700.0, 600.0).1, 600.0);
    }
}
