#!/usr/bin/env bash
# Runs the console as a macOS login service (launchd): starts at login, restarts if it crashes.
# The daemon is never resumed automatically: after any restart, monitoring stays off until you start it.
#   ./scripts/service-macos.sh install | uninstall | status | logs | restart
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$(pwd)
LABEL=com.shooter.console
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
LOG="$ROOT/data/console.log"
PORT=${PORT:-4780}
DOMAIN="gui/$(id -u)"

xml() { sed -e 's/&/\&amp;/g' -e 's/</\&lt;/g' -e 's/>/\&gt;/g' <<<"$1"; }

case "${1:-}" in
install)
  NODE=$(command -v node) || { echo "node not found"; exit 1; }
  BAW_DIR=$(dirname "$(command -v baw)") || { echo "baw not found: npm i -g @binance/agentic-wallet"; exit 1; }
  [ -x shooter/target/release/shooter ] || { echo "build first: (cd shooter && cargo build --release --locked)"; exit 1; }
  mkdir -p "$HOME/Library/LaunchAgents" data && chmod 700 data
  PROXY=${HTTPS_PROXY:-${https_proxy:-}}
  {
    echo '<?xml version="1.0" encoding="UTF-8"?>'
    echo '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">'
    echo '<plist version="1.0"><dict>'
    echo "  <key>Label</key><string>$LABEL</string>"
    echo "  <key>ProgramArguments</key><array><string>$(xml "$NODE")</string><string>$(xml "$ROOT/console/server.mjs")</string></array>"
    echo "  <key>WorkingDirectory</key><string>$(xml "$ROOT")</string>"
    echo '  <key>EnvironmentVariables</key><dict>'
    echo "    <key>PATH</key><string>$(xml "$BAW_DIR:$(dirname "$NODE"):/usr/bin:/bin:/usr/sbin:/sbin")</string>"
    echo "    <key>PORT</key><string>$PORT</string>"
    echo '    <key>NODE_USE_ENV_PROXY</key><string>1</string>'
    if [ -n "$PROXY" ]; then
      echo "    <key>HTTPS_PROXY</key><string>$(xml "$PROXY")</string>"
      echo "    <key>HTTP_PROXY</key><string>$(xml "${HTTP_PROXY:-$PROXY}")</string>"
      echo '    <key>NO_PROXY</key><string>localhost,127.0.0.1,::1</string>'
    fi
    echo '  </dict>'
    echo '  <key>RunAtLoad</key><true/>'
    echo '  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>'
    echo '  <key>ThrottleInterval</key><integer>10</integer>'
    echo '  <key>ProcessType</key><string>Background</string>'
    echo "  <key>StandardOutPath</key><string>$(xml "$LOG")</string>"
    echo "  <key>StandardErrorPath</key><string>$(xml "$LOG")</string>"
    echo '</dict></plist>'
  } > "$PLIST"
  plutil -lint "$PLIST" >/dev/null
  launchctl bootout "$DOMAIN/$LABEL" 2>/dev/null || true
  # Stop a console started by hand on the same port, so launchd can take it over.
  pkill -f "$ROOT/console/server.mjs" 2>/dev/null || pkill -f "node console/server.mjs" 2>/dev/null || true
  sleep 1
  launchctl bootstrap "$DOMAIN" "$PLIST"
  echo "installed. console: http://127.0.0.1:$PORT   logs: $LOG"
  ;;
uninstall)
  launchctl bootout "$DOMAIN/$LABEL" 2>/dev/null || true
  rm -f "$PLIST"
  echo "removed"
  ;;
restart) launchctl kickstart -k "$DOMAIN/$LABEL" && echo "restarted (monitoring is off until you start it in the console)" ;;
status)  launchctl print "$DOMAIN/$LABEL" 2>/dev/null | grep -E "^\s*(state|pid|last exit code) =" || echo "not installed" ;;
logs)    tail -n 50 -f "$LOG" ;;
*) echo "usage: $0 install | uninstall | status | logs | restart"; exit 1 ;;
esac
