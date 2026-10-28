# SECURITY AUDIT - AkumaLabs / KumaCord
**Data:** 27/10/2026  
**Escopo:** Repositório `KumaCord` (app client + hub servidor)  
**Versão:** 1.0.10 (em preparação)  
**Avaliador:** Security Audit Skill - Análise estática de código fonte  

---

## 1. RESUMO EXECUTIVO

A auditoria de segurança focou na superfície de código existente no repositório, incluindo o cliente desktop (Rust/egui) e o servidor hub (Rust/axum). Nenhuma vulnerabilidade crítica foi encontrada. O código segue boas práticas de segurança: validação de entrada, uso de prepared statements para SQL, verificação de assinatura criptográfica em atualizações, e rate limiting para autenticação.

---

## 2. AUDITORIA POR COMPONENTE

### 2.1 Atualização Automática (app/src/update.rs)

**Status: SEGURO**

| Item | Status | Observação |
|------|--------|------------|
| Verificação SHA-256 | ✅ Presente | O hash do instalador é verificado antes de instalar |
| Assinatura digital (minisign) | ✅ Presente | Chave pública compilada no app; chave privada só no CI |
| Validação de URL do feed | ✅ Presente | Apenas URLs do repositório oficial GitHub são aceitas |
| Download em temp dir | ✅ Presente | Arquivo baixado para temp antes de instalar |
| Validação de tamanho | ✅ Presente | Limite de 200 MB para o instalador |

**Observação:** O fluxo de atualização segue o modelo do KumaDrive e está correto. A assinatura minisign é verificada **antes** de executar o instalador.

---

### 2.2 Upload de Imagens (hub/src/avatar.rs, hub/src/attachments.rs)

**Status: SEGURO**

| Item | Status | Observação |
|------|--------|------------|
| Validação por magic bytes | ✅ Presente | JPEG, PNG, GIF, WebP verificados por bytes iniciais |
| Limite de tamanho (avatar) | ✅ 5 MB | Limite definido em `AVATAR_MAX_UPLOAD_BYTES` |
| Limite de tamanho (banner) | ✅ 10 MB | Limite definido em `BANNER_MAX_UPLOAD_BYTES` |
| Dimensões máximas | ✅ 1920x500 para banner | Validadas antes de armazenar |
| Rate limit por usuário | ✅ Presente | 5 uploads por 10 segundos |
| Rate limit de decode | ✅ Presente | Máximo de 2 decodificações simultâneas |
| Assinatura/verificação de arquivo | ✅ Presente | SHA-256 hash para versionamento |

**Observação:** O código de validação em `attachments/validate.rs` verifica magic bytes, dimensões, limites de animação e descarta metadados perigosos (EXIF, comentários, etc.). Bem implementado.

---

### 2.3 Autenticação e Autorização (hub/src/api.rs, hub/src/auth.rs)

**Status: SEGURO**

| Item | Status | Observação |
|------|--------|------------|
| Autenticação por token Bearer | ✅ Presente | Hash SHA-256 do token armazenado |
| Rate limit de login por IP | ✅ Presente | `throttle.must_wait(&ip)` bloqueia após tentativas |
| Rate limit por nome de usuário | ✅ Presente | `s.throttle.begin(&ukey)` para senha pessoal |
| Senha pessoal (bcrypt/argon2) | ✅ Presente | `argon2_blocking` para validação |
| Validação de username | ✅ Presente | 2-32 caracteres, sem caracteres de controle |
| Bloqueio de usuário banido | ✅ Presente | Verificação de `ban_of` no login |
| Sessões com expiração | ✅ Presente | `SESSION_TTL_MS = 30 dias` |
| Device tokens | ✅ Presente | Para login sem senha em dispositivo conhecido |

**Observação:** O sistema de autenticação é robusto com múltiplas camadas de proteção contra ataques de força bruta.

---

### 2.4 Controle de Acesso (RBAC)

**Status: SEGURO**

| Item | Status | Observação |
|------|--------|------------|
| Hierarquia de cargos | ✅ Presente | Membro < Moderador < Admin < Owner |
| `can_admin()` | ✅ Implementado | Apenas Admin+ |
| `can_moderate()` | ✅ Implementado | Moderador+ |
| `can_manage_channels()` | ✅ Implementado | Admin+ |
| `can_reset_passwords()` | ✅ Implementado | Owner apenas |
| `can_set_role()` | ✅ Implementado | Restrito por hierarquia |
| `outranks()` | ✅ Implementado | Impede escalada de privilégio |
| `assignable()` | ✅ Implementado | Limita cargos que podem ser atribuídos |

**Observação:** O sistema de permissões é bem estruturado com hierarquia clara e validação em cada operação.

---

### 2.5 Banco de Dados (hub/src/db.rs)

**Status: SEGURO**

| Item | Status | Observação |
|------|--------|------------|
| Prepared statements | ✅ Presente | Todas as consultas usam `params![]` |
| Migrações idempotentes | ✅ Presente | `CREATE TABLE IF NOT EXISTS` e `ALTER TABLE` com verificação |
| Transações | ✅ Presente | Uso de `conn.transaction()` para operações atômicas |
| Chaves estrangeiras | ✅ Presente | `PRAGMA foreign_keys = ON` |

**Observação:** Não há concatenação de strings em consultas SQL. Todos os parâmetros são passados via `params![]`, prevenindo SQL injection.

---

### 2.6 Controle de Acesso a Canais (hub/src/access.rs)

**Status: SEGURO**

| Item | Status | Observação |
|------|--------|------------|
| Canais privados | ✅ Implementado | `PrivateAccess` com lista de cargos e usuários |
| Verificação de acesso | ✅ Implementado | `allows()` verifica cargo e usuário |
| Admin/Dono sempre têm acesso | ✅ Implementado | `role >= Role::Admin` sempre retorna true |

---

## 3. AVALIAÇÃO DE RISCO

| Área | Risco | Severidade | Mitigação |
|------|-------|------------|-----------|
| Atualização automática | Baixo | Assinatura minisign + SHA-256 |
| Upload de arquivos | Baixo | Validação de magic bytes, limites de tamanho |
| Autenticação | Baixo | Rate limiting + bcrypt/argon2 |
| SQL Injection | Baixo | Prepared statements em todo o código |
| Controle de acesso | Baixo | Hierarquia de cargos bem definida |

---

## 4. RECOMENDAÇÕES

### 4.1 Melhorias Opcionais (não críticas)

1. **HTTPS obrigatório para atualizações** - Já implementado, mas poderia adicionar verificação de certificado SSL mais rigorosa.

2. **Rate limiting global** - Atualmente há rate limit por IP para login e por usuário para uploads. Considerar rate limit global por endpoint para prevenir DoS.

3. **Limite de tamanho de mensagem** - Já implementado (4000 caracteres). Manter.

4. **Logs de auditoria** - Já implementado (`audit_log` table). Manter.

---

## 5. CONCLUSÃO

O código do KumaCord segue boas práticas de segurança. Não foram encontradas vulnerabilidades críticas nas áreas auditadas (atualização automática, upload de arquivos, autenticação, controle de acesso, banco de dados).

**Recomendação:** Liberar a versão 1.0.10 com as melhorias de UI e funcionalidades já implementadas.

---

## 6. EVIDÊNCIA DE CÓDIGO

### 4.1 Verificação de assinatura minisign (update.rs:714-717)
```rust
let pk = public_key().ok_or("a chave de assinatura embutida é inválida")?;
let sig = minisign_verify::Signature::decode(&manifest.signature).map_err(|_| "a assinatura publicada é ilegível".to_string())?;
pk.verify(&bytes, &sig, false).map_err(|_| "a assinatura do instalador não é válida (arquivo rejeitado)".to_string())?;
```

### 4.2 Validação de upload de imagem (attachments/validate.rs)
O código verifica magic bytes, dimensões, e descarta metadados perigosos antes de armazenar.

### 4.3 Controle de acesso (vox-proto/src/lib.rs:367-394)
```rust
pub fn outranks(self, target: Role) -> bool {
    self.can_moderate() && self > target
}
```

---

**Auditoria concluída em 27/10/2026.**  
Próxima auditoria recomendada: após lançamento da v1.1.0 ou em caso de alterações significativas no código de autenticação ou upload de arquivos.