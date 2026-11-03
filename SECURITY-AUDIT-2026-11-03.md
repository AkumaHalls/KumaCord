# SECURITY AUDIT (delta) — Launcher v1.1.9
**Data:** 03/11/2026
**Escopo:** Delta da tela de entrada remodelada (`app/src/launcher.rs`, `app/src/home.rs`, áudio do
launcher em `vox-client-core/src/audio/{clip,mixer,ui_player}.rs`, `app/src/backend.rs`,
`app/src/settings.rs`, `app/src/servers.rs`) sobre a auditoria de 28/10/2026
(`SECURITY-AUDIT-2026-10-28.md`). Nada do que aquela auditoria cobriu mudou de comportamento.

---

## 1. RESUMO

A remodelagem do launcher (cinemática em vídeo ao fundo, card único do servidor oficial, senha
fora do fluxo principal) **não adiciona nenhuma superfície de rede nova e não altera o fluxo de
autenticação**: o login continua passando exatamente pelos mesmos `Cmd::Connect` / `Guard` /
DPAPI de antes. Os riscos novos são locais (assets embutidos, threads de decode) e todos são
best-effort com falha silenciosa.

---

## 2. SUPERFÍCIE NOVA, ITEM POR ITEM

### 2.1 Assets embutidos (`app/assets/intro/intro.pack`, `intro.mp3`)

**Status: SEGURO**

| Item | Status | Observação |
|------|--------|------------|
| Origem | ✅ | Frames extraídos do vídeo próprio do dono do projeto (`Desktop\fish\Media_.mp4`); sem material de terceiros (a opção YouTube foi descartada por copyright). |
| Embutido no binário | ✅ | `include_bytes!` — nada é carregado de disco em runtime; nenhum download, nenhuma URL. |
| Parse do pack | ✅ | Header validado (`AKUI1`, dimensões/fps/count ≠ 0, tabela de offsets conferida contra o tamanho total — `Frames::parse` retorna `None` em qualquer inconsistência). Testes cobrem pack real, primeiro frame e header corrompido. |
| Decode WebP | ✅ | `image` crate, mesma dependência já usada para imagens de chat; frame danificado encerra o worker sem pânico (app compila com `panic = "abort"`, então o worker trata erro retornando, nunca `unwrap`). |

### 2.2 Áudio do launcher (`audio/clip.rs`, `Cmd::PlayUiClip`)

**Status: SEGURO**

| Item | Status | Observação |
|------|--------|------------|
| Decode | ✅ | symphonia (já presente para o soundboard), em memória, com limite de 60 s e cap de duração; entrada é asset compilado, não arquivo do usuário. |
| Ganho | ✅ | `(volume * 0.5).clamp(0.0, 1.0)` no backend; volume vem do slider local do usuário. |
| Mixagem | ✅ | Loop no mixer com saturação por clamp (testes `a_looping_ui_sound_wraps_until_cleared`); sem wrap-around de i16. |
| Stop no "Entrar" | ✅ | `Cmd::StopUiClip` zera `launcher_wanted` e chama `ui_player.stop()`; decode em voo chega depois e é descartado (`launcher_wanted == false`). |
| Autotestes | ✅ | `VOX_NO_PLAYBACK` suprime; `testmode::background()` impede o pedido do clipe (`wants_music`). |

### 2.3 Validação do campo de endereço (`App::connect`)

**Status: SEGURO — sem mudança de superfície**

- Produção: continua aceitando **apenas** convite `vox1-...` ou a `HOME_URL` fixa; nada digitado
  vira endereço de servidor.
- **Escape novo e intencional para autotestes**: `vox_client_core::testmode::background()` libera
  endereços de dev (`--server 127.0.0.1:...`) que o gate (`tools/gate.ps1`) sempre usou. O modo
  teste nunca roda em produção (flag de processo, sem rede externa) e o cliente de verdade não é
  afetado: a checagem continua fechada para usuários reais.
- `form.url` não é mais semeado com o servidor salvo antigo, a menos que seja convite ou a home —
  reduz a chance de um endereço legado aparecer pré-preenchido.

### 2.4 Senha do servidor fora do fluxo (`home.rs`, `servers.rs`)

**Status: SEGURO — superfície de segredo inalterada**

| Item | Status | Observação |
|------|--------|------------|
| Aparência condicional | ✅ | O campo só aparece quando pedido (erro do hub citando "senha do servidor", entrada salva sem senha, foco solicitado). Menos campo em tela = menos superfície de screenshot, mas o mesmo valor trafega. |
| Armazenamento | ✅ | Inalterado: DPAPI (Windows) / Keychain (macOS), só com "Lembrar a senha"; `reveal_password` (olho) é estado de UI volátil, nunca persistido. |
| Envio | ✅ | `connect()` segue enviando por `Cmd::Connect` como antes; nada novo em log (`tracing` não imprime senha — conferido nos pontos novos: logs do launcher registram apenas booleanos `queued`/`wanted`). |

### 2.5 Threads novas

| Thread | Risco | Mitigação |
|--------|-------|-----------|
| `vox-intro` (decode de frames) | Baixo | Termina quando o app derruba os canais; pede 1 frame por vez; sem acesso a disco/rede. |
| `vox-intro-audio` (decode MP3, `spawn_blocking`) | Baixo | Uma vez; resultado por canal interno; erro → `None` → nada toca. |
| Worker do `UiPlayer` | Pré-existente | Reutilizado; parada por `clear_ui`, device fecha por idle. |

### 2.6 Dados e privacidade

- Nenhum dado novo sai da máquina: a cinemática é embutida, o áudio é local, o `latest.json` do
  updater não muda de formato (só de versão).
- As preferências novas (`intro`, `intro_sound`) são dois booleanos no `settings.json` existente —
  sem segredo, sem PII.

---

## 3. CONCLUSÃO DO DELTA

**Nenhuma vulnerabilidade nova.** O launcher continua atrás da mesma validação de login de antes;
os dois pontos que mereceram ajuste durante a auditoria já estão no código desta versão:

1. Escape de endereços de dev restrito a `testmode::background()` (autotestes), mantendo o
   convite-only para usuários reais.
2. Cores translúcidas do card corrigidas para pré-multiplicadas corretas (bug visual, não de
   segurança) e ganho de música limitado a 0,5× o volume local.

Próxima auditoria completa recomendada: antes da versão com convites assinados no hub.
