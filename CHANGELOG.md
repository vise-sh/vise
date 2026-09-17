# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
Entries are generated from [Conventional Commits](https://www.conventionalcommits.org/)
by [release-plz](https://release-plz.dev); edit commit messages, not this file.

## [Unreleased]

## [0.1.1](https://github.com/vise-sh/vise/compare/v0.1.0...v0.1.1) - 2026-09-17

### Added

- *(docs)* add Cloudflare Workers deploy config for docs.vise.sh

### Fixed

- *(install)* diagnose a private server image instead of failing on compose up
- *(install)* correct PAT permissions and make the server image public

### Other

- require manual approval before publishing the server image
- add Starlight site with Getting Started section

## [0.1.0](https://github.com/vise-sh/vise/releases/tag/v0.1.0) - 2026-09-15

### Added

- *(cli)* add `vise host start|stop|status|logs` to manage the local host
- track PRs after completion and spawn follow-up sessions

### Other

- dev tooling, CI gate, licensing, community files, and release automation
- Initial commit
