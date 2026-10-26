# syntax=docker/dockerfile:1
# KumaCord — o servidor AkumaLabs (hub + LiveKit) em um container.
#
# Build:   docker build -t kumacord .
# Run:     docker compose up -d      (veja docker-compose.yml)
#
# Multi-stage: compila o hub (voxhub-server) do fonte e baixa o binário oficial do
# LiveKit (versão pinada, SHA-256 verificado) para a arquitetura da build.
#
# O container roda como o usuário fixo "kumacord" (uid/gid 1000). O diretório ./data no
# host precisa ser gravável por esse uid — o compose espera que você o crie antes:
#   mkdir -p data && sudo chown 1000:1000 data   # se seu usuário não for uid 1000

ARG LIVEKIT_VERSION=1.13.7

# ---------------------------------------------------------------- build stage
FROM rust:1-slim-bookworm AS build
ARG LIVEKIT_VERSION
ARG TARGETARCH
WORKDIR /src

RUN apt-get update \
 && apt-get install -y --no-install-recommends build-essential pkg-config \
 && rm -rf /var/lib/apt/lists/*

COPY . .

# Só o servidor compila aqui (o client é Windows/macOS).
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo build --release -p voxhub-server

# curl para baixar o LiveKit (o rust:slim não o traz; layer separada para
# não invalidar o cache da compilação acima).
RUN apt-get update \
 && apt-get install -y --no-install-recommends curl \
 && rm -rf /var/lib/apt/lists/*

# LiveKit oficial, versão pinada, hash conferido (arm64 e amd64).
RUN case "$TARGETARCH" in \
      arm64) LK=livekit_${LIVEKIT_VERSION}_linux_arm64.tar.gz; SHA=5d167fdf52cf43c0c72972f25325364479f41f854bfef651056eab2504da5de9 ;; \
      amd64) LK=livekit_${LIVEKIT_VERSION}_linux_amd64.tar.gz; SHA=6634aeeb2fb1366b6723708ae4320b9d5408106a4c63457c5e845ae3979c90e2 ;; \
      *) echo "arquitetura não suportada: $TARGETARCH" >&2; exit 1 ;; \
    esac \
 && curl -sSLO "https://github.com/livekit/livekit/releases/download/v${LIVEKIT_VERSION}/${LK}" \
 && echo "${SHA}  ${LK}" | sha256sum -c - \
 && tar xzf "${LK}" livekit-server \
 && chmod +x livekit-server \
 && mkdir -p /out \
 && cp target/release/kumacord-server /out/ \
 && mv livekit-server /out/

# --------------------------------------------------------------- runtime stage
FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && groupadd --gid 1000 kumacord \
 && useradd --uid 1000 --gid 1000 --home /data --shell /usr/sbin/nologin kumacord

COPY --from=build /out/kumacord-server /usr/local/bin/kumacord-server
COPY --from=build /out/livekit-server /usr/local/bin/livekit-server

WORKDIR /data
# O diretório já nasce do dono certo (para uso sem bind mount); com bind, a pasta do
# host que manda — crie-a gravável pelo uid 1000.
RUN mkdir -p /data && chown -R 1000:1000 /data
VOLUME /data

USER kumacord
EXPOSE 7890 7880 7881 7882/udp

# umask 0027: segredos criados no volume (config.toml, voxhub.db, livekit.yaml) não
# ficam legíveis por outros usuários locais. --no-diag: dentro de um container as
# checagens de roteador/CGNAT/firewall do Windows não se aplicam (e evita a busca
# STUN/UPnP de saída no boot); a conectividade se verifica por fora (ports do provedor).
ENTRYPOINT ["/bin/sh", "-c", "umask 0027 && exec /usr/local/bin/kumacord-server --data-dir /data --livekit-bin /usr/local/bin/livekit-server --console --no-diag"]
