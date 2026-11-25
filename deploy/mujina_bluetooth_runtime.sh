#!/bin/sh
# Start the packaged Nano3S audio runtime after the Wi-Fi handoff.
set -eu

archive=/opt/mujina-bluetooth-runtime.tar.gz
run=/tmp/mujina-bluetooth
runtime=$run/root
state=/data/userconfig/bluetooth
if [ ! -r "$archive" ]; then
    echo "Bluetooth audio runtime is missing at $archive"
    exit 0
fi

umask 077
mkdir -p "$run/dbus" "$state"
if [ ! -d "$runtime" ]; then
    mkdir -p "$runtime.new"
    gzip -dc "$archive" | tar -xof - -C "$runtime.new"
    test -x "$runtime.new/usr/bin/dbus-daemon"
    test -x "$runtime.new/usr/libexec/bluetooth/bluetoothd"
    test -x "$runtime.new/usr/bin/bluealsa"
    mkdir -p "$runtime.new/dev" "$runtime.new/run/dbus" "$runtime.new/sys" "$runtime.new/var/lib/bluetooth"
    for node in 'null 1 3' 'zero 1 5' 'random 1 8' 'urandom 1 9'; do
        set -- $node
        [ -e "$runtime.new/dev/$1" ] || mknod -m 600 "$runtime.new/dev/$1" c "$2" "$3"
    done
    mv "$runtime.new" "$runtime"
fi
for target in "$runtime/run/dbus" "$runtime/sys" "$runtime/var/lib/bluetooth"; do
    if ! awk -v path="$target" '$2 == path { found=1 } END { exit !found }' /proc/mounts; then
        case "$target" in
            "$runtime/run/dbus") source=$run/dbus ;;
            "$runtime/sys") source=/sys ;;
            *) source=$state ;;
        esac
        mount -o bind "$source" "$target"
    fi
done
if [ ! -s "$runtime/etc/machine-id" ]; then
    tr -d '-' < /proc/sys/kernel/random/uuid > "$runtime/etc/machine-id"
fi

dbus_pid=
bluez_pid=
bluealsa_pid=
stop_audio() {
    for pid in "$bluealsa_pid" "$bluez_pid" "$dbus_pid"; do
        if [ -n "$pid" ]; then
            kill "$pid" 2>/dev/null || :
            wait "$pid" 2>/dev/null || :
        fi
    done
}
trap 'stop_audio; exit' INT TERM

while true; do
    rm -f "$run/dbus/system_bus_socket"
    DBUS_SYSTEM_BUS_ADDRESS=unix:path=/run/dbus/system_bus_socket
    export DBUS_SYSTEM_BUS_ADDRESS
    chroot "$runtime" /usr/bin/dbus-daemon --nofork --config-file=/etc/mujina-dbus.conf > "$run/dbus.log" 2>&1 &
    dbus_pid=$!
    for attempt in 1 2 3 4 5; do
        [ ! -S "$run/dbus/system_bus_socket" ] || break
        sleep 1
    done
    if [ -S "$run/dbus/system_bus_socket" ]; then
        chroot "$runtime" /usr/libexec/bluetooth/bluetoothd --nodetach --configfile=/etc/bluetooth/mujina.conf > "$run/bluez.log" 2>&1 &
        bluez_pid=$!
        chroot "$runtime" /usr/bin/bluealsa -p a2dp-source > "$run/bluealsa.log" 2>&1 &
        bluealsa_pid=$!
        while kill -0 "$dbus_pid" 2>/dev/null &&
              kill -0 "$bluez_pid" 2>/dev/null &&
              kill -0 "$bluealsa_pid" 2>/dev/null; do
            sleep 2
        done
    fi
    stop_audio
    dbus_pid=
    bluez_pid=
    bluealsa_pid=
    sleep 3
done
