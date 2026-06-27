#!/usr/bin/env bash
set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

pass() { echo -e "${GREEN}[PASS]${NC} $1"; }
fail() { echo -e "${RED}[FAIL]${NC} $1"; exit 1; }
info() { echo -e "${YELLOW}[INFO]${NC} $1"; }

CLASH_CLI="${1:-./target/debug/clash-cli}"
RUNTIME_YAML="${2:-$("$CLASH_CLI" paths 2>/dev/null | grep "runtime:" | sed 's/runtime: //')}"

echo "=== clash-cli TUN mode integration test ==="
echo "binary: $CLASH_CLI"
echo "runtime: $RUNTIME_YAML"
echo ""

# --- 1. Unit tests ---
info "Running unit tests..."
cargo test --quiet 2>&1 | tail -1
pass "Unit tests passed"

# --- 2. Runtime config has dns.listen ---
info "Checking runtime config for dns.listen..."
if [ -f "$RUNTIME_YAML" ]; then
    if python3 - "$RUNTIME_YAML" <<'PY'
import sys
import yaml

with open(sys.argv[1], encoding="utf-8") as f:
    cfg = yaml.safe_load(f)
assert cfg["dns"]["listen"] == "0.0.0.0:53"
PY
    then
        pass "dns.listen present: 0.0.0.0:53"
    else
        fail "dns.listen not found in $RUNTIME_YAML"
    fi
else
    info "Runtime config not found, generating..."
    $CLASH_CLI update 2>/dev/null
    RUNTIME_YAML=$("$CLASH_CLI" paths 2>/dev/null | grep "runtime:" | sed 's/runtime: //')
    if python3 - "$RUNTIME_YAML" <<'PY'
import sys
import yaml

with open(sys.argv[1], encoding="utf-8") as f:
    cfg = yaml.safe_load(f)
assert cfg["dns"]["listen"] == "0.0.0.0:53"
PY
    then
        pass "dns.listen present after update"
    else
        fail "dns.listen still missing after update"
    fi
fi

# --- 3. TUN section in runtime config ---
info "Checking TUN section..."
if python3 - "$RUNTIME_YAML" <<'PY'
import sys
import yaml

with open(sys.argv[1], encoding="utf-8") as f:
    cfg = yaml.safe_load(f)
assert cfg["tun"]["enable"] is True
PY
then
    pass "TUN enabled in runtime config"
else
    fail "TUN not enabled in runtime config"
fi

# --- 4. Check mihomo process is running ---
info "Checking mihomo process..."
MIHOMO_PID=$(pgrep -f "mihomo" 2>/dev/null | head -1 || true)
if [ -n "$MIHOMO_PID" ]; then
    pass "mihomo running (PID $MIHOMO_PID)"
else
    fail "mihomo not running"
fi

# --- 5. Check TUN interface ---
info "Checking TUN interface..."
TUN_IP=$(ifconfig 2>/dev/null | grep -A1 "utun" | grep "inet 198.18" | awk '{print $2}' || true)
if [ -n "$TUN_IP" ]; then
    pass "TUN interface found: $TUN_IP"
else
    fail "No TUN interface with 198.18.x.x"
fi

# --- 6. Check ports ---
info "Checking mixed proxy port 7890..."
if netstat -an 2>/dev/null | grep -qE "7890.*LISTEN|LISTEN.*7890"; then
    pass "Port 7890 listening"
else
    fail "Port 7890 not listening"
fi

info "Checking controller port 9090..."
if netstat -an 2>/dev/null | grep -qE "9090.*LISTEN|LISTEN.*9090"; then
    pass "Port 9090 listening"
else
    fail "Port 9090 not listening"
fi

# --- 7. Check DNS port 53 ---
info "Checking DNS port 53..."
if netstat -an 2>/dev/null | grep -qE "53.*LISTEN|LISTEN.*53"; then
    pass "Port 53 in use (DNS hijack active)"
else
    info "Port 53 not detected via netstat (may still work via TUN intercept)"
fi

# --- 8. DNS fake-ip test ---
info "Testing DNS fake-ip resolution..."
DNS_RESULT=$(dig +short www.google.com 2>/dev/null || true)
if echo "$DNS_RESULT" | grep -q "^198\.18\."; then
    pass "DNS returns fake-ip: $DNS_RESULT"
else
    info "DNS returns real IP: $DNS_RESULT (DNS may bypass TUN, but proxy still works)"
fi

# --- 9. Proxy connectivity test ---
info "Testing proxy connectivity (cloudflare 204)..."
HTTP_CODE=$(curl -s -o /dev/null -w "%{http_code}" --connect-timeout 10 http://cp.cloudflare.com/generate_204 2>/dev/null || echo "000")
if [ "$HTTP_CODE" = "204" ] || [ "$HTTP_CODE" = "200" ]; then
    pass "Proxy connectivity OK (HTTP $HTTP_CODE)"
else
    fail "Proxy connectivity failed (HTTP $HTTP_CODE)"
fi

# --- 10. Google via proxy ---
info "Testing google.com via proxy..."
HTTP_CODE=$(curl -s -o /dev/null -w "%{http_code}" --connect-timeout 10 -x http://127.0.0.1:7890 https://www.google.com 2>/dev/null || echo "000")
if [ "$HTTP_CODE" = "200" ]; then
    pass "Google via proxy OK (HTTP $HTTP_CODE)"
else
    info "Google via proxy returned HTTP $HTTP_CODE (may depend on proxy nodes)"
fi

# --- 11. Routing table check ---
info "Checking routing table..."
TUN_DEVICE=$(ifconfig 2>/dev/null | awk '
  /^[a-z0-9]+:/ { iface=$1; sub(":", "", iface) }
  /inet 198\.18\./ { print iface; exit }
')
CIDR_ROUTES=0
if [ -n "$TUN_DEVICE" ]; then
    CIDR_ROUTES=$(netstat -rn 2>/dev/null | grep -c "$TUN_DEVICE" || true)
fi
if [ "$CIDR_ROUTES" -gt 3 ]; then
    pass "Routing table has $CIDR_ROUTES $TUN_DEVICE entries"
else
    fail "Routing table missing TUN routes (device=${TUN_DEVICE:-unknown}, found $CIDR_ROUTES)"
fi

echo ""
echo "=== All critical checks passed ==="
