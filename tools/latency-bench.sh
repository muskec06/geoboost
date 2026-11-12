#!/usr/bin/env bash
# Reproduce the README latency benchmark on one Linux machine (root is needed for port 443).
#
#   ./tools/latency-bench.sh            # 290 ms one-way (≈583 ms RTT), 5 Mbps
#   OW=300 RATE=10 ./tools/latency-bench.sh
#
# Topology:  curl ──(satrelay: OW ms, RATE Mbps)── origin            = "Direct"
#            curl ── GeoBoost client ──(satrelay)── satmux.py ── origin = "GeoBoost"
set -euo pipefail
OW="${OW:-290}"; RATE="${RATE:-5}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"; W="$(mktemp -d)"; trap 'kill $(jobs -p) 2>/dev/null; rm -rf "$W"' EXIT
for v in $(env | grep -i _proxy | cut -d= -f1); do unset "$v"; done

echo "==> Preparing ($W)"
mkdir -p "$W/www" "$W/home"
head -c 1200 /dev/urandom | base64 > "$W/www/small.txt"
head -c 2000000 /dev/urandom > "$W/www/rnd2m.bin"
python3 - "$W/www/page.html" <<'PY'
import random, sys
w = "satellite latency compression carrier tunnel packet connection server client browser".split()
open(sys.argv[1], "w").write("<html>" + " ".join(random.choice(w) for _ in range(400000)) + "</html>")
PY
openssl req -x509 -newkey rsa:2048 -nodes -keyout "$W/key.pem" -out "$W/cert.pem" -days 1 -subj /CN=localhost 2>/dev/null
PSK="$(openssl rand -hex 32)"
(cd "$ROOT" && cargo build --release -q)

python3 -I "$ROOT/tools/origins.py" "$W/www" "$W/cert.pem" "$W/key.pem" &
SATMUX_PSK="$PSK" python3 -I "$ROOT/server/satmux.py" server --host 127.0.0.1 --port 8444 >/dev/null 2>&1 &
python3 -I "$ROOT/tools/satrelay.py" 19444 127.0.0.1 8444 "$OW" "$RATE" &
python3 -I "$ROOT/tools/satrelay.py" 19000 127.0.0.1 9000 "$OW" "$RATE" &
python3 -I "$ROOT/tools/satrelay.py" 19443 127.0.0.1 443  "$OW" "$RATE" &
cat > "$W/home/config.json" <<JSON
{"server_host":"127.0.0.1","server_port":19444,"psk_protected":"plain:$PSK","carriers":12,
 "socks_port":31080,"http_port":38080,"gate_port":38081,"panel_port":38090}
JSON
sleep 1
GEOBOOST_HOME="$W/home" "$ROOT/target/release/geoboost" --connect >/dev/null 2>&1 &
sleep 8

med(){ sort -n | awk '{a[NR]=$1} END{printf "%.2f", a[int((NR+1)/2)]}'; }
run(){ for _ in 1 2 3; do curl -s -k -o /dev/null --max-time 120 -w "%{time_total}\n" "$@"; done | med; }
burst(){ local t0; t0=$(date +%s.%N); for _ in 1 2 3 4 5 6; do curl -s -k -o /dev/null --max-time 60 "$@" & done; wait; printf "%.2f" "$(echo "$(date +%s.%N) - $t0" | bc)"; }
S="--socks5-hostname 127.0.0.1:31080"; G="http://127.0.0.1:38081"
printf "\n%-30s %10s %10s %10s\n" "Scenario (RTT $((OW*2)) ms, ${RATE} Mbps)" "Direct" "Tunnel" "1-RTT"
printf "%-30s %10s %10s %10s\n" "HTTPS small page"  "$(run https://localhost:19443/small.txt)" "$(run $S https://localhost/small.txt)" "-"
printf "%-30s %10s %10s %10s\n" "6 parallel HTTPS"    "$(burst https://localhost:19443/small.txt)" "$(burst $S https://localhost/small.txt)" "-"
printf "%-30s %10s %10s %10s\n" "HTTP small page"   "$(run http://127.0.0.1:19000/small.txt)" "$(run $S http://127.0.0.1:9000/small.txt)" "$(run $G/http://127.0.0.1:9000/small.txt)"
printf "%-30s %10s %10s %10s\n" "HTTP 3 MB text"    "$(run http://127.0.0.1:19000/page.html)" "$(run $S http://127.0.0.1:9000/page.html)" "$(run $G/http://127.0.0.1:9000/page.html)"
printf "%-30s %10s %10s %10s\n" "HTTPS 3 MB text"   "$(run https://localhost:19443/page.html)" "$(run $S https://localhost/page.html)" "-"
printf "%-30s %10s %10s %10s\n" "HTTPS 2 MB random" "$(run https://localhost:19443/rnd2m.bin)" "$(run $S https://localhost/rnd2m.bin)" "-"
