#!/usr/bin/env bash
# Install corvus-pluto on a Pluto (or Pluto+) and start it:  firmware/pluto/install.sh [HOST]
#   HOST defaults to 192.168.2.1 (the Pluto's USB network link). The login is root; the stock password is
#   "analog" (set PLUTO_PASS and install sshpass to skip typing it).
# The program goes to /mnt/jffs2, the board's small persistent flash area. Firmware that runs
# /mnt/jffs2/backdoor.sh at boot (F5OEO's PlutoDVB2) gets a line there so it starts at every boot; on other
# firmware it runs until the next reboot (run this again, or start it by hand: /mnt/jffs2/corvus-pluto &).
# STOP_DATV=1 (PlutoDVB2 only) also installs plutodvb2-quiet.sh, which stops PlutoDVB2's DATV services at
# every boot: its MQTT daemon polls the AD936x all the time, which keeps corvus-pluto out of fast mode.
set -euo pipefail
HOST=${1:-192.168.2.1}
BIN="$(cd "$(dirname "$0")" && pwd)/target/armv7-unknown-linux-musleabihf/release/corvus-pluto"
[ -x "$BIN" ] || BIN="$(cd "$(dirname "$0")" && pwd)/corvus-pluto" # release archives keep it beside this script
[ -x "$BIN" ] || { echo "no corvus-pluto binary: run build.sh first" >&2; exit 1; }
QUIET="$(cd "$(dirname "$0")" && pwd)/plutodvb2-quiet.sh"
SSH=(ssh -o ConnectTimeout=5 "root@$HOST")
SCP=(scp -O -o ConnectTimeout=5)
if [ -n "${PLUTO_PASS:-}" ] && command -v sshpass >/dev/null; then
  SSH=(sshpass -p "$PLUTO_PASS" "${SSH[@]}")
  SCP=(sshpass -p "$PLUTO_PASS" "${SCP[@]}")
fi
"${SSH[@]}" 'killall corvus-pluto 2>/dev/null; rm -f /mnt/jffs2/corvus-pluto; true'
"${SCP[@]}" "$BIN" "root@$HOST:/mnt/jffs2/corvus-pluto"
"${SSH[@]}" 'set -e
chmod +x /mnt/jffs2/corvus-pluto
if grep -qs /mnt/jffs2/backdoor.sh /etc/init.d/*; then
  touch /mnt/jffs2/backdoor.sh
  grep -q corvus-pluto /mnt/jffs2/backdoor.sh || echo "/mnt/jffs2/corvus-pluto serve >/dev/null 2>&1 &" >> /mnt/jffs2/backdoor.sh
  chmod +x /mnt/jffs2/backdoor.sh
  echo "starts at boot (/mnt/jffs2/backdoor.sh)"
else
  echo "this firmware has no boot hook: corvus-pluto runs until the next reboot"
fi
(/mnt/jffs2/corvus-pluto serve >/dev/null 2>&1 &)
sleep 0.5
/mnt/jffs2/corvus-pluto --version
pidof corvus-pluto >/dev/null && echo "running, port 30433"'
if [ "${STOP_DATV:-}" = 1 ]; then
  "${SSH[@]}" '[ -x /root/pluto_mqtt_ctrl ] && grep -qs /mnt/jffs2/backdoor.sh /etc/init.d/*' ||
    { echo "STOP_DATV: this isn't PlutoDVB2 (no /root/pluto_mqtt_ctrl or no boot hook); nothing to stop" >&2; exit 0; }
  "${SCP[@]}" "$QUIET" "root@$HOST:/mnt/jffs2/plutodvb2-quiet.sh"
  "${SSH[@]}" 'chmod +x /mnt/jffs2/plutodvb2-quiet.sh
grep -q "quiet.sh" /mnt/jffs2/backdoor.sh || echo "/mnt/jffs2/plutodvb2-quiet.sh >/dev/null 2>&1 &" >> /mnt/jffs2/backdoor.sh
/mnt/jffs2/plutodvb2-quiet.sh >/dev/null 2>&1
echo "PlutoDVB2 DATV services stopped, and at every boot (log: /tmp/plutodvb2-quiet.log)"'
fi
