//! The colorful emoji picker (v1.1): the Discord-style picker with categories, PT search
//! and "Recentes" ranked by frecency (how much *and* how recently you use each one).
//!
//! The art is Twemoji (CC-BY 4.0, `app/assets/emoji/`), embedded and decoded once per
//! emoji — the same trick as `badges.rs`. The chat input's 🙂 button and the reaction
//! picker both open this; the 40 emojis of the old font-glyph grid are all here, so
//! nobody's favorites disappear. Anything typed or pasted that is not in the catalog
//! still renders through egui's bundled emoji font, as before.

use std::collections::HashMap;

use eframe::egui;
use serde::{Deserialize, Serialize};

use crate::theme;
use crate::ui_kit;

// ---------------------------------------------------------------- catalog

/// The categories of the picker, in tab order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cat {
    Rostos,
    Gestos,
    Coracoes,
    Natureza,
    Comida,
    Jogos,
    Viagem,
    Objetos,
}

impl Cat {
    /// The tab label (a small icon from the picker's own art).
    pub fn label(self) -> &'static str {
        match self {
            Cat::Rostos => "😀",
            Cat::Gestos => "👍",
            Cat::Coracoes => "❤️",
            Cat::Natureza => "🐻",
            Cat::Comida => "🍕",
            Cat::Jogos => "🎮",
            Cat::Viagem => "🚀",
            Cat::Objetos => "💡",
        }
    }

    pub fn all() -> [Cat; 8] {
        [Cat::Rostos, Cat::Gestos, Cat::Coracoes, Cat::Natureza, Cat::Comida, Cat::Jogos, Cat::Viagem, Cat::Objetos]
    }
}

/// One emoji: its text (what gets typed), its PT name (what gets searched) and its art.
pub struct EmojiDef {
    pub ch: &'static str,
    pub name: &'static str,
    pub cat: Cat,
    pub art: &'static [u8],
}

/// The catalog. The emojis of the old reaction grid come first in `Rostos`/`Gestos`, so
/// the muscle memory of who used the old picker keeps working.
macro_rules! e {
    ($ch:expr, $name:expr, $cat:expr, $file:expr) => {
        EmojiDef { ch: $ch, name: $name, cat: $cat, art: include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/emoji/", $file)) }
    };
}

pub const CATALOG: &[EmojiDef] = &[
    // ---- Rostos (the old grid first, then the rest)
    e!("😀", "sorriso", Cat::Rostos, "1f600.png"),
    e!("😂", "rindo chorando", Cat::Rostos, "1f602.png"),
    e!("🥹", "segurando choro", Cat::Rostos, "1f979.png"),
    e!("😭", "chorando alto", Cat::Rostos, "1f62d.png"),
    e!("😅", "suor riso", Cat::Rostos, "1f605.png"),
    e!("🤣", "rolando de rir", Cat::Rostos, "1f923.png"),
    e!("😁", "sorriso brilhante", Cat::Rostos, "1f601.png"),
    e!("🙃", "de cabeça pra baixo", Cat::Rostos, "1f643.png"),
    e!("😉", "piscando", Cat::Rostos, "1f609.png"),
    e!("😊", "olhos sorrindo", Cat::Rostos, "1f60a.png"),
    e!("😇", "anjo", Cat::Rostos, "1f607.png"),
    e!("🥰", "apaixonado", Cat::Rostos, "1f970.png"),
    e!("😍", "olhos de coração", Cat::Rostos, "1f60d.png"),
    e!("😘", "beijo", Cat::Rostos, "1f618.png"),
    e!("😗", "beijo de lado", Cat::Rostos, "1f617.png"),
    e!("😋", "saboroso", Cat::Rostos, "1f60b.png"),
    e!("😜", "lingua piscando", Cat::Rostos, "1f61c.png"),
    e!("🤪", "doido", Cat::Rostos, "1f92a.png"),
    e!("🤨", "sobrancelha erguida", Cat::Rostos, "1f928.png"),
    e!("🧐", "monóculo", Cat::Rostos, "1f9d0.png"),
    e!("🤓", "nerd", Cat::Rostos, "1f913.png"),
    e!("😎", "óculos escuros", Cat::Rostos, "1f60e.png"),
    e!("🥳", "festa", Cat::Rostos, "1f973.png"),
    e!("😏", "sorriso maroto", Cat::Rostos, "1f60f.png"),
    e!("😒", "sem graça", Cat::Rostos, "1f612.png"),
    e!("😞", "desapontado", Cat::Rostos, "1f61e.png"),
    e!("😔", "pensativo triste", Cat::Rostos, "1f614.png"),
    e!("🙁", "sobrancelha franzida", Cat::Rostos, "1f641.png"),
    e!("☹️", "triste", Cat::Rostos, "2639.png"),
    e!("😟", "preocupado", Cat::Rostos, "1f61f.png"),
    e!("🥺", "olhos pidões", Cat::Rostos, "1f97a.png"),
    e!("😢", "lágrima", Cat::Rostos, "1f622.png"),
    e!("😤", "bufando", Cat::Rostos, "1f624.png"),
    e!("😡", "raiva", Cat::Rostos, "1f621.png"),
    e!("😠", "bravo", Cat::Rostos, "1f620.png"),
    e!("🤯", "mente explodindo", Cat::Rostos, "1f92f.png"),
    e!("😳", "vergonha", Cat::Rostos, "1f633.png"),
    e!("🥵", "calor", Cat::Rostos, "1f975.png"),
    e!("🥶", "frio", Cat::Rostos, "1f976.png"),
    e!("😱", "grito", Cat::Rostos, "1f631.png"),
    e!("😨", "medo", Cat::Rostos, "1f628.png"),
    e!("🤗", "abraço", Cat::Rostos, "1f917.png"),
    e!("🤔", "pensando", Cat::Rostos, "1f914.png"),
    e!("🤭", "tapando riso", Cat::Rostos, "1f92d.png"),
    e!("🤫", "silêncio", Cat::Rostos, "1f92b.png"),
    e!("🤥", "mentira", Cat::Rostos, "1f925.png"),
    e!("🙄", "revirando olhos", Cat::Rostos, "1f644.png"),
    e!("😬", "dentes apertados", Cat::Rostos, "1f62c.png"),
    e!("😴", "dormindo", Cat::Rostos, "1f634.png"),
    e!("🤤", "babando", Cat::Rostos, "1f924.png"),
    e!("😵", "cabeça explodida", Cat::Rostos, "1f635.png"),
    e!("🥴", "tonto", Cat::Rostos, "1f974.png"),
    e!("🤢", "nauseado", Cat::Rostos, "1f922.png"),
    e!("🤮", "vomitando", Cat::Rostos, "1f92e.png"),
    e!("🤧", "espirrando", Cat::Rostos, "1f927.png"),
    e!("😷", "máscara", Cat::Rostos, "1f637.png"),
    e!("🤒", "termômetro", Cat::Rostos, "1f912.png"),
    e!("🤕", "band-aid", Cat::Rostos, "1f915.png"),
    e!("💀", "caveira", Cat::Rostos, "1f480.png"),
    e!("👻", "fantasma", Cat::Rostos, "1f47b.png"),
    e!("🤖", "robô", Cat::Rostos, "1f916.png"),
    e!("💩", "cocô", Cat::Rostos, "1f4a9.png"),
    e!("🤡", "palhaço", Cat::Rostos, "1f921.png"),
    e!("😈", "diabinho", Cat::Rostos, "1f608.png"),
    e!("🎃", "abóbora", Cat::Rostos, "1f383.png"),
    e!("👽", "alien", Cat::Rostos, "1f47d.png"),
    // ---- Gestos
    e!("👍", "jóia", Cat::Gestos, "1f44d.png"),
    e!("👎", "ruim", Cat::Gestos, "1f44e.png"),
    e!("👌", "ok", Cat::Gestos, "1f44c.png"),
    e!("✌️", "paz e amor", Cat::Gestos, "270c.png"),
    e!("🤞", "dedos cruzados", Cat::Gestos, "1f91e.png"),
    e!("🤟", "amor você", Cat::Gestos, "1f91f.png"),
    e!("🤘", "rock", Cat::Gestos, "1f918.png"),
    e!("🤙", "me liga", Cat::Gestos, "1f919.png"),
    e!("👊", "soco", Cat::Gestos, "1f44a.png"),
    e!("✊", "punho", Cat::Gestos, "270a.png"),
    e!("👋", "tchau", Cat::Gestos, "1f44b.png"),
    e!("🤚", "mão erguida", Cat::Gestos, "1f91a.png"),
    e!("✋", "pare", Cat::Gestos, "270b.png"),
    e!("🖖", "spock", Cat::Gestos, "1f596.png"),
    e!("👈", "apontando esquerda", Cat::Gestos, "1f448.png"),
    e!("👉", "apontando direita", Cat::Gestos, "1f449.png"),
    e!("👆", "pra cima", Cat::Gestos, "1f446.png"),
    e!("👇", "pra baixo", Cat::Gestos, "1f447.png"),
    e!("☝️", "um dedo", Cat::Gestos, "261d.png"),
    e!("💪", "força", Cat::Gestos, "1f4aa.png"),
    e!("🙏", "rezando", Cat::Gestos, "1f64f.png"),
    e!("👏", "palmas", Cat::Gestos, "1f44f.png"),
    e!("🙌", "mãos pra cima", Cat::Gestos, "1f64c.png"),
    e!("🤝", "aperto de mão", Cat::Gestos, "1f91d.png"),
    e!("🤷", "não sei", Cat::Gestos, "1f937.png"),
    e!("🤦", "face na mão", Cat::Gestos, "1f926.png"),
    e!("👀", "olhos", Cat::Gestos, "1f440.png"),
    // ---- Corações
    e!("❤️", "coração", Cat::Coracoes, "2764.png"),
    e!("🧡", "coração laranja", Cat::Coracoes, "1f9e1.png"),
    e!("💛", "coração amarelo", Cat::Coracoes, "1f49b.png"),
    e!("💚", "coração verde", Cat::Coracoes, "1f49a.png"),
    e!("💙", "coração azul", Cat::Coracoes, "1f499.png"),
    e!("💜", "coração roxo", Cat::Coracoes, "1f49c.png"),
    e!("🖤", "coração preto", Cat::Coracoes, "1f5a4.png"),
    e!("🤍", "coração branco", Cat::Coracoes, "1f90d.png"),
    e!("💔", "coração partido", Cat::Coracoes, "1f494.png"),
    e!("💕", "dois corações", Cat::Coracoes, "1f495.png"),
    e!("💖", "brilhando", Cat::Coracoes, "1f496.png"),
    e!("✨", "estrelinhas", Cat::Coracoes, "2728.png"),
    e!("💫", "tontinho", Cat::Coracoes, "1f4ab.png"),
    e!("🔥", "fogo", Cat::Coracoes, "1f525.png"),
    e!("💯", "cem", Cat::Coracoes, "1f4af.png"),
    e!("👑", "coroa", Cat::Coracoes, "1f451.png"),
    e!("⭐", "estrela", Cat::Coracoes, "2b50.png"),
    e!("🌟", "estrela brilhante", Cat::Coracoes, "1f31f.png"),
    e!("💤", "sono", Cat::Coracoes, "1f4a4.png"),
    e!("💢", "raiva símbolo", Cat::Coracoes, "1f4a2.png"),
    e!("💬", "balão fala", Cat::Coracoes, "1f4ac.png"),
    e!("💭", "pensamento", Cat::Coracoes, "1f4ad.png"),
    e!("⚡", "raio", Cat::Coracoes, "26a1.png"),
    e!("🌈", "arco-íris", Cat::Coracoes, "1f308.png"),
    // ---- Natureza
    e!("🐶", "cachorro", Cat::Natureza, "1f436.png"),
    e!("🐱", "gato", Cat::Natureza, "1f431.png"),
    e!("🐭", "rato", Cat::Natureza, "1f42d.png"),
    e!("🐹", "hamster", Cat::Natureza, "1f439.png"),
    e!("🐰", "coelho", Cat::Natureza, "1f430.png"),
    e!("🦊", "raposa", Cat::Natureza, "1f98a.png"),
    e!("🐻", "urso", Cat::Natureza, "1f43b.png"),
    e!("🐼", "panda", Cat::Natureza, "1f43c.png"),
    e!("🐨", "koala", Cat::Natureza, "1f428.png"),
    e!("🐯", "tigre", Cat::Natureza, "1f42f.png"),
    e!("🦁", "leão", Cat::Natureza, "1f981.png"),
    e!("🐮", "vaca", Cat::Natureza, "1f42e.png"),
    e!("🐷", "porco", Cat::Natureza, "1f437.png"),
    e!("🐸", "sapo", Cat::Natureza, "1f438.png"),
    e!("🐵", "macaco", Cat::Natureza, "1f435.png"),
    e!("🐔", "galinha", Cat::Natureza, "1f414.png"),
    e!("🐧", "pinguim", Cat::Natureza, "1f427.png"),
    e!("🦉", "coruja", Cat::Natureza, "1f989.png"),
    e!("🐺", "lobo", Cat::Natureza, "1f43a.png"),
    e!("🐴", "cavalo", Cat::Natureza, "1f434.png"),
    e!("🦄", "unicornio", Cat::Natureza, "1f984.png"),
    e!("🐝", "abelha", Cat::Natureza, "1f41d.png"),
    e!("🦋", "borboleta", Cat::Natureza, "1f98b.png"),
    e!("🐙", "polvo", Cat::Natureza, "1f419.png"),
    e!("🐬", "golfinho", Cat::Natureza, "1f42c.png"),
    e!("🐳", "baleia", Cat::Natureza, "1f433.png"),
    e!("🦈", "tubarão", Cat::Natureza, "1f988.png"),
    e!("🌵", "cacto", Cat::Natureza, "1f335.png"),
    e!("🌲", "pinheiro", Cat::Natureza, "1f332.png"),
    e!("🌴", "palmeira", Cat::Natureza, "1f334.png"),
    e!("🍀", "trevo", Cat::Natureza, "1f340.png"),
    e!("🌹", "rosa", Cat::Natureza, "1f339.png"),
    e!("🌻", "girassol", Cat::Natureza, "1f33b.png"),
    e!("🌸", "flor rosa", Cat::Natureza, "1f338.png"),
    e!("🌍", "planeta", Cat::Natureza, "1f30d.png"),
    e!("🌙", "lua", Cat::Natureza, "1f319.png"),
    e!("☀️", "sol", Cat::Natureza, "2600.png"),
    e!("☁️", "nuvem", Cat::Natureza, "2601.png"),
    e!("🌧️", "chuva", Cat::Natureza, "1f327.png"),
    e!("❄️", "floco", Cat::Natureza, "2744.png"),
    e!("⛄", "boneco de neve", Cat::Natureza, "26c4.png"),
    e!("🌊", "onda", Cat::Natureza, "1f30a.png"),
    // ---- Comida
    e!("🍎", "maçã", Cat::Comida, "1f34e.png"),
    e!("🍊", "laranja", Cat::Comida, "1f34a.png"),
    e!("🍋", "limão", Cat::Comida, "1f34b.png"),
    e!("🍌", "banana", Cat::Comida, "1f34c.png"),
    e!("🍉", "melancia", Cat::Comida, "1f349.png"),
    e!("🍇", "uva", Cat::Comida, "1f347.png"),
    e!("🍓", "morango", Cat::Comida, "1f353.png"),
    e!("🍍", "abacaxi", Cat::Comida, "1f34d.png"),
    e!("🥝", "kiwi", Cat::Comida, "1f95d.png"),
    e!("🍅", "tomate", Cat::Comida, "1f345.png"),
    e!("🥑", "abacate", Cat::Comida, "1f951.png"),
    e!("🌽", "milho", Cat::Comida, "1f33d.png"),
    e!("🌶️", "pimenta", Cat::Comida, "1f336.png"),
    e!("🥦", "brócolis", Cat::Comida, "1f966.png"),
    e!("🍄", "cogumelo", Cat::Comida, "1f344.png"),
    e!("🍞", "pão", Cat::Comida, "1f35e.png"),
    e!("🥐", "croissant", Cat::Comida, "1f950.png"),
    e!("🧀", "queijo", Cat::Comida, "1f9c0.png"),
    e!("🍳", "ovo frito", Cat::Comida, "1f373.png"),
    e!("🥓", "bacon", Cat::Comida, "1f953.png"),
    e!("🍔", "hamburguer", Cat::Comida, "1f354.png"),
    e!("🍟", "fritas", Cat::Comida, "1f35f.png"),
    e!("🍕", "pizza", Cat::Comida, "1f355.png"),
    e!("🌭", "cachorro quente", Cat::Comida, "1f32d.png"),
    e!("🌮", "taco", Cat::Comida, "1f32e.png"),
    e!("🌯", "burrito", Cat::Comida, "1f32f.png"),
    e!("🥗", "salada", Cat::Comida, "1f957.png"),
    e!("🍝", "espaguete", Cat::Comida, "1f35d.png"),
    e!("🍜", "lámen", Cat::Comida, "1f35c.png"),
    e!("🍣", "sushi", Cat::Comida, "1f363.png"),
    e!("🍚", "arroz", Cat::Comida, "1f35a.png"),
    e!("🍛", "curry", Cat::Comida, "1f35b.png"),
    e!("🍦", "sorvete", Cat::Comida, "1f366.png"),
    e!("🍰", "bolo fatia", Cat::Comida, "1f370.png"),
    e!("🎂", "bolo aniversário", Cat::Comida, "1f382.png"),
    e!("🍫", "chocolate", Cat::Comida, "1f36b.png"),
    e!("🍬", "bala", Cat::Comida, "1f36c.png"),
    e!("🍿", "pipoca", Cat::Comida, "1f37f.png"),
    e!("☕", "café", Cat::Comida, "2615.png"),
    e!("🍵", "chá", Cat::Comida, "1f375.png"),
    e!("🧋", "bubble tea", Cat::Comida, "1f9cb.png"),
    e!("🥤", "refrigerante", Cat::Comida, "1f964.png"),
    e!("🍺", "cerveja", Cat::Comida, "1f37a.png"),
    e!("🍻", "canecas", Cat::Comida, "1f37b.png"),
    e!("🍷", "vinho", Cat::Comida, "1f377.png"),
    // ---- Jogos
    e!("⚽", "futebol", Cat::Jogos, "26bd.png"),
    e!("🏀", "basquete", Cat::Jogos, "1f3c0.png"),
    e!("🏈", "futebol americano", Cat::Jogos, "1f3c8.png"),
    e!("⚾", "beisebol", Cat::Jogos, "26be.png"),
    e!("🎾", "tenis", Cat::Jogos, "1f3be.png"),
    e!("🏐", "volei", Cat::Jogos, "1f3d0.png"),
    e!("🎮", "controle", Cat::Jogos, "1f3ae.png"),
    e!("🎲", "dado", Cat::Jogos, "1f3b2.png"),
    e!("🧩", "quebra-cabeça", Cat::Jogos, "1f9e9.png"),
    e!("🎯", "alvo", Cat::Jogos, "1f3af.png"),
    e!("🎨", "arte", Cat::Jogos, "1f3a8.png"),
    e!("🎤", "microfone", Cat::Jogos, "1f3a4.png"),
    e!("🎧", "fone", Cat::Jogos, "1f3a7.png"),
    e!("🎸", "guitarra", Cat::Jogos, "1f3b8.png"),
    e!("🎹", "teclado", Cat::Jogos, "1f3b9.png"),
    e!("🥁", "bateria", Cat::Jogos, "1f941.png"),
    e!("🎺", "trompete", Cat::Jogos, "1f3ba.png"),
    e!("🎻", "violino", Cat::Jogos, "1f3bb.png"),
    e!("🎉", "festa chiclete", Cat::Jogos, "1f389.png"),
    e!("🎊", "confete", Cat::Jogos, "1f38a.png"),
    e!("🎈", "balão", Cat::Jogos, "1f388.png"),
    e!("🎁", "presente", Cat::Jogos, "1f381.png"),
    // ---- Viagem
    e!("🚗", "carro", Cat::Viagem, "1f697.png"),
    e!("🚌", "ônibus", Cat::Viagem, "1f68c.png"),
    e!("🚓", "polícia", Cat::Viagem, "1f693.png"),
    e!("🚑", "ambulância", Cat::Viagem, "1f691.png"),
    e!("🚒", "bombeiro", Cat::Viagem, "1f692.png"),
    e!("🚜", "trator", Cat::Viagem, "1f69c.png"),
    e!("🛵", "moto", Cat::Viagem, "1f6f5.png"),
    e!("🚲", "bicicleta", Cat::Viagem, "1f6b2.png"),
    e!("✈️", "avião", Cat::Viagem, "2708.png"),
    e!("🚀", "foguete", Cat::Viagem, "1f680.png"),
    e!("🛸", "disco voador", Cat::Viagem, "1f6f8.png"),
    e!("🚁", "helicóptero", Cat::Viagem, "1f681.png"),
    e!("⛵", "vela", Cat::Viagem, "26f5.png"),
    e!("🗺️", "mapa", Cat::Viagem, "1f5fa.png"),
    e!("🗽", "estátua liberdade", Cat::Viagem, "1f5fd.png"),
    e!("🏰", "castelo", Cat::Viagem, "1f3f0.png"),
    e!("🏝️", "ilha", Cat::Viagem, "1f3dd.png"),
    e!("🏔️", "montanha", Cat::Viagem, "1f3d4.png"),
    e!("🌋", "vulcão", Cat::Viagem, "1f30b.png"),
    e!("🏕️", "acampamento", Cat::Viagem, "1f3d5.png"),
    // ---- Objetos
    e!("💻", "notebook", Cat::Objetos, "1f4bb.png"),
    e!("🖥️", "computador", Cat::Objetos, "1f5a5.png"),
    e!("📱", "celular", Cat::Objetos, "1f4f1.png"),
    e!("💡", "ideia", Cat::Objetos, "1f4a1.png"),
    e!("🔋", "bateria", Cat::Objetos, "1f50b.png"),
    e!("🔍", "lupa", Cat::Objetos, "1f50d.png"),
    e!("📷", "câmera", Cat::Objetos, "1f4f7.png"),
    e!("📸", "flash", Cat::Objetos, "1f4f8.png"),
    e!("📺", "tv", Cat::Objetos, "1f4fa.png"),
    e!("⏰", "alarme", Cat::Objetos, "23f0.png"),
    e!("💰", "dinheiro", Cat::Objetos, "1f4b0.png"),
    e!("💳", "cartão", Cat::Objetos, "1f4b3.png"),
    e!("💎", "diamante", Cat::Objetos, "1f48e.png"),
    e!("⚙️", "engrenagem", Cat::Objetos, "2699.png"),
    e!("🔧", "chave inglesa", Cat::Objetos, "1f527.png"),
    e!("🔨", "martelo", Cat::Objetos, "1f528.png"),
    e!("🏆", "troféu", Cat::Objetos, "1f3c6.png"),
    e!("🥇", "ouro", Cat::Objetos, "1f947.png"),
    e!("📚", "livros", Cat::Objetos, "1f4da.png"),
    e!("📖", "livro aberto", Cat::Objetos, "1f4d6.png"),
    e!("💾", "disquete", Cat::Objetos, "1f4be.png"),
    e!("📌", "alfinete", Cat::Objetos, "1f4cc.png"),
    e!("📍", "marcador", Cat::Objetos, "1f4cd.png"),
    e!("✂️", "tesoura", Cat::Objetos, "2702.png"),
    e!("🇧🇷", "brasil", Cat::Objetos, "1f1e7-1f1f7.png"),
];

/// Find a catalog entry by its text (what gets typed).
pub fn def(ch: &str) -> Option<&'static EmojiDef> {
    CATALOG.iter().find(|d| d.ch == ch)
}

// ---------------------------------------------------------------- frecency

/// One remembered use of an emoji: how many times and when was the last.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Use {
    pub ch: String,
    pub count: u32,
    pub last_ms: u64,
}

/// How many "Recentes" the picker shows.
pub const RECENT_MAX: usize = 16;

/// The frecency ranking: how much *and* how recently — exactly the trick the Discord
/// client uses (they call it frecency too). Score = count, weighted by how old the last
/// use is: nothing older than a day still counts half. Ties (same score) go to the
/// most recent.
pub fn frecency(u: &Use, now_ms: u64) -> u64 {
    let age_h = now_ms.saturating_sub(u.last_ms) / 3_600_000;
    let freshness = match age_h {
        0 => 2.0,
        1..=6 => 1.5,
        7..=24 => 1.0,
        25..=72 => 0.6,
        _ => 0.3,
    };
    (u.count as f64 * freshness * 100.0) as u64
}

/// Records one use: bumps the count and stamps the time.
pub fn track(uses: &mut Vec<Use>, ch: &str, now_ms: u64) {
    if let Some(u) = uses.iter_mut().find(|u| u.ch == ch) {
        u.count = u.count.saturating_add(1);
        u.last_ms = now_ms;
    } else {
        uses.push(Use { ch: ch.to_owned(), count: 1, last_ms: now_ms });
    }
    // Keep it small: only the ones that can still show are worth keeping.
    if uses.len() > RECENT_MAX * 2 {
        uses.sort_by(|a, b| frecency(b, now_ms).cmp(&frecency(a, now_ms)).then(b.last_ms.cmp(&a.last_ms)));
        uses.truncate(RECENT_MAX * 2);
    }
}

// ---------------------------------------------------------------- textures + picker

/// The decoded emoji textures of a session (one per emoji, tiny).
#[derive(Default)]
pub struct Textures {
    map: HashMap<&'static str, egui::TextureHandle>,
}

impl Textures {
    pub fn get(&mut self, ctx: &egui::Context, ch: &'static str) -> Option<egui::TextureId> {
        let def = def(ch)?;
        if let Some(t) = self.map.get(ch) {
            return Some(t.id());
        }
        let img = vox_client_core::images::decode(def.art).ok()?;
        let rgba = img.to_rgba8();
        let (w, h) = rgba.dimensions();
        let color = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
        let handle = ctx.load_texture(format!("emoji/{}", def.name), color, egui::TextureOptions::LINEAR);
        self.map.insert(ch, handle);
        self.map.get(ch).map(|t| t.id())
    }
}

/// The picker's memory: the search and the last category (Discord remembers the last
/// tab too). Each surface that opens it (the chat's 🙂, "Reagir") manages its own popup.
#[derive(Default)]
pub struct Picker {
    pub search: String,
    pub cat: Option<Cat>,
}

impl Picker {
    /// As a menu below a button (the chat input's 🙂): opens on click, closes on pick.
    pub fn menu(
        ui: &mut egui::Ui,
        b: &egui::Response,
        picker: &mut Picker,
        textures: &mut Textures,
        uses: &mut Vec<Use>,
        now_ms: u64,
    ) -> Option<&'static str> {
        let mut picked = None;
        egui::Popup::menu(b).frame(crate::ui_kit::popup_frame(ui.style())).style(crate::ui_kit::menu_style).show(|ui| {
            picked = picker.content(ui, textures, uses, now_ms);
        });
        picked
    }

    /// The picker content itself: search, the category strip, and the grid (recentes
    /// when idle, the category or the search when picked). Returns the chosen emoji.
    pub fn content(&mut self, ui: &mut egui::Ui, textures: &mut Textures, uses: &mut Vec<Use>, now_ms: u64) -> Option<&'static str> {
        let mut picked = None;
        ui.spacing_mut().item_spacing.y = 6.0;
        ui.add(ui_kit::field(&mut self.search).hint_text(format!("{}  Buscar emoji", theme::ph::MAGNIFYING_GLASS_PLUS)).desired_width(288.0));
        let q = self.search.trim().to_lowercase();
        if q.is_empty() {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                for c in Cat::all() {
                    let sel = self.cat == Some(c);
                    let (r, resp) = ui.allocate_exact_size(egui::vec2(28.0, 26.0), egui::Sense::click());
                    if sel {
                        ui.painter().rect_filled(r, theme::radius::BUTTON, theme::MOVEL);
                    }
                    ui.painter().text(r.center(), egui::Align2::CENTER_CENTER, c.label(), theme::body(15.0), if sel { theme::GIZ } else { theme::NEVOA });
                    if resp.clicked() {
                        self.cat = Some(c);
                    }
                }
            });
        }
        // The grid: recents when idle, the category (or the search) otherwise.
        let mut cells: Vec<&'static EmojiDef> = Vec::new();
        if !q.is_empty() {
            cells = CATALOG.iter().filter(|d| d.name.contains(&q)).collect();
        } else if self.cat.is_none() {
            let mut recent: Vec<&Use> = uses.iter().collect();
            recent.sort_by(|a, b| frecency(b, now_ms).cmp(&frecency(a, now_ms)).then(b.last_ms.cmp(&a.last_ms)));
            for u in recent.iter().take(RECENT_MAX) {
                if let Some(d) = def(&u.ch) {
                    cells.push(d);
                }
            }
        } else {
            cells = CATALOG.iter().filter(|d| Some(d.cat) == self.cat).collect();
        }
        if cells.is_empty() {
            ui_kit::hint(ui, if q.is_empty() && self.cat.is_none() { "Use um emoji e ele aparece aqui." } else { "Nada encontrado." });
        }
        egui::Grid::new(("emoji_grid", self.cat.is_none(), self.search.clone())).spacing(egui::vec2(2.0, 2.0)).show(ui, |ui| {
            for (i, d) in cells.iter().enumerate() {
                let (r, resp) = ui.allocate_exact_size(egui::vec2(30.0, 30.0), egui::Sense::click());
                if resp.clicked() {
                    picked = Some(d.ch);
                }
                if resp.hovered() {
                    ui.painter().rect_filled(r, theme::radius::BUTTON, theme::ROW_HOVER);
                }
                if let Some(tex) = textures.get(ui.ctx(), d.ch) {
                    let ir = egui::Rect::from_center_size(r.center(), egui::vec2(22.0, 22.0));
                    ui.painter().image(tex, ir, egui::Rect::from_min_max(egui::Pos2::ZERO, egui::Pos2::new(1.0, 1.0)), egui::Color32::WHITE);
                }
                resp.on_hover_text(d.name);
                if (i + 1) % 8 == 0 {
                    ui.end_row();
                }
            }
        });
        if let Some(ch) = picked {
            track(uses, ch, now_ms);
            Some(ch)
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    /// Every catalog entry has real embedded art (a PNG header).
    #[test]
    fn every_emoji_has_art_and_a_name() {
        assert!(CATALOG.len() >= 200, "um catálogo digno (hoje: {})", CATALOG.len());
        for d in CATALOG {
            assert_eq!(&d.art[0..4], &[0x89, b'P', b'N', b'G'], "{} tem PNG", d.name);
            assert!(!d.name.is_empty());
            assert!(!d.ch.is_empty());
        }
        // No duplicates: the search and the textures are keyed by the text.
        let mut seen = std::collections::HashSet::new();
        for d in CATALOG {
            assert!(seen.insert(d.ch), "{} duplicado", d.ch);
        }
    }

    /// Frecency: more use wins; among equals, the most recent; and a fresh use outranks
    /// an old heavier one only within its weight class.
    #[test]
    fn frecency_ranks_by_use_and_freshness() {
        let now = 1_000_000_000_000u64;
        let a = Use { ch: "😀".into(), count: 10, last_ms: now };
        let b = Use { ch: "😂".into(), count: 3, last_ms: now };
        assert!(frecency(&a, now) > frecency(&b, now), "quem usa mais ganha");
        let old = Use { ch: "😀".into(), count: 10, last_ms: now - 200 * 3_600_000 };
        let fresh = Use { ch: "😂".into(), count: 2, last_ms: now };
        assert!(frecency(&fresh, now) > frecency(&old, now), "uso recente vale mais que uso velho");
    }

    /// Tracking bumps, re-stamps and keeps the list bounded.
    #[test]
    fn tracking_bumps_and_bounds() {
        let mut uses = Vec::new();
        let now = 1_000_000_000_000u64;
        track(&mut uses, "😀", now);
        track(&mut uses, "😀", now + 1);
        assert_eq!(uses[0].count, 2);
        assert_eq!(uses[0].last_ms, now + 1);
        for i in 0..(RECENT_MAX * 3) {
            track(&mut uses, "🎉", now + i as u64);
        }
        assert!(uses.len() <= RECENT_MAX * 2 + 2, "a lista fica pequena: {}", uses.len());
    }
}
