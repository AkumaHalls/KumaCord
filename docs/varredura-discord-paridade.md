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
| **Categorias de canais** (os headers colapsáveis do Discord) | ❌ | M | `parent_id` no canal + UI colapsável — pedido explícito |
| **Tópico do canal** (descrição em cima) | ❌ | P | Um campo texto no canal + uma linha na UI |
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
| **Avatar animado (GIF)** | ❌ | M | O hub re-codifica tudo; precisa guardar o GIF + animar |
| **Banner de perfil** (imagem/GIF atrás do card) | ❌ | M | Nem imagem estática temos hoje (só cor) |
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
| Paleta dark do Discord (blurple #5865F2, cinzas #313338/#2B2D31/#1E1F22, texto #DBDEE1) | ❌ | P | Troca de tokens no theme.rs — pedido explícito |
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
7. ❌ **GIF avatar + banner** (M)
8. ❌ **Tópico de canal** + **slowmode** + **silenciar canal** (P cada)
9. ❌ **GIFs no chat** (M) → depois **emojis do servidor** (M) → **stickers** (M)
10. ❌ **Ícone do servidor** (P) → depois o resto da lista (G: threads, DM grupo, stage)
