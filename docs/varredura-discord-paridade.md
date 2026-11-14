# Varredura Discord × AkumaLabs — inventário completo de paridade

> Gerado em 27/10/2026 a partir da engenharia reversa do Discord instalado (bundles
> do client 1.0.9265, com o tool REA), do meowcord (fork AGPL de referência) e do
> nosso código. **Meta: clone do Discord, mas nosso.** A ÚNICA exceção sagrada:
> **compartilhamentoamento de tela (Go Live) — nosso é melhor e fica intacto.**
>
> Status: ✅ temos · 🔨 em andamento · ❌ falta. Esforço: P (pequeno) · M (médio) · G (grande).

## 1. Canais e salas

| Feature | Status | Esforço | Nota |
|---|---|---|---|
| Canais de texto e voz | ✅ | — | Completo (criar/renomear/reordenar/apagar/pública-privada) |
| **Categorias de canais** (os headers colapsáveis do Discord) | ✅ | — | Desde a v1.1; colapso consertado na v1.5 (id estável do egui), persistido em settings, e na v1.6 a **intercalação real**: categoria entre dois canais soltos, arrastando nos dois sentidos |
| **Tópico do canal** (descrição em cima) | ✅ | — | Editável no editor de canal; uma linha cinza no header do chat |
| Overwrites por canal (permissão diferente num canal só) | ❌ | G | Depende do S4 (cargos) estar fechado |
| Slowmode (intervalo entre mensagens) | ❌ | P | `rate_limit_per_user` no canal + enforcement no hub |
| Canal NSFW / bloqueio por idade | ❌ | P | Flag + confirmação |
| Eventos do canal (o "events" do Discord) | ❌ | G | Fora do escopo comunitário? Avaliar |

## 2. Mensagens

| Feature | Status | Esforço | Nota |
|---|---|---|---|
| Markdown (negrito/itálico/spoiler/citação/`code`) | parcial | P | Tem o básico; spoiler e bloco de citação faltam |
| **Editar mensagem** (com a tag "(editada)") | ❌ | P | PATCH da mensagem + evento + UI (um dos mais sentidos) |
| **Fixar mensagem** (pin) | ❌ | P | endpoint + painel "Fixadas" + 📌 no menu |
| Apagar a própria mensagem | ✅ | — | Canais e DMs |
| Apagar mensagem de outro (moderação) | ✅ | — | Moderador+ |
| Responder (quote) | ✅ | — | v0.8 |
| Menções @user e @everyone | ✅ | — | v0.8 |
| Reações | ✅ | — | v1.0 + picker novo (1.0.5) |
| Anexos (imagens/arquivos) | ✅ | — | Com compressão própria |
| **Emoji picker colorido + categorias + frecency** | ✅ | — | 1.0.5 (Twemoji, 271) |
| **Emojis do servidor** (custom, a galera usa o que você subiu) | ❌ | M | Upload no admin + substituição no texto |
| **GIFs no chat** (busca embutida) | ❌ | M | Proxy no hub (key opcional) + aba no picker |
| Stickers | ❌ | M | Packs do servidor |
| Threads (ramos de conversa) | ❌ | G | Avaliar se a comunidade pede |

## 3. Mensagens diretas

| Feature | Status | Esforço | Nota |
|---|---|---|---|
| DM 1:1 + chamada de voz/vídeo | ✅ | — | Completo |
| **Remover conversa da lista** ("Fechar conversa") | ❌ | P | Flag `hidden` no hub, volta ao receber mensagem |
| Bloquear pessoa | ✅ | — | v0.8 |
| DM em grupo (3+) | ❌ | G | Protocolo novo (multi-participante) |

## 4. Voz e vídeo

| Feature | Status | Esforço | Nota |
|---|---|---|---|
| Salas de voz, câmera, mute/deafen | ✅ | — | — |
| **Go Live (compartilhar tela)** | ✅ **SAGRADO** | — | **Nosso é melhor: NÃO MEXER** |
| Mover membro entre salas (drag/menu) | parcial | P | `move` existe no hub; falta o drag na UI |
| "Chamar" alguém na sala (ring) | ❌ | P | O call dos DMs tem; falta o ring no canal de voz |
| Entrar mudo por padrão (config) | ❌ | P | Uma setting + o `muteOnJoin` do fluxo de conexão |
| Salas de palco (stage) | ❌ | G | Avaliar demanda |
| Som de entrada/saída de sala | ✅ | — | Soundboard cobre parte; validar |

## 5. Perfil

| Feature | Status | Esforço | Nota |
|---|---|---|---|
| Nome de exibição, "Sobre mim", cor de destaque | ✅ | — | v0.6 |
| Status custom + presença (Ausente/Não perturbe/Invisível) | ✅ | — | — |
| **Badges de perfil (catálogo, tenure com brilho, editor admin)** | ✅ | — | 1.0.4/1.0.5 |
| **Avatar animado (GIF)** | ✅ | — | v1.6: `?anim=1` + `User::avatar_animated`; o hub guarda o container limpo (`strip`) além do still |
| **Banner de perfil** (imagem/GIF atrás do card) | ✅ | — | v1.6: `banner.rs` (tabela `banner_blobs`), caixa 600x240, mesma bifurcação de animação |
| Efeitos/nomeplate (os cosméticos animados) | ❌ | G | Pós-GIF, se rolar |

## 6. Cargos e permissões 🔨 EM ANDAMENTO (S4)

| Feature | Status | Esforço | Nota |
|---|---|---|---|
| Cargos fixos (Membro/Moderador/Admin/Dono) | ✅ | — | Matriz no proto |
| **Cargos custom com cor, ordem e permissões** | 🔨 | M | Protocolo pronto; hub + editor na fila AGORA |
| Nome colorido pelo cargo mais alto (chat/lista/card) | 🔨 | P | Junto do S4 |
| Pílulas de cargos no card do perfil | 🔨 | P | Junto do S4 |
| Overwrites por canal (herdado do S4) | ❌ | G | Depois do S4 base |

## 7. Moderação

| Feature | Status | Esforço | Nota |
|---|---|---|---|
| Silenciar servidor, desconectar, mover, encerrar Go Live | ✅ | — | — |
| **Banir / remover banimento (com motivo)** | ✅ | — | 1.0.5 |
| **Kick** (sai, mas pode voltar sozinho) | ❌ | P | Meio-caminho do ban: apaga sessões, sem a linha `banned` |
| **Timeout** (silêncio temporário, "MODERATE_MEMBER" deles) | ❌ | P | `until_ms` no user + enforcement |
| Registro de moderação | ✅ | — | Todas as ações acima |

## 8. Configurações do servidor

| Feature | Status | Esforço | Nota |
|---|---|---|---|
| Nome, dono, senha, convites (criar/revogar/expiração) | ✅ | — | — |
| **Ícone/verão do servidor** | ❌ | P | Uma imagem (mesma infra de avatar) |
| Region/qualidade de voz | n/a | — | Self-hosted decide sozinho |

## 9. Notificações

| Feature | Status | Esforço | Nota |
|---|---|---|---|
| Não lidas por canal, menções, alertas de som, taskbar | ✅ | — | — |
| Silenciar UM canal específico | ❌ | P | Uma lista de ids no settings |

## 10. Estética ("a cara do Discord") 🔨

| Feature | Status | Esforço | Nota |
|---|---|---|---|
| Paleta dark do Discord (blurple #5865F2, cinzas #313338/#1E1F22, texto #DBDEE1) | ✅ | — | Tokens em `theme.rs`: `ABAJUR` #5865F2, `PAREDE` #313338, `NOITE` #1E1F22, `RISCO` #3F4147, `MOVEL` #4E5058, `NEVOA` #949BA4, `TEXTO` #DBDEE1, `GIZ` #F2F3F5, `MENU` #111214 |
| Superfícies na ordem do Discord (o chat é o claro, os painéis em volta são o escuro) | ✅ | — | v1.6: `CHAT` #313338 no centro, `PAINEL` #2B2D31 na sidebar/membros/aviso, `NOITE` #1E1F22 no rail e no palco. Antes estava invertido (sidebar mais clara que o chat) |
| Raios de canto medidos no bundle | ✅ | — | v1.6: `ROW` 12 (`CHANNEL_ITEM_RADIUS`), `COMPOSER` 24 (`CHAT_INPUT_BORDER_RADIUS`), controle 8 (`CONTROL_CHECKBOX_BORDER_RADIUS`) |
| Hovers, pílulas, popups no estilo | parcial | P | Kit próprio já é perto; ajustar com a paleta |
| Tooltips com delay/posição (como os deles) | parcial | P | O kit de tooltip da fila |

---

## Ordem proposta (pelos seus pedidos + impacto)

1. 🔨 **S4 cargos com permissões** (em andamento — protocolo pronto)
2. ❌ **Editar mensagem** + **fixar** (P, dos mais sentidos no dia a dia)
3. ❌ **Remover conversa DM** (P — o gap que você sentiu hoje)
4. ❌ **Kick** + **timeout** (P, completam a moderação)
5. ❌ **Tema Discord** (P visual)
6. ❌ **Categorias de canais** (M, pedido explícito)
7. ✅ **GIF avatar + banner** (v1.6)
8. ❌ **Tópico de canal** + **slowmode** + **silenciar canal** (P cada)
9. ❌ **GIFs no chat** (M) → depois **emojis do servidor** (M) → **stickers** (M)
10. ❌ **Ícone do servidor** (P) → depois o resto da lista (G: threads, DM grupo, stage)

---

## 11. Sessão 2026-11-11 — correções em execução (evidência REA, client 1.0.9267)

Nova extração do Discord instalado via REA (`npx rea-agents analyze-javascript-application`
sobre `core.asar` + renderer do cache Chromium). **Corrige o status stale da linha 15 deste
doc: categorias EXISTEM no KumaCord desde a v1.1 (`ChannelCategory` em `vox-proto`, header
colapsável em `app/src/sidebar.rs`) — o problema é que o toggle está quebrado e a ordenação
é frágil.**

### 11.1 Modelo do Discord (evidência do renderer)

- `ChannelType`: `GUILD_TEXT=0`, `GUILD_VOICE=2`, `GUILD_CATEGORY=4`, `GUILD_ANNOUNCEMENT=5`,
  `GUILD_STAGE_VOICE=13`, `GUILD_FORUM=15`.
- Categoria **é** canal (`type=4`, `parent_id=null`); canais soltos também `parent_id=null`.
- **Uma única sequência de `position`** por guild cobrindo categorias e canais; o cliente
  reordena com **um** batch: `PATCH /guilds/{id}/channels` com `[{id, position, parent_id}]`.
  Dentro de uma categoria, os filhos ordenam por `position` relativo.
- Campos de canal: `name, topic, bitrate, user_limit, rate_limit_per_user, nsfw,
  permission_overwrites, parent_id, position, rtc_region`. Criação:
  `{type, name, permission_overwrites: [], parent_id, topic}`.
- **Colapso de categoria persiste** entre sessões (`CategoryCollapseStore`,
  `persistKey: "collapsedCategories"`; categoria de voz tem store próprio).
- **Detecção de jogo**: catálogo `/games/detectable` (`executables:[{os,name}]`, `patterns`) +
  **polling nativo de processos** (win32; sem título de janela). Match por basename do
  executável, depois últimos dois segmentos do path.
- Atividade: `ActivityType PLAYING=0`; `{application_id, name, details, state, type, timestamps,
  assets, party, secrets}`; reporte `POST /activities`.

### 11.2 Nossos bugs (mapeados no código)

| Onde | Problema |
|---|---|
| `hub/src/db.rs:615` `reorder_channels` | reescreve `position 0..n` da lista de canais |
| `hub/src/db.rs:931` `reorder_categories` | reescreve `position 0..n` da lista de categorias |
| `hub/src/admin.rs` | dois endpoints separados — **cada reorder destrói a intercalação** entre categoria e canal solto |
| `app/src/sidebar.rs:274` | toggle de colapso não funciona (bug em diagnóstico ativo) |
| `app/src/app.rs:509` | `category_open` volátil — some ao reiniciar |
| `vox-proto/src/lib.rs:837` | `Channel` sem `topic/bitrate/user_limit`; `ChannelKind` só Text/Voice; sem troca de tipo |

**Resolvidos (v1.5):** os dois endpoints separados deram lugar a um único
`POST /api/admin/structure` atômico (`hub/src/admin.rs`), que reescreve as posições densas numa
transação e anuncia `ChannelsChanged` + `CategoriesChanged` juntos. O toggle de colapso usa id
estável do egui (`Rect::from_min_size` + `Id::new`), o que era a causa do clique engolido. O
`category_open` passa a ser lido/escrito em settings. `Channel` ganhou `topic`, `bitrate` e
`user_limit`. Resta de fora, por decisção: overwrite por canal e slowmode.

**Resolvidos (v1.6) — a intercalação:** o servidor já escrevia UMA sequência de `position`
compartilhada por categorias e canais soltos (`hub/src/db.rs` `reorder_structure` numera `top`
`0..n` nas duas tabelas, e uma migração garante que os ids nunca colidem), mas o cliente jogava
essa ordem fora: `SidebarPlan` carregava duas listas — todos os canais soltos primeiro, todas as
categorias depois. Uma categoria nunca podia ficar entre dois canais, não importando o que o
servidor dissesse. Agora é uma lista só (`enum TopItem { Channel, Category }`), ordenada por
essa sequência, e o arrastar aceita categoria↔canal nos dois sentidos: soltar um canal SOLTO
sobre o header de uma categoria o torna solto naquele slot; soltar sobre um canal dentro da
categoria continua entrando nela (o gesto comum do Discord). De brinde, "Colapsar/expandir todas
as categorias" no menu do servidor (atalho do Discord, um clique em vez de N).

### 11.3 Decisões de arquitetura

1. **Manter** `ChannelCategory` como entidade própria (tem `color`, feature nossa).
2. **Uma única sequência `position`** compartilhada por categorias e canais soltos; canais
   dentro de categoria ordenam por `position` relativo entre si. Espelha o Discord sem migrar
   categoria para `kind=4`.
3. **Um endpoint de reorder atômico** com a ordem completa (categorias + canais soltos +
   ordem interna de cada categoria), aplicado numa transação. Substitui os dois endpoints no
   DnD da sidebar; os antigos ficam (compat com app velho).
4. **Feature flag em `Ready`** no padrão `ChatFeatures` para clientes antigos.
5. **Atividade efêmera** (não persistida, como `auto_away`), anunciada pelo `announce_presence`
   existente; some ao desconectar.

### 11.4 Checklist de execução

Backend (proto/hub/db)
- [x] `Activity {name, started_at_ms}` em `vox-proto` + campo em `PresenceChanged`
- [x] `PresenceEntry.activity`, `set_activity`, `event`/`is_default` atualizados (hub)
- [x] `PUT|DELETE /api/me/activity` com rate limit do padrão `RateWindow`
- [x] Colunas `topic`, `bitrate`, `user_limit` em `channels` + migração
- [x] `POST /api/admin/channels/{id}` (editar: nome, tópico, bitrate, user_limit)
- [x] `POST /api/admin/structure` (reorder unificado, transação única)
- [x] Feature flags em `Ready` (`ServerFeatures`: `structure_reorder`, `channel_extras`, `activity`)

App (cliente)
- [x] Detector de jogo Windows (Toolhelp32) + catálogo enxuto; poll ~5s, só anuncia mudança;
      toggle nas settings ("Detectar o jogo aberto")
- [x] `ProfileCmd::SetActivity` + `on_presence_changed` guarda atividade
- [x] Render "Jogando X": card de perfil, membros online, painel de voz, meu card
- [x] Colapso de categoria: consertado (id estável) + persistido em settings
- [x] v1.6: UMA lista de top-level (`TopItem`), então categoria fica ENTRE canais soltos; o
      arrastar aceita categoria↔canal (canal solto sobre header = solto naquele slot)
- [x] v1.6: "Colapsar/expandir todas as categorias" no menu do servidor
- [x] v1.6: superfícies na ordem do Discord (`CHAT` no centro, `PAINEL` em volta) + raios
      medidos no bundle (`ROW` 12, `COMPOSER` 24, controle 8)
- [x] Menu de contexto em canal/categoria: editar, excluir, convidar, marcar como lido,
      criar texto/voz. **Mover** é pelo arrastar na sidebar (não há item de menu), e pelo
      submenu "Mover para" para membros de voz.
- [x] Criação rápida na sidebar ("+" por categoria, "Criar categoria..." no menu do header)
- [x] Tópico do canal no header do chat; configs de voz (user_limit/bitrate) no editor

Verificação
- [x] `cargo test -p voxhub-app --bin KumaCord` = 340 passed; hub 304; proto 77
- [x] revisão `ecc:rust-reviewer` nas mudanças de hub/proto (4 achados fechados)

### 11.5 O ponteiro que virava cursor de texto (relevado: "o ponteiro muda pra como se fosse digitar texto")

O menu de presença do painel "você" funcionava, mas ao chegar com o mouse sobre o avatar para
abri-lo o ponteiro virava I-beam. Não era o menu: era o egui.

**Causa raiz:** `egui-0.36.2` liga `interaction.selectable_labels = true` por padrão
(`style.rs:1489`), e `Label` sem `.selectable(false)` cai nisso (`widgets/label.rs:308`). Um label
selecionável, em hover, pede `CursorIcon::Text` (`text_selection/label_text_selection.rs:541`) — e
`PlatformOutput::cursor_icon` é **sticky entre frames** (`data/output.rs:246`, "sticky between
frames"). Somando: qualquer label da sidebar (um nome de canal, por exemplo) deixava o I-beam
marcado, e o ponteiro continuava com ele sobre o widget seguinte que não define cursor — o avatar
do painel "você", que é um `allocate_exact_size` cru (`avatars.rs:678`).

**Correção:** uma linha em `app/src/theme.rs` — `s.interaction.selectable_labels = false`. Nenhum
lugar do app pedia `Label::selectable(true)` (há vários `.selectable(false)` explicitos só para
escapar do I-beam), então nada se perdeu. O chat, onde copiar um treco importa, pede a seleção de
volta, explícito: `chat_extras::text_part` e a closure do `rich_line` usam
`ui.add(Label::new(t).selectable(true))`. De brinde, `Hyperlink` (`widgets/hyperlink.rs:56`) também
lia o default: links deixam de ser selecionáveis e seguem apontando com a mão, como no Discord.

Teste que trava: `theme::tests::a_label_does_not_take_the_text_cursor_and_it_does_not_outlive_the_label`
(duas frames: label sob o ponteiro, depois um widget sem cursor — nenhuma das duas pode ser `Text`).

### 11.6 Auditoria de segurança antes do push (skill `security-audit`, modo guiado)

Dois agentes de revisão focados, um por superfície nova: `hub/src/banner.rs` + as 3 rotas em
`hub/src/api.rs:79-82` + as constantes/doc em `vox-proto`, e o caminho de decodificação de bytes
não-confiáveis. Somente leitura — nada foi construído, executado ou sondado.

**Achado confirmado e corrigido (medium, memória):**
`has_transparency` era chamada antes do resize, sobre a imagem decodificada inteira. Os limites
do decoder (`linkpreview/thumb.rs:46-53`: `max_alloc = 160 MB`, `MAX_PIXELS = 20_000_000`) cobrem
só o buffer do decoder — não uma segunda cópia dele. `to_rgba8()` sobre um `ImageRgba8` é um
`clone` do buffer inteiro: 80 MB de fonte + 80 MB de clone + o corpo do upload, por decode em voo,
com `MAX_CONCURRENT_DECODES = 2` → ~336 MB de pico, a cada janela de rate limit
(`BANNER_RATE_LIMIT = 5` / `BANNER_RATE_WINDOW_SECS = 600`), por qualquer membro autenticado.
Em instância pequena é OOM que derruba o hub inteiro.

Correção, duas partes, no último ponto de decisão confiável:
1. a decisão de transparência mudou para **depois** do resize, onde o buffer é no máximo
   600x240x4 = 576 KB (banner) / 256x256x4 = 256 KB (avatar);
2. as duas layouts que já *são* a resposta (`ImageRgba8`, `ImageLumaA8`) são lidas no lugar, sem
   cópia — era exatamente o caso que pagava.

Preço aceito e documentado no código: uma imagem cuja única transparência não sobreviva ao
Lanczos3 passa a ser JPEG. Diferença cosmética num upload já degenerado, não um controle
atravessado. `avatar.rs` tinha o padrão idêntico e recebeu o mesmo par de correções.

Teste que trava: `banner::tests::a_transparent_banner_bigger_than_the_box_still_lands_as_png`
(1600x640, metade transparente → ainda PNG, ainda transparente depois de caber na caixa).

**Fechado na revisão (era `needs_validation`, virou garantia):** o hub gravava a animação sem
decodificar o que guardava. `keep_animation` verificava os bytes **crus** e armazenava a saída
limpa do `strip` sem re-verificar; o irmão (`attachments/validate.rs:119,128,134`) faz as duas.
Reestruturado: `strip` primeiro, e `verify_animation` roda sobre o container **limpo** — que é o
que chega ao banco e a cada cliente que abre o perfil. De brinde, metade do trabalho de decode
(uma decodificação em vez de duas).

**Rejeitado com motivo (não era achado):** path/traversal e injeção por id/version
(`Path<UserId>` casa um segmento, todo SQL é parametrizado, `version` sai de
`hex(&Sha256::digest[..6])` = 12 hex, `mime` só de `Kind::mime()` ou constantes — sem SVG, sem
XSS armazenado); race no rate limit (expurgo→compara→push sob um único guarda de Mutex com
recuperação de poison, sem TOCTOU); membro comum chamando `remove_banner`
(`admin::moderation_target` exige `can_moderate()` **e** `outranks` estrito, rejeita auto-alvo, e
`me.role` é `effective_role`, que colapsa para Member com senha pendente → fail-secure); falta de
auth (`Authed` / `AuthUser` em todas as rotas); `?v=` gigante (`max_buf_size(64 KB)` no builder
HTTP/1); fila ilimitada no semáforo (tetada por `max_conns = 1024` / `max_conns_per_ip = 128` +
`request_timeout` de 120 s); vazamento 404 vs 403 (`get_banner` é 404 indistinguível); EXIF
sobrevivendo (o still é re-encode do zero, `banner.rs:440-455` prova que comentário e EXIF saem,
`INSERT OR REPLACE` reescreve todas as colunas então um still depois de uma animação zera
`anim`/`anim_mime`).

**Duas notas de doc, corrigidas:** `vox-proto` documentava `?size=` no banner (o campo não
existe — um banner tem um tamanho só) e `profile.rs:140` tinha um intra-doc link quebrado
(`crate::PresenceEntry` não existe no crate).

**Não coberto (ficou registrado, não é achado):** nada foi executado — sem build sob carga, sem
fuzz dos decodificadores, sem teste de concorrência. Fora do código: TLS, proxy reverso, sidecar
LiveKit, segredos em `config.toml`, permissões e backup do SQLite, e se `Limits` é sobrescrito
abaixo dos defaults. O lado do app (`app/**`) e o fan-out do broadcast não foram auditados.

**Verificação:** hub 164 + 12 + 6 + 7 + 3 + 6 + 11 + 13 + 11 + 4 testes, 0 falhas;
`vox-proto` 77 passed, `cargo doc` sem links quebrados.

