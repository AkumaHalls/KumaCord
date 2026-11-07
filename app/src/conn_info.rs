use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, CornerRadius, Margin, Vec2};

use crate::app::App;
use crate::brand;
use crate::theme::{self, ph};
use crate::ui_kit;

const PING_TIMEOUT: Duration = Duration::from_millis(2500);
const PING_EVERY: Duration = Duration::from_secs(3);
const PING_GRAPH: Duration = Duration::from_millis(900);
const GRAPH_BINS: usize = 40;
const DOCK_H: f32 = 64.0;
const BAR_SPEED: f32 = 3.2;

const PING_GOOD: Color32 = Color32::from_rgb(0x23, 0xA5, 0x59);
const PING_FAIR: Color32 = Color32::from_rgb(0xF0, 0xB2, 0x32);
const PING_BAD: Color32 = Color32::from_rgb(0xED, 0x42, 0x45);

pub fn ping_colour(ms: Option<u32>) -> Color32 {
    match ms {
        None => theme::NEVOA,
        Some(m) if m < 80 => PING_GOOD,
        Some(m) if m < 200 => PING_FAIR,
        _ => PING_BAD,
    }
}

fn targets(ms: Option<u32>) -> [f32; 4] {
    match ms {
        None => [0.25; 4],
        Some(m) if m < 80 => [1.0; 4],
        Some(m) if m < 200 => [0.72, 0.72, 0.72, 0.34],
        Some(m) if m < 400 => [0.62, 0.62, 0.30, 0.30],
        _ => [0.50, 0.26, 0.26, 0.26],
    }
}

fn measure_rtt(host: &str, port: u16) -> Option<u32> {
    use std::net::ToSocketAddrs;
    let addrs = match (host, port).to_socket_addrs() {
        Ok(a) => a.collect::<Vec<_>>(),
        Err(_) => return None,
    };
    let start = Instant::now();
    for addr in addrs {
        if let Ok(s) = std::net::TcpStream::connect_timeout(&addr, PING_TIMEOUT) {
            drop(s);
            return Some(start.elapsed().as_millis() as u32);
        }
    }
    None
}

fn endpoint() -> String {
    let url = brand::HOME_URL;
    let hostport = url
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .split('/')
        .next()
        .unwrap_or("");
    hostport.to_string()
}

pub fn region_code(ep: &str) -> String {
    let host = ep.split(':').next().unwrap_or(ep);
    let mut h: u32 = 2166136261;
    for b in host.bytes() {
        h = (h ^ b as u32).wrapping_mul(16_777_619);
    }
    format!("fcc-gru-{}", &format!("{h:08x}")[..6])
}

fn row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = theme::space::S;
        ui.label(egui::RichText::new(label).color(theme::NEVOA));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(egui::RichText::new(value).color(theme::GIZ));
        });
    });
}

fn stat_line(ui: &mut egui::Ui, label: &str, value: &str, colour: Color32) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = theme::space::S;
        ui.label(egui::RichText::new(label).color(theme::NEVOA));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(egui::RichText::new(value).color(colour).strong());
        });
    });
}

impl App {
    pub fn conn_toggle(&mut self) {
        self.voice.conn_open = true;
        self.voice.conn_ping_at = None;
        self.conn_measure_start();
    }

    fn conn_measure_start(&mut self) {
        if self.voice.conn_measuring {
            return;
        }
        self.voice.conn_measuring = true;
        let ep = endpoint();
        let (host, port) = match ep.split_once(':') {
            Some((h, p)) => (h.to_string(), p.parse().unwrap_or(80)),
            None => (ep, 80),
        };
        let (tx, rx) = std::sync::mpsc::channel::<Option<u32>>();
        self.voice.conn_rx = Some(rx);
        std::thread::spawn(move || {
            let _ = tx.send(measure_rtt(&host, port));
        });
    }

    pub fn conn_tick(&mut self, ctx: &egui::Context) {
        if let Some(rx) = self.voice.conn_rx.take() {
            match rx.try_recv() {
                Ok(ms) => {
                    self.voice.conn_ping_ms = ms;
                    self.voice.conn_ping_at = Some(Instant::now());
                    self.voice.conn_measuring = false;
                    if let Some(m) = ms {
                        let h = &mut self.voice.conn_ping_hist;
                        h.push(m);
                        if h.len() > 20 {
                            h.remove(0);
                        }
                        let g = &mut self.voice.conn_graph;
                        g.push(m);
                        if g.len() > 40 {
                            g.remove(0);
                        }
                    }
                }
                Err(_) => self.voice.conn_rx = Some(rx),
            }
        }
        if self.voice.conn_open {
            let due = self.voice.conn_ping_at.is_none_or(|t| t.elapsed() >= PING_GRAPH);
            if due && !self.voice.conn_measuring {
                self.conn_measure_start();
            }
            ctx.request_repaint_after(PING_GRAPH);
        } else {
            let due = self.voice.conn_ping_at.is_none_or(|t| t.elapsed() >= PING_EVERY);
            if due && !self.voice.conn_measuring {
                self.conn_measure_start();
            }
            ctx.request_repaint_after(PING_EVERY);
        }
        let dt = self
            .voice
            .conn_bars_at
            .map_or(0.0, |t| t.elapsed().as_secs_f32().min(0.1));
        self.voice.conn_bars_at = Some(Instant::now());
        if dt > 0.0 {
            let want = targets(self.voice.conn_ping_ms);
            for i in 0..4 {
                let cur = self.voice.conn_bars[i];
                let want_i = want[i];
                let step = BAR_SPEED * dt;
                self.voice.conn_bars[i] = if want_i > cur {
                    (cur + step).min(want_i)
                } else {
                    (cur - step).max(want_i)
                };
            }
            ctx.request_repaint();
        }
    }

    pub fn conn_signal(&mut self, ui: &mut egui::Ui) -> bool {
        let h = 13.0;
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(21.0, h), egui::Sense::click());
        self.paint_signal(ui, rect);
        let tip = match self.voice.conn_ping_ms {
            Some(ms) => format!("Ping {ms} ms — clique para ver a conexão"),
            None => "Medindo o ping… — clique para ver a conexão".to_string(),
        };
        resp.on_hover_text(tip).clicked()
    }

    fn paint_signal(&self, ui: &mut egui::Ui, rect: egui::Rect) {
        let colour = ping_colour(self.voice.conn_ping_ms);
        let w = 3.0;
        let gap = 2.5;
        let h = rect.height();
        for i in 0..4 {
            let bh = 3.5 + self.voice.conn_bars[i] * (h - 3.5);
            let bar = egui::Rect::from_min_size(
                egui::pos2(rect.left() + i as f32 * (w + gap), rect.bottom() - bh),
                Vec2::new(w, bh),
            );
            let fill = if self.voice.conn_bars[i] < 0.1 {
                theme::RISCO
            } else {
                colour
            };
            ui.painter().rect_filled(bar, 1.5, fill);
        }
    }

    fn conn_graph_widget(&self, ui: &mut egui::Ui) {
        let (rect, _) = ui.allocate_exact_size(Vec2::new(248.0, 34.0), egui::Sense::hover());
        ui.painter()
            .rect_filled(rect, 2.0, egui::Color32::from_rgb(0x0B, 0x0C, 0x0E));
        let pts: Vec<u32> = if self.voice.conn_graph.len() >= GRAPH_BINS {
            self.voice.conn_graph[self.voice.conn_graph.len() - GRAPH_BINS..].to_vec()
        } else {
            let mut v = vec![self.voice.conn_ping_ms.unwrap_or(0); GRAPH_BINS];
            let n = self.voice.conn_graph.len();
            v[GRAPH_BINS - n..].copy_from_slice(&self.voice.conn_graph);
            v
        };
        let top = pts.iter().copied().max().unwrap_or(1).max(40) as f32;
        let colour = ping_colour(self.voice.conn_ping_ms);
        let n = pts.len();
        let points: Vec<egui::Pos2> = (0..n)
            .map(|i| {
                let x = rect.left() + rect.width() * (i as f32) / ((n - 1) as f32);
                let y = rect.bottom() - rect.height() * (pts[i] as f32 / top);
                egui::pos2(x, y)
            })
            .collect();
        let fill = egui::Color32::from_rgba_unmultiplied(colour.r(), colour.g(), colour.b(), 46);
        for w in points.windows(2) {
            let quad = vec![
                w[0],
                w[1],
                egui::pos2(w[1].x, rect.bottom()),
                egui::pos2(w[0].x, rect.bottom()),
            ];
            ui.painter()
                .add(egui::Shape::convex_polygon(quad, fill, egui::Stroke::NONE));
        }
        ui.painter()
            .add(egui::Shape::line(points.clone(), egui::Stroke::new(1.6, colour)));
        if let Some(last) = points.last() {
            ui.painter().circle_filled(*last, 2.6, egui::Color32::WHITE);
        }
    }

    pub fn conn_panel(&mut self, ctx: &egui::Context) {
        if !self.voice.conn_open || self.voice.channel.is_none() {
            return;
        }
        let channel_name = self
            .voice
            .channel
            .and_then(|c| self.channels.iter().find(|x| x.id == c))
            .map(|c| c.name.clone())
            .unwrap_or_else(|| "—".to_string());
        let ep = endpoint();
        let ping = self.voice.conn_ping_ms;
        let avg = if self.voice.conn_ping_hist.is_empty() {
            None
        } else {
            Some(
                (self.voice.conn_ping_hist.iter().map(|&v| v as u64).sum::<u64>()
                    / self.voice.conn_ping_hist.len() as u64) as u32,
            )
        };
        let colour = ping_colour(ping);
        let height = self.voice.conn_card_h.max(140.0);

        let area = egui::Area::new(egui::Id::new("conn_info"))
            .order(egui::Order::Foreground)
            .anchor(
                egui::Align2::LEFT_BOTTOM,
                Vec2::new(12.0, -(DOCK_H + height + 8.0)),
            )
            .interactable(true);
        let resp = area.show(ctx, |ui| {
            let frame = egui::Frame::new()
                .fill(theme::MENU)
                .stroke(egui::Stroke::new(theme::stroke::HAIRLINE, theme::RISCO))
                .corner_radius(CornerRadius::same(theme::radius::POPUP))
                .inner_margin(Margin::symmetric(16, 14))
                .shadow(egui::epaint::Shadow {
                    offset: [0, 4],
                    blur: 16,
                    spread: 0,
                    color: Color32::from_black_alpha(80),
                });
            frame.show(ui, |ui| {
                ui.set_width(280.0);
                ui.spacing_mut().item_spacing = Vec2::new(0.0, 6.0);

                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("Conexão de voz")
                            .color(theme::GIZ)
                            .strong(),
                    );
                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            let r = ui_kit::icon_button(
                                ui,
                                ph::CORNERS_IN,
                                "Fechar",
                                ui_kit::IconState::Normal,
                            );
                            if r.clicked() {
                                self.voice.conn_open = false;
                            }
                        },
                    );
                });

                ui.add_space(theme::space::XS);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = theme::space::S;
                    // Reserva o espaço antes de pintar, ou o rótulo sobe nas barras.
                    let (bars, _) = ui.allocate_exact_size(Vec2::new(21.0, 13.0), egui::Sense::hover());
                    self.paint_signal(ui, bars);
                    ui.label(
                        egui::RichText::new(&region_code(&ep))
                            .color(theme::GIZ)
                            .strong(),
                    );
                });

                ui.add_space(theme::space::S);
                self.conn_graph_widget(ui);
                ui.add_space(theme::space::S);
                stat_line(
                    ui,
                    "Ping médio",
                    &match avg {
                        Some(m) => format!("{m} ms"),
                        None => "medindo…".to_string(),
                    },
                    ping_colour(avg),
                );
                stat_line(
                    ui,
                    "Último ping",
                    &match ping {
                        Some(m) => format!("{m} ms"),
                        None => "medindo…".to_string(),
                    },
                    colour,
                );
                // Perda real do nosso uplink, quando a voz já mandou stats.
                let loss_txt = if self.voice.conn_rtc_at.is_some() {
                    format!("{:.1}%", self.voice.conn_loss_pct)
                } else {
                    "medindo…".to_string()
                };
                stat_line(
                    ui,
                    "Taxa de perda de pacotes enviados",
                    &loss_txt,
                    theme::GIZ,
                );
                ui.add_space(theme::space::S);
                ui.label(
                    egui::RichText::new(
                        "Você pode notar atraso no áudio quando em 250 ms ou mais. \
                         Se seu ping está acima, tente cabos Ethernet ou conecte-se a \
                         um servidor mais próximo.",
                    )
                    .color(theme::NEVOA)
                    .small(),
                );
                ui.add_space(theme::space::S);
                row(ui, "Servidor", brand::APP_NAME);
                row(ui, "Canal", &channel_name);
                row(ui, "Região", &region_code(&ep));
                row(ui, "Segurança", "Criptografada de ponta a ponta");
            });
        });
        self.voice.conn_card_h = resp.response.rect.height();
    }
}
