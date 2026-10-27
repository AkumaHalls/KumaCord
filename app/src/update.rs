//! Self-update — o modelo do KumaDrive, nativo em Rust.
//!
//! O app verifica o feed da última release do GitHub (boot silencioso + botão em
//! Configurações → Sobre), avisa o usuário em **qualquer tela** com um banner
//! persistente (não é toast: só o usuário ou a atualização de verdade o tiram
//! dali) e atualiza pela tela dedicada: baixa o instalador NSIS, confere o
//! SHA-256 e a **assinatura minisign** com a chave pública compilada aqui (a
//! privada vive só como secret do GitHub Actions), executa o `/S` e o app
//! reinicia sozinho pela mão do instalador.
//!
//! - Assinatura inválida = nada instala. Hash errado = nada instala. O download
//!   só sai do repositório oficial (a URL do feed é fixa; um override de
//!   ambiente serve para testes e continua sujeito à assinatura).
//! - Enquanto baixa, a tela não fecha: o botão some e um aviso explica o
//!   reinício automático.

use std::io::Read;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use eframe::egui;
use eframe::egui::Color32;
use serde::Deserialize;

use crate::theme::{self, ph};
use crate::ui_kit::{self, Kind};
use crate::app::App;

/// O feed da última release (latest.json, anexado pelo GitHub Actions a cada tag `v*`).
pub const FEED_URL: &str = "https://github.com/AkumaHalls/KumaCord/releases/latest/download/latest.json";
/// Override de teste: um feed servido localmente continua sujeito à assinatura.
const FEED_ENV: &str = "AKUMALABS_UPDATE_FEED";
/// Override de teste: força o estado "prazo acabou" (a atualização obrigatória).
const MANDATORY_ENV: &str = "AKUMALABS_UPDATE_MANDATORY";
/// A chave pública de assinatura (a privada é um secret do CI; veja o README).
const PUBLIC_KEY: &str = include_str!("../assets/update-key.pub");
/// Nada de download maior que isso (o instalador tem ~35 MB).
const MAX_DOWNLOAD: usize = 200 * 1024 * 1024;
/// Carência padrão para atualizar (horas) quando o feed não diz; depois disso
/// a atualização vira obrigatória.
const DEFAULT_GRACE_HOURS: u64 = 72;

/// O brilho da notificação: o dourado virou o blurple do Discord (a marca do espelho
/// v1.1.1) sobre o dark novo — contraste alto, sem inventar cor solta pelo app.
const GOLD: Color32 = theme::ABAJUR;
/// O mesmo, para preenchimentos com texto claro por cima.
const GOLD_DIM: Color32 = theme::ABAJUR_TINTA;
/// O aviso de "o prazo acabou": vermelho claro legível sobre o tema noturno.
const WARN: Color32 = Color32::from_rgb(0xFF, 0x7A, 0x7A);

// ---------------------------------------------------------------- o feed

/// `latest.json`, anexado à release a cada tag.
#[derive(Debug, Clone, Deserialize)]
struct Manifest {
    version: String,
    #[serde(default)]
    notes: Option<String>,
    /// O instalador NSIS (`AkumaLabs-<versão>-setup.exe`).
    url: String,
    /// SHA-256 hex do instalador.
    sha256: String,
    /// A assinatura minisign do instalador (o conteúdo do `.sig`, com comentário).
    signature: String,
    /// Quantas horas o usuário pode adiar antes de a atualização virar
    /// obrigatória (`None` = [`DEFAULT_GRACE_HOURS`]).
    #[serde(default)]
    grace_hours: Option<u64>,
}

impl Manifest {
    fn grace_hours(&self) -> u64 {
        self.grace_hours.unwrap_or(DEFAULT_GRACE_HOURS).max(1)
    }
}

/// `Some((a, b, c))` quando o texto é um "a.b.c" (com ou sem `v` inicial).
fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let t = v.trim().trim_start_matches(['v', 'V']);
    let mut parts = t.split('.');
    let a = parts.next()?.parse().ok()?;
    let b = parts.next()?.parse().ok()?;
    let raw = parts.next().unwrap_or("0");
    let c = raw.split(['-', '+']).next().unwrap_or("0").parse().unwrap_or(0);
    Some((a, b, c))
}

/// A versão do feed é mais nova que a compilada? (ilegível = não).
fn is_newer(v: &str) -> bool {
    parse_version(v) > parse_version(env!("CARGO_PKG_VERSION"))
}

// ---------------------------------------------------------------- estado

/// O que a UI mostra.
#[derive(Default)]
enum Phase {
    #[default]
    Idle,
    Checking,
    UpToDate,
    /// A última checagem falhou (o motivo vive no `manual_status`, que o Sobre mostra).
    Failed,
    /// Uma versão nova existe (a janela/banner trabalham com ela).
    Available(Box<Manifest>),
    /// Baixando: bytes prontos, total (quando o servidor diz).
    Downloading { manifest: Box<Manifest>, done: u64, total: Option<u64> },
    /// Baixou; conferindo hash e assinatura antes de instalar.
    Verifying,
    /// Instalador lançado: reiniciando.
    Restarting,
}

/// O que a thread de fundo manda.
enum Msg {
    UpToDate,
    Available(Box<Manifest>),
    Failed(String),
    Progress { done: u64, total: Option<u64> },
    Verifying,
    Restarting,
}

/// O estado do updater na `App` (uma por sessão).
#[derive(Default)]
pub struct UpdateUi {
    phase: Phase,
    /// O banner some quando o usuário dispensa (por sessão; o boot traz de volta
    /// enquanto houver versão nova — igual ao aviso do KumaDrive).
    notice_dismissed: bool,
    /// A tela de atualização está aberta (o banner não vira ruído com ela aberta).
    window_open: bool,
    /// A última resposta de uma checagem manual (o Sobre mostra).
    manual_status: Option<String>,
    /// v1.0.3: quando a versão disponível foi oferecida pela primeira vez
    /// `(version, unix ms)` — vem do settings e volta pra ele (o prazo conta
    /// daqui, não de cada boot, então adiar não reinicia a carência).
    pub first_seen: Option<(String, u64)>,
    rx: Option<Receiver<Msg>>,
    /// Conta os passos de "verificando" para animar as reticências.
    blink: u32,
}

impl UpdateUi {
    /// A `App` monta o estado com a carência que vinha do settings; os demais
    /// campos ficam privados ao módulo e nascem no default.
    pub fn with_first_seen(first_seen: Option<(String, u64)>) -> Self {
        Self { first_seen, ..Default::default() }
    }
}

impl App {
    // ------------------------------------------------------------ acessores (o Sobre usa)

    /// Alguma checagem/download em voo?
    pub fn update_busy(&self) -> bool {
        matches!(self.update.phase, Phase::Checking | Phase::Downloading { .. } | Phase::Verifying | Phase::Restarting)
    }

    /// A versão nova que está esperando decisão, se houver.
    pub fn update_available_version(&self) -> Option<String> {
        match &self.update.phase {
            Phase::Available(m) | Phase::Downloading { manifest: m, .. } => Some(m.version.clone()),
            _ => None,
        }
    }

    /// A última resposta de uma checagem manual (status do Sobre).
    pub fn update_status(&self) -> Option<String> {
        self.update.manual_status.clone()
    }

    /// O Sobre (ou o banner) pediu para ver a versão nova.
    pub fn update_open_window(&mut self) {
        if matches!(self.update.phase, Phase::Available(_)) {
            self.update.window_open = true;
        }
    }

    // ------------------------------------------------------------ prazo de carência

    /// Hora (unix ms) em que a carência desta versão acaba, se há versão nova.
    fn update_deadline_ms(&self) -> Option<(String, u64)> {
        let manifest = match &self.update.phase {
            Phase::Available(m) | Phase::Downloading { manifest: m, .. } => m,
            _ => return None,
        };
        let (version, seen) = self.update.first_seen.clone()?;
        if version != manifest.version {
            return None;
        }
        Some((version, seen + manifest.grace_hours() * 3_600_000))
    }

    /// A atualização é obrigatória: a carência acabou (ou o override de teste manda).
    pub fn update_mandatory(&self) -> bool {
        if std::env::var(MANDATORY_ENV).is_ok_and(|v| !v.is_empty()) {
            return true;
        }
        match self.update_deadline_ms() {
            Some((_, deadline)) => crate::app::now_ms() as u64 >= deadline,
            // Sem marcação de quando apareceu (settings antigo): começa a contar agora
            // em vez de obrigar na hora — justo com quem atualizou do 1.0.2.
            None => false,
        }
    }

    /// "Adie por mais Xh" para a UI opcional (carência correndo).
    fn update_hours_left(&self) -> Option<u64> {
        let (_, deadline) = self.update_deadline_ms()?;
        let left = deadline.saturating_sub(crate::app::now_ms() as u64);
        Some((left + 3_599_000) / 3_600_000) // arredonda pra cima: "3h" enquanto houver minuto
    }

    // ------------------------------------------------------------ ações

    /// Checa o feed em uma thread (boot ou botão do Sobre). Silenciosa no boot:
    /// qualquer erro vira nada; manual mostra o resultado na tela.
    pub fn update_check_start(&mut self, manual: bool) {
        if !cfg!(windows) {
            if manual {
                self.update.manual_status = Some("As atualizações automáticas são para Windows; no macOS baixe a versão nova no GitHub.".into());
            }
            return;
        }
        if !matches!(self.update.phase, Phase::Idle | Phase::UpToDate | Phase::Failed) {
            return; // já há algo em voo (ou uma versão esperando decisão)
        }
        if manual {
            self.update.manual_status = Some("Verificando…".into());
        }
        self.update.phase = Phase::Checking;
        let (tx, rx) = std::sync::mpsc::channel::<Msg>();
        self.update.rx = Some(rx);
        std::thread::spawn(move || check_thread(tx));
    }

    /// O usuário pediu a atualização: baixa, confere e instala.
    pub fn update_install_start(&mut self) {
        let Phase::Available(m) = &self.update.phase else { return };
        let manifest: Manifest = (**m).clone();
        self.update.phase = Phase::Downloading { manifest: Box::new(manifest.clone()), done: 0, total: None };
        let (tx, rx) = std::sync::mpsc::channel::<Msg>();
        self.update.rx = Some(rx);
        std::thread::spawn(move || install_thread(tx, manifest));
    }

    /// Consome as mensagens da thread em voo (uma por quadro).
    pub fn update_poll(&mut self, ctx: &egui::Context) {
        let Some(rx) = self.update.rx.take() else { return };
        let mut keep = None;
        for msg in rx.try_iter() {
            match msg {
                Msg::UpToDate => {
                    self.update.phase = Phase::UpToDate;
                    self.update.manual_status = Some("Você está na versão mais recente.".into());
                    if self.update.window_open && matches!(self.update.phase, Phase::UpToDate) {
                        self.update.window_open = false;
                    }
                }
                Msg::Available(m) => {
                    self.update.manual_status = Some(format!("Nova versão v{} disponível.", m.version));
                    self.update.notice_dismissed = false;
                    // O prazo conta da primeira vez que a versão foi vista (persiste no
                    // settings): adiar hoje não recomeça a carência amanhã.
                    let first = self.update.first_seen.clone();
                    if !first.is_some_and(|(v, _)| v == m.version) {
                        self.update.first_seen = Some((m.version.clone(), crate::app::now_ms() as u64));
                        self.settings.update_first_seen = self.update.first_seen.clone();
                        self.save_settings();
                        tracing::info!(version = %m.version, "update: carência de {}h começa agora", m.grace_hours());
                    }
                    self.update.phase = Phase::Available(m);
                    // Obrigatória (prazo vencido): a tela abre sozinha, sem espera.
                    if self.update_mandatory() {
                        tracing::warn!("update: prazo vencido - atualização obrigatória");
                        self.update.window_open = true;
                    }
                }
                Msg::Failed(e) => {
                    self.update.manual_status = Some(format!("Não deu para verificar: {e}"));
                    self.update.phase = Phase::Failed;
                }
                Msg::Progress { done, total } => {
                    if let Phase::Downloading { done: d, total: t, .. } = &mut self.update.phase {
                        *d = done;
                        *t = total;
                    }
                }
                Msg::Verifying => self.update.phase = Phase::Verifying,
                Msg::Restarting => {
                    self.update.phase = Phase::Restarting;
                }
            }
        }
        // Enquanto algo está em voo, a UI precisa continuar animando.
        if matches!(self.update.phase, Phase::Checking | Phase::Downloading { .. } | Phase::Verifying | Phase::Restarting) {
            ctx.request_repaint_after(Duration::from_millis(150));
            self.update.blink = self.update.blink.wrapping_add(1);
            keep = Some(rx);
        } else if matches!(self.update.phase, Phase::Idle | Phase::UpToDate | Phase::Failed) {
            // Fim de linha da thread: o canal é descartado.
        } else {
            // Available (sem thread em voo): nada a consumir; descarta o canal.
        }
        if let Some(rx) = keep {
            self.update.rx = Some(rx);
        }
        // Reinício: dá um instante para a mensagem pintar e sai — o instalador
        // (já lançado com um atraso de 1 s) substitui os arquivos e reabre o app.
        if matches!(self.update.phase, Phase::Restarting) {
            if self.update.blink > 4 {
                tracing::info!("update: saindo para o instalador");
                std::process::exit(0);
            }
        }
    }

    /// O banner em qualquer tela (não é toast: não some sozinho).
    pub fn update_notice(&mut self, ctx: &egui::Context) {
        let (version, notes_len) = match &self.update.phase {
            Phase::Available(m) => (m.version.clone(), m.notes.as_deref().map(str::len).unwrap_or(0)),
            _ => return,
        };
        let mandatory = self.update_mandatory();
        if (self.update.notice_dismissed && !mandatory) || self.update.window_open {
            return;
        }
        let _ = notes_len;
        egui::Area::new(egui::Id::new("update_notice"))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 6.0))
            .show(ctx, |ui| {
                // O card: um brilho dourado por baixo (o "cha" que faltava), o rail
                // na esquerda e o conteúdo por cima.
                let glow = egui::epaint::Shadow { offset: [0, 3], blur: 18, spread: 3, color: (if mandatory { WARN } else { GOLD }).gamma_multiply(0.35) };
                egui::Frame::new()
                    .fill(theme::PAREDE)
                    .stroke(egui::Stroke::new(1.0, if mandatory { WARN.gamma_multiply(0.6) } else { GOLD.gamma_multiply(0.55) }))
                    .corner_radius(theme::radius::POPUP)
                    .shadow(glow)
                    .inner_margin(egui::Margin::same(0))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            // O rail: a tarja de cor na borda esquerda.
                            let rail = if mandatory { WARN } else { GOLD };
                            let h = ui.available_height().max(52.0);
                            let (r, _) = ui.allocate_exact_size(egui::vec2(5.0, h), egui::Sense::hover());
                            ui.painter().rect_filled(r, 2.0, rail);
                            ui.add_space(12.0);
                            ui.label(egui::RichText::new(ph::SPARKLE).font(theme::body(20.0)).color(rail));
                            ui.vertical(|ui| {
                                ui.label(
                                    egui::RichText::new(if mandatory { "Atualização obrigatória" } else { "Atualização disponível" })
                                        .font(theme::semibold(13.5))
                                        .color(theme::GIZ),
                                );
                                ui.horizontal(|ui| {
                                    ui.label(egui::RichText::new(format!("v{}", env!("CARGO_PKG_VERSION"))).font(theme::body(11.5)).color(theme::NEVOA));
                                    ui.label(egui::RichText::new(ph::ARROW_RIGHT).font(theme::body(11.5)).color(theme::NEVOA));
                                    ui.label(egui::RichText::new(format!("v{version}")).font(theme::semibold(11.5)).color(rail));
                                });
                            });
                            ui.add_space(10.0);
                            if ui_kit::button(ui, Kind::Secondary, None, if mandatory { "Atualizar agora" } else { "Ver detalhes" }).clicked() {
                                self.update.window_open = true;
                            }
                            if !mandatory {
                                ui.add_space(2.0);
                                if ui_kit::icon_button(ui, ph::X, "Dispensar aviso de atualização", ui_kit::IconState::Normal).clicked() {
                                    self.update.notice_dismissed = true;
                                }
                            }
                            ui.add_space(12.0);
                        });
                    });
            });
    }

    /// A tela de atualização (par de versões, novidades, progresso, ações).
    pub fn update_window(&mut self, ctx: &egui::Context) {
        if !self.update.window_open {
            return;
        }
        let mandatory = self.update_mandatory();
        let hours_left = self.update_hours_left();
        let busy = matches!(self.update.phase, Phase::Checking | Phase::Downloading { .. } | Phase::Verifying | Phase::Restarting);
        // O que cada fase mostra, extraído antes (o match de baixo não pode
        // segurar `self` emprestado enquanto os botões chamam métodos mutáveis).
        let available: Option<Manifest> = match &self.update.phase {
            Phase::Available(m) => Some((**m).clone()),
            _ => None,
        };
        let downloading: Option<(u64, Option<u64>)> = match &self.update.phase {
            Phase::Downloading { done, total, .. } => Some((*done, *total)),
            _ => None,
        };
        let verifying = matches!(self.update.phase, Phase::Verifying);
        let restarting = matches!(self.update.phase, Phase::Restarting);
        let blink = self.update.blink;
        let mut open = true;
        let accent = if mandatory { WARN } else { GOLD };
        let glow = egui::epaint::Shadow { offset: [0, 4], blur: 26, spread: 4, color: accent.gamma_multiply(0.30) };
        egui::Window::new("atualizacao")
            .title_bar(false)
            .resizable(false)
            .collapsible(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .frame(
                egui::Frame::new()
                    .fill(theme::PAREDE)
                    .stroke(egui::Stroke::new(1.0, if mandatory { WARN.gamma_multiply(0.55) } else { GOLD.gamma_multiply(0.45) }))
                    .corner_radius(theme::radius::POPUP)
                    .shadow(glow),
            )
            .open(&mut open)
            .show(ctx, |ui| {
                ui.set_width(560.0);
                ui.spacing_mut().item_spacing.y = 10.0;
                // A faixa de marca no topo (como o aviso do KumaDrive, na cor da AkumaLabs).
                {
                    let (r, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 4.0), egui::Sense::hover());
                    ui.painter().rect_filled(r, 0.0, accent);
                }
                ui.add_space(10.0);
                // Cabeçalho: o spark num chip da cor, título claro.
                ui.horizontal(|ui| {
                    let chip = accent.gamma_multiply(0.16);
                    let (r, _) = ui.allocate_exact_size(egui::vec2(46.0, 46.0), egui::Sense::hover());
                    ui.painter().rect_filled(r, theme::radius::BUTTON, chip);
                    ui.painter().rect_stroke(r, theme::radius::BUTTON, egui::Stroke::new(1.0, accent.gamma_multiply(0.5)), egui::StrokeKind::Inside);
                    let icon = ui.painter().layout_no_wrap(ph::SPARKLE.to_string(), theme::body(24.0), accent);
                    ui.painter().galley(r.center() - icon.size() / 2.0, icon, accent);
                    ui.add_space(4.0);
                    ui.vertical(|ui| {
                        ui.label(
                            egui::RichText::new(if mandatory { "Atualização obrigatória" } else { "Nova versão disponível" })
                                .font(theme::display(19.0))
                                .color(theme::GIZ),
                        );
                        ui.label(
                            egui::RichText::new(if mandatory {
                                "O tempo de adiamento acabou: atualize para continuar usando o app."
                            } else {
                                "Uma versão mais recente do AkumaLabs foi publicada. Atualize para receber as novidades e correções."
                            })
                            .font(theme::body(12.5))
                            .color(theme::NEVOA),
                        );
                    });
                });
                ui.add_space(8.0);
                if let Some(manifest) = &available {
                    self.update_version_pair(ui, &manifest.version, accent);
                    ui.add_space(6.0);
                    self.update_notes(ui, manifest.notes.as_deref(), accent);
                    ui.add_space(8.0);
                    // O prazo: quanto resta de carência (ou o aviso de que acabou).
                    if let Some(h) = hours_left.filter(|_| !mandatory) {
                        ui.label(
                            egui::RichText::new(format!("Você pode adiar por mais {h}h; depois a atualização se torna obrigatória."))
                                .font(theme::body(11.5))
                                .color(theme::NEVOA),
                        );
                    }
                    ui.add_space(2.0);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if gold_button(ui, &format!("{}  Atualizar para v{}", ph::DOWNLOAD_SIMPLE, manifest.version)).clicked() {
                            self.update_install_start();
                        }
                        if !mandatory && ui_kit::button_sized(ui, Kind::Secondary, ui_kit::Size::Large, None, "Agora não").clicked() {
                            self.update.window_open = false;
                        }
                        if mandatory && ui_kit::button_sized(ui, Kind::Secondary, ui_kit::Size::Large, None, "Sair do app").clicked() {
                            tracing::info!("update: saindo (atualização obrigatória recusada)");
                            std::process::exit(0);
                        }
                    });
                } else if let Some((done, total)) = downloading {
                    self.update_progress(ui, "Baixando e instalando…", done, total, blink, accent);
                    ui.add_space(2.0);
                    self.update_restarting_note(ui);
                } else if verifying {
                    let dots = ".".repeat(1 + (blink / 3) as usize % 3);
                    ui.label(egui::RichText::new(format!("Verificando assinatura{dots}")).font(theme::semibold(13.0)).color(theme::GIZ));
                    self.update_restarting_note(ui);
                } else if restarting {
                    ui.label(egui::RichText::new("Reiniciando para atualizar…").font(theme::semibold(13.0)).color(theme::GIZ));
                    self.update_restarting_note(ui);
                }
                ui.add_space(10.0);
            });
        // Só fecha de verdade quando não está ocupada (e nunca quando é obrigatória).
        if !open && !busy && !mandatory {
            self.update.window_open = false;
        }
    }

    /// O par "Instalada vA → Disponível vB".
    fn update_version_pair(&self, ui: &mut egui::Ui, new: &str, accent: Color32) {
        ui.horizontal(|ui| {
            let panel = |ui: &mut egui::Ui, tag: &str, version: &str, accent_it: bool| {
                let (fill, stroke, tag_color) = if accent_it {
                    (accent.gamma_multiply(0.14), egui::Stroke::new(1.0, accent.gamma_multiply(0.55)), accent)
                } else {
                    (theme::NOITE.gamma_multiply(0.6), egui::Stroke::new(1.0, theme::RISCO), theme::NEVOA)
                };
                egui::Frame::new().fill(fill).stroke(stroke).corner_radius(theme::radius::BUTTON).inner_margin(egui::Margin::symmetric(18, 14)).show(ui, |ui| {
                    ui.set_min_width(170.0);
                    ui.vertical(|ui| {
                        ui.label(egui::RichText::new(tag).font(theme::semibold(10.0)).color(tag_color));
                        ui.label(egui::RichText::new(format!("v{version}")).font(theme::semibold(14.0)).color(theme::GIZ));
                    });
                });
            };
            panel(ui, "INSTALADA", env!("CARGO_PKG_VERSION"), false);
            ui.label(egui::RichText::new(ph::ARROW_RIGHT).font(theme::body(18.0)).color(theme::NEVOA));
            panel(ui, "DISPONÍVEL", new, true);
        });
    }

    /// "Novidades desta versão" (com fallback honesto quando a release veio sem notas).
    fn update_notes(&self, ui: &mut egui::Ui, notes: Option<&str>, accent: Color32) {
        egui::Frame::new().fill(theme::NOITE.gamma_multiply(0.6)).stroke(egui::Stroke::new(1.0, theme::RISCO)).corner_radius(theme::radius::BUTTON).inner_margin(egui::Margin::symmetric(14, 10)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(egui::RichText::new("NOVIDADES DESTA VERSÃO").font(theme::semibold(10.0)).color(accent));
            ui.add_space(4.0);
            let text = notes.filter(|n| !n.trim().is_empty());
            match text {
                Some(n) => {
                    let short = n.len() < 600;
                    if short {
                        ui.add(egui::Label::new(egui::RichText::new(n).font(theme::body(12.0)).color(theme::TEXTO)).wrap());
                    } else {
                        egui::ScrollArea::vertical().id_salt("update_notes").max_height(180.0).auto_shrink([false, false]).show(ui, |ui| {
                            ui.add(egui::Label::new(egui::RichText::new(n).font(theme::body(12.0)).color(theme::TEXTO)).wrap());
                        });
                    }
                }
                None => {
                    ui.add(egui::Label::new(
                        egui::RichText::new("Esta versão traz melhorias e correções internas. Os detalhes ficam no CHANGELOG.md do repositório.")
                            .font(theme::body(12.0))
                            .color(theme::NEVOA),
                    ).wrap());
                }
            }
        });
    }

    /// A barra de progresso com os bytes e o percentual.
    fn update_progress(&self, ui: &mut egui::Ui, label: &str, done: u64, total: Option<u64>, blink: u32, accent: Color32) {
        ui.vertical(|ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(label).font(theme::semibold(12.5)).color(theme::GIZ));
                if let Some(t) = total.filter(|t| *t > 0) {
                    let pct = (done as f32 / t as f32 * 100.0).clamp(0.0, 100.0);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(egui::RichText::new(format!("{:.0}%", pct)).font(theme::semibold(12.5)).color(theme::MOVEL));
                    });
                }
            });
            ui.add_space(4.0);
            let w = ui.available_width();
            let (rect, _) = ui.allocate_exact_size(egui::vec2(w, 8.0), egui::Sense::hover());
            ui.painter().rect_filled(rect, theme::radius::PILL, theme::RISCO);
            if let Some(t) = total.filter(|t| *t > 0) {
                let frac = (done as f32 / t as f32).clamp(0.0, 1.0);
                let mut bar = rect;
                bar.set_width(rect.width() * frac);
                ui.painter().rect_filled(bar, theme::radius::PILL, accent);
            } else {
                // Tamanho desconhecido: uma barra migalha animada.
                let b = blink as f32;
                let x = (b * 6.0) % (rect.width().max(1.0) - 40.0);
                let mut bar = rect;
                bar.set_width(40.0);
                bar.set_left(rect.left() + x);
                ui.painter().rect_filled(bar, theme::radius::PILL, accent);
            }
            ui.add_space(2.0);
            let mb = |n: u64| format!("{:.1} MB", n as f64 / 1_048_576.0);
            let text = match total {
                Some(t) => format!("{} de {}", mb(done), mb(t)),
                None => mb(done),
            };
            ui.label(egui::RichText::new(text).font(theme::body(11.0)).color(theme::NEVOA));
        });
    }

    fn update_restarting_note(&self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.add(
            egui::Label::new(
                egui::RichText::new("Não feche o AkumaLabs — ele reinicia sozinho quando a instalação termina.")
                    .font(theme::body(11.5))
                    .color(theme::NEVOA),
            )
            .wrap(),
        );
    }
}

// ---------------------------------------------------------------- ui helpers

/// O botão primário da atualização — o mesmo papel do gold_button antigo, agora no
/// blurple do Discord (`theme::ABAJUR`) com texto branco: o `Kind::Primary` do kit
/// segue o mesmo tom desde a troca da paleta, mas aqui o botão é desenhado na mão
/// pra carregar o tamanho maior que a tela pede.
fn gold_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    let ink = theme::ABAJUR_TINTA;
    let galley = ui.painter().layout_no_wrap(label.to_owned(), theme::semibold(13.5), ink);
    let size = egui::vec2(galley.size().x + 34.0, 36.0);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    let hovered = resp.hovered() && ui.is_enabled();
    let fill = if resp.has_focus() || ui.is_enabled() && hovered { theme::ABAJUR_HOVER } else { theme::ABAJUR };
    ui.painter().rect_filled(rect, theme::radius::BUTTON, fill);
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, ink);
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), label));
    resp
}

// ---------------------------------------------------------------- threads

fn feed_url() -> String {
    std::env::var(FEED_ENV).ok().filter(|s| !s.is_empty()).unwrap_or_else(|| FEED_URL.to_string())
}

fn http_get(url: &str) -> reqwest::Result<reqwest::blocking::Response> {
    reqwest::blocking::Client::builder()
        .user_agent(concat!("AkumaLabs/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .build()?
        .get(url)
        .timeout(Duration::from_secs(30))
        .send()
}

fn check_thread(tx: Sender<Msg>) {
    let url = feed_url();
    tracing::info!(%url, "update: verificando");
    let result = (|| -> Result<Msg, String> {
        let body = http_get(&url).map_err(|e| format!("sem conexão com o GitHub ({e})"))?;
        let body = body.text().map_err(|e| format!("resposta ilegível ({e})"))?;
        let manifest: Manifest = serde_json::from_str(&body).map_err(|_| "o feed de atualização está em formato desconhecido".to_string())?;
        if !is_newer(&manifest.version) {
            tracing::info!("update: em dia");
            return Ok(Msg::UpToDate);
        }
        // Producao: so https e so no repositorio oficial. Com o override de
        // teste (AKUMALABS_UPDATE_FEED), qualquer URL - a assinatura minisign
        // continua sendo o portao de verdade em ambos os casos.
        let testing = std::env::var(FEED_ENV).is_ok_and(|v| !v.is_empty());
        if !testing && (!manifest.url.starts_with("https://") || !manifest.url.contains("github.com/AkumaHalls/KumaCord/releases/")) {
            return Err("o feed aponta para um endereço fora do repositório oficial".into());
        }
        tracing::info!(version = %manifest.version, "update: versão nova");
        Ok(Msg::Available(Box::new(manifest)))
    })();
    match result {
        Ok(msg) => { let _ = tx.send(msg); }
        Err(e) => { tracing::info!(error = %e, "update: checagem falhou"); let _ = tx.send(Msg::Failed(e)); }
    }
}

fn install_thread(tx: Sender<Msg>, manifest: Manifest) {
    let result = (|| -> Result<(), String> {
        // Baixa para o temp, conferindo o hash na stream.
        let dest = std::env::temp_dir().join(format!("AkumaLabs-{}-setup.exe", manifest.version));
        let mut rsp = http_get(&manifest.url).map_err(|e| format!("o download não começou ({e})"))?;
        let total = rsp.content_length();
        let mut file = std::fs::File::create(&dest).map_err(|e| format!("não deu para criar o arquivo temporário ({e})"))?;
        use sha2::{Digest as _, Sha256};
        let mut hasher = Sha256::new();
        let mut done: u64 = 0;
        loop {
            let mut chunk = [0u8; 64 * 1024];
            let n = rsp.read(&mut chunk).map_err(|e| format!("o download parou no meio ({e})"))?;
            if n == 0 {
                break;
            }
            done += n as u64;
            if done as usize > MAX_DOWNLOAD {
                let _ = std::fs::remove_file(&dest);
                return Err("o arquivo baixado é maior do que o esperado".into());
            }
            hasher.update(&chunk[..n]);
            std::io::Write::write_all(&mut file, &chunk[..n]).map_err(|e| format!("o disco encheu ou negou o arquivo ({e})"))?;
            let _ = tx.send(Msg::Progress { done, total });
        }
        drop(file);
        let _ = tx.send(Msg::Verifying);
        // SHA-256 do manifesto.
        let got: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
        if !got.eq_ignore_ascii_case(manifest.sha256.trim()) {
            let _ = std::fs::remove_file(&dest);
            return Err("o arquivo baixado não confere com o hash publicado (download corrompido)".into());
        }
        // Assinatura minisign (a chave pública está compilada no app).
        let bytes = std::fs::read(&dest).map_err(|e| format!("não deu para reler o instalador ({e})"))?;
        let pk = public_key().ok_or("a chave de assinatura embutida é inválida")?;
        let sig = minisign_verify::Signature::decode(&manifest.signature).map_err(|_| "a assinatura publicada é ilegível".to_string())?;
        pk.verify(&bytes, &sig, false).map_err(|_| "a assinatura do instalador não é válida (arquivo rejeitado)".to_string())?;
        tracing::info!(path = %dest.display(), "update: instalador verificado; lançando");
        // Lança direto: o instalador mata este app no primeiro passo (taskkill
        // no .nsi) e segue com os arquivos livres - nada de trampolim com
        // `timeout`, que morre sem console anexo.
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            std::process::Command::new(&dest)
                .arg("/S")
                .creation_flags(0x0800_0000)
                .spawn()
                .map_err(|e| format!("não deu para executar o instalador ({e})"))?;
        }
        let _ = tx.send(Msg::Restarting);
        Ok(())
    })();
    if let Err(e) = result {
        tracing::warn!(error = %e, "update: instalação falhou");
        let _ = tx.send(Msg::Failed(e));
    }
}

/// A segunda linha do `.pub` (a base64); a primeira é só comentário.
fn public_key() -> Option<minisign_verify::PublicKey> {
    let body = PUBLIC_KEY.lines().find(|l| l.starts_with("RW"))?;
    minisign_verify::PublicKey::from_base64(body).ok()
}
