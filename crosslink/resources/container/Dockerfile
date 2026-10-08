







# The base image is pinned by digest and the agent tools by version (gosu also
# by checksum); bump them deliberately. Still floating: Ubuntu's apt packages,
# the gh apt keyring, Codex's npm dependencies, and the Claude Code and uv
# installer scripts (the installers fetch the pinned versions).
FROM ubuntu:24.04@sha256:534baea6a22c03a63003dbc8dbe78fe34bc0d7e595d9a9dc9834884ff530eb55

# A failed download in a piped install must fail the build.
SHELL ["/bin/bash", "-o", "pipefail", "-c"]

ARG TARGETARCH=amd64
ARG GH_VERSION=2.102.0
ARG CODEX_VERSION=0.160.1
ARG CLAUDE_CODE_VERSION=2.1.292
ARG UV_VERSION=0.12.23
ARG GOSU_VERSION=1.17
ARG GOSU_SHA256_AMD64=bbc4136d03ab138b1ad66fa4fc051bafc6cc7ffae632b069a53657279a450de3
ARG GOSU_SHA256_ARM64=c3805a85d17f4454c23d7059bcb97e1ec1af272b90126e79ed002342de08389b




RUN apt-get update && apt-get install -y --no-install-recommends \
    git openssh-client curl ca-certificates jq python3 sudo build-essential nodejs npm \
    && rm -rf /var/lib/apt/lists/*

# GitHub CLI, from GitHub's apt repository, so a container can hold a `gh`
# login and publish hub refs over HTTPS (`crosslink container auth login --provider github`).
RUN mkdir -p -m 755 /etc/apt/keyrings \
    && curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg -o /etc/apt/keyrings/githubcli-archive-keyring.gpg \
    && chmod go+r /etc/apt/keyrings/githubcli-archive-keyring.gpg \
    && echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" > /etc/apt/sources.list.d/github-cli.list \
    && apt-get update && apt-get install -y --no-install-recommends "gh=${GH_VERSION}" \
    && rm -rf /var/lib/apt/lists/*



RUN npm install -g "@openai/codex@${CODEX_VERSION}"


RUN userdel -r ubuntu 2>/dev/null || true


RUN useradd -m -s /bin/bash agent && echo "agent ALL=(ALL) NOPASSWD:ALL" >> /etc/sudoers.d/agent
USER agent
WORKDIR /home/agent


SHELL ["/bin/bash", "-o", "pipefail", "-c"]


# The installer verifies the checksum of the Claude Code binary it downloads.
RUN curl -fsSL https://claude.ai/install.sh | bash -s "${CLAUDE_CODE_VERSION}"


RUN curl -LsSf "https://astral.sh/uv/${UV_VERSION}/install.sh" | sh




USER root
RUN curl -fsSL "https://github.com/tianon/gosu/releases/download/${GOSU_VERSION}/gosu-${TARGETARCH}" -o /usr/local/bin/gosu \
    && case "${TARGETARCH}" in \
         amd64) expected="${GOSU_SHA256_AMD64}" ;; \
         arm64) expected="${GOSU_SHA256_ARM64}" ;; \
         *) echo "no gosu checksum for ${TARGETARCH}" >&2; exit 1 ;; \
       esac \
    && echo "${expected}  /usr/local/bin/gosu" | sha256sum -c - \
    && chmod +x /usr/local/bin/gosu




COPY --chown=agent crosslink-${TARGETARCH} /usr/local/bin/crosslink
RUN chmod +x /usr/local/bin/crosslink
COPY entrypoint.sh /usr/local/bin/crosslink-entrypoint.sh
RUN chmod +x /usr/local/bin/crosslink-entrypoint.sh

WORKDIR /workspaces

ENTRYPOINT ["/usr/local/bin/crosslink-entrypoint.sh"]
