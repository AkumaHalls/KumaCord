//! A landing page e o feed de atualização, servidos pelo próprio hub.
//!
//! Duas peças que existem para a página poder ser escrita **uma vez**:
//!
//! - `GET /` devolve o HTML (embutido no binário com [`include_str!`], então não há
//!   arquivo solto no volume, nem pasta para servir);
//! - `GET /api/update` espelha o `latest.json` do GitHub com cache. O browser não
//!   consegue ler o GitHub direto (os assets de release não mandam header de CORS),
//!   então a página pergunta aqui — mesma origem, sem precisar de proxy externo.
//!
//! A URL de download não muda nunca: o CI publica uma cópia do instalador com nome
//! fixo, `KumaCord-setup.exe`, e `releases/latest/download/...` aponta para a release
//! mais recente. Por isso a página não carrega versão nem link: ela lê do feed.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};

/// O HTML da landing, embutido no binário (mudar a página = subir o hub).
const LANDING_HTML: &str = include_str!("../assets/landing.html");

/// O ícone da instalação (favicon da aba).
const FAVICON: &[u8] = include_bytes!("../assets/favicon.ico");

/// A logo do KumaCord, no tamanho que a página desenha (e o dobro, para telas
/// de densidade alta). Embedada: a página nasce junto com o hub, sem pasta solta.
const LOGO_256: &[u8] = include_bytes!("../assets/logo-256.png");
const LOGO_512: &[u8] = include_bytes!("../assets/logo-512.png");

/// De quanto em quanto tempo o hub vai ao GitHub buscar o feed. A versão muda a cada
/// release, não a cada minuto: 10 minutos é fresco e não gasta rate limit à toa.
const FEED_TTL: Duration = Duration::from_secs(600);

/// Tempo máximo de espera do GitHub (a página não pode pendurar por causa disso).
const FEED_TIMEOUT: Duration = Duration::from_secs(15);

/// O feed do GitHub (`latest.json`, anexado a cada tag pelo CI).
const FEED_URL: &str = "https://github.com/AkumaHalls/KumaCord/releases/latest/download/latest.json";

/// O que a landing mostra. Só o que ela usa de verdade: a página é pública, então o
/// feed não é repassado cru — apenas estes campos.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UpdateFeed {
    pub version: String,
    #[serde(default)]
    pub notes: Option<String>,
}

/// A entrada em cache: o feed e de quando ele é. `None` enquanto nunca buscou.
#[derive(Default)]
struct FeedCache {
    /// O último feed que o GitHub deu ( `None` = ainda não buscou, ou a última falhou ).
    feed: Option<UpdateFeed>,
    /// Quando ele foi guardado ( para o TTL ).
    at: Option<Instant>,
}

/// O cache compartilhado pelos pedidos.
#[derive(Clone, Default)]
pub struct Landing {
    cache: Arc<Mutex<FeedCache>>,
}

impl Landing {
    pub fn new() -> Self {
        Self::default()
    }
}

/// O cache do feed, um por processo (o hub é um só). `LazyLock` porque o cache precisa
/// nascer com o `Arc` dentro — um `static` não chama função não-const.
static CACHE: std::sync::LazyLock<Landing> = std::sync::LazyLock::new(Landing::new);

/// `GET /` — a landing page.
///
/// `no-cache`: a página muda do lado do servidor (rebranding, copy) e o navegador
/// não pode ficar servindo uma versão velha da memória — foi exatamente isso que
/// fez parecer que "nada mudou" depois do deploy.
pub async fn page() -> impl IntoResponse {
    (
        [
            (axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (axum::http::header::CACHE_CONTROL, "no-cache"),
        ],
        LANDING_HTML,
    )
}

/// `GET /favicon.ico` — o ícone da aba.
pub async fn favicon() -> impl IntoResponse {
    ([(axum::http::header::CONTENT_TYPE, "image/x-icon")], FAVICON)
}

/// `GET /logo-256.png` — a logo na marca e no alto da página.
pub async fn logo_256() -> impl IntoResponse {
    ([(axum::http::header::CONTENT_TYPE, "image/png")], LOGO_256)
}

/// `GET /logo-512.png` — a logo em dobro, para telas de densidade alta.
pub async fn logo_512() -> impl IntoResponse {
    ([(axum::http::header::CONTENT_TYPE, "image/png")], LOGO_512)
}

/// `GET /api/update` — o feed para a página, com cache.
pub async fn update_feed() -> axum::response::Response {
    feed(&CACHE).await
}

/// O feed para a página, com cache.
///
/// Se o GitHub não responder, devolve o último feed conhecido (a página continua
/// certa) e, se nunca houve nenhum, um `503` — a landing mostra "a versão mais
/// recente no GitHub" e o botão de download, que aponta para o GitHub, segue
/// funcionando.
async fn feed(this: &Landing) -> axum::response::Response {
    // Dentro do TTL: nem toca na rede.
    {
        let c = this.cache.lock().unwrap();
        if let (Some(feed), Some(at)) = (&c.feed, c.at) {
            if at.elapsed() < FEED_TTL {
                return json(feed.clone());
            }
        }
    }

    let fresh = match fetch_feed().await {
        Ok(f) => f,
        Err(e) => {
            tracing::debug!(error = %e, "landing: feed do GitHub indisponível");
            // O último conhecido ainda vale para a página.
            let c = this.cache.lock().unwrap();
            return match &c.feed {
                Some(f) => json(f.clone()),
                None => (
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    axum::Json(serde_json::json!({ "erro": e })),
                )
                    .into_response(),
            };
        }
    };
    {
        let mut c = this.cache.lock().unwrap();
        c.feed = Some(fresh.clone());
        c.at = Some(Instant::now());
    }
    json(fresh)
}

/// A resposta JSON (com o tipo que a página espera).
fn json(feed: UpdateFeed) -> axum::response::Response {
    axum::Json(feed).into_response()
}

/// Busca o feed no GitHub ( pedido único, com tempo limite ).
async fn fetch_feed() -> Result<UpdateFeed, String> {
    let http = reqwest::Client::builder().timeout(FEED_TIMEOUT).user_agent("KumaCord-hub").build().map_err(|e| e.to_string())?;
    let r = http.get(FEED_URL).send().await.map_err(|e| format!("o GitHub não respondeu ({e})"))?;
    if !r.status().is_success() {
        return Err(format!("o GitHub respondeu {}", r.status()));
    }
    // `text()` + parse: não depende da feature `json` do reqwest, e dá mensagem boa
    // quando o feed vem ilegível.
    let body = r.text().await.map_err(|e| format!("o feed não veio ({e})"))?;
    serde_json::from_str(&body).map_err(|e| format!("o feed veio ilegível ({e})"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn o_html_embutido_e_uma_pagina_de_verdade() {
        assert!(LANDING_HTML.starts_with("<!DOCTYPE html>"), "o HTML tem de ser uma página");
        assert!(LANDING_HTML.contains("Baixar para Windows"), "o botão principal existe");
        assert!(LANDING_HTML.contains("/api/update"), "a página pergunta a versão ao hub");
        assert!(LANDING_HTML.contains("KumaCord-setup.exe"), "o link é o de nome fixo");
        assert!(LANDING_HTML.contains("lang=\"pt-BR\""), "a página é em português");
        assert!(LANDING_HTML.contains("/logo-256.png"), "a logo pequena (marca) é servida pelo hub");
        assert!(LANDING_HTML.contains("/logo-512.png"), "a logo grande (herói) é servida pelo hub");
        assert!(LANDING_HTML.contains("logo-grande"), "a logo do herói não fica pequena/escondida");
        assert!(LANDING_HTML.contains("vigneta"), "a escuridão das bordas existe");
    }

    /// A página sai com `no-cache`: se o navegador guardasse a versão antiga, o
    /// rebranding parecia "não ter acontecido" — foi exatamente essa reclamação.
    #[tokio::test]
    async fn a_pagina_nao_fica_em_cache_do_navegador() {
        use axum::response::IntoResponse;
        let resp = page().await.into_response();
        let cache = resp
            .headers()
            .get(axum::http::header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(cache.contains("no-cache"), "GET / manda no-cache, achou: {cache:?}");
        let ct = resp
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(ct.contains("text/html"), "é HTML, achou: {ct:?}");
    }

    /// O rebranding: o código fechou e o servidor não roda mais no PC de ninguém.
    /// Estes testes existem para essas promessas não voltarem por descuido.
    #[test]
    fn a_pagina_nao_promete_o_que_nao_e() {
        assert!(!LANDING_HTML.contains("Ver no GitHub"), "o botão do GitHub saiu: o código é privado");
        assert!(!LANDING_HTML.contains("github.com/AkumaHalls/KumaCord/releases/latest\""), "nenhum link para a página do repo");
        assert!(!LANDING_HTML.contains("código aberto"), "a licença MIT não é mais o que é");
        assert!(!LANDING_HTML.contains("MIT"), "nada de MIT no rodapé");
        assert!(!LANDING_HTML.contains("roda no PC"), "o servidor não roda na máquina de ninguém");
    }

    /// O monitor de status: três estados de verdade, com texto em português.
    #[test]
    fn o_monitor_tem_os_tres_estados() {
        assert!(LANDING_HTML.contains("/api/health"), "o monitor lê o health de verdade");
        assert!(LANDING_HTML.contains("Servidor Online"), "o estado bom existe");
        assert!(LANDING_HTML.contains("Servidor Instável"), "o estado médio existe");
        assert!(LANDING_HTML.contains("Servidor Offline"), "o estado ruim existe");
        assert!(LANDING_HTML.contains("livekit_ok"), "instável é quando a voz cai");
    }

    #[test]
    fn a_logo_embutida_e_png_de_verdade() {
        // PNG começa com a assinatura 89 50 4E 47.
        assert!(LOGO_256.len() > 10_000, "a logo veio embutida");
        assert_eq!(&LOGO_256[..4], &[0x89, 0x50, 0x4E, 0x47], "o arquivo é um PNG de verdade");
        assert_eq!(&LOGO_512[..4], &[0x89, 0x50, 0x4E, 0x47], "o arquivo é um PNG de verdade");
    }

    #[test]
    fn o_favicon_embutido_tem_assinatura_de_ico() {
        // Um .ico começa com 00 00 01 00 (reservado + tipo 1 = ícone).
        assert!(FAVICON.len() > 100, "o ícone veio embutido");
        assert_eq!(&FAVICON[..4], &[0, 0, 1, 0], "o arquivo é um .ico de verdade");
    }

    #[test]
    fn o_feed_le_so_o_que_a_pagina_usa() {
        // Campos de verdade do latest.json, e um extra qualquer tem de ser ignorado
        // (a página é pública: nada de repassar o feed cru).
        let raw = "{\"version\":\"1.4.0\",\"notes\":\"## Adicionado\\n\\n- teste\",\"url\":\"https://exemplo/x.exe\",\"sha256\":\"abc\",\"signature\":\"assinado\"}";
        let f: UpdateFeed = serde_json::from_str(raw).unwrap();
        assert_eq!(f.version, "1.4.0");
        assert!(f.notes.as_deref().is_some_and(|n| n.contains("teste")));
    }

    #[test]
    fn o_feed_sem_notas_ainda_le() {
        let raw = "{\"version\":\"1.4.0\"}";
        let f: UpdateFeed = serde_json::from_str(raw).unwrap();
        assert_eq!(f.version, "1.4.0");
        assert!(f.notes.is_none(), "sem notas é o normal de uma release antiga");
    }
}
