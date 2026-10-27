//! Profile badges (v1.1, Discord-style): the catalog that turns `User::flags` into the
//! little art people see on the profile card, the member rows and the admin editor.
//!
//! The bits live in [`vox_proto::badge`]; the hub is the only writer (automatic rules —
//! staff for the owner, the tenure ladder, the first members — OR the grants an admin
//! sets in "Membros e cargos"). Here they become images with names and stories: the art
//! is the classic badge set (embedded below, no network), decoded once per badge and
//! cached for the session.
//!
//! The display follows the Discord reading: the rare fixed badges first (staff, partner,
//! moderator), the houses, the dev/bug set, the "early" ones, and the tenure ladder last
//! — and a row shows at most [`ROW_MAX`] of them plus a "+N" count, exactly the cap the
//! official client uses.

use std::collections::HashMap;

use eframe::egui;
use vox_proto::badge;

use crate::theme;

/// One badge of the catalog: its bit, its embedded art (a small PNG), and the two lines
/// the tooltip tells — a name and why it exists.
pub struct BadgeDef {
    pub bit: u64,
    pub art: &'static [u8],
    pub title: &'static str,
    pub desc: &'static str,
}

/// How many badges a row shows before the "+N" (the official client's cap).
pub const ROW_MAX: usize = 6;
/// The size a badge renders at inside a row.
pub const ROW_SIZE: f32 = 18.0;

/// The catalog in display order. Every [`vox_proto::badge`] bit has an entry.
const fn d(bit: u64, art: &'static [u8], title: &'static str, desc: &'static str) -> BadgeDef {
    BadgeDef { bit, art, title, desc }
}

pub const CATALOG: &[BadgeDef] = &[
    d(badge::STAFF, include_bytes!("../assets/badges/staff.png"), "Staff da AkumaLabs", "A equipe que cuida do app e da comunidade."),
    d(badge::PARTNER, include_bytes!("../assets/badges/partner.png"), "Parceiro(a)", "Uma parceria oficial com a AkumaLabs."),
    d(badge::MODERATOR, include_bytes!("../assets/badges/moderator.png"), "Moderador(a) certificado(a)", "Formado(a) na academia de moderação da comunidade."),
    d(badge::HYPESQUAD, include_bytes!("../assets/badges/hypesquad.png"), "HypeSquad", "Representou a AkumaLabs em eventos."),
    d(badge::BRAVERY, include_bytes!("../assets/badges/bravery.png"), "Casa da Coragem", "HypeSquad: a casa de quem parte pra cima."),
    d(badge::BRILLIANCE, include_bytes!("../assets/badges/brilliance.png"), "Casa do Brilho", "HypeSquad: a casa de quem cria."),
    d(badge::BALANCE, include_bytes!("../assets/badges/balance.png"), "Casa do Equilíbrio", "HypeSquad: a casa de quem pesa tudo."),
    d(badge::BUG_HUNTER, include_bytes!("../assets/badges/bughunter.png"), "Caçador(a) de bugs", "Encontrou e reportou bugs de verdade."),
    d(badge::BUG_HUNTER_GOLD, include_bytes!("../assets/badges/bughunter_gold.png"), "Caçador(a) de bugs Ouro", "Os bugs críticos que ninguém mais achou."),
    d(badge::VERIFIED_DEV, include_bytes!("../assets/badges/verified_dev.png"), "Dev verificado(a)", "Escreveu código que roda no app."),
    d(badge::ACTIVE_DEV, include_bytes!("../assets/badges/active_dev.png"), "Dev ativo(a)", "Contribui com o app agora."),
    d(badge::FIRST_MEMBERS, include_bytes!("../assets/badges/early_supporter.png"), "Primeiros membros", "Entre os 100 primeiros a entrar no servidor."),
    d(badge::QUEST, include_bytes!("../assets/badges/quest.png"), "Missão cumprida", "Completou uma missão da comunidade."),
    d(badge::TENURE_1M, include_bytes!("../assets/badges/tenure_1m.png"), "1 mês de casa", "Um mês inteiro na AkumaLabs."),
    d(badge::TENURE_3M, include_bytes!("../assets/badges/tenure_3m.png"), "3 meses de casa", "Três meses de AkumaLabs."),
    d(badge::TENURE_6M, include_bytes!("../assets/badges/tenure_6m.png"), "6 meses de casa", "Meio ano de AkumaLabs."),
    d(badge::TENURE_12M, include_bytes!("../assets/badges/tenure_12m.png"), "1 ano de casa", "Um ano inteiro de AkumaLabs."),
    d(badge::TENURE_24M, include_bytes!("../assets/badges/tenure_24m.png"), "2 anos de casa", "Dois anos de AkumaLabs."),
    d(badge::TENURE_36M, include_bytes!("../assets/badges/tenure_36m.png"), "3 anos de casa", "Três anos de AkumaLabs."),
    d(badge::TENURE_60M, include_bytes!("../assets/badges/tenure_60m.png"), "5 anos de casa", "Cinco anos de AkumaLabs."),
    d(badge::TENURE_72M, include_bytes!("../assets/badges/tenure_72m.png"), "6 anos de casa", "Seis anos de AkumaLabs."),
];

/// The definition of one bit, when the catalog has it.
pub fn def(bit: u64) -> Option<&'static BadgeDef> {
    CATALOG.iter().find(|d| d.bit == bit)
}

/// A user's badges in display order (the tenure ladder shows only the highest tier).
pub fn of(flags: u64) -> Vec<&'static BadgeDef> {
    let mut out: Vec<&'static BadgeDef> = CATALOG.iter().filter(|d| flags & d.bit != 0).collect();
    // The ladder is cumulative on the wire; the row shows only the highest tier.
    out.retain(|d| !is_tenure(d.bit));
    if let Some(top) = tenure_tier(flags).and_then(def) {
        out.push(top);
    }
    out
}

/// Whether a bit belongs to the tenure ladder.
pub fn is_tenure(bit: u64) -> bool {
    badge::TENURE_MONTHS.iter().any(|&(b, _)| b == bit)
}

/// The highest tenure tier bit a user has reached (`None`: under a month).
pub fn tenure_tier(flags: u64) -> Option<u64> {
    [badge::TENURE_72M, badge::TENURE_60M, badge::TENURE_36M, badge::TENURE_24M, badge::TENURE_12M, badge::TENURE_6M, badge::TENURE_3M, badge::TENURE_1M]
        .into_iter()
        .find(|&t| flags & t != 0)
}

// ---------------------------------------------------------------- textures

/// The badge textures of a session, decoded on first use (they are tiny; the whole set
/// costs less than one avatar). One per badge bit.
#[derive(Default)]
pub struct Textures {
    map: HashMap<u64, egui::TextureHandle>,
}

impl Textures {
    /// The texture of one badge bit, decoding it on first use (`None`: the bit has no art).
    pub fn get(&mut self, ctx: &egui::Context, bit: u64) -> Option<egui::TextureId> {
        let def = def(bit)?;
        if let Some(t) = self.map.get(&bit) {
            return Some(t.id());
        }
        let img = vox_client_core::images::decode(def.art).ok()?;
        let rgba = img.to_rgba8();
        let (w, h) = rgba.dimensions();
        let color = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
        let handle = ctx.load_texture(format!("badge/{bit}"), color, egui::TextureOptions::LINEAR);
        self.map.insert(bit, handle);
        self.map.get(&bit).map(|t| t.id())
    }

    /// The row of badges of one user: up to `max` images, then a "+N" count. Each
    /// one carries the badge's name and story on hover.
    pub fn row(&mut self, ui: &mut egui::Ui, flags: u64, size: f32, max: usize) {
        let badges = of(flags);
        let show = badges.len().min(max);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            for b in &badges[..show] {
                if let Some(id) = self.get(ui.ctx(), b.bit) {
                    let (r, resp) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
                    ui.painter().image(id, r, egui::Rect::from_min_max(egui::Pos2::ZERO, egui::Pos2::new(1.0, 1.0)), egui::Color32::WHITE);
                    resp.on_hover_text(format!("{}\n{}", b.title, b.desc));
                }
            }
            if badges.len() > show {
                ui.label(egui::RichText::new(format!("+{}", badges.len() - show)).font(theme::body(11.0)).color(theme::NEVOA)).on_hover_text(
                    badges[show..].iter().map(|b| b.title).collect::<Vec<_>>().join("\n"),
                );
            }
        });
    }
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use vox_proto::badge;

    /// Every bit of the proto's badge module has art, a name and a story — the catalog can
    /// never fall behind a new bit.
    #[test]
    fn every_grantable_bit_has_a_catalog_entry() {
        for def in CATALOG {
            assert_eq!(def.art.len() > 100, true, "{} tem arte embutida", def.title);
            assert!(!def.title.is_empty() && !def.desc.is_empty());
            assert_eq!(def.bit.count_ones(), 1, "{} é um bit único", def.title);
        }
        // Every grantable bit the proto knows is covered...
        let mut covered = 0u64;
        for def in CATALOG {
            covered |= def.bit;
        }
        assert_eq!(covered, badge::GRANTABLE, "o catálogo cobre exatamente os bits concedíveis");
    }

    /// The tenure ladder collapses to the highest tier on display.
    #[test]
    fn the_ladder_shows_only_the_highest_tier() {
        let flags = badge::tenure_bits(13); // 1, 3, 6 e 12 meses
        let shown = of(flags);
        assert_eq!(shown.len(), 1, "só o 12 meses aparece");
        assert_eq!(shown[0].bit, badge::TENURE_12M);
        // Mixed with a fixed badge, the order puts the fixed one first.
        let flags = badge::STAFF | badge::tenure_bits(13);
        let shown = of(flags);
        assert_eq!(shown.len(), 2);
        assert_eq!(shown[0].bit, badge::STAFF);
        assert_eq!(shown[1].bit, badge::TENURE_12M);
    }

    #[test]
    fn the_highest_tier_is_found() {
        assert_eq!(tenure_tier(badge::tenure_bits(50)), Some(badge::TENURE_36M));
        assert_eq!(tenure_tier(badge::STAFF), None);
    }
}
