#!/bin/sh
# Own hci0 in one place. BLE provisioning uses HCI_CHANNEL_USER, so BlueZ
# cannot start until the provisioning process has returned.

wifi_ready() {
    [ -n "$(ifconfig wlan0 2>/dev/null | awk '/inet addr:/ { print $2 }')" ]
}

while ! wifi_ready || [ "${BLE_SETUP_FORCE_ADVERTISE:-0}" = 1 ]; do
    if [ -x /data/ble_setup ]; then
        hciconfig hci0 down 2>/dev/null || :
        /data/ble_setup > /tmp/ble_setup.log 2>&1
    fi
    sleep 3
done

# Leave Wi-Fi reconnects to wpa_supplicant after the handoff. Reclaiming
# hci0 here would disconnect a paired speaker during a Wi-Fi interruption.
if [ "${MUJINA_BUNDLED_BLUETOOTH:-0}" = 1 ]; then
    exec "$(dirname "$0")/mujina_bluetooth_runtime.sh"
fi
bluetoothd=$(command -v bluetoothd || :)
if [ -z "$bluetoothd" ]; then
    for candidate in /usr/libexec/bluetooth/bluetoothd /usr/lib/bluetooth/bluetoothd; do
        if [ -x "$candidate" ]; then
            bluetoothd=$candidate
            break
        fi
    done
fi
bluealsa=$(command -v bluealsad || command -v bluealsa || :)
if [ -z "$bluetoothd" ] || [ -z "$bluealsa" ] || ! command -v dbus-daemon >/dev/null; then
    echo 'Bluetooth audio requires D-Bus, BlueZ with A2DP, and BlueALSA'
    exit 0
fi

umask 077
mkdir -p /run/mujina-mp3 || exit 1
rm -f /run/mujina-mp3/mujina-track.mp3 /run/mujina-mp3/mujina-track.part /run/mujina-mp3/mujina-playback.conf

bluez_pid=
bluealsa_pid=
stop_audio() {
    for pid in "$bluealsa_pid" "$bluez_pid"; do
        if [ -n "$pid" ]; then
            kill "$pid" 2>/dev/null || :
            wait "$pid" 2>/dev/null || :
        fi
    done
}
trap 'stop_audio; exit' INT TERM

while true; do
    if [ ! -S /run/dbus/system_bus_socket ] && [ ! -S /var/run/dbus/system_bus_socket ]; then
        if ! dbus-daemon --system --fork; then
            sleep 3
            continue
        fi
    fi
    "$bluetoothd" --nodetach > /tmp/mujina-bluetoothd.log 2>&1 &
    bluez_pid=$!
    "$bluealsa" -p a2dp-source > /tmp/mujina-bluealsa.log 2>&1 &
    bluealsa_pid=$!
    while kill -0 "$bluez_pid" 2>/dev/null && kill -0 "$bluealsa_pid" 2>/dev/null; do
        sleep 2
    done
    stop_audio
    bluez_pid=
    bluealsa_pid=
    sleep 3
done
