#!/usr/bin/env bash
set -Eeuo pipefail

RINGS_BIN="${RINGS_BIN:-/usr/local/bin/rings}"
NODE_COUNT="${RINGS_NODE_COUNT:-3}"
CLUSTER_DIR="${RINGS_CLUSTER_DIR:-/var/lib/rings-cluster}"
KEYS_FILE="${RINGS_PRIVATE_KEYS_FILE:-/run/secrets/rings-private-keys}"
ALLOW_RANDOM_KEYS="${RINGS_ALLOW_RANDOM_KEYS:-true}"
TOPOLOGY="${RINGS_CONNECT_TOPOLOGY:-ring}"
BASE_INTERNAL_PORT="${RINGS_BASE_INTERNAL_PORT:-50000}"
BASE_EXTERNAL_PORT="${RINGS_BASE_EXTERNAL_PORT:-51000}"
NETWORK_ID="${RINGS_NETWORK_ID:-1}"
ICE_SERVERS="${RINGS_ICE_SERVERS:-stun://stun.l.google.com:19302}"
STABILIZE_INTERVAL="${RINGS_STABILIZE_INTERVAL:-3}"
SESSION_TTL_SECONDS="${RINGS_SESSION_TTL_SECONDS:-2592000}"
READY_RETRIES="${RINGS_READY_RETRIES:-60}"
READY_SLEEP_SECONDS="${RINGS_READY_SLEEP_SECONDS:-1}"
LOG_LEVEL="${RINGS_LOG_LEVEL:-info}"
RUNTIME="${RINGS_RUNTIME:-current-thread}"
WEBRTC_UDP_PORT_MIN="${RINGS_WEBRTC_UDP_PORT_MIN:-}"
WEBRTC_UDP_PORT_MAX="${RINGS_WEBRTC_UDP_PORT_MAX:-}"
STORAGE_CAPACITY="${RINGS_STORAGE_CAPACITY:-200000000}"

if [[ $# -gt 0 ]]; then
    exec "$@"
fi

log() {
    printf '[rings-cluster] %s\n' "$*"
}

die() {
    log "ERROR: $*"
    exit 1
}

is_true() {
    case "${1,,}" in
        1|true|yes|y|on) return 0 ;;
        *) return 1 ;;
    esac
}

require_uint() {
    local name="$1"
    local value="$2"
    [[ "$value" =~ ^[0-9]+$ ]] || die "$name must be a non-negative integer, got '$value'"
}

yaml_quote() {
    local value="$1"
    value="${value//\'/\'\'}"
    printf "'%s'" "$value"
}

normalize_private_key() {
    local key="$1"
    key="${key//$'\r'/}"
    key="${key#"${key%%[![:space:]]*}"}"
    key="${key%"${key##*[![:space:]]}"}"
    key="${key#0x}"
    key="${key#0X}"
    printf '%s' "$key"
}

generate_private_key() {
    od -An -N32 -tx1 /dev/urandom | tr -d ' \n'
}

require_uint RINGS_NODE_COUNT "$NODE_COUNT"
require_uint RINGS_BASE_INTERNAL_PORT "$BASE_INTERNAL_PORT"
require_uint RINGS_BASE_EXTERNAL_PORT "$BASE_EXTERNAL_PORT"
require_uint RINGS_NETWORK_ID "$NETWORK_ID"
require_uint RINGS_STABILIZE_INTERVAL "$STABILIZE_INTERVAL"
require_uint RINGS_SESSION_TTL_SECONDS "$SESSION_TTL_SECONDS"
require_uint RINGS_READY_RETRIES "$READY_RETRIES"
require_uint RINGS_READY_SLEEP_SECONDS "$READY_SLEEP_SECONDS"
require_uint RINGS_STORAGE_CAPACITY "$STORAGE_CAPACITY"

if (( NODE_COUNT < 1 )); then
    die "RINGS_NODE_COUNT must be at least 1"
fi

internal_last_port=$((BASE_INTERNAL_PORT + NODE_COUNT - 1))
external_last_port=$((BASE_EXTERNAL_PORT + NODE_COUNT - 1))
if (( BASE_INTERNAL_PORT <= external_last_port && BASE_EXTERNAL_PORT <= internal_last_port )); then
    die "internal port range ${BASE_INTERNAL_PORT}-${internal_last_port} overlaps external port range ${BASE_EXTERNAL_PORT}-${external_last_port}"
fi

case "$TOPOLOGY" in
    seed|ring|mesh) ;;
    *) die "RINGS_CONNECT_TOPOLOGY must be one of: seed, ring, mesh" ;;
esac

mkdir -p \
    "$CLUSTER_DIR/config" \
    "$CLUSTER_DIR/keys" \
    "$CLUSTER_DIR/logs" \
    "$CLUSTER_DIR/storage" \
    "$CLUSTER_DIR/tmp"
chmod 0700 "$CLUSTER_DIR/keys" "$CLUSTER_DIR/tmp"

declare -a supplied_keys=()
if [[ -f "$KEYS_FILE" ]]; then
    while IFS= read -r line || [[ -n "$line" ]]; do
        line="$(normalize_private_key "$line")"
        [[ -z "$line" || "$line" == \#* ]] && continue
        supplied_keys+=("$line")
    done < "$KEYS_FILE"
    log "loaded ${#supplied_keys[@]} private key entries from $KEYS_FILE (values redacted)"
elif is_true "$ALLOW_RANDOM_KEYS"; then
    log "no private key file found at $KEYS_FILE; generating ephemeral random keys (values redacted)"
else
    die "private key file $KEYS_FILE is missing and RINGS_ALLOW_RANDOM_KEYS is false"
fi

if (( ${#supplied_keys[@]} < NODE_COUNT )) && ! is_true "$ALLOW_RANDOM_KEYS"; then
    die "private key file contains ${#supplied_keys[@]} entries, but RINGS_NODE_COUNT is $NODE_COUNT"
fi

declare -a pids=()
declare -a internal_ports=()
declare -a external_ports=()
declare -a configs=()

cleanup() {
    local code=$?
    trap - EXIT INT TERM
    if (( ${#pids[@]} > 0 )); then
        log "stopping ${#pids[@]} node process(es)"
        for pid in "${pids[@]}"; do
            if kill -0 "$pid" 2>/dev/null; then
                kill "$pid" 2>/dev/null || true
            fi
        done
        for pid in "${pids[@]}"; do
            wait "$pid" 2>/dev/null || true
        done
    fi
    exit "$code"
}

trap cleanup EXIT
trap 'exit 143' INT TERM

write_key_file() {
    local node_index="$1"
    local key_file="$2"
    local key=""

    if (( node_index < ${#supplied_keys[@]} )); then
        key="${supplied_keys[$node_index]}"
    else
        key="$(generate_private_key)"
    fi

    umask 077
    printf '%s\n' "$key" > "$key_file"
}

create_session_file() {
    local node_index="$1"
    local session_file="$2"
    local key_file="$CLUSTER_DIR/tmp/node-${node_index}.key"
    local command_log="$CLUSTER_DIR/tmp/new-session-${node_index}.log"
    local generated_random=false

    if (( node_index >= ${#supplied_keys[@]} )); then
        generated_random=true
    fi

    for _ in $(seq 1 16); do
        write_key_file "$node_index" "$key_file"
        if "$RINGS_BIN" --log-level warn --runtime current-thread new-session \
            --session-sk "$session_file" \
            --key-file "$key_file" \
            --ttl "$SESSION_TTL_SECONDS" >"$command_log" 2>&1; then
            rm -f "$key_file" "$command_log"
            chmod 0600 "$session_file"
            return 0
        fi

        rm -f "$key_file" "$command_log"
        if [[ "$generated_random" != "true" ]]; then
            die "failed to create session key for node $node_index from external private key (value redacted)"
        fi
    done

    die "failed to generate a valid random private key for node $node_index after 16 attempts"
}

write_config() {
    local node_index="$1"
    local config_file="$2"
    local session_file="$3"
    local internal_port="$4"
    local external_port="$5"
    local storage_path="$6"
    local data_path="$storage_path/data"
    local measure_path="$storage_path/measure"

    {
        printf 'network_id: %s\n' "$NETWORK_ID"
        printf 'session_sk: %s\n' "$(yaml_quote "$session_file")"
        printf 'internal_api_port: %s\n' "$internal_port"
        printf 'external_api_addr: %s\n' "$(yaml_quote "0.0.0.0:$external_port")"
        printf 'endpoint_url: %s\n' "$(yaml_quote "http://127.0.0.1:$internal_port")"
        printf 'ice_servers: %s\n' "$(yaml_quote "$ICE_SERVERS")"
        printf 'stabilize_interval: %s\n' "$STABILIZE_INTERVAL"
        printf 'external_ip: null\n'
        if [[ -n "$WEBRTC_UDP_PORT_MIN" || -n "$WEBRTC_UDP_PORT_MAX" ]]; then
            [[ -n "$WEBRTC_UDP_PORT_MIN" && -n "$WEBRTC_UDP_PORT_MAX" ]] \
                || die "RINGS_WEBRTC_UDP_PORT_MIN and RINGS_WEBRTC_UDP_PORT_MAX must be set together"
            require_uint RINGS_WEBRTC_UDP_PORT_MIN "$WEBRTC_UDP_PORT_MIN"
            require_uint RINGS_WEBRTC_UDP_PORT_MAX "$WEBRTC_UDP_PORT_MAX"
            printf 'webrtc_udp_port_min: %s\n' "$WEBRTC_UDP_PORT_MIN"
            printf 'webrtc_udp_port_max: %s\n' "$WEBRTC_UDP_PORT_MAX"
        else
            printf 'webrtc_udp_port_min: null\n'
            printf 'webrtc_udp_port_max: null\n'
        fi
        printf 'data_storage:\n'
        printf '  path: %s\n' "$(yaml_quote "$data_path")"
        printf '  capacity: %s\n' "$STORAGE_CAPACITY"
        printf 'measure_storage:\n'
        printf '  path: %s\n' "$(yaml_quote "$measure_path")"
        printf '  capacity: %s\n' "$STORAGE_CAPACITY"
    } > "$config_file"

    chmod 0600 "$config_file"
}

wait_for_node() {
    local node_index="$1"
    local internal_port="$2"
    local log_file="$3"
    local url="http://127.0.0.1:$internal_port/status"

    for _ in $(seq 1 "$READY_RETRIES"); do
        if curl -fsS "$url" >/dev/null 2>&1; then
            return 0
        fi

        if ! kill -0 "${pids[$node_index]}" 2>/dev/null; then
            log "node $node_index exited before becoming ready; recent log follows"
            tail -n 80 "$log_file" || true
            return 1
        fi

        sleep "$READY_SLEEP_SECONDS"
    done

    log "node $node_index did not become ready at $url; recent log follows"
    tail -n 80 "$log_file" || true
    return 1
}

connect_pair() {
    local source_index="$1"
    local target_index="$2"
    local source_endpoint="http://127.0.0.1:${internal_ports[$source_index]}"
    local target_endpoint="http://127.0.0.1:${external_ports[$target_index]}"
    local cluster_log="$CLUSTER_DIR/logs/connect.log"
    local connect_output=""
    local status=0

    [[ "$source_index" == "$target_index" ]] && return 0

    log "connect node $source_index -> node $target_index"
    if connect_output=$("$RINGS_BIN" --log-level warn --runtime current-thread connect node \
        --config "${configs[$source_index]}" \
        --endpoint-url "$source_endpoint" \
        "$target_endpoint" 2>&1); then
        printf '%s\n' "$connect_output" >> "$cluster_log"
        return 0
    else
        status=$?
    fi

    printf '%s\n' "$connect_output" >> "$cluster_log"
    if grep -Eiq 'Found existing transport|ConnectionAlreadyExists|AlreadyConnected|already connected|connection already exists' <<<"$connect_output"; then
        log "connect node $source_index -> node $target_index already exists; continuing"
        return 0
    fi

    log "connect node $source_index -> node $target_index failed; recent connect log follows"
    tail -n 40 "$cluster_log" || true
    return "$status"
}

log "starting $NODE_COUNT Rings node(s): topology=$TOPOLOGY, internal=$BASE_INTERNAL_PORT+, external=$BASE_EXTERNAL_PORT+"

for i in $(seq 0 $((NODE_COUNT - 1))); do
    internal_port=$((BASE_INTERNAL_PORT + i))
    external_port=$((BASE_EXTERNAL_PORT + i))
    session_file="$CLUSTER_DIR/keys/node-$i.session_sk"
    config_file="$CLUSTER_DIR/config/node-$i.yaml"
    storage_path="$CLUSTER_DIR/storage/node-$i"
    log_file="$CLUSTER_DIR/logs/node-$i.log"

    mkdir -p "$storage_path"
    create_session_file "$i" "$session_file"
    write_config "$i" "$config_file" "$session_file" "$internal_port" "$external_port" "$storage_path"

    internal_ports[$i]="$internal_port"
    external_ports[$i]="$external_port"
    configs[$i]="$config_file"

    "$RINGS_BIN" --log-level "$LOG_LEVEL" --runtime "$RUNTIME" run \
        --config "$config_file" \
        --storage-path "$storage_path" \
        >"$log_file" 2>&1 &
    pids[$i]=$!
    log "node $i pid=${pids[$i]} internal=http://127.0.0.1:$internal_port external=http://0.0.0.0:$external_port log=$log_file"
done

for i in $(seq 0 $((NODE_COUNT - 1))); do
    wait_for_node "$i" "${internal_ports[$i]}" "$CLUSTER_DIR/logs/node-$i.log"
done

case "$TOPOLOGY" in
    seed)
        for i in $(seq 1 $((NODE_COUNT - 1))); do
            connect_pair "$i" 0
        done
        ;;
    ring)
        if (( NODE_COUNT > 1 )); then
            if (( NODE_COUNT == 2 )); then
                connect_pair 0 1
            else
                for i in $(seq 0 $((NODE_COUNT - 1))); do
                    connect_pair "$i" $(((i + 1) % NODE_COUNT))
                done
            fi
        fi
        ;;
    mesh)
        if (( NODE_COUNT > 1 )); then
            for i in $(seq 0 $((NODE_COUNT - 1))); do
                for j in $(seq $((i + 1)) $((NODE_COUNT - 1))); do
                    connect_pair "$i" "$j"
                done
            done
        fi
        ;;
esac

log "cluster ready; private key values were not printed. session keys are stored under $CLUSTER_DIR/keys"
log "node logs are under $CLUSTER_DIR/logs"

while true; do
    for i in $(seq 0 $((NODE_COUNT - 1))); do
        pid="${pids[$i]}"
        if ! kill -0 "$pid" 2>/dev/null; then
            wait "$pid"
            exit $?
        fi
    done
    sleep 1
done
