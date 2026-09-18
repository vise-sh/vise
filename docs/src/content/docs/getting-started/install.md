---
title: Install
description: Stand up the vise server, CLI and a host on one machine with a single command.
sidebar:
  order: 2
---

One line stands up the whole stack on a machine that has Docker and git:

```sh
curl -fsSL https://vise.sh/install | sh
```

`vise.sh/install` serves [`scripts/install.sh`](https://github.com/vise-sh/vise/blob/main/scripts/install.sh)
from the repository. Nothing needs sudo; everything lives under `~/.vise`.

## Prerequisites

| Requirement | Why |
|-------------|-----|
| macOS on Apple Silicon, or Linux on x86_64 or arm64 | Prebuilt `vise` and `vise-host` binaries are published for these three targets |
| Docker with the compose plugin | Runs Postgres and the `vise-server` image |
| git | The host clones repositories into per-session workspaces |
| `xz` (Linux only) | Release archives are `.tar.xz`; GNU tar shells out to `xz` to unpack them |
| Claude Code (`claude`), optional | Needed by the default `claude-code` harness. Without it the installer warns and continues; sessions created with `--harness echo` still work |

A GitHub personal access token is also required. The installer prompts for one
(or reads `VISE_GITHUB_PAT`). Create a
[fine-grained token](https://github.com/settings/personal-access-tokens)
restricted to the repositories vise will work on, with:

- **Contents**: read and write, so hosts can clone and push.
- **Pull requests**: read and write, so the agent can open PRs and the server
  can track them.
- **Actions**: read, so the server can read check runs. Fine-grained tokens
  cannot be given the *Checks* permission; only GitHub Apps can.

The token is stored only in `~/.vise/.env`. For organizations, a GitHub App
is the recommended alternative; see [GitHub authentication](#github-authentication)
below.

## What the installer does

Each step fails loudly on its own:

1. **Checks prerequisites**: `docker` (with compose) and `git`, and warns if
   `claude` is missing.
2. **Writes config**: reads `VISE_GITHUB_PAT` or asks for it on the terminal,
   validates it against the GitHub API, and writes `~/.vise/.env` plus a
   `~/.vise/docker-compose.yml` that runs Postgres 17 and
   `ghcr.io/vise-sh/vise-server`.
3. **Starts the server**: `docker compose up -d`, then waits up to two minutes
   for the API to answer on `http://localhost:3000`.
4. **Installs the binaries**: downloads the `vise-cli` and `vise-host` archives
   for your OS and architecture from the latest
   [GitHub release](https://github.com/vise-sh/vise/releases), verifies them
   against the `SHA256SUMS.txt` attached to that release, and puts them in
   `~/.vise/bin` as `vise` and `vise-host`.
5. **Enrolls this machine as a host** and starts `vise-host` in the background.

When it finishes you will see:

```text
vise: all set. The API is at http://localhost:3000 (Swagger UI at http://localhost:3000/docs) and a host is polling for work.
```

If `~/.vise/bin` is not on your `PATH`, the installer prints the line to add to
your shell profile:

```sh
export PATH="$HOME/.vise/bin:$PATH"
```

## What ends up on disk

```text
~/.vise/
├── .env                 VISE_URL, VISE_GITHUB_PAT, VISE_HOST_TOKEN, image and port settings
├── docker-compose.yml   postgres + ghcr.io/vise-sh/vise-server
├── bin/
│   ├── vise             the CLI
│   └── vise-host        the host process
├── host.pid             pid of the running vise-host
└── logs/
    └── host.log         combined stdout/stderr of vise-host
```

The CLI reads `VISE_URL` and `VISE_HOST_TOKEN` from `~/.vise/.env`, so after
installing you can run `vise` with no flags. Set `VISE_HOME` to move the
whole directory somewhere else; both the installer and the CLI honor it.

## Overrides

Set these in the environment when running the installer:

| Variable | Default | Effect |
|----------|---------|--------|
| `VISE_GITHUB_PAT` | prompted | GitHub personal access token |
| `VISE_HOME` | `~/.vise` | Install directory |
| `VISE_VERSION` | `latest` | Release tag to install, for example `v0.2.0` |
| `VISE_SERVER_TAG` | `VISE_VERSION` | `vise-server` image tag |
| `VISE_SERVER_IMAGE` | `ghcr.io/vise-sh/vise-server` | `vise-server` image |
| `VISE_PORT` | `3000` | Local port for the API |
| `VISE_HOST_NAME` | the machine's hostname | Name to enroll this machine under |
| `VISE_DOWNLOAD_BASE` | the GitHub release for `VISE_VERSION` | Base URL for release archives, for mirrors |
| `VISE_CHECKSUMS_URL` | the `SHA256SUMS.txt` on that release | Where to fetch checksums from, even when `VISE_DOWNLOAD_BASE` is a mirror |

For example, to pin a release on a non-default port without a prompt:

```sh
curl -fsSL https://vise.sh/install | VISE_GITHUB_PAT=github_pat_... VISE_VERSION=v0.1.0 VISE_PORT=3100 sh
```

## Re-running and upgrading

Re-running the installer is safe. An existing `~/.vise/.env` is kept unless
you say otherwise at the prompt, binaries are upgraded in place (stopping the
running host first), the server image is pulled again, and containers are only
recreated when their image or config changed. The existing host token is
checked against the server and reused if it still works.

## Managing the host

The host on this machine is a plain background process managed by `vise host`:

```sh
vise host status      # "vise-host: running (pid N)" or "vise-host: not running" (exit 1)
vise host logs -f     # tail ~/.vise/logs/host.log
vise host stop        # SIGTERM via ~/.vise/host.pid, SIGKILL after 10s
vise host start       # reads VISE_HOST_TOKEN and VISE_URL from ~/.vise/.env
```

`vise host start -- --keep-workspaces` passes extra flags through to
`vise-host`. Server logs are at:

```sh
docker compose -f ~/.vise/docker-compose.yml logs -f
```

## API token

The server is open by default, which is fine for a single-user install on
localhost. To require a credential for `vise sessions ...` and
`vise hosts ...`, set `VISE_API_TOKEN` in `~/.vise/.env` and recreate the
server container (`docker compose -f ~/.vise/docker-compose.yml up -d`): the
compose file passes it to the server, and the CLI reads the same file to send
it as a bearer token. `vise --api-token <token>` or `VISE_API_TOKEN` in the
environment override it. The host keeps using its own `VISE_HOST_TOKEN`.

### Signing in to a vise cloud server

Against a vise cloud server (one with a browser dashboard),
`vise login --url https://<server>` replaces the manual token setup: it
opens the dashboard in your browser, asks you to authorize the CLI for your
workspace, and writes the resulting API key to `~/.vise/.env` as
`VISE_API_TOKEN`, together with `VISE_URL`, so later commands need no
`--url`. On a headless machine, run `vise login --url <server> --paste` and
paste a key created under the dashboard's Settings → API keys section.
Self-hosted (OSS) servers have no browser login — set `VISE_API_TOKEN` as
described above instead.

## GitHub authentication

`github_repo` sessions need the server to authenticate to GitHub, both to hand
hosts a credential for cloning and pushing and to read pull requests for
tracking and follow-ups. The installer configures a PAT; the server also
supports a GitHub App:

- **GitHub App** (recommended for organizations): set `VISE_GITHUB_APP_ID` and
  `VISE_GITHUB_APP_PRIVATE_KEY_PATH`. The server mints a short-lived
  installation token scoped to the session's repository for every session.
  The App needs *Contents: read/write*, *Pull requests: read/write* and
  *Checks: read*. The repository's `docker-compose.github-app.yml` overlay
  mounts the private key into the container.
- **Personal access token**: set `VISE_GITHUB_PAT`. The same token is handed
  to every session and used for all server-side reads.

If both are configured the App wins and the PAT is ignored (the server logs
this at startup). With neither, `github_repo` sessions fail and PR tracking
is disabled.

## Installing from source

Every release ships the server as a container image and the binaries as
archives, so the installer needs no toolchain. To run from a checkout instead,
you need Rust (stable), [just](https://github.com/casey/just) and Docker:

```sh
git clone https://github.com/vise-sh/vise && cd vise
cp .env.example .env         # set VISE_GITHUB_PAT (or the App settings)
docker compose up -d         # Postgres 17 + the released vise-server image on :3000
```

Or run the server itself from source (it applies its own migrations on startup):

```sh
just db-up                   # Postgres only
cargo run -p vise-server     # serves on :3000
```

`just install-dev` builds `vise-cli` and `vise-host` in release mode and
symlinks them into `~/.vise/bin` as `vise` and `vise-host`, so `vise host
start` finds the host binary next to the CLI. Enroll a host and start it with
the token that is printed:

```sh
vise hosts create laptop
VISE_HOST_TOKEN=<token> vise host start
```

See [CONTRIBUTING.md](https://github.com/vise-sh/vise/blob/main/CONTRIBUTING.md)
for the development tooling and the checks CI runs.

## Uninstall

```sh
vise host stop
docker compose -f ~/.vise/docker-compose.yml down -v   # containers and the Postgres volume
rm -rf ~/.vise
```

Then remove the `~/.vise/bin` line from your shell profile.
