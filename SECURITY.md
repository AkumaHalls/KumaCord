# Segurança (KumaCord / AkumaLabs)

> Segurança do **KumaCord / AkumaLabs** (veja `LICENSE`).

## Para quem o KumaCord foi feito

Para **grupos e comunidades que confiam entre si**. Nesta distribuição o servidor roda em
**Docker numa VPS** e o app conecta **apenas ao servidor oficial** (endereço fixo compilado no
client); não há campo de endereço nem hospedagem pelo app. Os limites de confiança abaixo
continham valendo.

## A conexão ainda não é criptografada

- Entre o app e o servidor **não há HTTPS/TLS**. Senhas, códigos de convite, logins lembrados e
  mensagens passam pela rede sem criptografia. Alguém na mesma rede (Wi-Fi de café, faculdade,
  um roteador invadido) poderia ler esse tráfego. O áudio e o vídeo das chamadas vão
  criptografados; o resto, não.
- **Use uma senha de servidor forte e exclusiva**, e uma senha pessoal que você não usa em
  nenhum outro lugar.
- **Uma VPN (Tailscale ou ZeroTier) é a forma mais segura de conectar**: só quem está na sua
  VPN alcança a VPS, e o tráfego vai criptografado por ela. Se a comunidade conecta direto pelo
  IP público, aceite o risco acima e ao menos proteja nomes de Admin/Dono com senha pessoal.
- Na VPS, abra **só** as portas do KumaCord no firewall/security group do provedor
  (TCP 7890, 7880 e 7881, UDP 7882) — não exponha a máquina inteira.
- Servidor Windows nativo (sem Docker): permita o `kumacord-server.exe` e o `livekit-server.exe`
  no Firewall do Windows, em todas as redes (também nas que o Windows marca como Públicas,
  senão quem é de fora não entra).

## Convites

- **Um código de convite (`vox1-...`) funciona como uma senha**: quem tem o código entra no
  servidor sem a senha do servidor. Mande-o só em conversa privada, nunca num lugar público.
  Nesta distribuição, o código carrega só o token: o endereço que o app usa é sempre o fixo.
- Dá para pôr validade e limite de usos, e **cancelar** um convite a qualquer momento (o Dono ou
  um Admin, em "Convidar amigos"). Quem já entrou com ele continua no servidor.
- Um convite só **cria contas novas**. Ele não serve para entrar com o nome de alguém que já tem
  conta no servidor: esse nome continua precisando da senha do servidor (ou da própria senha
  pessoal da pessoa).
- O app só manda a senha do servidor que ele guardou, e o login lembrado deste computador, para o
  **mesmo servidor** em que eles foram salvos (cada servidor tem uma identidade própria). Se um
  endereço responder como outro servidor, o app avisa ("Este endereço não é o mesmo servidor que
  você salvou") e não manda nada do que está salvo.

## Onde ficam os segredos

- **Servidor em Docker (VPS):** tudo fica no volume `./data` do host — `config.toml` (com a senha
  do servidor, **gerada no primeiro boot e impressa no console do container**, e as chaves do
  LiveKit), `voxhub.db` (mensagens, hashes de senha, tokens) e `livekit.yaml` (chaves). Esses
  arquivos são criados legíveis só pelo dono do serviço.
- **A pasta `./data` é secreta.** Nunca envie-a para ninguém nem para o GitHub, e não a deixe
  numa pasta compartilhada. `docker logs kumacord` contém endereços e nomes: compartilhe só
  com quem opera o servidor.
- **No app de cada pessoa:** a senha do servidor (só com "Lembrar a senha") e o login lembrado
  deste computador ficam criptografados para o seu usuário do Windows, em
  `%APPDATA%\AkumaLabs\` (no macOS, no Keychain, serviço `AkumaLabs`). Os logs ficam em
  `%LOCALAPPDATA%\AkumaLabs\logs`.
- **Quem hospeda guarda os dados.** Mensagens, inclusive as diretas, e imagens ficam no banco de
  dados da VPS. O app não deixa ninguém (nem Admin, nem Dono) ler as mensagens diretas dos
  outros, mas quem tem acesso ao arquivo do banco consegue.

## Canais privados

- Quem não tem acesso a um canal privado **não vê o canal, as mensagens, as imagens, as reações
  nem quem está na voz dele**, e não consegue entrar na voz.
- Mas dá para **perceber que existe atividade escondida**: as mensagens do servidor são
  numeradas em sequência, e um app modificado nota os números que pulam (quantas mensagens foram
  mandadas em canais que ele não vê, e quando). O conteúdo, os nomes e os canais não vazam.
- Quem perde o acesso sai da voz do canal na hora. A entrada da voz vale só por cerca de um
  minuto, então quem perdeu o acesso não consegue voltar.

## Não volte para um servidor antigo (proteção contra downgrade)

Um servidor antigo (v0.8.x ou anterior) não conhece canais privados e, rodando no mesmo banco de
dados, mostraria esses canais para todo mundo. Por isso, enquanto existir algum canal privado, o
banco fica marcado de um jeito que **o servidor antigo se recusa a iniciar**. Se precisar mesmo
voltar, abra a versão nova antes e torne todos os canais públicos (ou apague os privados). Os
detalhes estão em `hub/README.md`.

## Outras dicas

- **Defina a senha do servidor.** O primeiro boot já gera uma senha e a mostra no console; troque
  à vontade no `config.toml`. Deixar `server_password` vazio é opt-in explícito para servidor
  aberto: qualquer pessoa que descobrir o endereço entra (o servidor avisa no console).
- **Proteja o seu nome** com uma senha pessoal (Configurações → Senha pessoal). Sem ela, quem
  tem a senha do servidor pode usar o seu nome. Dono, Admin e Moderador são obrigados a ter uma.
  Com `owner = "..."` fixado no `config.toml` (recomendado em VPS), o primeiro código de senha
  pessoal do Dono aparece no console (`docker logs kumacord`) — use-o e guarde o nome.
- **Baixe o app só de fontes que você confia** (o repositório oficial ou a pessoa que hospeda o
  seu grupo). O executável não é assinado digitalmente, então o Windows mostra um aviso na
  primeira vez.

## Encontrou uma falha?

Não publique detalhes que permitam explorar a falha. Use a opção "Report a vulnerability"
(Security Advisories) do GitHub, se estiver habilitada, ou abra uma *issue* sem os detalhes
pedindo um contato privado. Diga a versão do KumaCord e, se puder, como reproduzir com contas
de teste.
