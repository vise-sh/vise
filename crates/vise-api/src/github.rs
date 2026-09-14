use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::Deserialize;
use tokio::sync::Mutex;
use vise_core::sessions::pr_tracking::{
    CheckRunObservation, PrObservation, ReviewObservation, ReviewVerdict,
};

use crate::credentials::{CredentialProvider, GithubCredentialProvider, PatCredentialProvider};

/// Installation tokens are reused for server-side reads until this close to
/// their expiry (GitHub issues them with a one-hour lifetime).
const READ_TOKEN_EXPIRY_MARGIN: chrono::Duration = chrono::Duration::minutes(5);

pub struct GitHubAppClient {
    app_id: u64,
    encoding_key: jsonwebtoken::EncodingKey,
    api_base: String,
    http: reqwest::Client,
    /// Installation tokens cached for the server's own reads, by repository.
    read_tokens: Mutex<HashMap<String, InstallationToken>>,
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
        Ok(Self {
            app_id,
            encoding_key,
            api_base,
            http,
            read_tokens: Mutex::new(HashMap::new()),
        })
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

        let (_, name) = repo
            .split_once('/')
            .ok_or_else(|| anyhow::anyhow!("bad repo"))?;

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

    /// Installation token for the server's own reads on `repo`, minted once
    /// and reused until shortly before it expires. Hosts always get a fresh
    /// token from [`installation_token`](Self::installation_token) instead.
    async fn read_token(&self, repo: &str) -> anyhow::Result<String> {
        let mut cache = self.read_tokens.lock().await;
        if let Some(cached) = cache.get(repo)
            && cached.expires_at > Utc::now() + READ_TOKEN_EXPIRY_MARGIN
        {
            return Ok(cached.token.clone());
        }
        let minted = self.installation_token(repo).await?;
        let token = minted.token.clone();
        cache.insert(repo.to_string(), minted);
        Ok(token)
    }
}

/// How the server authenticates to GitHub: both the credentials it hands to
/// hosts and its own pull request reads (PR tracking, follow-up composition)
/// come from the same source.
#[derive(Clone)]
pub enum GithubAuth {
    /// GitHub App: short-lived installation tokens minted per repository.
    App(Arc<GitHubAppClient>),
    /// A personal access token used as-is for every repository.
    Pat(String),
}

impl GithubAuth {
    /// Pick the configured mode. The App takes precedence: a PAT configured
    /// alongside it is ignored (and said so in the log). Neither → `None`.
    pub fn select(app: Option<Arc<GitHubAppClient>>, pat: Option<String>) -> Option<Self> {
        match (app, pat) {
            (Some(app), pat) => {
                if pat.is_some() {
                    tracing::info!(
                        "both the github app and VISE_GITHUB_PAT are configured; using the app and ignoring the pat"
                    );
                }
                Some(GithubAuth::App(app))
            }
            (None, Some(pat)) => Some(GithubAuth::Pat(pat)),
            (None, None) => None,
        }
    }

    /// "app" or "pat", for logs.
    pub fn kind(&self) -> &'static str {
        match self {
            GithubAuth::App(_) => "app",
            GithubAuth::Pat(_) => "pat",
        }
    }

    /// The "github" credential provider hosts obtain tokens from.
    pub fn credential_provider(&self) -> Arc<dyn CredentialProvider> {
        match self {
            GithubAuth::App(client) => Arc::new(GithubCredentialProvider {
                client: client.clone(),
            }),
            GithubAuth::Pat(pat) => Arc::new(PatCredentialProvider::new(pat.clone())),
        }
    }

    /// Bearer token for the server's own reads on `repo` ("owner/name").
    async fn token_for(&self, repo: &str) -> anyhow::Result<String> {
        match self {
            GithubAuth::App(client) => client.read_token(repo).await,
            GithubAuth::Pat(pat) => Ok(pat.clone()),
        }
    }
}

// --- Pull request reads (PR tracking + follow-up composition) ---------------

/// A pull request identified from its `html_url`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRef {
    pub owner: String,
    pub repo: String,
    pub number: u64,
}

impl PullRef {
    /// Parse `https://github.com/{owner}/{repo}/pull/{number}`.
    pub fn parse(pr_url: &str) -> Option<Self> {
        let rest = pr_url
            .strip_prefix("https://github.com/")
            .or_else(|| pr_url.strip_prefix("http://github.com/"))?;
        let mut parts = rest.trim_end_matches('/').split('/');
        let owner = parts.next().filter(|s| !s.is_empty())?;
        let repo = parts.next().filter(|s| !s.is_empty())?;
        if parts.next()? != "pull" {
            return None;
        }
        let number = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            owner: owner.to_string(),
            repo: repo.to_string(),
            number,
        })
    }

    pub fn full_repo(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }
}

#[derive(Debug)]
pub enum GithubError {
    /// GitHub answered with a non-2xx status.
    Status {
        status: u16,
        rate_limited: bool,
        /// From `x-ratelimit-reset`, when present.
        reset_at: Option<DateTime<Utc>>,
    },
    /// Transport failure or undecodable body.
    Transport(anyhow::Error),
    /// No token could be obtained for the repository (App mint failed).
    Auth(anyhow::Error),
}

impl GithubError {
    /// 403/404: the token cannot see the PR (or it is gone). Persistent
    /// occurrences move the session to `sync_error` rather than going stale.
    pub fn is_not_visible(&self) -> bool {
        matches!(
            self,
            GithubError::Status {
                status: 403 | 404,
                rate_limited: false,
                ..
            }
        )
    }

    pub fn rate_limit_reset(&self) -> Option<Option<DateTime<Utc>>> {
        match self {
            GithubError::Status {
                rate_limited: true,
                reset_at,
                ..
            } => Some(*reset_at),
            _ => None,
        }
    }
}

impl std::fmt::Display for GithubError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GithubError::Status {
                status,
                rate_limited: true,
                ..
            } => write!(f, "github rate limited (http {status})"),
            GithubError::Status { status, .. } => write!(f, "github returned http {status}"),
            GithubError::Transport(error) => write!(f, "github request failed: {error}"),
            GithubError::Auth(error) => write!(f, "github credential unavailable: {error}"),
        }
    }
}

impl std::error::Error for GithubError {}

impl From<reqwest::Error> for GithubError {
    fn from(error: reqwest::Error) -> Self {
        GithubError::Transport(error.into())
    }
}

#[derive(Debug, Clone, Deserialize)]
struct GitRef {
    #[serde(rename = "ref")]
    name: String,
    sha: String,
}

#[derive(Debug, Clone, Deserialize)]
struct RawPull {
    state: String,
    #[serde(default)]
    merged: bool,
    head: GitRef,
    html_url: String,
}

/// The fields of a pull request the server cares about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullSnapshot {
    pub html_url: String,
    /// GitHub `state`: "open" | "closed" (merged PRs are closed + merged).
    pub closed: bool,
    pub merged: bool,
    pub head_ref: String,
    pub head_sha: String,
}

#[derive(Debug, Clone, Deserialize)]
struct RawUser {
    login: String,
}

#[derive(Debug, Clone, Deserialize)]
struct RawReview {
    user: Option<RawUser>,
    state: String,
    #[serde(default)]
    body: Option<String>,
    submitted_at: Option<DateTime<Utc>>,
    commit_id: Option<String>,
}

/// A review's top-level summary (the text entered when submitting a review).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewSummary {
    pub reviewer: String,
    pub state: String,
    pub body: String,
}

#[derive(Debug, Clone, Deserialize)]
struct RawReviewComment {
    id: u64,
    in_reply_to_id: Option<u64>,
    user: Option<RawUser>,
    path: String,
    line: Option<u64>,
    original_line: Option<u64>,
    body: String,
    created_at: DateTime<Utc>,
}

/// An inline review comment with its file/line context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewComment {
    pub id: u64,
    pub in_reply_to_id: Option<u64>,
    pub reviewer: String,
    pub path: String,
    pub line: Option<u64>,
    pub body: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawCheckRun {
    name: String,
    status: String,
    conclusion: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawCheckRuns {
    check_runs: Vec<RawCheckRun>,
}

/// Read-only GitHub REST client for pull requests, used by the PR poller and
/// the follow-up endpoint. Authenticates every call through its
/// [`GithubAuth`], so callers work the same with an App or a PAT.
///
/// The credential needs `pull_requests: read` and `checks: read` on the
/// repository (GitHub App permissions "Pull requests" and "Checks", or the
/// equivalent fine-grained PAT permissions).
#[derive(Clone)]
pub struct GitHubApi {
    http: reqwest::Client,
    api_base: String,
    auth: GithubAuth,
}

impl GitHubApi {
    pub fn new(api_base: String, auth: GithubAuth) -> Self {
        let http = reqwest::Client::builder()
            .user_agent("vise-server")
            .build()
            .expect("reqwest client");
        Self {
            http,
            api_base,
            auth,
        }
    }

    pub fn api_base(&self) -> &str {
        &self.api_base
    }

    pub fn auth(&self) -> &GithubAuth {
        &self.auth
    }

    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        pr: &PullRef,
        path: &str,
    ) -> Result<T, GithubError> {
        let token = self
            .auth
            .token_for(&pr.full_repo())
            .await
            .map_err(GithubError::Auth)?;
        let response = self
            .http
            .get(format!("{}{path}", self.api_base))
            .bearer_auth(token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let remaining_zero = response
                .headers()
                .get("x-ratelimit-remaining")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.trim() == "0");
            let retry_after = response.headers().contains_key("retry-after");
            let reset_at = response
                .headers()
                .get("x-ratelimit-reset")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<i64>().ok())
                .and_then(|secs| DateTime::from_timestamp(secs, 0));
            let rate_limited = status.as_u16() == 429
                || (status.as_u16() == 403 && (remaining_zero || retry_after));
            return Err(GithubError::Status {
                status: status.as_u16(),
                rate_limited,
                reset_at,
            });
        }

        Ok(response.json().await?)
    }

    pub async fn pull(&self, pr: &PullRef) -> Result<PullSnapshot, GithubError> {
        let raw: RawPull = self
            .get_json(
                pr,
                &format!("/repos/{}/{}/pulls/{}", pr.owner, pr.repo, pr.number),
            )
            .await?;
        Ok(PullSnapshot {
            html_url: raw.html_url,
            closed: raw.state != "open",
            merged: raw.merged,
            head_ref: raw.head.name,
            head_sha: raw.head.sha,
        })
    }

    async fn raw_reviews(&self, pr: &PullRef) -> Result<Vec<RawReview>, GithubError> {
        self.get_json(
            pr,
            &format!(
                "/repos/{}/{}/pulls/{}/reviews?per_page=100",
                pr.owner, pr.repo, pr.number
            ),
        )
        .await
    }

    /// Reviews reduced to what the tracking reducer needs.
    pub async fn reviews(&self, pr: &PullRef) -> Result<Vec<ReviewObservation>, GithubError> {
        Ok(self
            .raw_reviews(pr)
            .await?
            .into_iter()
            .filter_map(|review| {
                Some(ReviewObservation {
                    reviewer: review.user?.login,
                    verdict: ReviewVerdict::parse(&review.state)?,
                    submitted_at: review.submitted_at?,
                    commit_sha: review.commit_id,
                })
            })
            .collect())
    }

    /// Review summaries with a non-empty body, oldest first.
    pub async fn review_summaries(&self, pr: &PullRef) -> Result<Vec<ReviewSummary>, GithubError> {
        Ok(self
            .raw_reviews(pr)
            .await?
            .into_iter()
            .filter_map(|review| {
                let body = review.body?.trim().to_string();
                if body.is_empty() {
                    return None;
                }
                Some(ReviewSummary {
                    reviewer: review.user.map(|u| u.login).unwrap_or_default(),
                    state: review.state.to_ascii_lowercase(),
                    body,
                })
            })
            .collect())
    }

    /// Inline review comments, oldest first.
    pub async fn review_comments(&self, pr: &PullRef) -> Result<Vec<ReviewComment>, GithubError> {
        let raw: Vec<RawReviewComment> = self
            .get_json(
                pr,
                &format!(
                    "/repos/{}/{}/pulls/{}/comments?per_page=100",
                    pr.owner, pr.repo, pr.number
                ),
            )
            .await?;
        let mut comments: Vec<ReviewComment> = raw
            .into_iter()
            .map(|c| ReviewComment {
                id: c.id,
                in_reply_to_id: c.in_reply_to_id,
                reviewer: c.user.map(|u| u.login).unwrap_or_default(),
                path: c.path,
                line: c.line.or(c.original_line),
                body: c.body,
                created_at: c.created_at,
            })
            .collect();
        comments.sort_by_key(|c| (c.created_at, c.id));
        Ok(comments)
    }

    /// Latest check run per check name on `sha`.
    pub async fn check_runs(
        &self,
        pr: &PullRef,
        sha: &str,
    ) -> Result<Vec<CheckRunObservation>, GithubError> {
        let raw: RawCheckRuns = self
            .get_json(
                pr,
                &format!(
                    "/repos/{}/{}/commits/{sha}/check-runs?per_page=100",
                    pr.owner, pr.repo
                ),
            )
            .await?;
        Ok(raw
            .check_runs
            .into_iter()
            .map(|run| CheckRunObservation {
                name: run.name,
                status: run.status,
                conclusion: run.conclusion,
            })
            .collect())
    }

    /// One full observation of the PR: pull, reviews and head check runs.
    pub async fn observe(
        &self,
        pr: &PullRef,
    ) -> Result<(PullSnapshot, PrObservation), GithubError> {
        let pull = self.pull(pr).await?;
        let reviews = self.reviews(pr).await?;
        let check_runs = self.check_runs(pr, &pull.head_sha).await?;
        let observation = PrObservation {
            merged: pull.merged,
            closed: pull.closed,
            head_sha: pull.head_sha.clone(),
            reviews,
            check_runs,
        };
        Ok((pull, observation))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::test_support::session;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const APP_TOKEN: &str = "ghs_testtoken";
    const PAT: &str = "github_pat_test";

    /// Mount the App endpoints: installation lookup and a token mint whose
    /// token expires far in the future; exactly `mints` mints are expected.
    async fn mount_app(server: &MockServer, mints: u64) {
        Mock::given(method("GET"))
            .and(path("/repos/acme/widgets/installation"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": 42
            })))
            .mount(server)
            .await;

        Mock::given(method("POST"))
            .and(path("/app/installations/42/access_tokens"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "token": APP_TOKEN,
                "expires_at": "2099-01-01T00:00:00Z"
            })))
            .expect(mints)
            .mount(server)
            .await;
    }

    fn app_client(server: &MockServer) -> Arc<GitHubAppClient> {
        Arc::new(
            GitHubAppClient::new(
                12345,
                include_str!("../testdata/test-app-key.pem"),
                server.uri(),
            )
            .unwrap(),
        )
    }

    #[tokio::test]
    async fn mints_installation_token_for_repo() {
        let server = MockServer::start().await;
        mount_app(&server, 1).await;

        let minted = app_client(&server)
            .installation_token("acme/widgets")
            .await
            .unwrap();
        assert_eq!(minted.token, APP_TOKEN);
    }

    #[tokio::test]
    async fn app_wins_over_pat_when_both_are_configured() {
        let server = MockServer::start().await;
        mount_app(&server, 1).await;

        let auth = GithubAuth::select(Some(app_client(&server)), Some(PAT.to_string()))
            .expect("configured");
        assert_eq!(auth.kind(), "app");

        // Hosts get App-minted tokens, not the PAT.
        let issued = auth
            .credential_provider()
            .issue(&session("github_repo", Some("acme/widgets")))
            .await
            .ok()
            .expect("issued");
        assert_eq!(issued.secret, APP_TOKEN);
        assert!(issued.expires_at.is_some());
    }

    #[tokio::test]
    async fn pat_is_the_fallback_without_the_app() {
        let auth = GithubAuth::select(None, Some(PAT.to_string())).expect("configured");
        assert_eq!(auth.kind(), "pat");

        let issued = auth
            .credential_provider()
            .issue(&session("github_repo", Some("acme/widgets")))
            .await
            .ok()
            .expect("issued");
        assert_eq!(issued.secret, PAT);
        assert_eq!(issued.expires_at, None);

        assert!(GithubAuth::select(None, None).is_none());
    }

    #[tokio::test]
    async fn pat_reads_send_the_pat_as_bearer_token() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/widgets/pulls/17"))
            .and(header("authorization", format!("Bearer {PAT}").as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "state": "open", "merged": false,
                "html_url": "https://github.com/acme/widgets/pull/17",
                "head": { "ref": "feature", "sha": "abc" }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let api = GitHubApi::new(server.uri(), GithubAuth::Pat(PAT.to_string()));
        let pr = PullRef::parse("https://github.com/acme/widgets/pull/17").unwrap();
        let pull = api.pull(&pr).await.unwrap();
        assert_eq!(pull.head_ref, "feature");
    }

    #[tokio::test]
    async fn app_reads_reuse_the_installation_token_until_it_expires() {
        let server = MockServer::start().await;
        mount_app(&server, 1).await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/widgets/pulls/17"))
            .and(header(
                "authorization",
                format!("Bearer {APP_TOKEN}").as_str(),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "state": "open", "merged": false,
                "html_url": "https://github.com/acme/widgets/pull/17",
                "head": { "ref": "feature", "sha": "abc" }
            })))
            .expect(2)
            .mount(&server)
            .await;

        let api = GitHubApi::new(server.uri(), GithubAuth::App(app_client(&server)));
        let pr = PullRef::parse("https://github.com/acme/widgets/pull/17").unwrap();
        api.pull(&pr).await.unwrap();
        api.pull(&pr).await.unwrap();
    }

    #[test]
    fn parses_pull_urls() {
        assert_eq!(
            PullRef::parse("https://github.com/acme/widgets/pull/17"),
            Some(PullRef {
                owner: "acme".into(),
                repo: "widgets".into(),
                number: 17
            })
        );
        assert_eq!(
            PullRef::parse("https://github.com/acme/widgets/pull/17/"),
            PullRef::parse("https://github.com/acme/widgets/pull/17")
        );
        for bad in [
            "https://github.com/acme/widgets",
            "https://github.com/acme/widgets/issues/17",
            "https://github.com/acme/widgets/pull/x",
            "https://github.com/acme/widgets/pull/17/files",
            "https://gitlab.com/acme/widgets/pull/17",
            "",
        ] {
            assert_eq!(PullRef::parse(bad), None, "{bad:?}");
        }
    }

    #[tokio::test]
    async fn observes_pull_reviews_and_checks() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/widgets/pulls/17"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "state": "open", "merged": false,
                "html_url": "https://github.com/acme/widgets/pull/17",
                "head": { "ref": "feature", "sha": "abc" }
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/widgets/pulls/17/reviews"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                { "user": { "login": "alice" }, "state": "APPROVED", "body": "",
                  "submitted_at": "2026-09-12T10:00:00Z", "commit_id": "abc" },
                { "user": { "login": "bot" }, "state": "PENDING", "body": null,
                  "submitted_at": null, "commit_id": "abc" }
            ])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/widgets/commits/abc/check-runs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "total_count": 1,
                "check_runs": [ { "name": "test", "status": "completed", "conclusion": "success" } ]
            })))
            .mount(&server)
            .await;

        let api = GitHubApi::new(server.uri(), GithubAuth::Pat(PAT.to_string()));
        let pr = PullRef::parse("https://github.com/acme/widgets/pull/17").unwrap();
        let (pull, observation) = api.observe(&pr).await.unwrap();

        assert_eq!(pull.head_ref, "feature");
        assert!(!pull.closed);
        assert_eq!(observation.reviews.len(), 1, "pending review is dropped");
        assert_eq!(observation.reviews[0].reviewer, "alice");
        assert_eq!(observation.check_runs[0].name, "test");
    }

    #[tokio::test]
    async fn classifies_rate_limits_and_missing_prs() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/widgets/pulls/1"))
            .respond_with(
                ResponseTemplate::new(403)
                    .insert_header("x-ratelimit-remaining", "0")
                    .insert_header("x-ratelimit-reset", "1800000000"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/widgets/pulls/2"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let api = GitHubApi::new(server.uri(), GithubAuth::Pat(PAT.to_string()));
        let limited = api
            .pull(&PullRef::parse("https://github.com/acme/widgets/pull/1").unwrap())
            .await
            .unwrap_err();
        assert!(limited.rate_limit_reset().is_some());
        assert!(!limited.is_not_visible());
        assert_eq!(
            limited.rate_limit_reset().flatten(),
            DateTime::from_timestamp(1_800_000_000, 0)
        );

        let missing = api
            .pull(&PullRef::parse("https://github.com/acme/widgets/pull/2").unwrap())
            .await
            .unwrap_err();
        assert!(missing.is_not_visible());
        assert!(missing.rate_limit_reset().is_none());
    }
}
