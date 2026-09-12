# Security Policy

## Reporting a vulnerability

**Please do not report security vulnerabilities through public GitHub issues,
discussions or pull requests.**

Report them privately through GitHub's security advisory form:

<https://github.com/vise-sh/vise/security/advisories/new>

This opens a private thread with the maintainers where we can discuss the
issue, agree on a fix and coordinate disclosure. If you cannot use the form
for some reason, email security@vise.sh instead.

Please include as much of the following as you can:

- The component affected (`vise-server`, `vise-host`, `vise-cli`, the API, the
  generated client, ...) and the version or commit SHA.
- Steps to reproduce, or a proof of concept.
- The impact you believe it has, and any mitigations you are aware of.

## What to expect

- We will acknowledge your report within 5 business days.
- We will keep you informed as we investigate and work on a fix, and we will
  credit you in the advisory and release notes unless you ask us not to.
- Once a fix is released we publish the advisory. We ask that you give us a
  reasonable window (up to 90 days) before disclosing publicly.

## Supported versions

vise is pre-1.0 and moves quickly. Security fixes are made on `main` and
shipped in the **latest release** only; older releases do not receive
backported patches. If you are running an older version, upgrade to the latest
release before reporting, in case the issue has already been fixed.

| Version | Supported |
|---------|-----------|
| Latest release | Yes |
| Older releases | No |
| `main` (unreleased) | Best effort |

## Scope

vise executes agent workloads on hosts you enroll and talks to GitHub on your
behalf, so we are especially interested in reports about:

- Host enrollment tokens and session credentials leaking or being reusable
  beyond their intended scope.
- Sessions escaping the workspace they were given on a host.
- The server being coerced into acting on repositories it was not configured
  for.
- Anything in the dependency tree flagged by `cargo deny check advisories`
  that we have not already addressed.
