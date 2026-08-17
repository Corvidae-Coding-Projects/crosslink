







FROM ubuntu:24.04



ARG TARGETARCH=amd64
ARG GOSU_VERSION=1.17




RUN apt-get update && apt-get install -y --no-install-recommends \
    git openssh-client curl ca-certificates jq python3 sudo build-essential nodejs npm \
    && rm -rf /var/lib/apt/lists/*



RUN npm install -g @openai/codex


RUN userdel -r ubuntu 2>/dev/null || true


RUN useradd -m -s /bin/bash agent && echo "agent ALL=(ALL) NOPASSWD:ALL" >> /etc/sudoers.d/agent
USER agent
WORKDIR /home/agent


SHELL ["/bin/bash", "-c"]


RUN curl -fsSL https://claude.ai/install.sh | bash


RUN curl -LsSf https://astral.sh/uv/install.sh | sh




USER root
RUN curl -fsSL "https://github.com/tianon/gosu/releases/download/${GOSU_VERSION}/gosu-${TARGETARCH}" -o /usr/local/bin/gosu \
    && chmod +x /usr/local/bin/gosu




COPY --chown=agent crosslink-${TARGETARCH} /usr/local/bin/crosslink
RUN chmod +x /usr/local/bin/crosslink
COPY entrypoint.sh /usr/local/bin/crosslink-entrypoint.sh
RUN chmod +x /usr/local/bin/crosslink-entrypoint.sh

WORKDIR /workspaces

ENTRYPOINT ["/usr/local/bin/crosslink-entrypoint.sh"]
