#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP_DIR="$(mktemp -d)"
SERVER_PID=""

cleanup() {
  if [[ -n "${SERVER_PID}" ]]; then
    kill "${SERVER_PID}" >/dev/null 2>&1 || true
    wait "${SERVER_PID}" >/dev/null 2>&1 || true
  fi
  rm -rf "${TMP_DIR}"
}
trap cleanup EXIT

log() {
  printf '\n==> %s\n' "$1"
}

require() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "missing required command: $1" >&2
    exit 1
  }
}

require cargo
require python3

cd "${ROOT_DIR}"

log "Build and unit tests"
cargo test
cargo build --release

BIN="${ROOT_DIR}/target/release/clash-cli"
CONFIG="${TMP_DIR}/config.yaml"
HOME_DIR="${TMP_DIR}/home"
WWW_DIR="${TMP_DIR}/www"
mkdir -p "${HOME_DIR}" "${WWW_DIR}"

log "Create local test subscription"
python3 - "${WWW_DIR}/sub.txt" <<'PY'
import base64
import json
import sys

out = sys.argv[1]
ss_main = base64.b64encode(b"aes-128-gcm:pass@127.0.0.1:8388").decode()
vmess = {
    "v": "2",
    "ps": "VM-Local",
    "add": "127.0.0.1",
    "port": "443",
    "id": "11111111-1111-1111-1111-111111111111",
    "aid": "0",
    "scy": "auto",
    "net": "ws",
    "type": "none",
    "host": "localhost",
    "path": "/ws",
    "tls": "tls",
    "sni": "localhost",
}
vmess_uri = "vmess://" + base64.b64encode(json.dumps(vmess).encode()).decode()
lines = f"ss://{ss_main}#SS-Local\n{vmess_uri}\n"
with open(out, "w", encoding="utf-8") as f:
    f.write(base64.b64encode(lines.encode()).decode())
PY

PORT="$(python3 - <<'PY'
import socket
with socket.socket() as s:
    s.bind(("127.0.0.1", 0))
    print(s.getsockname()[1])
PY
)"
python3 -m http.server "${PORT}" --bind 127.0.0.1 --directory "${WWW_DIR}" \
  >"${TMP_DIR}/server.log" 2>&1 &
SERVER_PID="$!"
SUB_URL="http://127.0.0.1:${PORT}/sub.txt"

for _ in $(seq 1 30); do
  if python3 - "${SUB_URL}" <<'PY' >/dev/null 2>&1
import sys
import urllib.request
urllib.request.urlopen(sys.argv[1], timeout=1).read(1)
PY
  then
    break
  fi
  sleep 0.1
done

log "Verify help"
"${BIN}" --help | grep -q "SUBSCRIPTION_URL"
"${BIN}" --help | grep -q -- "--subscribe"
"${BIN}" --help | grep -q "doctor"

log "Verify first-run subscription save"
HOME="${HOME_DIR}" "${BIN}" \
  --config "${CONFIG}" \
  --subscribe "${SUB_URL}" \
  --mihomo-bin /bin/echo \
  paths >"${TMP_DIR}/paths.out"
grep -q "config saved:" "${TMP_DIR}/paths.out"
grep -q "config: ${CONFIG}" "${TMP_DIR}/paths.out"
grep -q "${SUB_URL}" "${CONFIG}"
grep -q "bin: /bin/echo" "${CONFIG}"

log "Verify update command generates cached runtime"
HOME="${HOME_DIR}" "${BIN}" --config "${CONFIG}" update >"${TMP_DIR}/update.out"
RUNTIME="$(awk -F': ' '/runtime:/ {print $2}' "${TMP_DIR}/paths.out")"
test -f "${RUNTIME}"
grep -q "^proxies:" "${RUNTIME}"
grep -q "^proxy-groups:" "${RUNTIME}"
grep -q "SS-Local" "${RUNTIME}"
grep -q "VM-Local" "${RUNTIME}"
grep -q "MATCH,PROXY" "${RUNTIME}"

log "Verify doctor command reports diagnostics"
HOME="${HOME_DIR}" "${BIN}" --config "${CONFIG}" doctor >"${TMP_DIR}/doctor.out"
grep -q "clash-cli doctor" "${TMP_DIR}/doctor.out"
grep -q "mixed proxy tcp" "${TMP_DIR}/doctor.out"
grep -q "proxy request" "${TMP_DIR}/doctor.out"

log "Verify TUN shortcut writes config"
HOME="${HOME_DIR}" "${BIN}" --config "${CONFIG}" --tun paths >"${TMP_DIR}/tun.out"
python3 - "${CONFIG}" <<'PY'
import sys
import yaml

with open(sys.argv[1], encoding="utf-8") as f:
    cfg = yaml.safe_load(f)
assert cfg["tun"]["enable"] is True
PY

log "Verify explicit init command still works"
INIT_CONFIG="${TMP_DIR}/init-config.yaml"
HOME="${HOME_DIR}" "${BIN}" \
  --config "${INIT_CONFIG}" \
  init \
  --subscription "${SUB_URL}" \
  --mihomo-bin /bin/echo \
  --force
grep -q "${SUB_URL}" "${INIT_CONFIG}"

log "Smoke test passed"
