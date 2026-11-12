#!/usr/bin/env bash
# GeoBoost ground server installer (Ubuntu / Debian, run as root).
#
#   scp -r server root@SERVER_IP:/root/geoboost-server
#   ssh root@SERVER_IP 'cd /root/geoboost-server && bash install-server.sh'
#
# Custom port:  PORT=8444 bash install-server.sh
#
# The ground server is the unmodified satmux.py (https://github.com/Th0rgal/satmux),
# installed as a hardened systemd service with BBR and satellite-friendly TCP settings.
set -euo pipefail

PORT="${PORT:-8444}"
HERE="$(cd "$(dirname "$0")" && pwd)"

if [ "$(id -u)" != "0" ]; then
  echo "Please run as root (sudo bash install-server.sh)." >&2
  exit 1
fi
if [ ! -f "$HERE/satmux.py" ]; then
  echo "satmux.py not found next to this script ($HERE)." >&2
  exit 1
fi

echo "==> Checking Python"
if ! command -v python3 >/dev/null 2>&1; then
  apt-get update -qq && apt-get install -y -qq python3
fi
python3 -c 'import sys; assert sys.version_info >= (3, 9), "Python 3.9+ is required"'

echo "==> Service user and files"
id geoboost >/dev/null 2>&1 || useradd --system --no-create-home --shell /usr/sbin/nologin geoboost
install -d -m 755 /opt/geoboost
install -m 644 "$HERE/satmux.py" /opt/geoboost/satmux.py

echo "==> Pre-shared key"
if [ ! -f /etc/geoboost.env ]; then
  PSK="$(openssl rand -hex 32 2>/dev/null || python3 -c 'import secrets; print(secrets.token_hex(32))')"
  umask 077
  echo "SATMUX_PSK=$PSK" > /etc/geoboost.env
  chmod 600 /etc/geoboost.env
  echo "    Generated a new key."
else
  echo "    Keeping the existing key (/etc/geoboost.env)."
fi
PSK="$(sed -n 's/^SATMUX_PSK=//p' /etc/geoboost.env)"

echo "==> Kernel tuning (BBR, fq, no slow-start after idle)"
cat > /etc/sysctl.d/90-geoboost.conf <<'EOF'
net.core.default_qdisc = fq
net.ipv4.tcp_congestion_control = bbr
net.ipv4.tcp_slow_start_after_idle = 0
net.core.rmem_max = 8388608
net.core.wmem_max = 8388608
net.ipv4.tcp_notsent_lowat = 131072
EOF
modprobe tcp_bbr 2>/dev/null || true
echo tcp_bbr > /etc/modules-load.d/geoboost-bbr.conf
sysctl --system >/dev/null || true
echo "    Congestion control: $(sysctl -n net.ipv4.tcp_congestion_control)"

echo "==> systemd service"
cat > /etc/systemd/system/geoboost.service <<EOF
[Unit]
Description=GeoBoost ground server (satmux protocol)
After=network-online.target
Wants=network-online.target

[Service]
EnvironmentFile=/etc/geoboost.env
ExecStart=/usr/bin/python3 -I /opt/geoboost/satmux.py server --host 0.0.0.0 --port ${PORT}
Restart=always
RestartSec=2
User=geoboost
Group=geoboost
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
EOF
systemctl daemon-reload
systemctl enable --now geoboost.service
systemctl restart geoboost.service

echo "==> Firewall"
if command -v ufw >/dev/null 2>&1 && ufw status | grep -q "Status: active"; then
  ufw allow "${PORT}/tcp" >/dev/null && echo "    ufw: opened ${PORT}/tcp"
else
  echo "    ufw is not active. Remember to allow ${PORT}/tcp in your cloud provider's firewall."
fi

sleep 1
if systemctl is-active --quiet geoboost.service; then
  IP="$(curl -s --max-time 4 https://api.ipify.org 2>/dev/null || hostname -I | awk '{print $1}')"
  echo
  echo "================================================================"
  echo " GeoBoost server is running."
  echo
  echo " Enter these in the GeoBoost app on Windows:"
  echo "   Server address : ${IP}"
  echo "   Server port    : ${PORT}"
  echo "   PSK key        : ${PSK}"
  echo
  echo " Logs:  journalctl -u geoboost -f"
  echo " Stop:  systemctl stop geoboost"
  echo "================================================================"
else
  echo "The service failed to start. Details: journalctl -u geoboost -n 50" >&2
  exit 1
fi
