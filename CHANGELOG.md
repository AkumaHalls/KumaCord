# Changelog

## [1.0.8] — 2026-10-27

### Mudado

- **Os menus ficaram do Discord de verdade**: fundo quase-preto `#111214` (antes um bloco
  cinza claro que parecia "quadrado" em cima do dark novo), itens com hover `#4E5058`
  arredondados como os deles, e largura fina (~220 px, a medida do menu do Discord).

- **O botão direito numa pessoa agora é o do Discord**:
  - **Perfil** (era "Ver perfil"), **Mencionar** (novo — joga o @nome no chat),
    **Mensagem** (era "Enviar mensagem"), Ligar, Copiar nome de usuário.
  - **Banir...** agora também vive aqui (com o diálogo do motivo), para quem pode banir —
    antes só existia no painel de membros.
  - **Copiar ID do usuário** no rodapé, como o modo desenvolvedor deles.
  - A ordem das seções segue a deles: perfil → voz → transmissão → câmera → moderação →
    bloqueio → ID.

## [1.0.7] — 2026-10-27

### Mudado

- **A cara do Discord**: a paleta inteira do app agora é a do client oficial — o chat
  `#313338`, a lateral `#2B2D31`, o rail `#1E1F22`, os textos `#DBDEE1`/`#949BA4`, os
  botões `#4E5058` e o **blurple `#5865F2`** de marca nos lugares de ação (falar,
  botão primário, menções), com os passos oficiais de hover `#4752C4` e pressed
  `#3C45A5`. O navy/dourado antigo fica no histórico do git.

### Corrigido

- **O editor de permissões**: cada permissão é uma linha inteira (nome e explicação à
  esquerda, caixa de marcar à direita, como no Discord), com widgets nativos — nada de
  desenho manual que quebrava o visual.
- **O editor de "Cargos..." da pessoa** ganhou o mesmo tratamento: ponto da cor, nome e
  a caixa de marcar, nativos e legíveis.
- **A tela de atualização** está mais larga (560 px) e arejada, e o botão de atualizar
  agora é o botão primário blurple com texto branco, como o resto do tema.

## [1.0.6] — 2026-10-27

### Adicionado

- **Cargos custom, estilo Discord**: a aba **"Cargos"** das Configurações do servidor cria,
  edita e apaga cargos com **cor, ordem e 21 permissões** (ver canais, enviar, anexos,
  reações, voz completa, apagar mensagens, moderar a voz, banir, gerenciar canais/convites/
  cargos/badges, registro, senhas e Administrador). Eles **somam** poder em cima dos cargos
  fixos (Membro/Moderador/Admin/Dono).
- **Dar cargos a alguém**: no menu "⋯" da pessoa, em "Membros e cargos" → "Cargos..." —
  o cargo mais alto pinta o nome dela na lista (e um pontinho da cor aparece na linha).
- Tudo sincronizado ao vivo (`CustomRolesChanged` + `UserUpdated`), e cada criação, edição,
  exclusão e atribuição entra no **registro de moderação**.
- **Ninguém concede o que não tem**: um cargo só carrega as permissões que quem o criou
  já tinha (a regra do Discord), e as permissões declaradas de badges e ban agora são
  honradas de verdade na porta do servidor — não só na interface.

### Corrigido

- Segurança (auditoria da superfície nova): os endpoints admin de badges e ban validam a
  permissão específica, e a criação/edição de cargos é limitada aos bits do criador.

## [1.0.5] — 2026-10-27

### Adicionado

- **Sistema de banimento, estilo Discord**:
  - "Banir..." no menu ⋯ de cada membro (Dono/Admin, respeitando a hierarquia: ninguém
    se bane, ninguém bane o Dono, Admin só quem está abaixo). Um diálogo pede o motivo.
  - A pessoa banida **não entra mais**: o login recusa com o motivo ("Você foi banido
    deste servidor..."), e senha, convite ou dispositivo lembrado não contornam — a
    banimento apaga sessões e tokens na hora.
  - Se estiver online, recebe o aviso ao vivo e sai (da voz também) no mesmo instante.
  - "Remover banimento" na linha da pessoa (pill vermelha "Banido" com o motivo), e
    tudo que acontece fica no registro de moderação.

### Corrigido

- **O botão de badges que sumia** no painel de membros: o editor de badges mudou para o
  menu ⋯ ("Badges..."), onde nenhum layout consegue escondê-lo, e a fileira de badges da
  linha agora mostra no máximo três (o card de perfil mostra todas).
- Compatibilidade: o campo `flags` do protocolo é aditivo — um membro sem badges
  serializa exatamente como antes, e um cliente antigo ignora o campo novo.

## [1.0.4] — 2026-10-27

### Adicionado

- **Badges de perfil, estilo Discord**:
  - O protocolo ganha `flags` por usuário: um bitfield que o servidor preenche com as
    regras automáticas (badge de **Staff** para o dono, a escada de **tempo de casa** —
    1, 3, 6, 12, 24, 36, 60 e 72 meses — e **Primeiros Membros** para as 100 primeiras
    contas) somadas às **badges que o dono concede à mão** para quem ele quiser.
  - Editor de badges no painel "Membros e cargos": botão de medalha em cada membro,
    com o catálogo completo (Staff, Parceiro, Moderador, HypeSquad e as três casas,
    Caça-Bugs, Dev verificado/ativo, Primeiros Membros, Missão e cada nível de tempo
    de casa). Toda concessão vira uma linha no registro de moderação e aparece para
    todo mundo na hora.
  - O cartão de perfil mostra a fileira de badges com nome e história em cada uma
    (o padrão do Discord: até 6 na fileira e um "+N"), e o "Membro desde" ganha o
    ícone do tempo de casa — **com brilho dourado quando o nível acabou de subir**.
  - A arte é o conjunto clássico de badges (veja `THIRD-PARTY-NOTICES.md`).

### Corrigido

- O servidor agora instala o provedor criptográfico do rustls no arranque de verdade:
  antes, uma requisição de preview de link podia derrubar o processo quando o app
  (que usa o mesmo `reqwest`) não estava no grafo de compilação.

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
