#!/usr/bin/env bash

set -euo pipefail

PROJECT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CONFIG_FILE=""
BACKEND_SERVICE="cyberstrike-ai"
FRONTEND_SERVICE="cyberstrike-chat-web"
SERVICE_USER="${SUDO_USER:-$(id -un)}"
SERVICE_GROUP=""
FRONTEND_HOST="0.0.0.0"
FRONTEND_PORT="4177"
API_BASE_URL=""
BACKEND_HTTPS=""
RESTART_SERVICES=1
ENABLE_SERVICES=1
KILL_LEGACY=1
RUN_HEALTH_CHECK=1
INSTALL_FRONTEND_DEPS=1
INSTALL_GO_DEPS=1

usage() {
  cat <<'USAGE'
Usage:
  scripts/deploy-systemd.sh [options]

Builds CyberStrikeAI from source, installs systemd units, and restarts both
backend and chat-web frontend services.

Options:
  --project-dir DIR          Project root. Default: directory above this script.
  --config FILE              Backend config file. Default: PROJECT_DIR/config.yaml.
  --backend-service NAME     Backend systemd service name. Default: cyberstrike-ai.
  --frontend-service NAME    Frontend systemd service name. Default: cyberstrike-chat-web.
  --user USER                systemd service User. Default: current user, or SUDO_USER.
  --group GROUP              systemd service Group. Default: user's primary group.
  --frontend-host HOST       Vite preview bind host. Default: 0.0.0.0.
  --frontend-port PORT       Vite preview port. Default: 4177.
  --api-base-url URL         Backend URL for frontend /api proxy.
                            Default: inferred as http(s)://<host-ip>:<server.port>.
  --https                    Start backend with --https / CYBERSTRIKE_HTTPS=1.
  --http                     Do not force backend HTTPS.
  --no-enable                Do not run systemctl enable.
  --no-restart               Install units but do not restart services.
  --no-kill-legacy           Do not kill existing non-systemd project processes.
  --no-health-check          Skip post-restart curl checks.
  --skip-npm-install         Skip npm install in apps/chat-web.
  --skip-go-mod-download     Skip go mod download.
  -h, --help                 Show this help.

Environment:
  GOPROXY                    Used by go mod download/build when set.
  NPM_CONFIG_REGISTRY        Used by npm when set.

Examples:
  scripts/deploy-systemd.sh
  scripts/deploy-systemd.sh --api-base-url http://192.168.64.2:51282
  sudo scripts/deploy-systemd.sh --project-dir /home/user/CyberStrikeAI --user user
USAGE
}

log() {
  printf '[deploy] %s\n' "$*"
}

fail() {
  printf '[deploy] ERROR: %s\n' "$*" >&2
  exit 1
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || fail "required command not found: $1"
}

sudo_cmd() {
  if [ "$(id -u)" -eq 0 ]; then
    "$@"
  else
    sudo "$@"
  fi
}

abs_path() {
  local p="$1"
  if [[ "$p" = /* ]]; then
    printf '%s\n' "$p"
  else
    printf '%s\n' "$PROJECT_DIR/$p"
  fi
}

parse_server_value() {
  local key="$1"
  local default_value="$2"
  awk -v wanted="$key" -v default_value="$default_value" '
    /^server:[[:space:]]*$/ { in_server=1; next }
    in_server && /^[^[:space:]#]/ { in_server=0 }
    in_server {
      line=$0
      sub(/[[:space:]]*#.*/, "", line)
      pattern="^[[:space:]]+" wanted ":[[:space:]]*"
      if (line ~ pattern) {
        sub(pattern, "", line)
        gsub(/^["'\'']|["'\'']$/, "", line)
        print line
        found=1
        exit
      }
    }
    END { if (!found) print default_value }
  ' "$CONFIG_FILE"
}

infer_public_host() {
  if command -v hostname >/dev/null 2>&1; then
    local ip
    ip="$(hostname -I 2>/dev/null | awk '{print $1}' || true)"
    if [ -n "$ip" ]; then
      printf '%s\n' "$ip"
      return
    fi
  fi
  printf '127.0.0.1\n'
}

bool_true() {
  case "${1,,}" in
    1|true|yes|on) return 0 ;;
    *) return 1 ;;
  esac
}

quote_systemd_env() {
  local value="$1"
  value="${value//\\/\\\\}"
  value="${value//\"/\\\"}"
  printf '"%s"' "$value"
}

write_backend_unit() {
  local unit_file="$1"
  local backend_bin="$2"
  local https_env="0"
  local https_arg=()
  if bool_true "$BACKEND_HTTPS"; then
    https_env="1"
    https_arg=(--https)
  fi

  cat > "$unit_file" <<UNIT
[Unit]
Description=CyberStrikeAI backend
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=$SERVICE_USER
Group=$SERVICE_GROUP
WorkingDirectory=$PROJECT_DIR
Environment=CYBERSTRIKE_HTTPS=$https_env
Environment=PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
ExecStart=$backend_bin -config $CONFIG_FILE ${https_arg[*]}
Restart=on-failure
RestartSec=3
KillSignal=SIGTERM
KillMode=control-group
TimeoutStopSec=30
LimitNOFILE=65535

[Install]
WantedBy=multi-user.target
UNIT
}

write_frontend_unit() {
  local unit_file="$1"
  local vite_bin="$2"
  local frontend_dir="$PROJECT_DIR/apps/chat-web"
  local api_env
  api_env="$(quote_systemd_env "$API_BASE_URL")"

  cat > "$unit_file" <<UNIT
[Unit]
Description=CyberStrikeAI chat-web frontend
After=network-online.target $BACKEND_SERVICE.service
Wants=network-online.target

[Service]
Type=simple
User=$SERVICE_USER
Group=$SERVICE_GROUP
WorkingDirectory=$frontend_dir
Environment=NODE_ENV=production
Environment=VITE_API_BASE_URL=$api_env
Environment=PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
ExecStart=$vite_bin preview --host $FRONTEND_HOST --port $FRONTEND_PORT --strictPort
Restart=on-failure
RestartSec=3
KillSignal=SIGTERM
KillMode=control-group
TimeoutStopSec=20

[Install]
WantedBy=multi-user.target
UNIT
}

matching_pids() {
  local pattern="$1"
  ps -eo pid=,args= | awk -v pattern="$pattern" -v self="$$" '
    index($0, pattern) > 0 {
      pid=$1
      if (pid != self && index($0, "deploy-systemd.sh") == 0 && index($0, "awk -v pattern=") == 0) {
        print pid
      }
    }
  '
}

stop_matching_processes() {
  local label="$1"
  local pattern="$2"
  local pid
  while read -r pid; do
    [ -n "$pid" ] || continue
    log "Stopping legacy $label process $pid"
    sudo_cmd kill -TERM "$pid" 2>/dev/null || true
  done < <(matching_pids "$pattern")
}

force_stop_matching_processes() {
  local label="$1"
  local pattern="$2"
  local pid
  while read -r pid; do
    [ -n "$pid" ] || continue
    log "Force stopping legacy $label process $pid"
    sudo_cmd kill -KILL "$pid" 2>/dev/null || true
  done < <(matching_pids "$pattern")
}

kill_legacy_processes() {
  [ "$KILL_LEGACY" -eq 1 ] || return 0
  local backend_bin="$PROJECT_DIR/cyberstrike-ai"
  local runtime_bin="$PROJECT_DIR/agent-runtime/target/release/cyberstrike-agent-runtime"

  stop_matching_processes "backend" "$backend_bin"
  stop_matching_processes "agent-runtime" "$runtime_bin"
  stop_matching_processes "chat-web" "$PROJECT_DIR/apps/chat-web"

  sleep 1

  force_stop_matching_processes "backend" "$backend_bin"
  force_stop_matching_processes "agent-runtime" "$runtime_bin"
  force_stop_matching_processes "chat-web" "$PROJECT_DIR/apps/chat-web"
}

build_backend() {
  local build_dir="$PROJECT_DIR/.deploy-build"
  local output="$build_dir/cyberstrike-ai"
  mkdir -p "$build_dir"

  log "Building Go backend"
  if [ "$INSTALL_GO_DEPS" -eq 1 ]; then
    (cd "$PROJECT_DIR" && go mod download)
  fi
  (cd "$PROJECT_DIR" && go build -o "$output" cmd/server/main.go)
  install -m 0755 "$output" "$PROJECT_DIR/cyberstrike-ai"
}

build_runtime() {
  log "Building Rust Agent Runtime"
  (cd "$PROJECT_DIR/agent-runtime" && cargo build --release)
}

build_frontend() {
  log "Building chat-web frontend"
  if [ "$INSTALL_FRONTEND_DEPS" -eq 1 ]; then
    if [ -f "$PROJECT_DIR/apps/chat-web/package-lock.json" ]; then
      (cd "$PROJECT_DIR/apps/chat-web" && npm ci)
    else
      (cd "$PROJECT_DIR/apps/chat-web" && npm install --no-package-lock --no-audit --no-fund)
    fi
  fi
  (cd "$PROJECT_DIR/apps/chat-web" && env -u VITE_API_BASE_URL npm run build)
}

install_units() {
  local build_dir="$PROJECT_DIR/.deploy-build"
  local backend_unit="$build_dir/$BACKEND_SERVICE.service"
  local frontend_unit="$build_dir/$FRONTEND_SERVICE.service"
  local vite_bin

  vite_bin="$PROJECT_DIR/apps/chat-web/node_modules/.bin/vite"
  [ -x "$vite_bin" ] || fail "vite executable not found after frontend install: $vite_bin"
  mkdir -p "$build_dir"
  write_backend_unit "$backend_unit" "$PROJECT_DIR/cyberstrike-ai"
  write_frontend_unit "$frontend_unit" "$vite_bin"

  log "Installing systemd units"
  sudo_cmd install -m 0644 "$backend_unit" "/etc/systemd/system/$BACKEND_SERVICE.service"
  sudo_cmd install -m 0644 "$frontend_unit" "/etc/systemd/system/$FRONTEND_SERVICE.service"
  sudo_cmd systemctl daemon-reload

  if [ "$ENABLE_SERVICES" -eq 1 ]; then
    sudo_cmd systemctl enable "$BACKEND_SERVICE.service" "$FRONTEND_SERVICE.service"
  fi
}

restart_units() {
  [ "$RESTART_SERVICES" -eq 1 ] || return 0
  log "Restarting systemd services"
  sudo_cmd systemctl stop "$FRONTEND_SERVICE.service" "$BACKEND_SERVICE.service" 2>/dev/null || true
  kill_legacy_processes
  sudo_cmd systemctl reset-failed "$BACKEND_SERVICE.service" "$FRONTEND_SERVICE.service" 2>/dev/null || true
  sudo_cmd systemctl restart "$BACKEND_SERVICE.service"
  sudo_cmd systemctl restart "$FRONTEND_SERVICE.service"
}

wait_service_active() {
  local service="$1"
  local i
  for i in $(seq 1 20); do
    if sudo_cmd systemctl is-active --quiet "$service"; then
      return 0
    fi
    sleep 1
  done
  sudo_cmd systemctl --no-pager --full status "$service" || true
  return 1
}

wait_url() {
  local url="$1"
  local insecure="${2:-0}"
  local i
  for i in $(seq 1 20); do
    if [ "$insecure" = "1" ]; then
      if curl -kfsS --max-time 5 "$url" >/dev/null 2>&1; then
        return 0
      fi
    else
      if curl -fsS --max-time 5 "$url" >/dev/null 2>&1; then
        return 0
      fi
    fi
    sleep 1
  done
  if [ "$insecure" = "1" ]; then
    curl -kfsS --max-time 10 "$url" >/dev/null
  else
    curl -fsS --max-time 10 "$url" >/dev/null
  fi
}

health_check() {
  [ "$RUN_HEALTH_CHECK" -eq 1 ] || return 0
  local backend_port="$1"
  local backend_http="http://127.0.0.1:$backend_port/api/config"
  local backend_https="https://127.0.0.1:$backend_port/api/config"
  local frontend_url="http://127.0.0.1:$FRONTEND_PORT/"
  local frontend_api_url="http://127.0.0.1:$FRONTEND_PORT/api/config"

  log "Checking service status"
  wait_service_active "$BACKEND_SERVICE.service"
  wait_service_active "$FRONTEND_SERVICE.service"

  log "Checking backend health"
  if ! wait_url "$backend_http" 0 >/dev/null 2>&1; then
    wait_url "$backend_https" 1 >/dev/null
  fi

  log "Checking frontend health"
  wait_url "$frontend_url" 0 >/dev/null

  log "Checking frontend API proxy"
  wait_url "$frontend_api_url" 0 >/dev/null
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --project-dir) PROJECT_DIR="$(cd "$2" && pwd)"; shift 2 ;;
    --config) CONFIG_FILE="$2"; shift 2 ;;
    --backend-service) BACKEND_SERVICE="$2"; shift 2 ;;
    --frontend-service) FRONTEND_SERVICE="$2"; shift 2 ;;
    --user) SERVICE_USER="$2"; shift 2 ;;
    --group) SERVICE_GROUP="$2"; shift 2 ;;
    --frontend-host) FRONTEND_HOST="$2"; shift 2 ;;
    --frontend-port) FRONTEND_PORT="$2"; shift 2 ;;
    --api-base-url) API_BASE_URL="$2"; shift 2 ;;
    --https) BACKEND_HTTPS="true"; shift ;;
    --http) BACKEND_HTTPS="false"; shift ;;
    --no-enable) ENABLE_SERVICES=0; shift ;;
    --no-restart) RESTART_SERVICES=0; shift ;;
    --no-kill-legacy) KILL_LEGACY=0; shift ;;
    --no-health-check) RUN_HEALTH_CHECK=0; shift ;;
    --skip-npm-install) INSTALL_FRONTEND_DEPS=0; shift ;;
    --skip-go-mod-download) INSTALL_GO_DEPS=0; shift ;;
    -h|--help) usage; exit 0 ;;
    *) fail "unknown option: $1" ;;
  esac
done

CONFIG_FILE="${CONFIG_FILE:-$PROJECT_DIR/config.yaml}"
CONFIG_FILE="$(abs_path "$CONFIG_FILE")"
SERVICE_GROUP="${SERVICE_GROUP:-$(id -gn "$SERVICE_USER" 2>/dev/null || id -gn)}"

[ -d "$PROJECT_DIR" ] || fail "project directory not found: $PROJECT_DIR"
[ -f "$CONFIG_FILE" ] || fail "config file not found: $CONFIG_FILE"
[ -d "$PROJECT_DIR/agent-runtime" ] || fail "agent-runtime directory not found"
[ -f "$PROJECT_DIR/apps/chat-web/package.json" ] || fail "apps/chat-web/package.json not found"

need_cmd awk
need_cmd cargo
need_cmd curl
need_cmd go
need_cmd install
need_cmd npm
need_cmd pgrep
need_cmd readlink
need_cmd systemctl

SERVER_PORT="$(parse_server_value port 8080)"
SERVER_TLS_ENABLED="$(parse_server_value tls_enabled false)"
SERVER_TLS_AUTO_SELF_SIGN="$(parse_server_value tls_auto_self_sign false)"
SERVER_TLS_CERT="$(parse_server_value tls_cert_path "")"
SERVER_TLS_KEY="$(parse_server_value tls_key_path "")"

if [ -z "$BACKEND_HTTPS" ]; then
  if bool_true "$SERVER_TLS_ENABLED" || bool_true "$SERVER_TLS_AUTO_SELF_SIGN" || { [ -n "$SERVER_TLS_CERT" ] && [ -n "$SERVER_TLS_KEY" ]; }; then
    BACKEND_HTTPS="true"
  else
    BACKEND_HTTPS="false"
  fi
fi

if [ -z "$API_BASE_URL" ]; then
  scheme="http"
  if bool_true "$BACKEND_HTTPS"; then
    scheme="https"
  fi
  API_BASE_URL="$scheme://$(infer_public_host):$SERVER_PORT"
fi

log "Project: $PROJECT_DIR"
log "Config: $CONFIG_FILE"
log "Backend service: $BACKEND_SERVICE"
log "Frontend service: $FRONTEND_SERVICE"
log "Service user/group: $SERVICE_USER:$SERVICE_GROUP"
log "Backend port: $SERVER_PORT"
log "Frontend URL: http://127.0.0.1:$FRONTEND_PORT/"
log "Frontend API proxy target: $API_BASE_URL"

build_backend
build_runtime
build_frontend
install_units
restart_units
health_check "$SERVER_PORT"

log "Deployment complete"
log "Backend: systemctl status $BACKEND_SERVICE"
log "Frontend: systemctl status $FRONTEND_SERVICE"
