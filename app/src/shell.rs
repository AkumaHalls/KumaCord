//! The v1.0 main screen (docs/PLANO-v1.0.md, "Navegação como no Discord"): the sidebar on the
//! left, and the center showing what is open there (`layout.rs`): a text channel's chat (with the
//! member list on the right), a DM, or the call stage, alone or with a text channel docked beside
//! it. The center header carries the mode buttons. A child module of `app`.

use eframe::egui::{self, Align, Layout, Margin, RichText};
use vox_proto::ChannelKind;

use super::{App, Center, JOIN_UI_LIMIT_SECS};
use crate::backend::Cmd;
use crate::layout::{CenterMode, Open};
use crate::theme::{self, ph};
use crate::ui_kit::{self, IconState};

/// Height of the center header and of the dock's header.
pub const HEADER_H: f32 = 52.0;
/// Width of the member list.
const MEMBERS_W: f32 = 232.0;

/// The frame of a header bar: the night background, side padding.
fn header_frame() -> egui::Frame {
    egui::Frame::new().fill(theme::NOITE).inner_margin(Margin::symmetric(16, 0))
}

/// A header's title: an icon, then the name in the display font.
fn header_title(ui: &mut egui::Ui, icon: &str, name: &str) -> egui::Response {
    ui.label(RichText::new(icon).font(theme::body(19.0)).color(theme::NEVOA));
    ui.add(egui::Label::new(RichText::new(name).font(theme::display(17.0)).color(theme::GIZ)).truncate().selectable(false))
}

impl App {
    pub(super) fn main_screen(&mut self, ui: &mut egui::Ui) {
        if self.stage_fullscreen && self.call_channel().is_some() {
            // Nothing but the stage (phase 2b toggles the OS fullscreen and handles Esc).
            egui::CentralPanel::no_frame().show(ui, |ui| self.call_stage_ui(ui));
            return;
        }
        if self.call.fullscreen {
            self.fullscreen_view(ui);
            return;
        }
        // The stage needs a call: without one the center goes back to the chat.
        if self.center == Center::Call && self.call_channel().is_none() {
            self.center = Center::Chat;
        }
        self.voice_join_watchdog(ui.ctx());
        // What arrives in the chat on screen (the center's or the docked one) is read while the
        // window is in front (`layout::text_on_screen`; coming back to it clears it, `notify_tick`).
        if let Some(ch) = self.chat_on_screen().filter(|_| self.window_focused()) {
            self.channel_read(ch);
        }
        self.toast_bar(ui);
        self.banners(ui);
        egui::Panel::left("left")
            .default_size(248.0)
            .size_range(200.0..=380.0)
            .frame(egui::Frame::new().fill(theme::PAREDE))
            .show(ui, |ui| self.left_panel(ui));
        match self.effective_mode() {
            CenterMode::Chat => self.chat_center(ui),
            mode => self.stage_center(ui, mode),
        }
        let ctx = ui.ctx().clone();
        self.reaction_picker(&ctx);
        self.delete_message_window(&ctx);
    }

    /// "Conexão perdida" and the dismissable notice, across the top (the kit's notice colours and
    /// icons).
    fn banners(&mut self, ui: &mut egui::Ui) {
        let bar = |fill| egui::Frame::new().fill(fill).inner_margin(Margin::symmetric(16, 8));
        if !self.hub_up {
            egui::Panel::top("banner").resizable(false).show_separator_line(false).frame(bar(theme::MOVEL)).show(ui, |ui| {
                ui_kit::notice_text(ui, ui_kit::Notice::Warning, "Conexão com o servidor perdida. Tentando reconectar...");
            });
        }
        if let Some(msg) = self.notice.clone() {
            let mut dismiss = false;
            egui::Panel::top("notice").resizable(false).frame(bar(theme::PAREDE)).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        dismiss = ui_kit::button_sized(ui, ui_kit::Kind::Secondary, ui_kit::Size::Small, None, "OK").clicked();
                        ui.with_layout(Layout::left_to_right(Align::Center), |ui| ui_kit::notice_text(ui, ui_kit::Notice::Info, &msg));
                    });
                });
            });
            if dismiss {
                self.notice = None;
            }
        }
    }

    /// The last-resort limit of a voice join (the backend gives up on its own long before).
    fn voice_join_watchdog(&mut self, ctx: &egui::Context) {
        let Some(j) = self.voice.joining else { return };
        let secs = self.voice.joining_since.map(|t| t.elapsed().as_secs()).unwrap_or(0);
        if secs >= JOIN_UI_LIMIT_SECS {
            tracing::warn!(secs, "voice join: UI gave up waiting for the backend");
            self.backend.send(Cmd::LeaveVoice);
            self.voice.joining = None;
            self.voice.joining_since = None;
            self.voice.retry = Some(j);
            self.voice.error = Some("A conexão de voz demorou demais e foi cancelada. Verifique sua internet e tente de novo.".into());
        } else {
            // (The sidebar's counter.)
            ctx.request_repaint_after(std::time::Duration::from_secs(1));
        }
    }

    /// The mode buttons (right to left: the rightmost first): only chat, only cards, cards with
    /// the chat docked. The ones with cards need a call.
    fn mode_buttons(&mut self, ui: &mut egui::Ui) {
        let in_call = self.call_channel().is_some();
        let now = self.effective_mode();
        let buttons = [
            (CenterMode::CardsWithChat, ph::SQUARE_SPLIT_HORIZONTAL, "Cards com o chat ao lado"),
            (CenterMode::Cards, ph::SQUARES_FOUR, "Só os cards"),
            (CenterMode::Chat, ph::CHAT_CENTERED_TEXT, "Só o chat"),
        ];
        for (mode, icon, hover) in buttons {
            let enabled = in_call || mode == CenterMode::Chat;
            let state = if now == mode { IconState::On } else { IconState::Normal };
            let r = ui.add_enabled_ui(enabled, |ui| ui_kit::icon_button(ui, icon, hover, state)).inner;
            let r = r.on_disabled_hover_text("Entre em uma sala de voz para ver os cards");
            if r.clicked() && now != mode {
                self.set_center_mode(mode);
            }
        }
    }

    // ------------------------------------------------------------ chat in the center

    fn chat_center(&mut self, ui: &mut egui::Ui) {
        let open = self.open_in_center();
        match open {
            Open::Dm(dm) => self.dm_center(ui, dm),
            Open::Text(ch) => {
                let channel = self.channels.iter().find(|c| c.id == ch).cloned();
                egui::Panel::top("center_head").resizable(false).exact_size(HEADER_H).frame(header_frame()).show(ui, |ui| {
                    let (icon, name) = match &channel {
                        Some(c) if c.private.is_some() => (super::ICON_PRIVATE, c.name.as_str()),
                        Some(c) => (ph::HASH, c.name.as_str()),
                        None => (ph::HASH, ""),
                    };
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 2.0;
                        self.mode_buttons(ui);
                        ui.add_space(10.0);
                        let shown = self.settings.layout.show_members;
                        let hover = if shown { "Esconder a lista de membros" } else { "Mostrar a lista de membros" };
                        if ui_kit::icon_button(ui, ph::USERS, hover, if shown { IconState::On } else { IconState::Normal }).clicked() {
                            self.settings.layout.show_members = !shown;
                            self.save_settings();
                        }
                        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                            ui.spacing_mut().item_spacing.x = 10.0;
                            let t = header_title(ui, icon, name);
                            if channel.as_ref().is_some_and(|c| c.private.is_some()) {
                                t.on_hover_text(super::PRIVATE_HOVER);
                            }
                        });
                    });
                });
                if self.settings.layout.show_members {
                    egui::Panel::right("members").resizable(false)
                        .exact_size(MEMBERS_W)
                        .frame(egui::Frame::new().fill(theme::PAREDE).inner_margin(Margin { left: 8, right: 8, top: 12, bottom: 8 }))
                        .show(ui, |ui| self.online_panel(ui));
                }
                egui::CentralPanel::default().frame(egui::Frame::new().fill(theme::NOITE)).show(ui, |ui| self.channel_chat(ui, ch));
            }
            Open::Nothing | Open::Stage(_) => {
                egui::Panel::top("center_head").resizable(false).exact_size(HEADER_H).frame(header_frame()).show(ui, |ui| {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| self.mode_buttons(ui));
                });
                egui::CentralPanel::default().frame(egui::Frame::new().fill(theme::NOITE)).show(ui, |ui| {
                    ui.centered_and_justified(|ui| {
                        let text = if self.ready { "Escolha um canal de texto." } else { "Carregando..." };
                        ui.label(RichText::new(text).color(theme::NEVOA));
                    });
                });
            }
        }
    }

    /// A DM in the center: its header (the person, the call buttons, the options), the note about
    /// where the messages are kept, the lines and the input.
    fn dm_center(&mut self, ui: &mut egui::Ui, dm: vox_proto::DmId) {
        let Some(conv) = self.dms.conv(dm).cloned() else {
            egui::CentralPanel::default().frame(egui::Frame::new().fill(theme::NOITE)).show(ui, |ui| {
                ui.centered_and_justified(|ui| ui.label(RichText::new("Carregando...").color(theme::NEVOA)));
            });
            return;
        };
        egui::Panel::top("center_head").resizable(false).exact_size(HEADER_H).frame(header_frame()).show(ui, |ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                self.mode_buttons(ui);
                ui.add_space(10.0);
                self.dm_header_actions(ui, dm, &conv);
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    crate::avatars::avatar_widget(&mut self.avatars.store, ui, &conv.peer, 26.0, crate::avatars::Surface::Chat);
                    ui.add(egui::Label::new(RichText::new(conv.peer.shown_name()).font(theme::display(17.0)).color(theme::GIZ)).truncate().selectable(false));
                    if conv.peer.has_display_name() {
                        ui.label(RichText::new(format!("@{}", conv.peer.username)).color(theme::NEVOA));
                    }
                });
            });
        });
        let name = conv.peer.shown_name().to_string();
        egui::Panel::bottom(egui::Id::new("dm_input_panel")).resizable(false)
            .frame(egui::Frame::new().fill(theme::NOITE).inner_margin(Margin { left: 12, right: 12, top: 4, bottom: 12 }))
            .show_separator_line(false)
            .show(ui, |ui| self.dm_input_box(ui, dm, &name, conv.blocked));
        egui::CentralPanel::default().frame(egui::Frame::new().fill(theme::NOITE)).show(ui, |ui| {
            let ui = &mut ui.new_child(egui::UiBuilder::new().max_rect(ui.max_rect().shrink2(egui::vec2(12.0, 0.0))));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{}  {}", ph::INFO, super::dms::HOST_NOTE)).font(theme::body(12.0)).color(theme::NEVOA))
                    .on_hover_text("Só vocês dois veem esta conversa no AkumaLabs (nem admins nem o dono pelo app). Mas ela fica gravada no computador que hospeda o servidor, e quem hospeda poderia ler o arquivo.");
            });
            if conv.blocked {
                // The block state: a kit notice with the way out right in it.
                ui.add_space(6.0);
                let mut unblock = false;
                ui_kit::notice_frame(ui_kit::Notice::Warning).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            unblock = ui_kit::button_sized(ui, ui_kit::Kind::Secondary, ui_kit::Size::Small, Some(ph::CHECK), "Desbloquear").clicked();
                            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                                let text = format!("Você bloqueou {}. Desbloqueie para conversar e ligar de novo.", conv.peer.shown_name());
                                ui_kit::notice_text(ui, ui_kit::Notice::Warning, &text);
                            });
                        });
                    });
                });
                if unblock {
                    self.block_user(&conv.peer, false);
                }
            }
            self.dm_messages(ui, dm, &conv.peer, conv.blocked);
        });
    }

    // ------------------------------------------------------------ the stage

    fn stage_center(&mut self, ui: &mut egui::Ui, mode: CenterMode) {
        if mode == CenterMode::CardsWithChat {
            if let Some(ch) = self.docked_channel() {
                let width = self.settings.layout.dock_width.clamp(280.0, 640.0);
                let r = egui::Panel::right("dock")
                    .default_size(width)
                    .size_range(280.0..=640.0)
                    .resizable(true)
                    .frame(egui::Frame::new().fill(theme::NOITE))
                    .show(ui, |ui| self.dock(ui, ch));
                // Remember the width once the drag is over.
                let w = r.response.rect.width();
                if (w - self.settings.layout.dock_width).abs() > 1.0 && !ui.input(|i| i.pointer.any_down()) {
                    self.settings.layout.dock_width = w;
                    self.save_settings();
                }
            }
        }
        let title = match self.call_channel() {
            // (Without its phone icon: the header draws its own.)
            Some(super::dms::DM_CALL_CHANNEL) => self.dm_call_title().trim_start_matches(ph::PHONE).trim().to_string(),
            Some(ch) => self.channels.iter().find(|c| c.id == ch).map(|c| c.name.clone()).unwrap_or_default(),
            None => String::new(),
        };
        let meta = self.stage_header_meta();
        egui::Panel::top("center_head").resizable(false).exact_size(HEADER_H).frame(header_frame()).show(ui, |ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                self.mode_buttons(ui);
                ui.add_space(10.0);
                self.stage_header_tools(ui);
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 10.0;
                    let icon = if self.in_dm_call() { ph::PHONE } else { ph::SPEAKER_HIGH };
                    header_title(ui, icon, &title);
                    if !meta.is_empty() {
                        ui.label(RichText::new(meta).font(theme::body(13.0)).color(theme::NEVOA));
                    }
                });
            });
        });
        egui::CentralPanel::default().frame(egui::Frame::new().fill(theme::NOITE)).show(ui, |ui| self.call_stage_ui(ui));
    }

    /// The chat docked beside the cards: its header (the channel, a picker of the text channels,
    /// hide) and the chat itself. It never takes the sidebar's "open" highlight.
    fn dock(&mut self, ui: &mut egui::Ui, ch: vox_proto::ChannelId) {
        let channel = self.channels.iter().find(|c| c.id == ch).cloned();
        egui::Panel::top("dock_head").resizable(false).exact_size(HEADER_H).frame(egui::Frame::new().fill(theme::NOITE).inner_margin(Margin::symmetric(12, 0))).show(ui, |ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui_kit::icon_button(ui, ph::CARET_RIGHT, "Esconder o chat (só os cards)", IconState::Normal).clicked() {
                    self.set_center_mode(CenterMode::Cards);
                }
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    let (icon, name) = match &channel {
                        Some(c) if c.private.is_some() => (super::ICON_PRIVATE, c.name.as_str()),
                        Some(c) => (ph::HASH, c.name.as_str()),
                        None => (ph::HASH, ""),
                    };
                    let picker = self.dock_title_button(ui, icon, name);
                    egui::Popup::menu(&picker).frame(ui_kit::popup_frame(ui.style())).show(|ui| {
                        ui.set_min_width(220.0);
                        ui_kit::section(ui, "Chat ao lado dos cards");
                        ui.add_space(2.0);
                        let texts: Vec<(vox_proto::ChannelId, String, bool)> =
                            self.channels.iter().filter(|c| c.kind == ChannelKind::Text).map(|c| (c.id, c.name.clone(), c.private.is_some())).collect();
                        for (id, name, private) in texts {
                            let icon = if private { super::ICON_PRIVATE } else { ph::HASH };
                            if ui_kit::menu_row(ui, icon, &name, false).clicked() {
                                self.dock_channel(id);
                                ui.close();
                            }
                        }
                    });
                });
            });
        });
        self.channel_chat(ui, ch);
    }

    /// The dock's title: the channel and a caret (a click opens the channel picker).
    fn dock_title_button(&mut self, ui: &mut egui::Ui, icon: &str, name: &str) -> egui::Response {
        let p = ui.painter();
        let icon_g = p.layout_no_wrap(icon.to_owned(), theme::body(19.0), theme::NEVOA);
        let name_g = crate::app::notify::fit_name(p, name, theme::display(17.0), theme::GIZ, (ui.available_width() - 70.0).max(40.0));
        let caret = p.layout_no_wrap(ph::CARET_DOWN.to_owned(), theme::body(14.0), theme::NEVOA);
        let w = icon_g.size().x + 8.0 + name_g.size().x + 6.0 + caret.size().x + 12.0;
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, 34.0), egui::Sense::click());
        if ui.is_rect_visible(rect) {
            let p = ui.painter();
            if resp.hovered() {
                p.rect_filled(rect, theme::radius::ROW, theme::ROW_HOVER);
            }
            let cy = rect.center().y;
            let mut x = rect.left() + 6.0;
            let (iw, nw) = (icon_g.size().x, name_g.size().x);
            p.galley(egui::pos2(x, cy - icon_g.size().y / 2.0), icon_g, theme::NEVOA);
            x += iw + 8.0;
            p.galley(egui::pos2(x, cy - name_g.size().y / 2.0), name_g, theme::GIZ);
            x += nw + 6.0;
            p.galley(egui::pos2(x, cy - caret.size().y / 2.0), caret, theme::NEVOA);
        }
        resp.on_hover_text("Escolher o canal de texto deste chat")
    }
}
