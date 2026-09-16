#!/bin/sh
# vise installer: stands up the full vise stack on a developer machine.
#
#   curl -fsSL https://vise.sh/install | sh
#
# Everything lives under ~/.vise and nothing needs sudo:
#
#   ~/.vise/.env                 config: GitHub PAT, Postgres password, host token
#   ~/.vise/docker-compose.yml   postgres + ghcr.io/vise-sh/vise-server
#   ~/.vise/bin/                 vise (the CLI) and vise-host
#   ~/.vise/logs/host.log        vise-host output (`vise host logs`)
#
# Steps, each of which fails loudly on its own:
#   1. prerequisites: docker (with compose) and git; claude is optional
#   2. config: read VISE_GITHUB_PAT or prompt for it on the terminal
#   3. docker compose up -d, then wait for the server to answer
#   4. download the vise-cli and vise-host release archives for this OS/arch
#      and check them against the SHA256SUMS.txt attached to the GitHub release
#   5. enroll this machine as a host and start vise-host in the background
#
# Re-running is safe: an existing ~/.vise/.env is kept unless you say
# otherwise at the prompt, binaries are upgraded in place and the containers
# are only recreated when their image or config changed.
#
# Optional environment overrides:
#   VISE_GITHUB_PAT     GitHub personal access token (prompted for otherwise)
#   VISE_HOME           install directory (default: ~/.vise)
#   VISE_VERSION        release tag to install, e.g. v0.2.0 (default: latest)
#   VISE_SERVER_TAG     vise-server image tag (default: VISE_VERSION, i.e. latest)
#   VISE_SERVER_IMAGE   vise-server image (default: ghcr.io/vise-sh/vise-server)
#   VISE_PORT           local port for the API (default: 3000)
#   VISE_HOST_NAME      name to enroll this machine under (default: hostname)
#   VISE_DOWNLOAD_BASE  base URL for release archives, for mirrors
#                       (default: the GitHub release for VISE_VERSION)
#   VISE_CHECKSUMS_URL  URL of the SHA256SUMS.txt to verify archives against
#                       (default: the one attached to the GitHub release for
#                       VISE_VERSION, even when VISE_DOWNLOAD_BASE is a mirror)
#
# Uninstall:
#   ~/.vise/bin/vise host stop
#   docker compose -f ~/.vise/docker-compose.yml down -v
#   rm -rf ~/.vise

set -eu

VISE_HOME="${VISE_HOME:-$HOME/.vise}"
VISE_REPO="${VISE_REPO:-vise-sh/vise}"
VISE_VERSION="${VISE_VERSION:-latest}"
# Remember which knobs were given explicitly: on a re-run they override the
# values kept in ~/.vise/.env, everything else in that file is left alone.
REQUESTED_SERVER_IMAGE="${VISE_SERVER_IMAGE:-}"
REQUESTED_SERVER_TAG="${VISE_SERVER_TAG:-}"
REQUESTED_PORT="${VISE_PORT:-}"
VISE_SERVER_IMAGE="${VISE_SERVER_IMAGE:-ghcr.io/vise-sh/vise-server}"
VISE_SERVER_TAG="${VISE_SERVER_TAG:-$VISE_VERSION}"
VISE_PORT="${VISE_PORT:-3000}"
VISE_HOST_NAME="${VISE_HOST_NAME:-}"
VISE_DOWNLOAD_BASE="${VISE_DOWNLOAD_BASE:-}"
VISE_CHECKSUMS_URL="${VISE_CHECKSUMS_URL:-}"
VISE_GITHUB_PAT="${VISE_GITHUB_PAT:-}"
export VISE_HOME
# The CLI prefers VISE_HOST_TOKEN from the environment over ~/.vise/.env. A
# token inherited from an unrelated shell (say, another vise host) would then
# silently win over the one enrolled below, so drop it for this run.
unset VISE_HOST_TOKEN

# The CLI reads the same directory, so make sure it agrees with us.
ENV_FILE="$VISE_HOME/.env"
COMPOSE_FILE="$VISE_HOME/docker-compose.yml"
BIN_DIR="$VISE_HOME/bin"
VISE_URL="http://localhost:$VISE_PORT"
HEALTH_TIMEOUT_SECS=120

# ---------------------------------------------------------------------------
# helpers
# ---------------------------------------------------------------------------

say() { printf 'vise: %s\n' "$*"; }
warn() { printf 'vise: warning: %s\n' "$*" >&2; }
die() { printf 'vise: error: %s\n' "$*" >&2; exit 1; }
step() { printf '\n==> %s\n' "$*"; }

TMP_DIR=""
cleanup() {
    if has_tty; then
        stty echo </dev/tty 2>/dev/null || true
    fi
    [ -n "$TMP_DIR" ] && rm -rf "$TMP_DIR"
}
trap cleanup EXIT

need_cmd() {
    # need_cmd NAME HINT
    command -v "$1" >/dev/null 2>&1 || die "$1 is required but was not found. $2"
}

# stdin is the curl pipe, so every interactive question goes through /dev/tty.
has_tty() {
    # Opening /dev/tty fails without a controlling terminal (cron, CI, docker).
    (exec </dev/tty) 2>/dev/null
}

# ask PROMPT DEFAULT: print the answer (DEFAULT when empty). Needs a tty.
ask() {
    printf '%s' "$1" >/dev/tty
    answer=""
    read -r answer </dev/tty || true
    [ -n "$answer" ] || answer="$2"
    printf '%s' "$answer"
}

# ask_secret PROMPT: like ask, without echo and without a default.
ask_secret() {
    printf '%s' "$1" >/dev/tty
    stty -echo </dev/tty 2>/dev/null || true
    answer=""
    read -r answer </dev/tty || true
    stty echo </dev/tty 2>/dev/null || true
    printf '\n' >/dev/tty
    printf '%s' "$answer"
}

# env_get KEY: value of KEY in $ENV_FILE (last one wins), empty when absent.
env_get() {
    [ -f "$ENV_FILE" ] || return 0
    sed -n "s/^[[:space:]]*\\(export[[:space:]]\\{1,\\}\\)\\{0,1\\}$1=//p" "$ENV_FILE" \
        | tail -n 1 \
        | sed "s/^[\"']//; s/[\"']\$//"
}

# env_set KEY VALUE: replace or append KEY in $ENV_FILE, keeping mode 0600.
env_set() {
    if [ -f "$ENV_FILE" ] && grep -q "^[[:space:]]*\\(export[[:space:]]\\{1,\\}\\)\\{0,1\\}$1=" "$ENV_FILE"; then
        # sed -i differs between GNU and BSD; go through a temp file instead.
        sed "s|^[[:space:]]*\\(export[[:space:]]\\{1,\\}\\)\\{0,1\\}$1=.*|$1=$2|" "$ENV_FILE" >"$ENV_FILE.tmp"
        mv "$ENV_FILE.tmp" "$ENV_FILE"
    else
        printf '%s=%s\n' "$1" "$2" >>"$ENV_FILE"
    fi
    chmod 600 "$ENV_FILE"
}

random_hex() {
    # 32 hex chars from the kernel RNG; od is POSIX, openssl is not always there.
    od -An -N16 -tx1 /dev/urandom | tr -d ' \n'
}

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | cut -d' ' -f1
    else
        printf ''
    fi
}

http_code() {
    # http_code URL [curl args...]: status code, 000 when unreachable.
    url="$1"
    shift
    code="$(curl -sS -o /dev/null -w '%{http_code}' --max-time 10 "$@" "$url" 2>/dev/null)" || true
    printf '%s' "${code:-000}"
}

compose() {
    docker compose --project-directory "$VISE_HOME" -f "$COMPOSE_FILE" --env-file "$ENV_FILE" "$@"
}

host_running() {
    [ -x "$BIN_DIR/vise" ] && "$BIN_DIR/vise" host status >/dev/null 2>&1
}

# ---------------------------------------------------------------------------
# 1. prerequisites
# ---------------------------------------------------------------------------

step "Checking prerequisites"

OS="$(uname -s)"
ARCH="$(uname -m)"
case "$OS" in
    Darwin)
        case "$ARCH" in
            arm64 | aarch64) TARGET="aarch64-apple-darwin" ;;
            *) die "no prebuilt binaries for macOS on $ARCH (Apple Silicon only). Build from source: https://github.com/$VISE_REPO#quickstart" ;;
        esac
        ;;
    Linux)
        case "$ARCH" in
            x86_64 | amd64) TARGET="x86_64-unknown-linux-gnu" ;;
            aarch64 | arm64) TARGET="aarch64-unknown-linux-gnu" ;;
            *) die "no prebuilt binaries for Linux on $ARCH (x86_64 and aarch64 only). Build from source: https://github.com/$VISE_REPO#quickstart" ;;
        esac
        # GNU tar shells out to xz for .tar.xz archives; bsdtar on macOS does not.
        need_cmd xz "Install it with your package manager (xz-utils on Debian/Ubuntu, xz on Fedora/Alpine)."
        ;;
    *) die "unsupported operating system: $OS (macOS and Linux only)" ;;
esac
say "platform: $OS $ARCH ($TARGET)"

need_cmd curl "Install curl and re-run."
need_cmd tar "Install tar and re-run."
need_cmd git "Install git (https://git-scm.com/downloads) and re-run; hosts clone repositories with it."
need_cmd docker "Install Docker Desktop or Docker Engine (https://docs.docker.com/get-docker/) and re-run."
docker info >/dev/null 2>&1 || die "docker is installed but the daemon is not reachable. Start Docker (Desktop) and re-run."
docker compose version >/dev/null 2>&1 || die "docker compose (v2 plugin) is required. Docker Desktop includes it; on Linux install docker-compose-plugin."
say "docker: $(docker --version)"
say "compose: $(docker compose version --short 2>/dev/null || docker compose version)"

if command -v claude >/dev/null 2>&1; then
    say "claude: $(command -v claude)"
else
    warn "claude (Claude Code) was not found on PATH. The host needs it for the claude-code harness;"
    warn "sessions created with --harness echo still work. Install it from https://claude.com/claude-code"
fi
if ! command -v npx >/dev/null 2>&1; then
    warn "npx was not found on PATH. The host runs the Claude Code ACP adapter through npx (Node.js 18+)."
fi

# ---------------------------------------------------------------------------
# 2. config
# ---------------------------------------------------------------------------

step "Configuring $VISE_HOME"

mkdir -p "$VISE_HOME" "$BIN_DIR" "$VISE_HOME/logs"

if [ -f "$ENV_FILE" ]; then
    say "existing install detected ($ENV_FILE)"
    keep="y"
    if has_tty; then
        keep="$(ask "Keep the existing config in $ENV_FILE? [Y/n] " y)"
    else
        say "no terminal available; keeping it as is (delete it to start over)"
    fi
    case "$keep" in
        n | N | no | NO)
            backup="$ENV_FILE.bak.$(date +%Y%m%d%H%M%S)"
            mv "$ENV_FILE" "$backup"
            say "moved the old config to $backup"
            ;;
        *)
            if [ -n "$VISE_GITHUB_PAT" ] && [ "$VISE_GITHUB_PAT" != "$(env_get VISE_GITHUB_PAT)" ]; then
                warn "VISE_GITHUB_PAT from the environment differs from $ENV_FILE and was ignored; edit the file or answer 'n' to rewrite it"
            fi
            ;;
    esac
fi

if [ ! -f "$ENV_FILE" ]; then
    if [ -z "$VISE_GITHUB_PAT" ]; then
        has_tty || die "VISE_GITHUB_PAT is not set and there is no terminal to ask on. Run: curl -fsSL https://vise.sh/install | VISE_GITHUB_PAT=ghp_... sh"
        printf '\nvise needs a GitHub personal access token: hosts use it to clone and push, the\n' >/dev/tty
        printf 'agent to open pull requests and the server to track them. Create a fine-grained\n' >/dev/tty
        printf 'token at https://github.com/settings/personal-access-tokens, restricted to the\n' >/dev/tty
        printf 'repositories vise works on, with Contents: read/write, Pull requests: read/write\n' >/dev/tty
        printf 'and Actions: read (check runs are read through Actions; fine-grained tokens have\n' >/dev/tty
        printf 'no Checks permission). It is stored only in %s.\n\n' "$ENV_FILE" >/dev/tty
        VISE_GITHUB_PAT="$(ask_secret "GitHub personal access token: ")"
    fi
    [ -n "$VISE_GITHUB_PAT" ] || die "no GitHub personal access token given"

    case "$(http_code https://api.github.com/user -H "Authorization: Bearer $VISE_GITHUB_PAT")" in
        200) say "GitHub token accepted" ;;
        401) die "GitHub rejected the token (401). Check it at https://github.com/settings/tokens and re-run." ;;
        000) warn "could not reach api.github.com to validate the token; continuing" ;;
        *) warn "unexpected answer from api.github.com while validating the token; continuing" ;;
    esac

    old_umask="$(umask)"
    umask 077
    cat >"$ENV_FILE" <<EOF
# vise configuration, written by the installer on $(date -u +%Y-%m-%dT%H:%M:%SZ).
# Read by docker compose (server + postgres) and by the vise CLI (host start).

# GitHub personal access token used by the server for clones, pushes and PRs.
VISE_GITHUB_PAT=$VISE_GITHUB_PAT

# Password for the bundled Postgres; generated, only reachable inside compose.
POSTGRES_PASSWORD=$(random_hex)

# Where the vise CLI and vise-host find the API.
VISE_URL=$VISE_URL

# Image and tag the server container runs; re-run the installer to upgrade.
VISE_SERVER_IMAGE=$VISE_SERVER_IMAGE
VISE_SERVER_TAG=$VISE_SERVER_TAG

# Port the API is published on (loopback only).
VISE_PORT=$VISE_PORT

# Log filter for the server container.
RUST_LOG=info
EOF
    umask "$old_umask"
    say "wrote $ENV_FILE"
fi

# Knobs given explicitly on this run win over a kept config.
[ -z "$REQUESTED_SERVER_IMAGE" ] || env_set VISE_SERVER_IMAGE "$REQUESTED_SERVER_IMAGE"
[ -z "$REQUESTED_SERVER_TAG" ] || env_set VISE_SERVER_TAG "$REQUESTED_SERVER_TAG"
if [ -n "$REQUESTED_PORT" ]; then
    env_set VISE_PORT "$REQUESTED_PORT"
    env_set VISE_URL "http://localhost:$REQUESTED_PORT"
fi

# Fill in anything a kept config predates, without touching what is there.
[ -n "$(env_get POSTGRES_PASSWORD)" ] || env_set POSTGRES_PASSWORD "$(random_hex)"
[ -n "$(env_get VISE_URL)" ] || env_set VISE_URL "$VISE_URL"
[ -n "$(env_get VISE_SERVER_IMAGE)" ] || env_set VISE_SERVER_IMAGE "$VISE_SERVER_IMAGE"
[ -n "$(env_get VISE_SERVER_TAG)" ] || env_set VISE_SERVER_TAG "$VISE_SERVER_TAG"
[ -n "$(env_get VISE_PORT)" ] || env_set VISE_PORT "$VISE_PORT"
[ -n "$(env_get RUST_LOG)" ] || env_set RUST_LOG info
[ -n "$(env_get VISE_GITHUB_PAT)" ] || die "$ENV_FILE has no VISE_GITHUB_PAT; add one or delete the file and re-run"
chmod 600 "$ENV_FILE"

# The kept config decides where the API is, so later steps talk to the right place.
VISE_URL="$(env_get VISE_URL)"
VISE_PORT="$(env_get VISE_PORT)"

# The compose file is generated; local edits belong in .env, not here.
cat >"$COMPOSE_FILE" <<'EOF'
# Generated by the vise installer; re-running it overwrites this file.
# Configuration lives in .env next to it.
name: vise-stack

services:
  postgres:
    image: postgres:17
    restart: unless-stopped
    environment:
      POSTGRES_USER: postgres
      POSTGRES_PASSWORD: ${POSTGRES_PASSWORD}
      POSTGRES_DB: vise
    volumes:
      - postgres-data:/var/lib/postgresql/data
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U postgres -d vise"]
      interval: 5s
      timeout: 5s
      retries: 10

  server:
    image: ${VISE_SERVER_IMAGE}:${VISE_SERVER_TAG}
    restart: unless-stopped
    depends_on:
      postgres:
        condition: service_healthy
    environment:
      DATABASE_URL: postgres://postgres:${POSTGRES_PASSWORD}@postgres:5432/vise
      VISE_GITHUB_PAT: ${VISE_GITHUB_PAT}
      RUST_LOG: ${RUST_LOG:-info}
    ports:
      - "127.0.0.1:${VISE_PORT}:3000"

volumes:
  postgres-data:
EOF
say "wrote $COMPOSE_FILE"

# ---------------------------------------------------------------------------
# 3. server
# ---------------------------------------------------------------------------

step "Starting postgres and vise-server with docker compose"

compose pull --quiet || warn "could not pull images; using whatever is already present locally"
compose up -d --remove-orphans || die "docker compose up failed. Inspect with: docker compose -f $COMPOSE_FILE logs"

say "waiting for the API at $VISE_URL (up to ${HEALTH_TIMEOUT_SECS}s)"
elapsed=0
healthy=""
while [ "$elapsed" -lt "$HEALTH_TIMEOUT_SECS" ]; do
    case "$(http_code "$VISE_URL/health")" in
        2*) healthy=1 ;;
        # Images that predate the health route: any answer from the API will do.
        404) case "$(http_code "$VISE_URL/hosts")" in 200) healthy=1 ;; esac ;;
    esac
    [ -z "$healthy" ] || break
    sleep 2
    elapsed=$((elapsed + 2))
done
if [ -z "$healthy" ]; then
    printf '\n' >&2
    compose logs --tail 30 server >&2 || true
    die "the server did not become healthy at $VISE_URL within ${HEALTH_TIMEOUT_SECS}s (last log lines above). Inspect with: docker compose -f $COMPOSE_FILE logs server"
fi
say "server is up at $VISE_URL"

# ---------------------------------------------------------------------------
# 4. binaries
# ---------------------------------------------------------------------------

step "Installing vise and vise-host into $BIN_DIR"

if [ "$VISE_VERSION" = "latest" ]; then
    RELEASE_BASE="https://github.com/$VISE_REPO/releases/latest/download"
else
    RELEASE_BASE="https://github.com/$VISE_REPO/releases/download/$VISE_VERSION"
fi
[ -n "$VISE_DOWNLOAD_BASE" ] || VISE_DOWNLOAD_BASE="$RELEASE_BASE"
# The checksums come from the GitHub release itself, not from wherever the
# archives are downloaded, so a mirror cannot swap an archive unnoticed and
# anyone can compare against the SHA256SUMS.txt shown on the release page.
[ -n "$VISE_CHECKSUMS_URL" ] || VISE_CHECKSUMS_URL="$RELEASE_BASE/SHA256SUMS.txt"

TMP_DIR="$(mktemp -d 2>/dev/null || mktemp -d -t vise-install)"
SUMS_FILE="$TMP_DIR/SHA256SUMS.txt"

say "downloading $VISE_CHECKSUMS_URL"
if ! curl -fsSL --retry 3 -o "$SUMS_FILE" "$VISE_CHECKSUMS_URL" 2>/dev/null; then
    rm -f "$SUMS_FILE"
    warn "could not fetch $VISE_CHECKSUMS_URL; falling back to per-archive .sha256 files"
fi

# sums_lookup NAME: the checksum listed for NAME in SHA256SUMS.txt, or nothing.
# Accepts both "HASH  NAME" (text) and "HASH *NAME" (binary) sha256sum lines.
sums_lookup() {
    [ -s "$SUMS_FILE" ] || return 0
    tr -d '\r' <"$SUMS_FILE" | awk -v name="$1" '
        NF >= 2 { f = $2; sub(/^\*/, "", f); if (f == name) { print $1; exit } }'
}

# verify_archive ARCHIVE URL: check TMP_DIR/ARCHIVE against the release
# SHA256SUMS.txt, else against URL.sha256 served next to the archive. A
# missing checksum only warns; a wrong one is fatal.
verify_archive() {
    archive="$1"
    url="$2"
    expected="$(sums_lookup "$archive")"
    source="SHA256SUMS.txt"
    if [ -z "$expected" ]; then
        [ ! -s "$SUMS_FILE" ] || warn "$archive is not listed in SHA256SUMS.txt; trying $archive.sha256"
        if curl -fsSL --retry 3 -o "$TMP_DIR/$archive.sha256" "$url.sha256" 2>/dev/null; then
            expected="$(cut -d' ' -f1 <"$TMP_DIR/$archive.sha256" | tr -d '\r\n')"
            source="$archive.sha256"
        fi
    fi
    if [ -z "$expected" ]; then
        warn "no checksum published for $archive; skipping verification"
        return 0
    fi

    actual="$(sha256_of "$TMP_DIR/$archive")"
    if [ -z "$actual" ]; then
        warn "no sha256sum or shasum found; skipping checksum verification of $archive"
    elif [ "$expected" != "$actual" ]; then
        die "checksum mismatch for $archive (expected $expected from $source, got $actual)"
    else
        say "verified $archive against $source"
    fi
}

# install_archive APP DEST: fetch APP-TARGET.tar.xz, verify, install as BIN_DIR/DEST.
install_archive() {
    app="$1"
    dest="$2"
    archive="$app-$TARGET.tar.xz"
    url="$VISE_DOWNLOAD_BASE/$archive"

    say "downloading $url"
    curl -fsSL --retry 3 -o "$TMP_DIR/$archive" "$url" \
        || die "download failed: $url (is there a release for $VISE_VERSION with a $TARGET build?)"

    verify_archive "$archive" "$url"

    mkdir -p "$TMP_DIR/$app"
    tar -xf "$TMP_DIR/$archive" -C "$TMP_DIR/$app" || die "could not extract $archive"
    extracted="$(find "$TMP_DIR/$app" -type f -name "$app" | head -n 1)"
    [ -n "$extracted" ] || die "$archive does not contain a $app binary"
    chmod +x "$extracted"
    # mv replaces the inode, so a running vise-host keeps its old file.
    mv -f "$extracted" "$BIN_DIR/$dest" || die "could not install $dest into $BIN_DIR"
}

if host_running; then
    say "stopping the running vise-host before upgrading it"
    "$BIN_DIR/vise" host stop || die "could not stop the running vise-host; stop it by hand and re-run"
fi

install_archive vise-cli vise
install_archive vise-host vise-host

"$BIN_DIR/vise" --version >/dev/null 2>&1 || die "$BIN_DIR/vise does not run on this machine"
say "installed $("$BIN_DIR/vise" --version) and $("$BIN_DIR/vise-host" --version)"

case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *)
        case "${SHELL:-}" in
            */zsh) rc="$HOME/.zshrc" ;;
            */bash) rc="$HOME/.bashrc" ;;
            *) rc="your shell profile" ;;
        esac
        printf '\n'
        say "$BIN_DIR is not on your PATH. Add it to $rc:"
        # shellcheck disable=SC2016 # the literal $PATH is the point of the hint
        printf '\n    export PATH="%s:$PATH"\n\n' "$BIN_DIR"
        ;;
esac

# ---------------------------------------------------------------------------
# 5. host
# ---------------------------------------------------------------------------

step "Enrolling this machine as a host"

token="$(env_get VISE_HOST_TOKEN)"
if [ -n "$token" ]; then
    # A wiped database (docker compose down -v) leaves a token nothing accepts.
    # A heartbeat for a session id that cannot exist is side-effect free: 401
    # for a bad token, 409 (host does not hold that session) for a good one.
    case "$(http_code "$VISE_URL/hosts/sessions/token-check/heartbeat" -X POST -H "Authorization: Bearer $token")" in
        401)
            warn "the host token in $ENV_FILE is no longer accepted by the server; enrolling again"
            token=""
            ;;
        *) say "host token in $ENV_FILE still works; keeping it" ;;
    esac
fi

if [ -z "$token" ]; then
    name="$VISE_HOST_NAME"
    if [ -z "$name" ]; then
        name="$(hostname 2>/dev/null || uname -n)"
        name="${name%%.*}"
        [ -n "$name" ] || name="local"
    fi
    token="$("$BIN_DIR/vise" --url "$VISE_URL" hosts create "$name" --token-only)" \
        || die "could not enroll host '$name' at $VISE_URL"
    [ -n "$token" ] || die "the server returned an empty host token"
    env_set VISE_HOST_TOKEN "$token"
    say "enrolled host '$name'; token saved to $ENV_FILE"
fi

step "Starting vise-host"

"$BIN_DIR/vise" host start || die "vise-host failed to start; see $VISE_HOME/logs/host.log"

# ---------------------------------------------------------------------------
# done
# ---------------------------------------------------------------------------

printf '\n'
say "all set. The API is at $VISE_URL (Swagger UI at $VISE_URL/docs) and a host is polling for work."
printf '\nTry it:\n\n'
printf '    vise sessions create "add a --json flag to the ls command" --repo owner/repo --watch\n'
printf '\nUseful commands:\n\n'
printf '    vise host status | logs -f | stop | start     manage the host on this machine\n'
printf '    vise sessions ls                              list sessions\n'
printf '    docker compose -f %s logs -f    server logs\n' "$COMPOSE_FILE"
printf '\nUninstall:\n\n'
printf '    vise host stop && docker compose -f %s down -v && rm -rf %s\n\n' "$COMPOSE_FILE" "$VISE_HOME"
