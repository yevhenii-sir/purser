#!/usr/bin/env sh
# Publishes (or upgrades) the billing module on the SpacetimeDB server of docker-compose.yml.
#
#   ./publish.sh                         # download the latest release and publish it to $BILLING_DB
#   ./publish.sh --release v0.1.0        # a specific release
#   ./publish.sh --wasm ../../server/target/wasm32-unknown-unknown/release/purser.wasm
#   ./publish.sh --token                 # print the owner token (admin page sign-in)
#
# The SpacetimeDB CLI runs inside the `cli` container; only Docker and curl are needed on the host.
# The first publish creates the owner identity, kept in the `stdb-cli` volume. Upgrading is the
# same command: it never deletes data (no --delete-data).
set -eu

REPO="yevhenii-sir/purser"
SERVER_URL="http://spacetimedb:3000"

# Git Bash on Windows: keep container paths like /module/... as they are.
export MSYS_NO_PATHCONV=1

usage() {
    sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//'
    exit "${1:-0}"
}

# KEY=VALUE from .env without evaluating it as shell code.
env_value() {
    [ -f .env ] || return 0
    sed -n "s/^[[:space:]]*$1[[:space:]]*=[[:space:]]*//p" .env | tail -n 1 | sed 's/[[:space:]]*$//; s/^"\(.*\)"$/\1/'
}

DB=""
RELEASE=""
WASM=""
SHOW_TOKEN=0

while [ $# -gt 0 ]; do
    case "$1" in
        --wasm) WASM="${2:?--wasm needs a path}"; shift 2 ;;
        --release) RELEASE="${2:?--release needs a tag}"; shift 2 ;;
        --token) SHOW_TOKEN=1; shift ;;
        -h|--help) usage 0 ;;
        -*) echo "Unknown option: $1" >&2; usage 1 ;;
        *) DB="$1"; shift ;;
    esac
done

# --wasm is relative to the caller's directory; everything else to this folder.
if [ -n "$WASM" ]; then
    [ -f "$WASM" ] || { echo "No such file: $WASM" >&2; exit 1; }
    WASM="$(cd "$(dirname "$WASM")" && pwd)/$(basename "$WASM")"
fi
cd "$(dirname "$0")"

DB="${DB:-${BILLING_DB:-$(env_value BILLING_DB)}}"
RELEASE="${RELEASE:-${BILLING_RELEASE:-$(env_value BILLING_RELEASE)}}"
RELEASE="${RELEASE:-latest}"

# `docker compose run` needs -T when there is no terminal (CI, pipes).
TTY_FLAG=""
[ -t 0 ] || TTY_FLAG="-T"

if [ "$SHOW_TOKEN" = 1 ]; then
    exec docker compose run --rm -T cli login show --token
fi

case "$DB" in
    ""|*"<"*|*">"*)
        echo "Set BILLING_DB in .env (copy .env.example) or pass the database name: ./publish.sh my-billing" >&2
        exit 1 ;;
esac
if ! printf '%s' "$DB" | grep -Eq '^[a-z0-9]+(-[a-z0-9]+)*$'; then
    echo "Invalid database name '$DB': lowercase letters and digits separated by dashes." >&2
    exit 1
fi

mkdir -p module
if [ -n "$WASM" ]; then
    cp "$WASM" module/purser.wasm
    rm -f module/purser.wasm.sha256
    echo "Using $WASM"
else
    if [ "$RELEASE" = "latest" ]; then
        BASE="https://github.com/$REPO/releases/latest/download"
    else
        BASE="https://github.com/$REPO/releases/download/$RELEASE"
    fi
    echo "Downloading $BASE/purser.wasm"
    curl -fL --retry 3 -o module/purser.wasm "$BASE/purser.wasm"
    if curl -fsL --retry 3 -o module/purser.wasm.sha256 "$BASE/purser.wasm.sha256"; then
        if command -v sha256sum >/dev/null 2>&1; then
            (cd module && sha256sum -c purser.wasm.sha256)
        elif command -v shasum >/dev/null 2>&1; then
            (cd module && shasum -a 256 -c purser.wasm.sha256)
        else
            echo "No sha256sum / shasum: checksum not verified" >&2
        fi
    else
        echo "No checksum file in the release: checksum not verified" >&2
    fi
fi

docker compose up -d --wait spacetimedb

# remote: the server is not localhost from the CLI container's point of view.
# skip-login: get the owner identity from this server instead of spacetimedb.com.
# shellcheck disable=SC2086
docker compose run --rm $TTY_FLAG cli publish \
    --server "$SERVER_URL" \
    --bin-path /module/purser.wasm \
    --yes=remote,skip-login \
    "$DB"

PORT="$(env_value SPACETIMEDB_PORT)"
PORT="${PORT:-3000}"
cat <<EOF

Published '$DB'.
  Admin page (on this host or through an SSH tunnel):
    http://localhost:$PORT/v1/database/$DB/route/admin
  Owner token for the sign-in:
    ./publish.sh --token
  Billing API base URL for the stores and the Unity config (with the Caddy proxy):
    https://<BILLING_DOMAIN>/v1/database/$DB/route
EOF
