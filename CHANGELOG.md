# Changelog

## [1.3.1] — 2026-11-08

### Corrigido (atualização automática)

A 1.3.0 publicou uma atualização que **não dava para instalar**: a setinha do canto
não aparecia e o botão do "Sobre" não fazia nada. Quem baixou ficou preso na versão
antiga sem nenhum aviso. Havia quatro caminhos para instalar e os quatro quebravam na
mesma fase:

- **A seta não aparecia**: o aviso usava `Order::Tooltip`, a única área do app com essa
  ordem — agora é `Order::Foreground`, como o painel de conexão, os avisos e as imagens.
- **O botão "Ver a nova versão" do Sobre não fazia nada**: ele só agia quando a versão
  tinha sido *detectada*. Como o app baixa sozinho em segundo plano, a fase já tinha
  mudado e o clique caía no vazio. Agora, com o instalador pronto, o botão instala.
- **A janela de atualização não tinha ação quando o instalador já estava pronto**:
  mostrava o título e as notas, mas nenhum botão. Agora oferece "Instalar agora".
- **Nada mostrava o download**: o instalador tem 72 MB e baixar leva minutos, mas o
  "Sobre" ficava calado sobre isso — parecia que o botão tinha morrido. Agora há barra
  de progresso com MB e porcentagem.

### Melhorado

- **A setinha chama atenção**: por 25 segundos ela balança devagar e pulsa um anel em
  volta, para o olho achar; depois fica parada e não gasta mais CPU.
- **A verificação automática passou de 2 h para 30 min**: quando uma versão sai com
  defeito, ela precisa circular rápido.

## [1.3.0] — 2026-11-08

### Adicionado

- **Arrastar canais e categorias na barra lateral**, como no Discord: pegue um canal (de texto
  ou de voz) e solte onde quiser — dentro do mesmo grupo para reordenar, ou em outro grupo
  (ou fora de todos) para mudar de lugar. Uma linha fina mostra onde vai cair, e o nome do
  canal vem junto com o ponteiro. Categorias também se arrastam, pelo cabeçalho, para trocar
  a ordem dos grupos. Quem não pode reordenar (qualquer membro) não pega nada: as linhas
  continuam normais.
  - Servidor: `POST /api/admin/categories/reorder` (nova; exige a lista de todas as
    categorias, cada id uma vez, em uma transação). O reordenamento de canais usa o
    `/api/admin/channels/reorder` que já existia.
  - Atenção: **o arraste de categorias precisa do hub atualizado**. Com um hub antigo o app
    avisa que não deu; o arraste de canais funciona com qualquer hub desde a v1.

## [1.2.2] — 2026-11-08

### Corrigido

- **Cargos criados no servidor não apareciam no perfil**: o cartão de perfil mostrava só o
  cargo do sistema (Dono/Admin/Moderador/Membro) e ignorava `User::role_ids`. Um membro com
  cargos customizados aparecia igual a um sem nenhum. O perfil agora desenha os cargos do
  servidor como pills coloridas, cada um com a cor que o cargo tem.
- **Ping da chamada era aproximação**: o card de conexão media o tempo de resposta de uma
  conexão TCP com o hub, que não é o caminho da voz. O RTT de verdade (do par ICE
  selecionado pelo LiveKit) agora é lido a cada 2 s e alimenta o "Ping médio", o "Último
  ping" e o gráfico. A medição TCP continua como reserva, e só roda quando o RTT de mídia
  não vem.

### Corrigido (desempenho)

- **A "IA máxima" caía sozinha por um falso positivo**: o primeiro quadro depois de carregar
  o DeepFilterNet é lento (aquecimento do modelo) e entrava na média de tempo por quadro.
  Com a carência de só 50 quadros, a média ainda estava envenenada quando o orçamento de
  5 ms era verificado, então a supressão mais pesada caía mesmo em máquinas que davam
  conta. Os 20 primeiros quadros de cada carregamento (e de cada reset) agora ficam fora
  da média, e a queda passou a registrar no log *quanto* o quadro levou — assim dá para
  distinguir defeito de máquina lenta.

## [1.2.1] — 2026-11-06

### Corrigido

- **`InviteId` não lia coluna SQLite `INTEGER`**: todo upload de convite falhava com
  `InvalidColumnType`. A coluna é `INTEGER PRIMARY KEY` (rowid) enquanto o id viaja como
  sua string decimal, então o `FromSql` passou a aceitar TEXT **ou** INTEGER.
- **`SoundId` com o mesmo problema**: o soundboard inteiro estava quebrado — upload de
  som falhava pelo mesmo motivo. Mesma correção no `FromSql`.

### Adicionado

- **Atualização automática, como no Discord**: o app verifica o feed sozinho a cada 2 h
  e, quando acha versão nova, baixa e verifica em segundo plano. A seta de atualização
  aparece no canto superior direito só quando o instalador já está conferido (hash +
  assinatura minisign); clicar instala. Antes a checagem só acontecia no boot e no
  clique manual, e a instalação disparava sozinha sem esperar o usuário.
- **Painel de conexão de voz**: clique no sinal ao lado de "Voz conectada" para ver a
  região, ping médio, último ping, taxa de perda de pacotes do uplink (medida a cada 2 s
  pelo LiveKit) e um gráfico de latência que rola.

### Corrigido (testes)

- Os quatro testes `ai_max_*` e `switching_across_all_three_levels_is_glitch_free`
  falhavam só em execução paralela: mediam o orçamento de CPU do worker, e a disputa
  por CPU com os outros testes fazia a cadeia cair legitimamente para "IA leve". Os
  testes agora usam `DfnShared::set_budget`, um hook que espelha o `set_slowdown` já
  existente, então julgam funcionalidade em vez de velocidade da máquina.

## [1.2.0] — 2026-11-04

### Mudado

- **O app agora se chama KumaCord**: título da janela, ícone na bandeja, atalhos, instalador
  (`KumaCord-<versão>-setup.exe` → `KumaCord.exe`) e todas as telas. O **servidor continua
  AkumaLabs** — é a comunidade que ele serve, e o cartão `OFICIAL` do launcher segue com esse nome.
  Quem já tem a versão antiga é atualizado no lugar: o instalador remove a instalação anterior
  (`AkumaLabs.exe`, atalhos e a entrada em "Aplicativos") sem tocar nos seus dados.
- **A cinemática inteira na abertura**: antes eram só 20 segundos do filme; agora a abertura toca
  **o vídeo completo (~3min50) em loop**, com a trilha inteira — e as duas param quando você clica
  em "Entrar no servidor". O filme e a música saem de dentro do exe e passam a viajar na pasta
  `intro\` ao lado do executável (instalador e `.zip` portátil já os levam), então o app em si
  continua leve para atualizar. Sem a pasta, o app abre normal, só sem a abertura.
- **O cartão do servidor não mostra mais o endereço/IP**: só o nome e o selo `OFICIAL`. O endereço
  continua fixo no código e é escolhido pelo launcher.

### Corrigido

- **A música da abertura parava nos primeiros segundos**: o vigia do dispositivo de som tratava
  uma faixa em loop (que "nunca termina", por definição) como aparelho travado e cortava o áudio
  aos 10 s. Agora ele mede o consumo real de amostras do mixer — um aparelho saudável toca a
  trilha inteira; só um aparelho que parou de consumir é fechado.

### Técnico

- Abertura entregue como frames WebP (960 px, 15 fps, filme inteiro) em `intro.pack`, lidos **sob
  demanda do disco** (o cabeçalho é lido uma vez; cada quadro é lido e decodificado na hora, numa
  thread própria), com duas texturas vivas no máximo. Nenhum decoder de vídeo novo no binário.
- `tools/intro/build.ps1` + `tools/intro/pack.py` regeneram os assets a partir do vídeo original
  (ffmpeg + Python), e `installer/release-assets.ps1` valida que eles existem antes de empacotar.
- Autotestes continuam recebendo um still do primeiro quadro (sem loop nem som).

## [1.1.9] — 2026-11-03

### Mudado

- **Launcher novo (estilo League of Legends)**: a tela de entrada virou uma abertura de verdade —
  a cinemática da comunidade roda em tela cheia ao fundo, com a marca no topo, o letreiro grande
  à esquerda e um cartão translúcido à direita. A música da abertura toca em loop (botão de som no
  canto) e **tudo para quando você clica em "Entrar no servidor"**. "Pular" deixa a abertura de
  lado na hora.
- **Um só servidor, já escolhido**: o launcher mostra o cartão do servidor oficial da AkumaLabs
  (`OFICIAL`), já preenchido — não existe mais lista de servidores nem campo de endereço na
  entrada. O campo aceita apenas um convite `vox1-...`.
- **Senha do servidor fora do fluxo**: o campo só aparece quando o servidor pede (senha errada,
  entrada salva sem senha ou convite fechado). Com ele vem o olho para mostrar a senha enquanto
  digita. Quem entra pelo convite nunca vê o campo.

### Adicionado

- Áudio da abertura com controle próprio: o alto-falante no canto silencia/liga e a escolha fica
  salva nas configurações (respeitando o volume local).
- A cinemática recomeça ao sair do servidor ("Trocar de servidor" ou logout): a abertura volta a
  tocar como na primeira vez.

### Técnico

- Vídeo entregue como 400 frames WebP (960×540, 20 fps, ~20 s) num pacote embutido no binário —
  sem decoder de vídeo novo; decode em thread própria, uma textura viva por vez.
- Música decodificada (symphonia) e tocada em loop pelo mixer, com saturação e parada limpa.
- Autotestes recebem um still do primeiro quadro (sem loop nem som) e o gate volta a aceitar os
  hubs de dev via modo de teste.
- Delta de auditoria de segurança: `SECURITY-AUDIT-2026-11-03.md`.

## [1.0.10] — 2026-10-28

### Mudado

- **Categorias de canais (Discord-style)**: canais de texto e voz agora podem ser agrupados
  em categorias colapsáveis na barra lateral, cada uma com cor própria. O dono e admins
  criam categorias em *Configurações do servidor → Cargos*; as categorias aparecem para
  todos.
- **GIF animado como avatar de perfil**: o hub armazena GIFs como estão (sem
  re-encodificar), então GIFs animados funcionam como avatar de perfil e banner de perfil.
- **Banner de perfil**: cada usuário pode ter um banner (imagem ou GIF) no topo do perfil,
  visível no popout do perfil. O dono/admin define em *Configurações do servidor →
  Membros e cargos*.

### Adicionado

- **Botão direito em uma pessoa**: agora inclui **Mencionar** (joga @nome no chat),
  **Copiar ID** (útil para devs e moderação) e **Banir...** (abre diálogo com motivo;
  banido não entra mais nem com senha/convite).
- **Pou p de atualização redesenhado**: espelho do "What's New" do Discord — modal de
  480px, margens de 24px, versão em pílula blurple, notas em card escuro com scroll,
  rodapé com botão blurple primário e fantasma discreto. O aviso-banner também trocou:
  cartão escuro `#111214` sem faixa, sem brilho.
- **Menu de contexto escuro**: fundo quase-preto `#111214`, itens com hover `#4E5058`
  arredondados como os do Discord, largura fina (~220 px).

### Corrigido

- O botão de atualização não aparecia em telas pequenas.
- A assinatura do instalador NSIS não era reconhecida no Windows 11 (caminho absoluto).

## [1.0.9] — 2026-10-27

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
