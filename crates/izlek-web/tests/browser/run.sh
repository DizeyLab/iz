#!/usr/bin/env bash
# Runs the browser checks against a throwaway İzlek: its own database, its
# own port, torn down after. Never touches config/izlek.toml or izlek.db —
# the server reads its config from the working directory, so it is given a
# directory of its own.
#
#     crates/izlek-web/tests/browser/run.sh
#
# Playwright is not a dependency of this repo and never will be: İzlek has
# no node in it. It is installed once into ~/.cache/izlek/browser-tests
# (Chromium included, ~120MB) the first time this runs, and found there
# every time after. IZLEK_PLAYWRIGHT and PLAYWRIGHT_BROWSERS_PATH override
# that if the install belongs somewhere else.
set -euo pipefail

repo=$(cd "$(dirname "$0")/../../../.." && pwd)
port=${IZLEK_BROWSER_PORT:-7791}
work=$(mktemp -d)
shots=${SHOT_DIR:-$work}

cleanup() {
    [[ -n ${server:-} ]] && kill "$server" 2>/dev/null || true
    rm -rf "$work"
}
trap cleanup EXIT

# The one-time Playwright install, kept out of the repo and out of the
# session scratchpad so it survives both.
home=${IZLEK_BROWSER_HOME:-${XDG_CACHE_HOME:-$HOME/.cache}/izlek/browser-tests}
export PLAYWRIGHT_BROWSERS_PATH=${PLAYWRIGHT_BROWSERS_PATH:-$home/browsers}
export IZLEK_PLAYWRIGHT=${IZLEK_PLAYWRIGHT:-$home/node_modules/playwright/index.mjs}
if [[ ! -f $IZLEK_PLAYWRIGHT ]]; then
    echo "installing playwright into $home (once)"
    mkdir -p "$home"
    # npm's own postinstall would drag in headed Chromium and ffmpeg — 390MB
    # nothing here launches. The shell build is the whole need.
    (cd "$home" && npm init -y > /dev/null &&
        PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1 npm install playwright > /dev/null)
    (cd "$home" && npx playwright install --only-shell chromium)
fi

# The bundler runs the build itself and rewrites the content-hash files in
# target/<profile>/assets. A plain `cargo build` leaves the previous bundle
# in place, and the server would serve the old css from it — a css change
# would pass every check here while the page shows the last build.
(cd "$repo" && topcoat asset bundle -p izlek-web)

mkdir -p "$work/config"
printf 'database = "izlek.db"\nlisten = "127.0.0.1:%s"\n' "$port" > "$work/config/izlek.toml"

# Started from $work so it claims that directory's config and database, and
# backgrounded here rather than down a pipeline so $! is the server itself —
# a stale pid leaves the port bound and the next run silently talks to the
# old process.
(cd "$work" && exec "$repo/target/debug/izlek-web" > "$work/server.log" 2>&1) &
server=$!

for _ in $(seq 1 60); do
    curl -sf "http://127.0.0.1:$port/healthz" > /dev/null && break
    sleep 0.25
done
if ! curl -sf "http://127.0.0.1:$port/healthz" > /dev/null; then
    echo "the server never came up on $port:"
    cat "$work/server.log"
    exit 1
fi

SHOT_DIR="$shots" node "$repo/crates/izlek-web/tests/browser/soft-nav.mjs" "http://127.0.0.1:$port"

# The moment field's own pass. soft-nav ran first and claimed the
# workspace, so this one signs the same admin in; it claims too when it
# runs standalone. Either script failing fails the run.
SHOT_DIR="$shots" node "$repo/crates/izlek-web/tests/browser/moment.mjs" "http://127.0.0.1:$port"
