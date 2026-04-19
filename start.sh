#!/bin/sh

set -eu

[ -f /home/root/mujina.env ] && . /home/root/mujina.env

: "${MUJINA_LOG_LEVEL:=debug}"
: "${MUJINA_CONFIG:=/home/root/mujina-hb2.toml}"
: "${MUJINA_POOL_URL:=stratum+tcp://pool.256foundation.org:3333}"
: "${MUJINA_POOL_USER:=npub1ql2zzp3g6yndgz05js7wdc4qkr88wkyne5nw2cc7csrtzqs0yeesgwrxya.mujina-jPro-amlogic}"
: "${MUJINA_POOL_PASS:=x}"
: "${MUJINA_API_LISTEN:=0.0.0.0:7785}"
: "${MUJINA_TARGET_FREQ_MHZ:=500}"

export RUST_LOG="${RUST_LOG:-${MUJINA_LOG_LEVEL}}"
export MUJINA_CONFIG
export MUJINA_POOL_URL
export MUJINA_POOL_USER
export MUJINA_POOL_PASS
export MUJINA_API_LISTEN
export MUJINA_TARGET_FREQ_MHZ

exec /home/root/mujina-minerd
