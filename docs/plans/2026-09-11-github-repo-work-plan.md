# GitHub Repo Work Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** A vise session can target a GitHub repo; the host clones it with GitHub App credentials, the agent does the work and opens the PR itself, and vise records the observed outcome.

**Architecture:** Design doc: `docs/plans/2026-09-11-github-repo-work-design.md`. New `github_repo` environment kind on sessions; `vise-server` exposes a generic host-authenticated credentials endpoint (`POST /hosts/sessions/{id}/credentials`, pluggable `CredentialProvider` registry — v1 ships only the GitHub App provider minting short-lived installation tokens); `vise-host` clones the repo before spawning the ACP runtime, injects credentials (env + git credential helper reading a token file), prepends prompt scaffolding, and after the run inspects the workspace to record an `outcome` on finish.

**Tech Stack:** Rust workspace (axum, sqlx/Postgres, utoipa → progenitor-generated `vise-client`), `jsonwebtoken` for GitHub App JWTs, `git` CLI on hosts, `gh` CLI available to agents.

**Critical codebase constraints (read before starting):**

1. **OpenAPI codegen order** (from prior work): edit routes/`openapi.rs` → `cargo check -p vise-api` → `just gen-spec` → rebuild. `vise-client` regenerates via build.rs from `openapi/openapi.json`; it silently builds against a stale spec otherwise.
2. **Progenitor 0.15**: each operation may have only ONE success response shape (envelope responses, no 200+204 mixes). utoipa's 3.1 `{"type": "null"}` output for `Option<RefType>` is down-converted by `normalize_openapi()` in `crates/vise-api/examples/openapi.rs` — if codegen panics after adding `Option<SessionOutcome>`, extend that function.
3. **Migrations**: pre-1.0 convention is editing `crates/vise-core/migrations/0001_create_sessions.sql` in place, then `just db-reset && just db-migrate`. sqlx `query_as!` macros are compile-time checked against the live DB, so migrate **before** writing code that references new columns.
4. **`unsafe` in edition 2024**: `std::env::set_var` is unsafe; we avoid it entirely (see Task 9).

---

### Task 1: `github_repo` environment model + validation (vise-core)

**Files:**
- Modify: `crates/vise-core/src/sessions/model.rs:13-17`
- Test: same file, `#[cfg(test)]` module at bottom

**Step 1: Write the failing tests**

Append to `crates/vise-core/src/sessions/model.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn github_env(repo: Option<&str>, base: Option<&str>) -> Environment {
        Environment {
            kind: "github_repo".into(),
            repo: repo.map(String::from),
            base_branch: base.map(String::from),
        }
    }

    #[test]
    fn self_hosted_needs_no_repo() {
        let env = Environment { kind: "self_hosted".into(), repo: None, base_branch: None };
        assert!(env.validate().is_ok());
    }

    #[test]
    fn github_repo_requires_repo() {
        assert!(github_env(None, None).validate().is_err());
    }

    #[test]
    fn github_repo_accepts_owner_slash_name() {
        assert!(github_env(Some("vise-sh/vise-new"), None).validate().is_ok());
        assert!(github_env(Some("vise-sh/vise-new"), Some("main")).validate().is_ok());
    }

    #[test]
    fn github_repo_rejects_bad_formats() {
        for bad in ["vise-sh", "a/b/c", "", "owner/", "/name", "https://github.com/a/b"] {
            assert!(github_env(Some(bad), None).validate().is_err(), "{bad:?} should fail");
        }
    }

    #[test]
    fn unknown_kind_rejected() {
        let env = Environment { kind: "kubernetes".into(), repo: None, base_branch: None };
        assert!(env.validate().is_err());
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p vise-core sessions::model`
Expected: compile error — `Environment` has no `repo` field / no `validate`.

**Step 3: Implement**

Replace the `Environment` struct (`model.rs:13-17`) with:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Environment {
    /// "self_hosted" or "github_repo"
    pub kind: String,
    /// Required when kind == "github_repo": "owner/name"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// Branch to base work on; None = repo default branch
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_branch: Option<String>,
}

impl Environment {
    pub fn validate(&self) -> Result<(), String> {
        match self.kind.as_str() {
            "self_hosted" => Ok(()),
            "github_repo" => {
                let repo = self.repo.as_deref().ok_or("github_repo requires `repo`")?;
                let mut parts = repo.split('/');
                match (parts.next(), parts.next(), parts.next()) {
                    (Some(owner), Some(name), None) if !owner.is_empty() && !name.is_empty() => {
                        Ok(())
                    }
                    _ => Err(format!("repo must be \"owner/name\", got {repo:?}")),
                }
            }
            other => Err(format!("unknown environment kind {other:?}")),
        }
    }
}
```

Note: a flat struct with optional fields (not a tagged enum) is deliberate — progenitor 0.15 handles this shape cleanly; tagged enums generate `oneOf` schemas that risk codegen breakage.

**Step 4: Run tests to verify they pass**

Run: `cargo test -p vise-core sessions::model`
Expected: 5 tests PASS. Then `cargo check --workspace` — expect errors only in `vise-cli` (Environment literal, fixed in Task 8; add `repo: None, base_branch: None` there now to keep the workspace green — the generated `vise-client` type updates after Task 7, so if `vise-cli`/`vise-host` still compile against the old client type, that's fine until then).

**Step 5: Commit**

```bash
git add crates/vise-core/src/sessions/model.rs
git commit -m "feat: github_repo environment kind with validation"
```

---

### Task 2: Reject invalid environments at session create (vise-api)

**Files:**
- Modify: `crates/vise-api/src/routes/sessions.rs:113-124` (create_session)

**Step 1: Add validation to the handler**

In `create_session`, before calling the service:

```rust
    if let Err(reason) = request.environment.validate() {
        tracing::warn!(%reason, "rejected session create");
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }
```

Add a `(status = 422, description = "Invalid environment")` line to the `#[utoipa::path]` responses for `create_session`. (Error responses don't count against progenitor's one-success-shape rule.)

**Step 2: Verify**

Run: `cargo check -p vise-api`
Expected: clean.

**Step 3: Commit**

```bash
git add crates/vise-api/src/routes/sessions.rs
git commit -m "feat: validate session environment at create (422)"
```

---

### Task 3: `outcome` column + model + persistence (vise-core)

**Files:**
- Modify: `crates/vise-core/migrations/0001_create_sessions.sql:2-16`
- Modify: `crates/vise-core/src/sessions/model.rs`
- Modify: `crates/vise-core/src/sessions/repository.rs` (finish signature)
- Modify: `crates/vise-core/src/sessions/postgres.rs` (SessionRow, all RETURNING/SELECT lists, finish)
- Modify: `crates/vise-core/src/sessions/service.rs:90-106` (finish)

**Step 1: Edit migration 0001 in place**

Add to the `sessions` table definition, after `error TEXT,`:

```sql
    outcome JSONB,
```

**Step 2: Reset + migrate (required before sqlx macros will compile)**

Run: `just db-reset && sleep 3 && just db-migrate`
Expected: migration applies cleanly.

**Step 3: Add the model**

In `model.rs`, add (flat struct, same progenitor rationale as Task 1):

```rust
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SessionOutcome {
    /// "pr_opened" | "pushed_no_pr" | "uncommitted_changes" | "no_changes"
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}
```

Add to `Session` (after `error`): `pub outcome: Option<SessionOutcome>,`

**Step 4: Thread through persistence**

- `SessionRow`: add `outcome: Option<sqlx::types::Json<SessionOutcome>>,` and map with `outcome: row.outcome.map(|j| j.0)` in `From<SessionRow>`.
- Every `SELECT`/`RETURNING` column list in `postgres.rs` (get, list, claim_pending, finish, request_cancel): add `outcome as "outcome: _"` after `error`.
- `create` INSERT: unchanged (outcome starts NULL).
- `finish` (repository trait, postgres impl, service): add `outcome: Option<SessionOutcome>` parameter; postgres impl sets `outcome = $6` (bind `outcome.map(sqlx::types::Json)`). Note `query_as!` with a custom-typed bind may need `as _` on the bind: `outcome.map(sqlx::types::Json) as _`.
- `service.rs:36-51` `create`: add `outcome: None` to the `Session` literal.

**Step 5: Verify**

Run: `cargo test -p vise-core && cargo check -p vise-api`
Expected: vise-core green; vise-api has one error at `routes/hosts.rs` finish call (fixed next task) — if so, pass `None` there now to stay green.

**Step 6: Commit**

```bash
git add crates/vise-core crates/vise-api
git commit -m "feat: session outcome column and model"
```

---

### Task 4: Host finish reports outcome (vise-api)

**Files:**
- Modify: `crates/vise-api/src/routes/hosts.rs:147-152` (FinishRequest), `:268-295` (finish)

**Step 1: Extend FinishRequest**

```rust
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct FinishRequest {
    pub status: vise_core::sessions::model::SessionStatus,
    pub stop_reason: Option<String>,
    pub error: Option<String>,
    #[serde(default)]
    pub outcome: Option<vise_core::sessions::model::SessionOutcome>,
}
```

Pass `request.outcome` through to `state.sessions.finish(...)`.

**Step 2: Verify + commit**

Run: `cargo check -p vise-api`
Expected: clean.

```bash
git add crates/vise-api/src/routes/hosts.rs
git commit -m "feat: hosts report session outcome on finish"
```

---

### Task 5: GitHub App client (vise-api)

**Files:**
- Create: `crates/vise-api/src/github.rs`
- Modify: `crates/vise-api/src/lib.rs` (add `pub mod github;`)
- Modify: `crates/vise-api/Cargo.toml`
- Test: inline `#[cfg(test)]` + wiremock

**Step 1: Add dependencies**

`crates/vise-api/Cargo.toml`:

```toml
jsonwebtoken = "9"
reqwest = { workspace = true }

[dev-dependencies]
wiremock = "0.6"
tokio = { workspace = true }
```

**Step 2: Write the failing test**

In `crates/vise-api/src/github.rs` (test module at bottom). Generate a throwaway RSA key for tests once with `openssl genrsa -out crates/vise-api/testdata/test-app-key.pem 2048` (test fixture only — never a real App key; commit it).

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn mints_installation_token_for_repo() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/repos/acme/widgets/installation"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": 42
            })))
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/app/installations/42/access_tokens"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "token": "ghs_testtoken",
                "expires_at": "2026-09-11T12:00:00Z"
            })))
            .mount(&server)
            .await;

        let client = GitHubAppClient::new(
            12345,
            include_str!("../testdata/test-app-key.pem"),
            server.uri(),
        )
        .unwrap();

        let minted = client.installation_token("acme/widgets").await.unwrap();
        assert_eq!(minted.token, "ghs_testtoken");
    }
}
```

**Step 3: Run test to verify it fails**

Run: `cargo test -p vise-api github`
Expected: compile error — `GitHubAppClient` not defined.

**Step 4: Implement**

```rust
use chrono::{DateTime, Utc};
use serde::Deserialize;

pub struct GitHubAppClient {
    app_id: u64,
    encoding_key: jsonwebtoken::EncodingKey,
    api_base: String,
    http: reqwest::Client,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InstallationToken {
    pub token: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Deserialize)]
struct Installation {
    id: u64,
}

impl GitHubAppClient {
    pub fn new(app_id: u64, private_key_pem: &str, api_base: String) -> anyhow::Result<Self> {
        let encoding_key = jsonwebtoken::EncodingKey::from_rsa_pem(private_key_pem.as_bytes())?;
        let http = reqwest::Client::builder()
            .user_agent("vise-server")
            .build()?;
        Ok(Self { app_id, encoding_key, api_base, http })
    }

    fn app_jwt(&self) -> anyhow::Result<String> {
        let now = Utc::now().timestamp();
        let claims = serde_json::json!({
            "iat": now - 60,          // clock-drift allowance
            "exp": now + 540,         // GitHub max is 10 min
            "iss": self.app_id.to_string(),
        });
        let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
        Ok(jsonwebtoken::encode(&header, &claims, &self.encoding_key)?)
    }

    /// Mint a short-lived installation access token scoped to `repo` ("owner/name").
    pub async fn installation_token(&self, repo: &str) -> anyhow::Result<InstallationToken> {
        let jwt = self.app_jwt()?;

        let installation: Installation = self
            .http
            .get(format!("{}/repos/{repo}/installation", self.api_base))
            .bearer_auth(&jwt)
            .header("Accept", "application/vnd.github+json")
            .send()
            .await?
            .error_for_status()
            .map_err(|e| anyhow::anyhow!("app not installed on {repo}? {e}"))?
            .json()
            .await?;

        let (_, name) = repo.split_once('/').ok_or_else(|| anyhow::anyhow!("bad repo"))?;

        let token: InstallationToken = self
            .http
            .post(format!(
                "{}/app/installations/{}/access_tokens",
                self.api_base, installation.id
            ))
            .bearer_auth(&jwt)
            .header("Accept", "application/vnd.github+json")
            .json(&serde_json::json!({ "repositories": [name] }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        Ok(token)
    }
}
```

Add `pub mod github;` to `crates/vise-api/src/lib.rs`.

**Step 5: Run test to verify it passes**

Run: `cargo test -p vise-api github`
Expected: PASS.

**Step 6: Commit**

```bash
git add crates/vise-api/src/github.rs crates/vise-api/src/lib.rs crates/vise-api/Cargo.toml crates/vise-api/testdata/test-app-key.pem
git commit -m "feat: GitHub App installation-token client"
```

---

### Task 6: Generic credentials endpoint + provider registry

The wire protocol is provider-agnostic (`provider: "github"` today; `"linear"`
etc. later add a provider impl + config with zero API changes).

**Files:**
- Create: `crates/vise-api/src/credentials.rs` (trait + GitHub provider)
- Modify: `crates/vise-api/src/lib.rs` (add `pub mod credentials;`)
- Modify: `crates/vise-api/src/state.rs` (add registry to AppState)
- Modify: `crates/vise-api/src/routes/hosts.rs` (new route)
- Modify: `crates/vise-api/Cargo.toml` (`async-trait = { workspace = true }`)
- Modify: `bins/vise-server/src/main.rs` (env config)

**Step 1: Provider seam** (`crates/vise-api/src/credentials.rs`)

```rust
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use vise_core::sessions::model::Session;

pub struct IssuedCredential {
    pub secret: String,
    pub expires_at: Option<DateTime<Utc>>,
}

pub enum IssueError {
    /// Provider is configured but doesn't apply to this session → 422
    NotApplicable(String),
    /// Upstream (GitHub/etc.) failure → 502
    Upstream(anyhow::Error),
}

#[async_trait]
pub trait CredentialProvider: Send + Sync {
    fn name(&self) -> &'static str;
    async fn issue(&self, session: &Session) -> Result<IssuedCredential, IssueError>;
}

/// v1: mints GitHub App installation tokens for github_repo sessions.
pub struct GithubCredentialProvider {
    pub client: crate::github::GitHubAppClient,
}

#[async_trait]
impl CredentialProvider for GithubCredentialProvider {
    fn name(&self) -> &'static str {
        "github"
    }

    async fn issue(&self, session: &Session) -> Result<IssuedCredential, IssueError> {
        let repo = session
            .environment
            .repo
            .as_deref()
            .filter(|_| session.environment.kind == "github_repo")
            .ok_or_else(|| {
                IssueError::NotApplicable("session has no github_repo environment".into())
            })?;

        let minted = self
            .client
            .installation_token(repo)
            .await
            .map_err(IssueError::Upstream)?;

        Ok(IssuedCredential {
            secret: minted.token,
            expires_at: Some(minted.expires_at),
        })
    }
}
```

**Step 2: AppState** (`crates/vise-api/src/state.rs`)

```rust
use std::collections::HashMap;

#[derive(Clone)]
pub struct AppState {
    pub sessions: Arc<SessionService<PostgresSessionRepository>>,
    pub hosts: Arc<HostService<PostgresHostRepository>>,
    /// Credential providers by name ("github", ...). Empty = none configured.
    pub credentials: HashMap<String, Arc<dyn crate::credentials::CredentialProvider>>,
}
```

Fix the `AppState` literal in `bins/vise-server/src/main.rs:48` (and any test/example constructing it).

**Step 3: Route** (`routes/hosts.rs`)

Add to `routes()`: `.route("/hosts/sessions/{id}/credentials", post(issue_credential))`

```rust
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct IssueCredentialRequest {
    /// Provider name, e.g. "github"
    pub provider: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct IssueCredentialResponse {
    pub provider: String,
    pub secret: String,
    /// None for non-expiring credentials
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[utoipa::path(
    post,
    path = "/hosts/sessions/{id}/credentials",
    operation_id = "issue_credential",
    tag = "hosts",
    params(("id" = String, Path, description = "Session ID")),
    request_body = IssueCredentialRequest,
    responses(
        (status = 200, description = "Short-lived credential for the session", body = IssueCredentialResponse),
        (status = 401, description = "Missing or invalid host token"),
        (status = 404, description = "Session not found"),
        (status = 409, description = "Host no longer holds this session"),
        (status = 422, description = "Provider not applicable to this session"),
        (status = 502, description = "Upstream credential issuer failed"),
        (status = 503, description = "Provider not configured on this server")
    )
)]
pub async fn issue_credential(
    State(state): State<AppState>,
    AuthedHost(host): AuthedHost,
    Path(id): Path<String>,
    Json(request): Json<IssueCredentialRequest>,
) -> Result<Json<IssueCredentialResponse>, StatusCode> {
    use crate::credentials::IssueError;

    let provider = state
        .credentials
        .get(&request.provider)
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    let session = state
        .sessions
        .get(&id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    // Only the host holding the running lease may obtain credentials.
    if session.host_id.as_deref() != Some(host.id.as_str())
        || !matches!(session.status, vise_core::sessions::model::SessionStatus::Running)
    {
        return Err(StatusCode::CONFLICT);
    }

    let issued = provider.issue(&session).await.map_err(|error| match error {
        IssueError::NotApplicable(reason) => {
            tracing::warn!(%reason, provider = %request.provider, "credential not applicable");
            StatusCode::UNPROCESSABLE_ENTITY
        }
        IssueError::Upstream(error) => {
            tracing::error!(%error, provider = %request.provider, "credential issue failed");
            StatusCode::BAD_GATEWAY
        }
    })?;

    Ok(Json(IssueCredentialResponse {
        provider: request.provider,
        secret: issued.secret,
        expires_at: issued.expires_at,
    }))
}
```

(Single 200 success shape — progenitor-safe. `Option<DateTime>` is a plain
nullable string in the schema, not an `Option<RefType>`, so `normalize_openapi`
shouldn't need changes — verify in Task 7.)

**Step 4: Server config** (`bins/vise-server/src/main.rs`, after pool setup)

```rust
    let mut credentials: std::collections::HashMap<
        String,
        Arc<dyn vise_api::credentials::CredentialProvider>,
    > = std::collections::HashMap::new();

    if let (Ok(app_id), Ok(key_path)) = (
        std::env::var("VISE_GITHUB_APP_ID"),
        std::env::var("VISE_GITHUB_APP_PRIVATE_KEY_PATH"),
    ) {
        let pem = std::fs::read_to_string(&key_path)?;
        let client = vise_api::github::GitHubAppClient::new(
            app_id.parse()?,
            &pem,
            "https://api.github.com".to_string(),
        )?;
        credentials.insert(
            "github".to_string(),
            Arc::new(vise_api::credentials::GithubCredentialProvider { client }),
        );
        tracing::info!(app_id, "github credential provider configured");
    } else {
        tracing::warn!("github app not configured; github_repo sessions will fail");
    }

    let state = AppState { sessions, hosts, credentials };
```

(Requires Task 5's `GitHubAppClient::new` to take ownership-friendly args as
written; if it stays `&str`-based this drops in unchanged.)

**Step 5: Verify + commit**

Run: `cargo check -p vise-api -p vise-server && cargo test -p vise-api`
Expected: clean.

```bash
git add crates/vise-api bins/vise-server
git commit -m "feat: generic session credentials endpoint with github provider"
```

---

### Task 7: Regenerate OpenAPI spec + client

**Files:**
- Modify: `crates/vise-api/src/openapi.rs` (register `issue_credential` path + `IssueCredentialRequest`/`IssueCredentialResponse`/`SessionOutcome` schemas, following the existing registration pattern in that file)
- Generated: `openapi/openapi.json`, `crates/vise-client` output

**Step 1:** Register the new operation and schemas in `openapi.rs` (mirror how existing hosts routes are registered).

**Step 2:** Run, in order:

```bash
cargo check -p vise-api
just gen-spec
cargo build -p vise-client
```

Expected: build.rs regenerates without panics. If you see `assertion failed: response_types.len() <= 1` — a route has two success shapes; fix the route. If it chokes on `{"type": "null"}` — extend `normalize_openapi()` in `crates/vise-api/examples/openapi.rs`.

**Step 3:** `cargo check --workspace` — fix any fallout in `vise-cli`/`vise-host` from the regenerated `Environment`/`Session` types (add `repo: None, base_branch: None` where Environment literals are built; the real CLI flags come next task).

**Step 4: Commit**

```bash
git add crates/vise-api openapi bins
git commit -m "feat: regenerate client with credentials endpoint and outcome"
```

---

### Task 8: CLI flags for repo sessions

**Files:**
- Modify: `bins/vise-cli/src/main.rs:61-81` (Create variant), `:128-157` (handler)

**Step 1:** Add to `SessionsCommand::Create`:

```rust
        /// Target GitHub repository ("owner/name"); switches environment to github_repo
        #[arg(long)]
        repo: Option<String>,

        /// Base branch for --repo (default: repo default branch)
        #[arg(long)]
        base_branch: Option<String>,
```

**Step 2:** In the handler, build the environment from the flags:

```rust
                let environment = match &repo {
                    Some(r) => Environment {
                        kind: "github_repo".into(),
                        repo: Some(r.clone()),
                        base_branch: base_branch.clone(),
                    },
                    None => Environment {
                        kind: "self_hosted".into(),
                        repo: None,
                        base_branch: None,
                    },
                };
```

**Step 3: Verify + commit**

Run: `cargo build -p vise-cli && cargo run -p vise-cli -- sessions create --help`
Expected: `--repo` and `--base-branch` appear.

```bash
git add bins/vise-cli
git commit -m "feat: vise sessions create --repo owner/name"
```

---

### Task 9: Host workspace preparation (clone + credentials)

**Files:**
- Create: `bins/vise-host/src/github.rs`
- Modify: `bins/vise-host/src/main.rs` (mod decl)
- Modify: `bins/vise-host/Cargo.toml` (`[dev-dependencies] tempfile = "3"`)
- Test: inline, against a local `file://` remote (no network)

**How credentials reach the agent** (decided; do NOT use `std::env::set_var` — it's `unsafe` in edition 2024 and racy under tokio):

- **git push auth**: clone-local `credential.helper` that reads a token file vise-host keeps fresh. Refresh = rewrite the file; nothing restarts.
- **`gh` CLI auth**: prefix the harness command with `env GH_TOKEN=<token>` — `resolve_command` returns the command string that `AcpAgent::from_str` parses, so `env GH_TOKEN=... npx -y ...` injects the var into the child only. Caveats: token visible in local `ps` for the session's duration (short-lived, repo-scoped token — accepted for v1) and the env value doesn't refresh mid-session (`gh` caveat documented in design doc).
- **cwd**: already handled — `acp.rs:122` passes the workspace dir via `NewSessionRequest::new(workspace)`.

**Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn sh(dir: &std::path::Path, cmd: &str) -> String {
        let out = std::process::Command::new("sh")
            .arg("-c").arg(cmd).current_dir(dir)
            .output().expect("spawn");
        assert!(out.status.success(), "{cmd}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Builds a local origin with one commit on `main`, returns its path.
    fn make_origin(tmp: &std::path::Path) -> std::path::PathBuf {
        let src = tmp.join("src");
        std::fs::create_dir_all(&src).unwrap();
        sh(&src, "git init -b main -q && git config user.email t@t && git config user.name t");
        std::fs::write(src.join("README.md"), "hi").unwrap();
        sh(&src, "git add . && git commit -qm init");
        let bare = tmp.join("origin.git");
        sh(tmp, &format!("git clone -q --bare {} {}", src.display(), bare.display()));
        bare
    }

    #[tokio::test]
    async fn prepares_workspace_from_clone_url() {
        let tmp = tempfile::tempdir().unwrap();
        let origin = make_origin(tmp.path());
        let clone_url = format!("file://{}", origin.display());
        let work = tmp.path().join("work");

        let prepared = prepare_workspace(&work, &clone_url, Some("main"), "tok_initial")
            .await
            .unwrap();

        assert!(prepared.repo_dir.join("README.md").exists());
        assert_eq!(
            sh(&prepared.repo_dir, "git rev-parse --abbrev-ref HEAD").trim(),
            "main"
        );
        // identity is clone-local
        assert_eq!(sh(&prepared.repo_dir, "git config user.name").trim(), "vise[bot]");
        // token file exists and is refreshable
        assert_eq!(std::fs::read_to_string(&prepared.token_file).unwrap(), "tok_initial");
        prepared.write_token("tok_refreshed").unwrap();
        assert_eq!(std::fs::read_to_string(&prepared.token_file).unwrap(), "tok_refreshed");
        // base commit recorded
        assert_eq!(
            prepared.base_commit,
            sh(&prepared.repo_dir, "git rev-parse HEAD").trim()
        );
    }
}
```

**Step 2:** `cargo test -p vise-host github` — expect compile failure.

**Step 3: Implement** (`bins/vise-host/src/github.rs`)

```rust
use std::path::{Path, PathBuf};

pub struct PreparedRepo {
    pub repo_dir: PathBuf,
    pub token_file: PathBuf,
    pub base_commit: String,
    pub base_branch: String,
}

impl PreparedRepo {
    pub fn write_token(&self, token: &str) -> anyhow::Result<()> {
        // Write-then-rename so the credential helper never reads a torn file.
        let tmp = self.token_file.with_extension("tmp");
        std::fs::write(&tmp, token)?;
        std::fs::rename(&tmp, &self.token_file)?;
        Ok(())
    }
}

async fn git(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = tokio::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .await?;
    if !out.status.success() {
        anyhow::bail!("git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Clone `clone_url` into `{workdir}/repo`, configure clone-local identity and
/// a credential helper that reads a token file vise-host keeps fresh.
pub async fn prepare_workspace(
    workdir: &Path,
    clone_url: &str,
    base_branch: Option<&str>,
    initial_token: &str,
) -> anyhow::Result<PreparedRepo> {
    std::fs::create_dir_all(workdir)?;
    let repo_dir = workdir.join("repo");
    let token_file = workdir.join("github-token");

    let mut args = vec!["clone", "--depth", "50"];
    if let Some(branch) = base_branch {
        args.extend(["--branch", branch]);
    }
    let repo_dir_s = repo_dir.to_string_lossy().into_owned();
    args.extend([clone_url, repo_dir_s.as_str()]);
    git(workdir, &args).await?;

    let helper = format!(
        "!f() {{ test \"$1\" = get && echo username=x-access-token && echo \"password=$(cat '{}')\"; }}; f",
        token_file.display()
    );
    git(&repo_dir, &["config", "user.name", "vise[bot]"]).await?;
    git(&repo_dir, &["config", "user.email", "vise-bot@users.noreply.github.com"]).await?;
    git(&repo_dir, &["config", "credential.helper", &helper]).await?;

    let base_commit = git(&repo_dir, &["rev-parse", "HEAD"]).await?.trim().to_string();
    let base_branch = git(&repo_dir, &["rev-parse", "--abbrev-ref", "HEAD"])
        .await?
        .trim()
        .to_string();

    let prepared = PreparedRepo { repo_dir, token_file, base_commit, base_branch };
    prepared.write_token(initial_token)?;
    Ok(prepared)
}
```

Add `mod github;` to `main.rs`. Note the test passes a `file://` URL directly; production callers build `https://github.com/{owner}/{name}.git` (Task 11) — the helper only supplies credentials when git asks.

**Step 4:** `cargo test -p vise-host github` — PASS.

**Step 5: Commit**

```bash
git add bins/vise-host
git commit -m "feat: host clones github_repo workspaces with credential helper"
```

---

### Task 10: Host outcome detection

**Files:**
- Modify: `bins/vise-host/src/github.rs`

**Step 1: Write the failing tests** (extend the test module; reuse `sh`/`make_origin`)

```rust
    async fn prepared(tmp: &std::path::Path) -> PreparedRepo {
        let origin = make_origin(tmp);
        let url = format!("file://{}", origin.display());
        prepare_workspace(&tmp.join("work"), &url, Some("main"), "tok").await.unwrap()
    }

    #[tokio::test]
    async fn detects_no_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let p = prepared(tmp.path()).await;
        let outcome = detect_outcome(&p, None).await.unwrap();
        assert_eq!(outcome.kind, "no_changes");
    }

    #[tokio::test]
    async fn detects_uncommitted_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let p = prepared(tmp.path()).await;
        std::fs::write(p.repo_dir.join("dirty.txt"), "x").unwrap();
        let outcome = detect_outcome(&p, None).await.unwrap();
        assert_eq!(outcome.kind, "uncommitted_changes");
    }

    #[tokio::test]
    async fn detects_pushed_branch_without_pr() {
        let tmp = tempfile::tempdir().unwrap();
        let p = prepared(tmp.path()).await;
        sh(&p.repo_dir, "git checkout -qb vise/test && git commit -qm work --allow-empty && git push -q origin vise/test");
        let outcome = detect_outcome(&p, None).await.unwrap();
        assert_eq!(outcome.kind, "pushed_no_pr");
        assert_eq!(outcome.branch.as_deref(), Some("vise/test"));
    }

    #[tokio::test]
    async fn detects_committed_but_unpushed_as_uncommitted_kind() {
        let tmp = tempfile::tempdir().unwrap();
        let p = prepared(tmp.path()).await;
        sh(&p.repo_dir, "git checkout -qb vise/test && git commit -qm work --allow-empty");
        let outcome = detect_outcome(&p, None).await.unwrap();
        // local-only work: without a push there is nothing on GitHub
        assert_eq!(outcome.kind, "uncommitted_changes");
        assert_eq!(outcome.branch.as_deref(), Some("vise/test"));
    }
```

**Step 2:** `cargo test -p vise-host github` — new tests fail to compile.

**Step 3: Implement**

```rust
#[derive(Debug)]
pub struct Outcome {
    pub kind: String,
    pub pr_url: Option<String>,
    pub branch: Option<String>,
}

/// Inspect the workspace after the agent ran. `pr_lookup` is Some((repo, token))
/// in production to query GitHub for an open PR; None in local tests.
pub async fn detect_outcome(
    prepared: &PreparedRepo,
    pr_lookup: Option<(&str, &str)>,
) -> anyhow::Result<Outcome> {
    let dir = &prepared.repo_dir;
    let branch = git(dir, &["rev-parse", "--abbrev-ref", "HEAD"]).await?.trim().to_string();
    let head = git(dir, &["rev-parse", "HEAD"]).await?.trim().to_string();
    let dirty = !git(dir, &["status", "--porcelain"]).await?.trim().is_empty();

    if dirty {
        return Ok(Outcome {
            kind: "uncommitted_changes".into(),
            pr_url: None,
            branch: Some(branch),
        });
    }

    if branch == prepared.base_branch && head == prepared.base_commit {
        return Ok(Outcome { kind: "no_changes".into(), pr_url: None, branch: None });
    }

    // Committed work exists; is it on the remote?
    let on_remote = !git(dir, &["ls-remote", "--heads", "origin", &branch])
        .await?
        .trim()
        .is_empty();

    if !on_remote {
        // Local-only commits: nothing landed on GitHub.
        return Ok(Outcome {
            kind: "uncommitted_changes".into(),
            pr_url: None,
            branch: Some(branch),
        });
    }

    if let Some((repo, token)) = pr_lookup
        && let Some(url) = find_open_pr(repo, &branch, token).await?
    {
        return Ok(Outcome { kind: "pr_opened".into(), pr_url: Some(url), branch: Some(branch) });
    }

    Ok(Outcome { kind: "pushed_no_pr".into(), pr_url: None, branch: Some(branch) })
}

async fn find_open_pr(repo: &str, branch: &str, token: &str) -> anyhow::Result<Option<String>> {
    let owner = repo.split('/').next().unwrap_or_default();
    let url = format!(
        "https://api.github.com/repos/{repo}/pulls?head={owner}:{branch}&state=open"
    );
    let pulls: serde_json::Value = reqwest::Client::new()
        .get(&url)
        .bearer_auth(token)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "vise-host")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(pulls
        .as_array()
        .and_then(|a| a.first())
        .and_then(|pr| pr["html_url"].as_str())
        .map(String::from))
}
```

**Step 4:** `cargo test -p vise-host github` — all PASS.

**Step 5: Commit**

```bash
git add bins/vise-host/src/github.rs
git commit -m "feat: host detects github session outcome from workspace"
```

---

### Task 11: Wire it into the host session loop

**Files:**
- Modify: `bins/vise-host/src/main.rs:99-153` (run_session), `:22-34` (Cli)
- Modify: `bins/vise-host/src/acp.rs:27-32` (resolve_command)

**Step 1: CLI flag**

```rust
    /// Keep session workspaces on disk after the session finishes
    #[arg(long, default_value_t = false)]
    keep_workspaces: bool,
```

Thread it into `run_session` (pass `&cli` or the bool).

**Step 2: Rework `run_session`**

Before the runtime starts, when `session.environment.kind == "github_repo"`:

```rust
    let workdir = std::env::temp_dir().join("vise-sessions").join(&session.id);
    let workspace = workdir.join("workspace");
    std::fs::create_dir_all(&workspace)?;

    let mut prepared: Option<github::PreparedRepo> = None;
    let mut session = session; // mutated below for github sessions

    if session.environment.kind == "github_repo" {
        let repo = session.environment.repo.clone()
            .ok_or_else(|| anyhow::anyhow!("github_repo session missing repo"))?;

        // Fail fast (before the agent starts) if credential issue or clone fails.
        let issued = client
            .issue_credential(
                &session.id,
                &vise_client::types::IssueCredentialRequest { provider: "github".into() },
            )
            .await
            .map_err(|e| anyhow::anyhow!("github credential issue failed: {e}"))?
            .into_inner();

        let clone_url = format!("https://x-access-token:{}@github.com/{repo}.git", issued.secret);
        let p = github::prepare_workspace(
            &workdir,
            &clone_url,
            session.environment.base_branch.as_deref(),
            &issued.secret,
        )
        .await
        .map_err(|e| anyhow::anyhow!("workspace clone failed: {e}"))?;

        // Strip the token from the persisted remote URL; pushes go through the
        // credential helper instead.
        // (add a `set_remote_url` helper in github.rs: git remote set-url origin https://github.com/{repo}.git)
        github::set_remote_url(&p.repo_dir, &format!("https://github.com/{repo}.git")).await?;

        session.input = format!(
            "You are working in a clone of {repo} (currently on branch {base}). \
             Complete the task below. When done: create a descriptively named branch, \
             commit your work with clear messages, push it, and open a pull request with \
             `gh pr create`. Report the PR URL in your final message.\n\n{input}",
            base = p.base_branch,
            input = session.input,
        );
        prepared = Some(p);
    }
```

The **agent must run with the repo as its workspace**: pass `prepared.as_ref().map(|p| p.repo_dir.clone()).unwrap_or(workspace)` as the workspace argument to `runtime.run(...)` (acp.rs already forwards it as ACP cwd).

**`gh` token injection**: change `resolve_command` in `acp.rs` to accept an optional token and, for github sessions, return
`format!("env GH_TOKEN={token} npx -y @agentclientprotocol/claude-agent-acp@latest")`.
Plumb it via a new field on the runtime call — simplest: change `SessionRuntime::run` to take `command_prefix: Option<String>` OR have `AcpProcessRuntime` hold `Option<String>` set per-run. Pick the smallest change that compiles; do not log the command string (it contains the token).

**Token refresh task** (alongside heartbeat):

```rust
    let refresher = prepared.as_ref().map(|p| {
        let client = client.clone();
        let session_id = session.id.clone();
        let token_file_writer = p.token_file.clone(); // or clone PreparedRepo paths
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(45 * 60));
            interval.tick().await; // skip immediate tick
            loop {
                interval.tick().await;
                let request =
                    vise_client::types::IssueCredentialRequest { provider: "github".into() };
                match client.issue_credential(&session_id, &request).await {
                    Ok(issued) => {
                        let tmp = token_file_writer.with_extension("tmp");
                        if std::fs::write(&tmp, &issued.into_inner().secret).is_ok() {
                            let _ = std::fs::rename(&tmp, &token_file_writer);
                        }
                    }
                    Err(error) => tracing::warn!(%error, "credential refresh failed"),
                }
            }
        })
    });
```

Abort it next to `heartbeat.abort()`.

**After the runtime finishes**, before `finish_session`:

```rust
    let outcome = match (&prepared, &run_result) {
        (Some(p), Ok(_)) => {
            let repo = session.environment.repo.as_deref().unwrap_or_default();
            let token = std::fs::read_to_string(&p.token_file).unwrap_or_default();
            match github::detect_outcome(p, Some((repo, &token))).await {
                Ok(o) => Some(vise_client::types::SessionOutcome {
                    kind: o.kind,
                    pr_url: o.pr_url,
                    branch: o.branch,
                }),
                Err(error) => {
                    tracing::warn!(%error, "outcome detection failed");
                    None
                }
            }
        }
        _ => None,
    };
```

Include `outcome` in `FinishRequest`.

**Cleanup**:

```rust
    let keep = keep_workspaces
        || outcome.as_ref().is_some_and(|o| o.kind == "uncommitted_changes");
    if !keep {
        let _ = std::fs::remove_dir_all(&workdir);
    } else {
        tracing::info!(path = %workdir.display(), "keeping workspace");
    }
```

**Step 3: Verify**

Run: `cargo build --workspace && cargo test --workspace`
Expected: green. (`clippy` too if configured: `cargo clippy --workspace`.)

**Step 4: Commit**

```bash
git add bins/vise-host
git commit -m "feat: host runs github_repo sessions end to end"
```

---

### Task 12: Surface outcome in the CLI

**Files:**
- Modify: `bins/vise-cli/src/main.rs:199-244` (watch_session / render)

**Step 1:** `watch_session` currently just prints the `done` event. After the stream ends (or on `done`), fetch the session and print the outcome:

```rust
    // in render loop plumbing: on "done", after printing the status line,
    // caller fetches and prints outcome
    let session = client.get_session(session_id).await?.into_inner();
    if let Some(outcome) = session.outcome {
        match (outcome.kind.as_str(), outcome.pr_url, outcome.branch) {
            ("pr_opened", Some(url), _) => println!("PR: {url}"),
            ("pushed_no_pr", _, Some(branch)) => println!("pushed branch {branch} (no PR)"),
            ("uncommitted_changes", _, _) => println!("warning: agent left uncommitted work; workspace kept on host"),
            ("no_changes", _, _) => println!("no changes made"),
            _ => {}
        }
    }
```

(`watch_session` needs the `ViseClient` — pass it in instead of constructing from `base_url`, or construct one inside.)

**Step 2: Verify + commit**

Run: `cargo build -p vise-cli`

```bash
git add bins/vise-cli
git commit -m "feat: show session outcome after watch"
```

---

### Task 13: Manual end-to-end verification

No code. Checklist (needs the user's GitHub App and a scratch repo):

1. Configure the App: install it on a scratch repo (e.g. `owner/vise-e2e-scratch`) with contents:read/write + pull_requests:read/write. Save the App ID and private-key PEM path.
2. `just db-reset && just db-migrate` (schema changed).
3. Run server: `VISE_GITHUB_APP_ID=... VISE_GITHUB_APP_PRIVATE_KEY_PATH=... just run-server`. Expect `github app configured` in logs.
4. Enroll + run a host: `just run-cli hosts create dev-host`, then `VISE_HOST_TOKEN=... cargo run -p vise-host`.
5. Create the session:
   `just run-cli sessions create --repo owner/vise-e2e-scratch --watch "Add a CONTRIBUTING.md with a one-paragraph placeholder, then open a PR"`
6. Verify: PR exists on GitHub authored by the App bot; CLI prints `PR: https://github.com/...`; `sessions get` shows `outcome.kind == "pr_opened"`; host workspace under `$TMPDIR/vise-sessions/<id>` was removed.
7. Negative checks: create a session for a repo the App is NOT installed on → session fails fast with a clear error, agent never starts. Create with `--repo bad-format` → CLI gets 422.

Record any deviations in the design doc.

---

## Status 2026-09-11: code complete, verification pending

Tasks 1–12 implemented (uncommitted) during a harness outage that blocked all
shell commands — every task was spec-reviewed by reading the code, and a
critical token-leak (credentialed clone URL reaching logs/FinishRequest.error
via git error text) was found and fixed (clone now authenticates via
`-c credential.helper=...`; `set_remote_url` removed; acp.rs redacts the
secret from error paths). Nothing has been compiled, tested, or committed.

**Verification cascade — run in this order once shell access returns:**

```bash
openssl genrsa -out crates/vise-api/testdata/test-app-key.pem 2048  # test fixture
just db-reset && sleep 3 && just db-migrate                          # outcome column
cargo test -p vise-core                                              # Task 1/3
cargo check -p vise-api && cargo test -p vise-api                    # Tasks 2,4,5,6 (github + wiremock)
just gen-spec && cargo build -p vise-client                          # Task 7 (watch for progenitor panics)
cargo build --workspace && cargo test --workspace                    # Tasks 8-12 resolve post-regen
cargo clippy --workspace
```

Expected pre-regen rust-analyzer errors (unresolved `Environment.repo`,
`SessionOutcome`, `issue_credential`, `FinishRequest.outcome` in bins/) clear
after `just gen-spec` + rebuild. Then Task 13 (manual E2E with the user's
GitHub App).

## Execution notes

- Tasks 1–4 are sequential (schema → model → API). Task 5 is independent of 1–4. Tasks 6–7 need both streams. 8–12 depend on 7.
- After each task: run the listed verification commands **before** committing (superpowers:verification-before-completion).
- If `sqlx` macros fail to compile with "no such column", the DB wasn't migrated — rerun Task 3 Step 2.
