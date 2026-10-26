<p align="center">
  <img src="app/assets/logo/akumalabs-logo-256.png" width="112" alt="logo AkumaLabs">
</p>

<h1 align="center">KumaCord</h1>

<p align="center"><b>Voz, tela e chat para a comunidade AkumaLabs — servidor self-hosted em Docker.</b></p>

KumaCord é o app + servidor da **AkumaLabs**: canais de texto, salas de voz, webcam e compartilhamento de tela (Go Live), no estilo Discord, mas rodando no **seu próprio servidor** — nada passa por terceiros.

> KumaCord é um projeto **open source (MIT)** da AkumaLabs — veja `LICENSE` e
> `THIRD-PARTY-NOTICES.md`. O client é **travado para conectar apenas no
> servidor oficial**: não há campo de endereço nem opção de hospedar pelo app.

## O que tem aqui

| Pasta | O que é |
|---|---|
| `app/` | Client Windows/macOS (egui) — gera `AkumaLabs.exe` |
| `hub/` | Servidor (axum + SQLite + LiveKit supervisor) — gera `kumacord-server` |
| `crates/` | Protocolo, núcleo do client, captura, logging |
| `Dockerfile` + `docker-compose.yml` | Deploy do servidor em container (Portainer-friendly) |

## Servidor (Docker / Portainer)

Requisitos: Docker (BuildKit ativo) e as portas livres — **TCP 7890, 7880, 7881 + UDP 7882**.

```bash
git clone https://github.com/AkumaHalls/KumaCord.git
cd KumaCord
mkdir -p data          # o container roda como o uid de KUMACORD_UID (padrão 1000):
                         # a pasta precisa ser gravável por ele
docker compose up -d --build   # se seu usuário não for uid 1000, crie um .env com
                               # KUMACORD_UID=1001 (o seu `id -u`) e KUMACORD_GID, ou exporte-os
```

Na primeira subida o servidor **gera uma senha e imprime no console** (`docker logs kumacord`). Veja-a, depois ajuste o que quiser em `data/config.toml`:

```toml
server_name = "AkumaLabs"
server_password = "uma senha forte"   # a senha gerada já serve; vazio = servidor aberto (opt-in)
public_host = "SEU.IP.PUBLICO"         # como o client alcança o LiveKit
owner = "Akuma"                        # nickname fixo como Dono (recomendado em VPS)

[livekit]
use_external_ip = true                  # atrás de NAT (VPS em nuvem), deixe true
```

Depois: `docker compose restart`. Os dados (config, `voxhub.db`, `livekit.yaml`, imagens) ficam em `./data` — **nunca compartilhe essa pasta** (e o `.gitignore` já a mantém fora do git).

No **Portainer**: *Stacks → Add stack → Repository* apontando para este repositório (compose já incluso), ou use *Images/Containers* com a imagem `kumacord:latest` construída acima.

Abrindo portas no provedor da VPS (security group / firewall): TCP 7890, 7880, 7881 e UDP 7882.

### Dono (Owner) do servidor

Em VPS não existe "PC do servidor" para reivindicar a propriedade no primeiro login, então:

- **Recomendado:** fixe `owner = "seu-nick"` no `config.toml` — esse nome é o Dono para sempre.
- Alternativa: sem dono definido, o console do container imprime um código único (`docker logs kumacord`); entre com qualquer nome e use o código em *Configurações* para virar Dono.

## Client (app AkumaLabs)

**Baixe em [Releases](https://github.com/AkumaHalls/KumaCord/releases/latest):**

| Arquivo | Auto-update |
|---|---|
| `AkumaLabs-<versão>-setup.exe` | ✅ **recomendado** — instala sem administrador e se atualiza sozinho |
| `AkumaLabs-<versão>-Windows.zip` | ❌ portátil (extraia e rode `AkumaLabs.exe`) |

O app verifica atualizações na inicialização (e em **Configurações → Sobre → Verificar atualizações**), avisa em qualquer tela quando há versão nova e atualiza sozinho: baixa o instalador, valida **SHA-256 + assinatura minisign** (a chave pública vai compilada no app; a privada é um secret do CI — assinatura inválida não instala) e reinicia na versão nova.

O client conecta **apenas** no servidor oficial — o endereço é uma constante:

```rust
// app/src/brand.rs (aqui no README vai um marcador; o IP real vive só no código)
pub const HOME_URL: &str = "http://SEU.IP.PUBLICO:7890";
```

Trocar de servidor/IP? Mude **somente** essa constante em `app/src/brand.rs` e recompile — é a única referência ao endereço em todo o repo (este README usa marcador de posição).

Build a partir do fonte (Windows): Rust stable (MSVC) + Visual Studio Build Tools com C++.

```powershell
cargo build --release -p voxhub-app
# target\release\AkumaLabs.exe
```

Build (macOS, Apple Silicon): `cargo build --release -p voxhub-app` → `target/release/AkumaLabs` (veja `tools/package-mac.sh`).

Entrar: **nome + senha do servidor**, ou colar um **convite** `vox1-...` (o convite carrega só o token — o endereço continua sendo o fixo; convite de outro servidor simplesmente não autentica). Admins criam convites dentro do app (*Convidar*), com expiração, limite de usos e revogação.

## Segurança (leia antes de expor na internet)

Herdado do projeto base (veja `SECURITY.md`):

- **Sem TLS ainda** entre app e servidor (voz e vídeo são SRTP/DTLS, criptografados). Em servidor público: use uma `server_password` forte e divulgue por canal seguro; uma VPN (Tailscale/ZeroTier) é ainda mais seguro. *(Issue conhecida de prioridade alta: um proxy TLS na frente da VPS + `HOME_URL` em `https://` resolve — o client já fala https/wss; tratamento completo planejado.)*
- O IP do servidor é conhecido por quem entra — convites escondem no texto, mas não são criptografia.
- Logs do container: `docker logs kumacord` (contêm endereços e nomes).
- O primeiro boot **gera** a `server_password` (impressa no console) e os arquivos de segredo em `./data` nascem legíveis só pelo dono do serviço — servidor aberto é opt-in explícito (`server_password = ""`).

Antes de publicar mudanças: `git status --porcelain` limpo, `git ls-files | grep -i logo` mostrando só `akumalabs-*`, e um build a partir de um clone fresco.

## Publicando uma versão nova (o CI faz o trabalho)

O fluxo é o mesmo do KumaDrive: o GitHub Actions compila, empacota, **assina** e publica.

```
# 1. Escreva a seção "## [x.y.z]" no CHANGELOG.md
# 2. Atualize a versão em Cargo.toml ([workspace.package]) e faça o commit
git commit -am "chore: bump para x.y.z"
git tag -a vx.y.z -m "AkumaLabs vx.y.z"
git push origin main && git push origin vx.y.z
# 3. O workflow .github/workflows/release.yml cria a release com
#    setup.exe + .sig + zip + latest.json (o feed do auto-update)
```

A chave privada de assinatura vive **somente** como secret do GitHub Actions
(`AKUMALABS_SIGNING_KEY`) — nunca no computador, nunca no repositório. Só a chave
pública fica no código (`app/assets/update-key.pub`), que é pública por natureza.

## Desenvolvimento

```powershell
cargo test --workspace        # testes unitários/de integração (offline)
cargo build --release -p voxhub-app -p voxhub-server
```

Nota: parte da suíte de autotests de UI do upstream pressupõe a tela inicial original (aba "Criar servidor" + campo de endereço), removidas nesta distribuição; os testes unitários do hub e do protocolo seguem válidos.

## Licença

Código: MIT (veja `LICENSE` — mantém os créditos do projeto original). A logo AkumaLabs (`app/assets/logo/`) é arte da marca própria. Bibliotecas, fontes e ícones de terceiros: `THIRD-PARTY-NOTICES.md`.
