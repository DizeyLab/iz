#!/usr/bin/env bash
# Runs the browser checks against a throwaway İz and a fake im beside it:
# its own database, its own ports, torn down after. Never touches
# config/iz.toml or iz.db — the server reads its config from the working
# directory, so it is given a directory of its own.
#
# There is no password form left to drive: the scripts sign in by cookie.
# run.sh seals one with the mint example and hands it to every script as
# IZ_SESSION_COOKIE; the fake im answers the per-request introspection for
# the minted token, and the first request provisions the workspace owner —
# what the claim form did before SSO.
#
#     crates/iz-web/tests/browser/run.sh
#
# Playwright is not a dependency of this repo and never will be: İz has
# no node in it. It is installed once into ~/.cache/iz/browser-tests
# (Chromium included, ~120MB) the first time this runs, and found there
# every time after. IZ_PLAYWRIGHT and PLAYWRIGHT_BROWSERS_PATH override
# that if the install belongs somewhere else.
set -euo pipefail

repo=$(cd "$(dirname "$0")/../../../.." && pwd)
port=${IZ_BROWSER_PORT:-7791}
im_port=${IZ_BROWSER_IM_PORT:-7792}
token=${IZ_BROWSER_TOKEN:-browser-ada-token}
work=$(mktemp -d)
shots=${SHOT_DIR:-$work}

cleanup() {
    [[ -n ${server:-} ]] && kill "$server" 2>/dev/null || true
    [[ -n ${fakeim:-} ]] && kill "$fakeim" 2>/dev/null || true
    rm -rf "$work"
}
trap cleanup EXIT

# The one-time Playwright install, kept out of the repo and out of the
# session scratchpad so it survives both.
home=${IZ_BROWSER_HOME:-${XDG_CACHE_HOME:-$HOME/.cache}/iz/browser-tests}
export PLAYWRIGHT_BROWSERS_PATH=${PLAYWRIGHT_BROWSERS_PATH:-$home/browsers}
export IZ_PLAYWRIGHT=${IZ_PLAYWRIGHT:-$home/node_modules/playwright/index.mjs}
if [[ ! -f $IZ_PLAYWRIGHT ]]; then
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
(cd "$repo" && topcoat asset bundle -p iz-web)

mkdir -p "$work/config"
# The cookie key, fixed so the mint below seals what the server opens: the
# server reads $work/iz.key beside its database (creating one if absent),
# and the mint example is pointed at the same file.
printf '\007%.0s' $(seq 1 32) > "$work/iz.key"
chmod 600 "$work/iz.key"
cat > "$work/config/iz.toml" <<EOF
database = "iz.db"
listen = "127.0.0.1:$port"
live_seconds = 300
[oidc]
issuer = "http://127.0.0.1:$im_port"
client_id = "iz-browser-test"
client_secret = "anything"
EOF

# The fake im first: iz only calls it per request, but a bound port before
# the server boot keeps the failure modes ordered.
FAKE_IM_TOKEN="$token" node "$repo/crates/iz-web/tests/browser/fake-im.mjs" "$im_port" > "$work/fake-im.log" 2>&1 &
fakeim=$!
for _ in $(seq 1 60); do
    curl -s -o /dev/null "http://127.0.0.1:$im_port/nope" && break
    sleep 0.25
done
if ! curl -s -o /dev/null "http://127.0.0.1:$im_port/nope"; then
    echo "the fake im never came up on $im_port:"
    cat "$work/fake-im.log"
    exit 1
fi

# Started from $work so it claims that directory's config and database, and
# backgrounded here rather than down a pipeline so $! is the server itself —
# a stale pid leaves the port bound and the next run silently talks to the
# old process.
(cd "$work" && exec "$repo/target/debug/iz-web" > "$work/server.log" 2>&1) &
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

# The sign-in: seals the token the fake im knows, exactly like
# /auth/callback would — the scripts carry it as their session cookie.
IZ_SESSION_COOKIE=$(cd "$repo" && cargo run -q -p iz-client --features test-seam --example mint -- "$work/iz.key" "$token")
export IZ_SESSION_COOKIE

SHOT_DIR="$shots" node "$repo/crates/iz-web/tests/browser/soft-nav.mjs" "http://127.0.0.1:$port"

# The moment field's own pass, same minted session and owner as soft-nav.
# Either script failing fails the run.
SHOT_DIR="$shots" node "$repo/crates/iz-web/tests/browser/moment.mjs" "http://127.0.0.1:$port"

# The avatar proxy's own pass, same minted session and owner as the moment
# field. Either script failing fails the run.
SHOT_DIR="$shots" node "$repo/crates/iz-web/tests/browser/photo.mjs" "http://127.0.0.1:$port"
