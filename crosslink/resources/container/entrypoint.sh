#!/bin/bash

set -euo pipefail







if [ -n "${HOST_UID:-}" ] && [ "$(id -u agent)" != "$HOST_UID" ]; then
    echo "[crosslink-entrypoint] Remapping agent UID to $HOST_UID:${HOST_GID:-$HOST_UID}..."
    usermod -u "$HOST_UID" agent 2>/dev/null || true
    groupmod -g "${HOST_GID:-$HOST_UID}" agent 2>/dev/null || true
    chown -R agent:agent /home/agent 2>/dev/null || true
fi





PROVIDER="${CROSSLINK_AGENT_PROVIDER:-claude}"
mkdir -p "/home/agent/.${PROVIDER}"
chown -R agent:agent "/home/agent/.${PROVIDER}" 2>/dev/null || true


AGENT_ID="${AGENT_ID:-container-agent}"
AGENT_HOME=$(getent passwd agent | cut -d: -f6)
GIT_CONFIG="$AGENT_HOME/.gitconfig"
cat > "$GIT_CONFIG" <<GITEOF
[user]
    name = crosslink-agent-${AGENT_ID}
    email = agent@crosslink.local
[safe]
    directory = *
GITEOF
chown agent:agent "$GIT_CONFIG"



WORKSPACE=$(find /workspaces -maxdepth 1 -mindepth 1 -type d 2>/dev/null | head -1)
if [ -n "$WORKSPACE" ]; then
    echo "[crosslink-entrypoint] Detected workspace: $WORKSPACE"

    if [ -f "$WORKSPACE/Cargo.toml" ] || [ -f "$WORKSPACE/crosslink/Cargo.toml" ]; then
        if ! gosu agent bash -c 'command -v cargo' &>/dev/null; then
            echo "[crosslink-entrypoint] Installing Rust toolchain..."
            gosu agent bash -c 'curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --quiet 2>&1'
        else
            echo "[crosslink-entrypoint] Rust toolchain already installed."
        fi
    fi

    if [ -f "$WORKSPACE/package.json" ]; then
        if ! command -v node &>/dev/null; then
            echo "[crosslink-entrypoint] Installing Node.js..."
            curl -fsSL https://deb.nodesource.com/setup_22.x | bash - 2>&1
            apt-get install -y --no-install-recommends nodejs 2>&1
        else
            echo "[crosslink-entrypoint] Node.js already installed."
        fi
    fi

    if [ -f "$WORKSPACE/pyproject.toml" ] || [ -f "$WORKSPACE/requirements.txt" ]; then
        if ! command -v uv &>/dev/null; then
            echo "[crosslink-entrypoint] Installing Python + uv..."
            apt-get update && apt-get install -y --no-install-recommends python3-pip python3-venv 2>&1
            gosu agent pip install --user --break-system-packages uv 2>&1
        else
            echo "[crosslink-entrypoint] Python + uv already installed."
        fi
    fi

    if [ -f "$WORKSPACE/go.mod" ]; then
        if ! command -v go &>/dev/null; then
            echo "[crosslink-entrypoint] Installing Go..."
            GO_ARCH="$(dpkg --print-architecture 2>/dev/null || uname -m)"
            case "$GO_ARCH" in
                amd64|x86_64) GO_ARCH=amd64 ;;
                arm64|aarch64) GO_ARCH=arm64 ;;
            esac
            curl -fsSL "https://go.dev/dl/go1.23.4.linux-${GO_ARCH}.tar.gz" | tar -C /usr/local -xzf - 2>&1
        else
            echo "[crosslink-entrypoint] Go already installed."
        fi
    fi
fi






if [ -n "$WORKSPACE" ] && command -v crosslink &>/dev/null; then
    echo "[crosslink-entrypoint] Initializing crosslink hooks in workspace..."
    gosu agent bash -c "cd '$WORKSPACE' && crosslink init --defaults --agent-integration both --skip-signing" 2>&1
fi

echo "[crosslink-entrypoint] Setup complete. Running command as agent..."

export PATH="/home/agent/.local/bin:/home/agent/.cargo/bin:/usr/local/go/bin:$PATH"
export HOME=/home/agent
exec gosu agent "$@"
