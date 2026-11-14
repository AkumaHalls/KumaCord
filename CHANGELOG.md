# Changelog

## [1.6.1] — 2026-11-14

Re-lançamento da 1.6.0: a release `v1.6.0` foi publicada a partir do commit
`d49e461`, antes dos últimos fixes. Esta tag (`v1.6.1`) empacota o conteúdo
completo da 1.6.0 mais:

- **Reações coloridas** — a pill "👍 3" saía cinza (glifo da fonte); agora o
  emoji vem da arte PNG do catálogo, com o glifo só como fallback.
- **Picker de emoji não fecha ao trocar de categoria** — o id do grid era
  dinâmico e cada troca de aba perdia a âncora do popup; agora é fixo.

## [1.6.0] — 2026-11-12

### Corrigido (estrutura de canais e categorias)

**Categoria no meio dos canais finalmente funciona.** O servidor já escrevia uma
única sequência de `position` para categorias e canais soltos, mas o cliente
jogava essa ordem fora: a sidebar desenhava todos os canais soltos primeiro e
todas as categorias depois. Arrastar uma categoria para o meio da lista não
grudava em lugar nenhum — era a reclamação. Agora existe uma lista só
(`TopItem`), ordenada pela mesma sequência do servidor, e o arrastar aceita os
dois sentidos:

- soltar um **canal solto sobre o header** de uma categoria → ele fica solto
  naquele slot (antes não havia como tirar um canal de categoria pela sidebar);
- soltar sobre um **canal dentro** da categoria → continua entrando nela, que é
  o gesto de sempre.

Tudo continua indo num lote único (`POST /api/admin/structure`), então as
categorias não pulam mais quando você mexe só nos canais.

### Adicionado

- **"Colapsar todas as categorias" / "Expandir todas as categorias"** no menu do
  servidor (o atalho do Discord): um clique em vez de um por categoria.

### Corrigido (estética)

- **As superfícies estavam invertidas.** A sidebar e a lista de membros eram mais
  claras que o chat; no Discord é o contrário — o chat é o cinza claro e os
  painéis em volta são os escuros. `CHAT` (#313338) no centro, `PAINEL` (#2B2D31)
  em volta, `NOITE` (#1E1F22) no rail.
- **Raios de canto medidos no bundle do cliente:** linha de canal e header de
  categoria 12 (`CHANNEL_ITEM_RADIUS`), caixa de digitar 24
  (`CHAT_INPUT_BORDER_RADIUS`), controles (checkbox/select/botão) 8
  (`CONTROL_CHECKBOX_BORDER_RADIUS`) — antes o checkbox herdava o raio da linha
  e virava círculo.
- **Reações coloridas:** a pill "👍 3" desenhava o emoji com o glifo
  monocromático da fonte (cinza). Agora o emoji da reação sai da arte PNG do
  catálogo, com o glifo só como fallback para emoji fora do catálogo.
- **Picker de emoji não fecha mais ao trocar de categoria:** o id do grid
  carregava a categoria e a busca, então cada troca de aba criava um id novo e
  o popup perdia a âncora. O id agora é fixo (`emoji_grid`); só o conteúdo
  muda.

### Adicionado (perfil)

- **Avatar animado (GIF) e banner de perfil.** O hub guarda o GIF limpo (sem
  metadados, EXEX incluído) além do still, e serve a animação em `?anim=1` quando
  o `User::avatar_animated`/`banner_animated` diz que existe uma — um avatar
  estático não paga nada por isso. O banner é novo de ponta a ponta (`banner.rs`,
  tabela `banner_blobs`, caixa 600x240), com remoção própria e pelo moderador.
- **Catálogo de emojis ampliado** para a paridade com o Discord, com a arte em
  PNG (Twemoji) em vez do glifo monocromático da fonte — o emote colorido chega
  colorido na sala, e cinza nunca mais.
- **Tile do jogo na atividade** ("Jogando X"), como no Discord: quadradinho
  escuro com raio concêntrico à linha, mostrando a arte oficial do jogo na
  Steam (appid mapeado em `game_icons.rs`, capsule 184x69 baixada uma vez da CDN
  pública e virada em textura numa thread, sem travar a UI) ou um emoji temático
  do próprio catálogo para jogos fora da Steam; jogo desconhecido ganha o
  gamepad. Aparece na lista de membros, na lista online e no painel "você", e
  no cartão de perfil ao lado de "Jogando X".

### Corrigido (ponteiro do mouse)

- **O ponteiro não vira mais cursor de texto.** O egui liga por padrão
  `selectable_labels`, o que faz qualquer rótulo pedir o cursor de texto ao ser
  tocado — e esse cursor gruda entre quadros, aparecendo sobre o avatar do painel
  "você" quando você ia mudar a presença. Agora os rótulos do app não são
  selecionáveis; o chat, onde copiar um treço importa, pede a seleção de volta.

### Corrigido (segurança, antes do push)

- **O hub não copia mais o buffer decodificado inteiro.** A decisão "tem
  transparência?" era tomada antes do resize, sobre a imagem decodificada de até
  20 MP — e o orçamento do decoder não cobria essa segunda cópia. Dois uploads
  decodificando ao mesmo tempo custavam ~336 MB de pico, a cada janela de rate
  limit, por qualquer membro: em instância pequena era o hub inteiro que caía.
  Agora a decisão é sobre a imagem já redimensionada (no máximo 576 KB) e as
  layouts que já são a resposta são lidas sem copiar. O avatar, que tinha o mesmo
  padrão, recebeu a mesma correção.
- **A animação guardada é verificada depois de limpa.** O hub conferia os bytes
  crus e gravava o container limpo sem re-verificar — que é justamente o que vai
  para o banco e para cada cliente que abre o perfil. Agora `strip` vem primeiro
  e a verificação roda sobre o que é armazenado (e o decode sai mais barato: um em
  vez de dois).

### Verificação

- `cargo test -p voxhub-app --bin KumaCord` = 340 passed, 0 failed; hub 304,
  0 failed (inclui 7 testes novos de banner e o da intercalação da sidebar).
- Auditoria de segurança (skill `security-audit`, modo guiado) antes do push:
  dois agentes de revisão focados, somente leitura, sobre `banner.rs` + as 3 rotas
  novas + as constantes do `vox-proto`, e sobre o caminho de decodificação de
  bytes não-confiáveis. Um achado medium confirmado e corrigido (acima), um
  `needs_validation` fechado em garantia, o resto rejeitado com motivo. Detalhe e
  o que ficou sem cobertura em `docs/varredura-discord-paridade.md` § 11.6.

## [1.5.0] — 2026-11-09

### Mudado (entrada sem senha)

- **Entrar no servidor agora é só o nome**: nenhuma senha é pedida no fluxo. O
  servidor oficial roda **aberto** (`server_password = ""` no `config.toml` do
  hub), que é a configuração certa para um grupo fechado de amigos — quem
  instala o app digita o nome e clica em entrar.
- O texto do instalador (`LEIA-ME.txt`) e o README não mandam mais "digite a
  senha do servidor"; explicam que não precisa e o que fazer se um dia o
  organizador colocar uma senha (o campo aparece na hora, com o aviso).
- O campo de senha, quando aparecer, agora traz uma dica clara:
  "a senha do servidor (peça ao organizador)" — antes aparecia sem explicação
  nenhuma para quem não sabia o que era.

**Para o dono do servidor:** edite `data/config.toml`, deixe
`server_password = ""` e reinicie (`docker compose restart` / reiniciar o
container `kumacord` no Portainer). O hub avisa no log que o servidor está
aberto — é o esperado. Se preferir manter uma senha, o app continua pedindo;
convites `vox1-...` entram sem senha de qualquer forma.

## [1.4.5] — 2026-11-08

### Corrigido (atualizador)

**Instalar não mudava a versão — e o aviso voltava, num loop.** A causa raiz: a
tag `v1.4.4` foi publicada sem o bump do `Cargo.toml`, então o binário dentro
dela continuava se anunciando como `1.4.3` (`env!("CARGO_PKG_VERSION")`). Depois
de instalar, o app comparava `1.4.4 (feed) > 1.4.3 (ele mesmo)` e achava que
havia versão nova de novo — o pill "pronta" reaparecia e cada clique instalava a
mesma coisa. O instalador em si estava certo; o número é que não mudava.

Agora a versão é `1.4.5` de verdade: instalar troca o número e o aviso para.

## [1.4.4] — 2026-11-08

### Corrigido (atualizador)

O clique no aviso da tela inicial não instalava: o closure da `Area` devolvia
`.clicked()` (um `bool`), e o clique era lido no response da área inteira — que
nunca registrava nada, porque o widget de dentro consumia o evento. O aviso
"Atualização pronta" era um enfeite. Agora o closure devolve o `Response` do
próprio pill e o clique é lido nele; o log registra `clique na seta do canto`.

> Obs.: esta release foi publicada com o `Cargo.toml` ainda em 1.4.3 — o que
> causou o loop corrigido na 1.4.5. Baixe a 1.4.5 direto.

## [1.4.3] — 2026-11-08

### Melhorado (landing page)

O rebranding pediu mudanças, e a página foi reescrita com a orientação da skill de
UI/UX (dark-mode-oled: preto de verdade, neon roxo, glow com parcimônia):

- **A logo entrou**: a arte "Neon Anime KumaCord Gamer" está no alto e no favicon,
  embedada no binário do hub (nada de pasta solta nem link externo).
- **O tema virou templo**: preto profundo, colunas de santuário nas laterais,
  scanlines finas, brasa subindo e os raios que já existiam — o clima de trevas que
  você pediu.
- **O monitor é de verdade**: três estados lidos do `/api/health` do hub — **Servidor
  Online** (bolinha verde pulsando), **Servidor Instável** (âmbar, quando o hub
  responde mas a voz cai) e **Servidor Offline** (cinza). Antes mostrava
  "servidor de pé · 1.4.0", que era ruído.
- **O texto não promete o que não é**: o servidor não "roda no PC de um de vocês" —
  é a comunidade AkumaLabs.
- **Saiu o "Ver no GitHub"**: o código fechou depois do rebranding.
- **O rodapé não diz mais "código aberto (MIT)"** — a licença não é mais essa.

Três testes novos garantem que nenhuma dessas promessas volte por descuido.

## [1.4.2] — 2026-11-08

### Melhorado (o aviso de atualização na tela inicial)

Duas reclamações diretas, as duas corrigidas com a orientação da skill de UI/UX:

- **O aviso ficava em cima de outros botões.** Ele era ancorado no canto superior direito
  sem contar os elementos fixos que já estavam lá — exatamente os botões de modo e o de
  membros do cabeçalho do centro (4 botões). A regra da skill é não empilhar elementos
  fixos descuidadamente; agora o aviso é ancorado à esquerda deles.
- **O botão era feio e a animação parecia travada.** O quadrado mudo de 52×52 virou um
  pill com texto — "v9.9.9 pronta", "v9.9.9 · 45%", "conferindo v9.9.9" — medido pelo
  próprio conteúdo, com borda e cantos totalmente redondos. E a animação, que mexia o
  botão de lugar a 16 fps (por isso o lag), agora é só um brilho pulsando em volta, a
  30 fps, sem mover nada.

## [1.4.1] — 2026-11-08

### Corrigido (atualização automática — o bug que deixou todo mundo preso)

**A atualização não instalava de jeito nenhum, e agora instala.** A causa raiz era um
 canal sobrescrito: quando o app achava uma versão nova, ele começava a baixar *dentro* do
 laço que estava consumindo as mensagens — e no fim do quadro o canal antigo era regravado
 por cima do novo. A thread do download ficava mandando "pronto, esperando o clique" para
 um canal que ninguém lia. Resultado: a fase parava em **`Downloading` para sempre**, com a
 barra travada, sem seta nenhuma e com os dois botões do "Sobre" mortos.

- **O canal novo sobrevive ao quadro** (`update_poll` só regrava quando não há outro).
- **A seta aparece em toda fase em que exista atualização**, não só quando está pronta:
  encontrada, baixando (com a porcentagem), conferindo a assinatura, ou pronta para
  instalar. Antes ela só existia em `Pending`, e o app passava quase todo o tempo
  baixando — por isso ninguém via nada.
- **Clicar na seta em qualquer fase faz algo**: na pronta, instala; nas outras, abre a
  janela com o progresso. Antes o clique simplesmente não existia.
- **Os botões do "Sobre" nunca mais ficam mudos**: "Verificar atualizações" agora diz o
  que está acontecendo ("Já estou cuidando disso: baixando… 50%"), e o segundo botão muda
  de texto conforme a fase — "Instalar agora", "Ver o progresso" ou "Ver a nova versão".
- **O timeout do download era de 30 s para 72 MB** — exigia 2,4 MB/s de quem baixa, e
  qualquer conexão mais modessa morria com "o download parou no meio". Agora são 10
  minutos.

### Testes

- Sete testes novos em `update.rs`, incluindo um que percorre **todas as fases** e exige
  que nenhuma fique sem seta — foi uma fase sem dono na interface que causou o estrago.

## [1.4.0] — 2026-11-08

### Adicionado

- **Landing page em `https://kumacord.akuma-labs.duckdns.org`** (o hub serve em `/`,
  atrás do Nginx Proxy Manager com certificado): a página que você manda nos links,
  com o botão "Baixar para Windows" como a do Discord. Tema escuro com céu de
  tempestade — raios procedurais em canvas, faíscas e clarão —, as três colunas de
  recursos e os três passos para entrar.
- **URL de download que nunca muda**: o CI publica, além do instalador versionado,
  uma cópia de nome fixo `KumaCord-setup.exe`. O link
  `.../releases/latest/download/KumaCord-setup.exe` aponta sempre para a release mais
  recente, então a página não precisa ser editada a cada versão.
- **`GET /api/update` no hub**: espelha o `latest.json` do GitHub com cache de 10
  minutos e devolve apenas versão e notas. O browser não consegue ler o GitHub direto
  (os assets de release não mandam CORS), então a landing pergunta aqui — mesma
  origem, sem proxy externo. Se o GitHub não responder, devolve o último conhecido; se
  nunca houve, `503`, e a página mostra "a versão mais recente no GitHub" com o botão
  funcionando (ele aponta para o GitHub, não para o hub).

### Servidor

- **A VPS foi atualizada da 1.0.6 para a 1.3.0**: o código estava 22 commits atrás.
  Rebuild da imagem, troca do container com backup do banco antes, e a migração
  (`user_version` 4 → 5) confirmida. Downtime de ~15 s; o app reconectou sozinho.

## [1.3.2] — 2026-11-08

### Corrigido (atualização automática)

- **A janela de atualização ficava muda quando o download falhava** (rede, hash, disco):
  mostrava o título e as notas, mas nenhum botão e nenhum motivo. Agora diz o que
  aconteceu e oferece "Tentar de novo". É o mesmo buraco das fases sem branch que
  quebrou a 1.3.0 — fechado nas fases `Pending` e `Failed`, que eram as duas que
  sobravam.

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
