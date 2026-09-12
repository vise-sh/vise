# Session prompt: best-in-class open source repo scaffolding

Bring this repo up to best-in-class open source standard. A previous PR already added rustfmt, clippy-with-denied-warnings, a Justfile (`just check` mirrors CI), CONTRIBUTING.md, a CI workflow (fmt, clippy, test, OpenAPI drift, sqlx cache drift, cargo-deny advisories), rust-toolchain.toml, and .editorconfig. Do NOT redo any of that. This PR adds the community, governance, and release layer around it.

## Licensing (do this first — other pieces depend on it)

- Dual-license MIT OR Apache-2.0, the Rust ecosystem standard. Add `LICENSE-MIT` and `LICENSE-APACHE` at the root.
- Set `license = "MIT OR Apache-2.0"` in the workspace `Cargo.toml` and make every crate inherit it (`license.workspace = true`). One crate currently has a placeholder license string — fix it.
- Re-enable the cargo-deny license check (it was disabled because of that placeholder). Configure `deny.toml` to allow MIT, Apache-2.0, and the licenses the dependency tree actually uses.

## Community health files

- `CODE_OF_CONDUCT.md`: Contributor Covenant v2.1, contact via GitHub issues or a placeholder email.
- `SECURITY.md`: report vulnerabilities via GitHub private security advisories, not public issues; state supported-version policy (latest release).
- `.github/ISSUE_TEMPLATE/`: YAML-form templates — `bug_report.yml` (version, OS, reproduction steps, expected/actual, logs), `feature_request.yml` (problem, proposed solution, alternatives), plus `config.yml` with blank issues disabled and a link to GitHub Discussions.
- `.github/PULL_REQUEST_TEMPLATE.md`: short — summary, how tested, checklist item "ran `just check`".
- `.github/CODEOWNERS`: `* @vise-sh` (or the org's maintainer team) as default owner.

## Dependency hygiene

- `.github/dependabot.yml`: weekly `cargo` updates (grouped: one PR for all patch/minor, separate for major) and weekly `github-actions` updates. Grouping matters — do not create per-crate PR noise.

## Release automation

- Adopt `release-plz` via GitHub Action: on push to main it maintains a release PR with version bumps and changelog; merging it tags and creates a GitHub release. Add `release-plz.toml` configured to release the workspace, with the client crate excluded from crates.io publishing if it is not meant to be published (set `publish = false` in that crate rather than configuring around it).
- Changelog generation with git-cliff conventions via release-plz defaults; add `CHANGELOG.md` seeded with an Unreleased section.
- Binary distribution: add `cargo-dist` for the CLI (`vise-cli`) — installers for shell (curl | sh) and Homebrew formula generation can be off for now; just build release archives for macOS (arm64) and Linux (x86_64, arm64) attached to each GitHub release. If cargo-dist's workflow conflicts with release-plz tagging, prefer release-plz as the tagger and have cargo-dist build on tag push.

## CI additions (extend the existing workflow, do not replace it)

- **Coverage**: a job running `cargo llvm-cov --workspace --lcov` uploading to Codecov (tokenless for public repos). Do not gate merges on a coverage percentage.
- **Typos**: `crate-ci/typos` action — fast, catches doc typos. Add `_typos.toml` only if false positives appear.
- **Concurrency**: add a `concurrency` group cancelling in-progress runs per ref, if not already present.
- **Caching**: ensure `Swatinem/rust-cache` is used in every Rust job, if not already present.
- Add a merge queue-compatible trigger (`merge_group`) alongside `pull_request`.

## README polish

- Badges at top: CI status, Codecov, crates.io version of `vise-cli` (once published; use the badge anyway), license.
- Sections: what vise is (one paragraph), quickstart (docker-compose up, run the server, one CLI example creating a session), architecture sketch (bins vs crates, one short table), link to CONTRIBUTING.md, license footer stating the dual license.
- Keep it under ~150 lines; depth belongs in docs/, not the README.

## Out of scope

- Publishing to crates.io (configure for it, don't do it).
- Homebrew tap, MSRV policy, mdBook docs site, benchmark CI, nightly jobs.
- Any changes to the existing lint/test/drift jobs.

## Verification

- `just check` must pass.
- `cargo deny check` must pass with the license check enabled.
- Actionlint the workflow files (`actionlint` binary or the action) — new YAML must be valid.
- Issue templates: validate YAML syntax renders (no tabs, correct `body:` schema).

Open a PR when done. In the PR description, list any judgment calls (e.g., licenses added to the deny allowlist, crates marked publish = false).
