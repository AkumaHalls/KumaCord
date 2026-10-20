# Segurança (concord v1.0.1)

## Para quem o concord foi feito

Para **grupos pequenos de amigos que confiam entre si**, com o servidor rodando no PC de um deles.
Ele não foi feito para comunidades abertas com desconhecidos.

## A conexão ainda não é criptografada

- Entre o app e o servidor **não há HTTPS/TLS**. Senhas, códigos de convite, logins lembrados e
  mensagens passam pela rede sem criptografia. Alguém na mesma rede (Wi-Fi de café, faculdade,
  um roteador invadido) poderia ler esse tráfego. O áudio e o vídeo das chamadas vão
  criptografados; o resto, não.
- **Use uma senha de servidor forte e exclusiva**, e uma senha pessoal que você não usa em
  nenhum outro lugar.
- **Uma VPN (Tailscale ou ZeroTier) é mais segura do que abrir portas no roteador**: só quem
  está na sua VPN alcança o servidor, e o tráfego vai criptografado por ela.
- **O app pode abrir as portas do roteador sozinho.** Quando você usa "Criar servidor" no app, ele
  pede ao roteador (UPnP) que abra as portas do servidor para a internet, e as fecha ao parar. É
  o que permite amigos de fora entrarem sem configurar nada, mas também deixa o servidor visível
  na internet enquanto ele está ligado. Se preferir não expor nada, desligue o UPnP no roteador e
  use uma VPN.
- Se você mesmo abrir portas, abra **só** as do concord (TCP 7890, 7880 e 7881, UDP 7882). Evite
  o DMZ do roteador, que expõe o PC inteiro. Mantenha o Firewall do Windows ligado. No servidor
  separado, o `liberar-firewall.bat` libera só os programas e as portas do concord, em todas as
  redes (também nas que o Windows marca como Públicas, senão quem é de fora não entra). Num
  notebook que usa Wi-Fi público, desligue o servidor fora de casa ou rode o `remover-firewall.bat`.

## Convites

- **Um código de convite (`vox1-...`) funciona como uma senha**: quem tem o código entra no
  servidor sem a senha do servidor. Mande-o só em conversa privada, nunca num lugar público.
- Dá para pôr validade e limite de usos, e **cancelar** um convite a qualquer momento (o Dono ou
  um Admin, em "Convidar amigos"). Quem já entrou com ele continua no servidor.
- Um convite só **cria contas novas**. Ele não serve para entrar com o nome de alguém que já tem
  conta no servidor: esse nome continua precisando da senha do servidor (ou da própria senha
  pessoal da pessoa).
- O app só manda a senha do servidor que ele guardou, e o login lembrado deste computador, para o
  **mesmo servidor** em que eles foram salvos (cada servidor tem uma identidade própria). Se um
  endereço, de um convite ou da lista, responder como outro servidor, o app avisa ("Este endereço
  não é o mesmo servidor que você salvou") e não manda nada do que está salvo.

## Onde ficam os segredos

- **Servidor criado pelo app ("Criar servidor"):** cada servidor fica em
  `%LOCALAPPDATA%\concord\host\<id>\`. Ali estão o `config.toml` (com a senha do servidor e a
  chave do servidor de voz) e o banco de dados, com as mensagens.
- **Servidor rodando pelo `iniciar-servidor.bat`:** tudo isso fica na pasta `data\`.
- **A pasta `data\` (e a de cada servidor em `host\`) é secreta.** Nunca envie essas pastas para
  ninguém nem para o GitHub, e não as deixe numa pasta compartilhada.
- **No app de cada pessoa:** a senha do servidor (só com "Lembrar a senha") e o login lembrado
  deste computador ficam criptografados para o seu usuário do Windows, em
  `%APPDATA%\concord\`.
- **Quem hospeda guarda os dados.** Mensagens, inclusive as diretas, e imagens ficam no banco de
  dados do PC de quem hospeda. O app não deixa ninguém (nem Admin, nem Dono) ler as mensagens
  diretas dos outros, mas quem tem acesso ao arquivo do banco consegue.

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

- **Defina a senha do servidor.** Sem ela (`server_password` vazio no `config.toml`), qualquer
  pessoa que descobrir o endereço pode entrar.
- **Proteja o seu nome** com uma senha pessoal (Configurações → Senha pessoal). Sem ela, quem
  tem a senha do servidor pode usar o seu nome. Dono, Admin e Moderador são obrigados a ter uma.
- **Baixe o app só de fontes que você confia** (o repositório oficial ou a pessoa que hospeda o
  seu grupo). O executável não é assinado digitalmente, então o Windows mostra um aviso na
  primeira vez.

## Encontrou uma falha?

Não publique detalhes que permitam explorar a falha. Use a opção "Report a vulnerability"
(Security Advisories) do GitHub, se estiver habilitada, ou abra uma *issue* sem os detalhes
pedindo um contato privado. Diga a versão do concord e, se puder, como reproduzir com contas de
teste.
