#!/bin/sh
# plutodvb2-quiet: stop PlutoDVB2's DATV services at boot so nothing else drives the AD9361 behind
# corvus-pluto: the MQTT control daemon (polls RSSI/temperature/gain over SPI ~24x/s), the DATV
# streamer, the MQTT command loops and broker, the LO/TX watchers (they wake on every LO write and can
# switch LNB power) and the NFS server (exports /tmp read-write to 192.168.0.0/16).
# Kept: iiod, SSH, DHCP, httpd, the button daemon, the firmware-update watcher, and passthrough.sh's
# one-shot FPGA mixer setup (the RX data path needs it).
# Installed by install.sh (STOP_DATV=1) as /mnt/jffs2/plutodvb2-quiet.sh and started from
# /mnt/jffs2/backdoor.sh, which S95bgcript runs *before* it starts those services, so this waits for them
# first. Log: /tmp/plutodvb2-quiet.log. To undo: delete its line from /mnt/jffs2/backdoor.sh and reboot.
LOG=/tmp/plutodvb2-quiet.log
echo "plutodvb2-quiet $(date)" > $LOG
i=0
while [ $i -lt 60 ] && ! pidof inotifywait >/dev/null; do sleep 1; i=$((i+1)); done
sleep 2

stop_cmd() { # kill processes whose command line contains $1
	for p in /proc/[0-9]*; do
		c=$(tr '\0' ' ' < $p/cmdline 2>/dev/null)
		case "$c" in
		*"$1"*) kill ${p#/proc/} 2>/dev/null && echo "stopped ${p#/proc/}: $c" >> $LOG ;;
		esac
	done
}

for s in watchconsoletx.sh watchconsolefreq.sh mqtt_reboot.sh mqtt_ifconfig.sh mqtt_iptable.sh \
	mqtt_route.sh mqtt_setcall.sh mqtt_longmynd.sh pluto_mqtt_ctrl pluto_stream; do
	stop_cmd "/root/$s"
done
killall inotifywait mosquitto_sub >> $LOG 2>&1
/etc/init.d/S50mosquitto stop >> $LOG 2>&1
/etc/init.d/S60nfs stop >> $LOG 2>&1
/etc/init.d/S30rpcbind stop >> $LOG 2>&1
stop_cmd /usr/sbin/rpcbind # its init script reports OK but leaves it running
echo done >> $LOG
