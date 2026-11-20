#!/usr/bin/env bash
# One-shot local setup: checks the toolchain, builds the engine, then starts the console.
set -euo pipefail
cd "$(dirname "$0")/.."

need() { command -v "$1" >/dev/null 2>&1 || { echo "missing: $1 ($2)"; exit 1; }; }
need git  "https://git-scm.com"
need node "Node 22+: https://nodejs.org"
need cargo "Rust: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
need baw  "npm i -g @binance/agentic-wallet"

node_major=$(node -p 'process.versions.node.split(".")[0]')
[ "$node_major" -ge 22 ] || { echo "Node $node_major found, 22+ required"; exit 1; }

git submodule update --init --recursive
echo "building engine (first build takes a few minutes)…"
(cd shooter && cargo build --release --locked)

echo "checking wallet session…"
if ! NODE_USE_ENV_PROXY=1 baw wallet status --json | grep -q CONNECTED; then
  echo "wallet not signed in. Run: baw auth signin   (scan with the Binance Wallet app), then re-run this script."
  exit 1
fi

echo "starting console on http://127.0.0.1:${PORT:-4780}"
exec node console/server.mjs
