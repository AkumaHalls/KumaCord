# Changelog

## [1.0.3] — 2026-10-26

### Adicionado

- **Instalador e atualização automática** (o modelo do KumaDrive, nativo em Rust):
  - Novo instalador NSIS per-user (`AkumaLabs-<versão>-setup.exe`) — instala sem
    administrador, atalho no Menu Iniciar e desinstalador no "Adicionar ou remover programas".
  - O app verifica atualizações na inicialização (silencioso) e em
    **Configurações → Sobre → Verificar atualizações**.
  - Aviso de nova versão em **qualquer tela** (banner persistente, não é toast) e
    tela de atualização com par de versões, novidades da release e progresso do download.
  - Segurança do fluxo: o instalador baixado é validado por **SHA-256 + assinatura
    minisign** com a chave pública compilada no app (a privada vive só como secret do
    GitHub Actions) — assinatura inválida não instala, ponto.
  - GitHub Actions compila, empacota, assina e publica a release a cada tag `v*`
    (workflow `release.yml`), anexando `latest.json` — o feed que o app consulta.
- O zip portátil continua disponível (sem atualização automática).

### Corrigido

- `AkumaLabs.icns` (ícone macOS) regenerado com os campos de tamanho no big-endian
  exigido pelo formato Apple — o anterior era inválido e não carregava.
- Novo teste estrutural valida o `.icns` e o `.ico` a cada `cargo test` (os PNGs
  já eram validados).

## [1.0.2] — 2026-10-25

    /// KumaCord: primeira distribuição AkumaLabs

- Client rebrandado para **AkumaLabs**: logo própria (PNGs/ICO/ICNS), wordmark,
  pastas de dados (`%APPDATA%\AkumaLabs`), tray, ícones do Windows.
- Client **travado no servidor oficial**: sem campo de endereço e sem aba de
  hospedar; convites `vox1-...` carregam só o token (o endereço é sempre o fixo
  compilado em `app/src/brand.rs`).
- Servidor renomeado para `kumacord-server`; deploy em **Docker/Portainer**
  (Dockerfile multi-stage + compose), LiveKit 1.13.7 pinado com SHA-256 por arquitetura.
- Endurecimento da auditoria pré-release: `server_password` **gerada no primeiro
  boot** (vazio = opt-in explícito de servidor aberto, com aviso), segredos em
  `./data` criados `0600` + umask `0027` no container, usuário do container fixado
  (`KUMACORD_UID`), diagnóstico do servidor sem referências a scripts removidos,
  SECURITY.md/README reescritos para a realidade da distribuição.
- Arquivos de logo do concord (não cobertos pela licença MIT) removidos;
  LICENSE mantém os créditos do upstream.

## [1.0.1] — upstream (concord, de Cezar Callado)

- Rebrand de voxhub para concord, ícones, assinatura e migração de pastas.
- Convites, canais privados, senhas pessoais (item 12), proteção anti-downgrade.

## [1.0.0] — upstream (voxhub)

- Lançamento inicial: voz, tela (Go Live), chat, câmeras, convites, cargos.
