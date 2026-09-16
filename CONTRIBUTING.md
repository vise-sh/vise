# Contributing to vise

This document describes the tooling and conventions every change is expected
to follow. CI enforces all of it on pull requests, so running the same checks
locally before you push saves a round trip.

## Prerequisites

| Tool | Why | Install |
|------|-----|---------|
| Rust (stable) | Toolchain, pinned by `rust-toolchain.toml`; rustup installs `rustfmt` and `clippy` automatically | https://rustup.rs |
| [just](https://github.com/casey/just) | Task runner; every recipe below is `just <name>` | `cargo install just` |
| Docker | Local Postgres via `docker compose`; building the `vise-server` image | https://docs.docker.com/get-docker/ |
| [sqlx-cli](https://github.com/launchbadge/sqlx/tree/main/sqlx-cli) | Migrations and the offline query cache | `cargo install sqlx-cli --no-default-features --features postgres` |
| [cargo-deny](https://github.com/EmbarkStudios/cargo-deny) | Dependency advisory and license audit (optional locally) | `cargo install cargo-deny` |
| [typos](https://github.com/crate-ci/typos) | Spell-check (optional locally; CI runs it) | `cargo install typos-cli` |
| [cargo-llvm-cov](https://github.com/taiki-e/cargo-llvm-cov) | Coverage report (optional locally) | `cargo install cargo-llvm-cov` |
| [cargo-watch](https://github.com/watchexec/cargo-watch) | `just run-server` / `just run-host` (optional) | `cargo install cargo-watch` |
| [shellcheck](https://www.shellcheck.net) | Lints `scripts/*.sh` (optional locally; CI runs it) | `brew install shellcheck` / `apt install shellcheck` |

## First-time setup

```sh
cp .env.example .env        # DATABASE_URL for the server, sqlx-cli and query macros
just db-up                  # start Postgres 17 in Docker
just db-migrate             # apply crates/vise-core/migrations (the server also does this on startup)
just check                  # confirm everything is green before you start
```

Optionally enable the pre-commit hook, which runs the fast formatting check:

```sh
git config core.hooksPath .githooks
```

## Before opening a pull request

Run the full gate. It is the same set of checks CI runs:

```sh
just check
```

That expands to:

| Recipe | What it enforces |
|--------|------------------|
| `just fmt-check` | Code is `rustfmt`-clean (`just fmt` fixes it) |
| `just lint` | `cargo clippy` on all targets and features with `-D warnings` |
| `just test` | `cargo test --workspace` |
| `just sqlx-check` | The committed `.sqlx/` query cache matches the code and DB schema |
| `just spec-check` | The committed `openapi/openapi.json` matches what `vise-api` generates |

`just audit` (cargo-deny advisories and licenses), `just typos`,
`just shellcheck` and `just coverage` also run in CI but are not part of
`just check` because they need network access or an extra tool. Coverage is reported to Codecov for
information only; a drop in coverage never blocks a merge.

## Formatting

We use `rustfmt` with default settings (`rustfmt.toml` only pins the edition).
Configure your editor to format on save, or run `just fmt`. Do not hand-format
around rustfmt; if the output looks bad, restructure the code instead.

## Linting

Clippy runs at its default lint level, and CI treats every warning as an
error. A few extra lints are enabled for the whole workspace in the
`[workspace.lints]` table of the root `Cargo.toml`; each crate opts in with
`[lints] workspace = true`, so add that block to any new crate.

Fix warnings rather than suppressing them. If a suppression is genuinely the
right call, scope it as narrowly as possible (a single item, not a module or
crate) and leave a one-line comment explaining why:

```rust
// Clippy wants `&[u8]`, but the FFI boundary requires an owned Vec.
#[allow(clippy::ptr_arg)]
fn send(buf: &Vec<u8>) { /* ... */ }
```

## Database queries and the sqlx offline cache

`vise-core` uses `sqlx::query!` / `query_as!`, which type-check SQL at compile
time. To keep the workspace buildable without a database (in CI and for
contributors who only touch the CLI or host), the results are cached in the
committed `.sqlx/` directory.

Whenever you add or edit a `query!` macro, or add a migration that changes a
column a query touches:

```sh
just db-migrate       # apply new migrations to your local DB
just sqlx-prepare     # regenerate .sqlx/
git add .sqlx
```

If you forget, `just sqlx-check` (and the `sqlx query cache up to date` CI
job) fails. With `DATABASE_URL` set, the macros validate against your live
database instead of the cache, so a stale cache will not show up as a local
build error; that is exactly why the check exists. The container image is
built with `SQLX_OFFLINE=true`, so it depends on the cache being current.

## Migrations

Migrations live in `crates/vise-core/migrations` and are embedded into
`vise-core` with `sqlx::migrate!`; `vise-server` applies pending ones on
startup, before it starts serving. `just db-migrate` (sqlx-cli) applies the
same files and records them in the same `_sqlx_migrations` table, so use it
whenever you need the schema without running the server: the query macros,
`just sqlx-prepare` and the tests all do. `crates/vise-core/tests/migrations.rs`
checks that the two stay interchangeable.

## Container image

`Dockerfile` builds `vise-server` into a slim Debian image; `just docker-build`
produces one for your machine's architecture, and the `docker build` CI job
builds linux/amd64 and linux/arm64 (the builder stage cross-compiles) on every
pull request. On a release tag, `.github/workflows/publish-docker.yml`, called
from the dist-generated release workflow, pushes the multi-arch image to
`ghcr.io/vise-sh/vise-server` as `<version>` and `latest`. The Dockerfile pins
its own Rust version (`RUST_VERSION`); bump it when the code needs a newer
compiler.

GHCR creates a package as private on its first push, whatever the visibility
of the repository, and has no API to change that, so the package has to be
made public once by hand after the first release, by an organization owner:
<https://github.com/orgs/vise-sh/packages/container/vise-server/settings>,
**Danger Zone**, **Change visibility**, **Public** (or: organization page,
**Packages**, `vise-server`, **Package settings**). If the dialog does not
offer *Public*, the organization's **Packages** settings restrict package
creation to private/internal; enable public packages there first. Later
pushes keep the setting. It must stay public: `scripts/install.sh` and the
quickstart pull the image anonymously, and a private package shows up for
every user as `docker compose pull` failing with `unauthorized`.

`scripts/check-image-public.sh IMAGE` tells whether an image can be pulled
without credentials. The publish workflow runs it on the image it just
pushed and fails the release otherwise, and `.github/workflows/image-public.yml`
runs it against `latest` daily and on demand (**Actions**, *Image is public*,
**Run workflow**), so a package that is private between releases is noticed
before users hit it.

## OpenAPI spec and the generated client

`crates/vise-client` is generated at build time from `openapi/openapi.json`,
which is in turn generated from the `vise-api` crate. After changing any route,
request or response type, regenerate and commit the spec:

```sh
just gen-spec
git add openapi/openapi.json
```

`just spec-check` fails if the committed file is out of date.

## Dependencies

- Add shared dependencies to `[workspace.dependencies]` in the root
  `Cargo.toml` and reference them with `{ workspace = true }` so every crate
  uses the same version and feature set.
- Keep `Cargo.lock` committed and up to date.
- `just audit` checks the lockfile against the RustSec advisory database and
  the license allowlist in `deny.toml`. If an advisory has no fix yet, add it
  to the `ignore` list with a comment and a link. If a new dependency brings in
  a license that is not on the allowlist, add the license (not the crate) with
  a comment saying which crate needs it, and mention it in the PR.
- Dependabot opens a weekly PR for patch and minor bumps (one grouped PR) and a
  separate PR per major bump. Prefer merging those over hand-editing versions.

## Commits and pull requests

- Keep PRs focused; one logical change per PR is easier to review and revert.
- Write commit messages in the imperative mood with a short subject line and,
  when useful, a body that explains *why*.
- Use [Conventional Commits](https://www.conventionalcommits.org/) prefixes on
  the subject line: `feat:`, `fix:`, `docs:`, `chore:`, `refactor:`, `test:`,
  `ci:`. Add a `!` (`feat!:`) or a `BREAKING CHANGE:` footer for breaking
  changes. The changelog and the next version number are generated from these,
  so a PR that is squash-merged should have a conventional title.
- By contributing you agree that your work is licensed under the project's
  dual MIT OR Apache-2.0 license (see the [README](README.md#license)).
- CI must be green before merge. If a check fails for a reason unrelated to
  your change, say so in the PR rather than working around it.

## Continuous integration

`.github/workflows/ci.yml` runs on every pull request and on pushes to `main`:

| Job | Command |
|-----|---------|
| rustfmt | `just fmt-check` |
| clippy | `just lint` |
| test | `just test` |
| openapi spec up to date | `just spec-check` |
| sqlx query cache up to date | `just db-migrate && just sqlx-check` against a Postgres 17 service |
| cargo deny advisories + licenses | `cargo deny check advisories licenses` |
| typos | `typos` |
| shellcheck | `just shellcheck` on `scripts/*.sh` (including the installer) |
| coverage | `cargo llvm-cov --workspace --lcov`, uploaded to Codecov |

All jobs except `sqlx query cache up to date` build with `SQLX_OFFLINE=true`,
which is why the `.sqlx/` cache must be committed. The workflow also runs for
merge queues (`merge_group`), and in-progress runs for the same ref are
cancelled when a new push arrives.

## Releases

Releases are automated; you should never bump a version by hand.

1. Every push to `main` runs [release-plz](https://release-plz.dev), which
   keeps a single "release" PR open containing the next version bump (derived
   from the Conventional Commit prefixes since the last release) and the
   corresponding `CHANGELOG.md` entries.
2. Merging that PR tags the commit `vX.Y.Z`.
3. The tag triggers [cargo-dist](https://opensource.axo.dev/cargo-dist/), which
   builds `vise-cli` and `vise-host` archives for macOS (arm64) and Linux
   (x86_64, arm64) and attaches them to a GitHub release whose notes are the
   changelog section. `scripts/install.sh` downloads these archives.
   The same release also carries a [CycloneDX](https://cyclonedx.org) SBOM
   for each archive (`<app>-<target>.cdx.json`, generated by
   `scripts/sbom.sh`; `just sbom` builds them locally) for feeding into
   vulnerability scanners.

All crates share the workspace version, so one tag covers everything.
`vise-client`, `vise-server` and `vise-host` are marked `publish = false`;
crates.io publishing is configured in `release-plz.toml` but not enabled yet.
