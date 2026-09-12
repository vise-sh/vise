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
        Ok(Self {
            app_id,
            encoding_key,
            api_base,
            http,
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
}

// Read-only access used by PR tracking and follow-up composition.
//
// The GitHub App installation token must carry `pull_requests: read` and
// `checks: read` (plus `contents: read`, already required for cloning). The
// same App credential that hosts obtain through the credentials endpoint is
// reused here; no extra secret is configured.

/// Failure modes the poller treats differently.
#[derive(Debug)]
pub enum GitHubError {
    /// Primary or secondary rate limit hit; back off the whole tick.
    RateLimited { retry_after: std::time::Duration },
    /// 401 / 403 / 404: the PR is unreadable with this credential. Repeated
    /// occurrences mark the snapshot `sync_error`.
    Unreadable { status: u16 },
    /// Anything else (5xx, network, decode); retried next tick.
    Transient(anyhow::Error),
}

impl std::fmt::Display for GitHubError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GitHubError::RateLimited { retry_after } => {
                write!(f, "github rate limited; retry after {retry_after:?}")
            }
            GitHubError::Unreadable { status } => write!(f, "github returned {status}"),
            GitHubError::Transient(error) => write!(f, "github request failed: {error}"),
        }
    }
}

impl std::error::Error for GitHubError {}

impl From<reqwest::Error> for GitHubError {
    fn from(error: reqwest::Error) -> Self {
        GitHubError::Transient(error.into())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct GitHubUser {
    pub login: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GitRef {
    #[serde(rename = "ref")]
    pub name: String,
    pub sha: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PullRequest {
    pub number: u64,
    /// "open" | "closed" (merged PRs are closed with `merged == true`)
    pub state: String,
    #[serde(default)]
    pub merged: bool,
    pub html_url: String,
    #[serde(default)]
    pub title: String,
    pub head: GitRef,
    pub base: GitRef,
    #[serde(default)]
    pub user: Option<GitHubUser>,
}

impl PullRequest {
    pub fn is_open(&self) -> bool {
        self.state == "open" && !self.merged
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Review {
    pub id: u64,
    #[serde(default)]
    pub user: Option<GitHubUser>,
    pub state: String,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub commit_id: Option<String>,
    #[serde(default)]
    pub submitted_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub html_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CheckRun {
    pub name: String,
    pub status: String,
    #[serde(default)]
    pub conclusion: Option<String>,
    #[serde(default)]
    pub details_url: Option<String>,
    #[serde(default)]
    pub html_url: Option<String>,
}

#[derive(Deserialize)]
struct CheckRunsPage {
    check_runs: Vec<CheckRun>,
}

/// An inline review comment. `line == None` means the comment is outdated:
/// the code it was attached to has since changed.
#[derive(Debug, Clone, Deserialize)]
pub struct ReviewComment {
    pub id: u64,
    #[serde(default)]
    pub in_reply_to_id: Option<u64>,
    #[serde(default)]
    pub user: Option<GitHubUser>,
    pub path: String,
    #[serde(default)]
    pub line: Option<u64>,
    #[serde(default)]
    pub original_line: Option<u64>,
    pub body: String,
    #[serde(default)]
    pub diff_hunk: Option<String>,
    #[serde(default)]
    pub html_url: Option<String>,
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
}

const PER_PAGE: usize = 100;
const MAX_PAGES: usize = 10;

#[derive(Clone)]
pub struct GitHubReadClient {
    api_base: String,
    http: reqwest::Client,
}

impl GitHubReadClient {
    pub fn new(api_base: String) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent("vise-server")
            .build()?;
        Ok(Self { api_base, http })
    }

    pub fn api_base(&self) -> &str {
        &self.api_base
    }

    pub async fn pull_request(
        &self,
        token: &str,
        repo: &str,
        number: u64,
    ) -> Result<PullRequest, GitHubError> {
        self.get_json(token, &format!("/repos/{repo}/pulls/{number}"))
            .await
    }

    pub async fn reviews(
        &self,
        token: &str,
        repo: &str,
        number: u64,
    ) -> Result<Vec<Review>, GitHubError> {
        self.get_paginated(
            token,
            &format!("/repos/{repo}/pulls/{number}/reviews"),
            |v| v,
        )
        .await
    }

    pub async fn review_comments(
        &self,
        token: &str,
        repo: &str,
        number: u64,
    ) -> Result<Vec<ReviewComment>, GitHubError> {
        self.get_paginated(
            token,
            &format!("/repos/{repo}/pulls/{number}/comments"),
            |v| v,
        )
        .await
    }

    pub async fn check_runs(
        &self,
        token: &str,
        repo: &str,
        sha: &str,
    ) -> Result<Vec<CheckRun>, GitHubError> {
        self.get_paginated(
            token,
            &format!("/repos/{repo}/commits/{sha}/check-runs"),
            |page: CheckRunsPage| page.check_runs,
        )
        .await
    }

    async fn get_paginated<Page, Item>(
        &self,
        token: &str,
        path: &str,
        extract: impl Fn(Page) -> Vec<Item>,
    ) -> Result<Vec<Item>, GitHubError>
    where
        Page: serde::de::DeserializeOwned,
    {
        let mut all = Vec::new();
        for page in 1..=MAX_PAGES {
            let url = format!("{path}?per_page={PER_PAGE}&page={page}");
            let items = extract(self.get_json::<Page>(token, &url).await?);
            let count = items.len();
            all.extend(items);
            if count < PER_PAGE {
                break;
            }
        }
        Ok(all)
    }

    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        token: &str,
        path: &str,
    ) -> Result<T, GitHubError> {
        let response = self
            .http
            .get(format!("{}{path}", self.api_base))
            .bearer_auth(token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await?;

        let status = response.status();
        if let Some(retry_after) = rate_limit_backoff(&response) {
            return Err(GitHubError::RateLimited { retry_after });
        }
        match status.as_u16() {
            200..=299 => {}
            code @ (401 | 403 | 404) => return Err(GitHubError::Unreadable { status: code }),
            code => {
                return Err(GitHubError::Transient(anyhow::anyhow!(
                    "GET {path} returned {code}"
                )));
            }
        }

        response
            .json::<T>()
            .await
            .map_err(|error| GitHubError::Transient(error.into()))
    }
}

/// GitHub signals primary limits with 403/429 + `x-ratelimit-remaining: 0`
/// and secondary limits with `retry-after`. Returns how long to back off.
fn rate_limit_backoff(response: &reqwest::Response) -> Option<std::time::Duration> {
    use std::time::Duration;

    const MAX_BACKOFF: Duration = Duration::from_secs(60 * 60);
    const MIN_BACKOFF: Duration = Duration::from_secs(1);

    let status = response.status().as_u16();
    if status != 403 && status != 429 {
        return None;
    }

    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<i64>().ok())
    };

    let backoff = if let Some(seconds) = header("retry-after") {
        Duration::from_secs(seconds.max(0) as u64)
    } else if header("x-ratelimit-remaining") == Some(0) {
        let reset = header("x-ratelimit-reset").unwrap_or(0);
        Duration::from_secs((reset - Utc::now().timestamp()).max(0) as u64)
    } else {
        return None;
    };

    Some(backoff.clamp(MIN_BACKOFF, MAX_BACKOFF))
}

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

    fn read_client(server: &MockServer) -> GitHubReadClient {
        GitHubReadClient::new(server.uri()).unwrap()
    }

    #[tokio::test]
    async fn reads_pull_request_and_paginates_reviews() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/repos/acme/widgets/pulls/7"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "number": 7, "state": "open", "merged": false, "title": "Add widgets",
                "html_url": "https://github.com/acme/widgets/pull/7",
                "head": {"ref": "feature", "sha": "abc"},
                "base": {"ref": "main", "sha": "def"},
                "user": {"login": "vise[bot]"}
            })))
            .mount(&server)
            .await;

        let page: Vec<serde_json::Value> = (0..100)
            .map(|i| serde_json::json!({"id": i, "state": "COMMENTED", "user": {"login": "x"}}))
            .collect();
        Mock::given(method("GET"))
            .and(path("/repos/acme/widgets/pulls/7/reviews"))
            .and(wiremock::matchers::query_param("page", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(page))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/widgets/pulls/7/reviews"))
            .and(wiremock::matchers::query_param("page", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": 100, "state": "APPROVED", "user": {"login": "ana"}, "commit_id": "abc"}
            ])))
            .mount(&server)
            .await;

        let client = read_client(&server);
        let pr = client.pull_request("tok", "acme/widgets", 7).await.unwrap();
        assert!(pr.is_open());
        assert_eq!(pr.head.name, "feature");

        let reviews = client.reviews("tok", "acme/widgets", 7).await.unwrap();
        assert_eq!(reviews.len(), 101);
        assert_eq!(reviews[100].state, "APPROVED");
    }

    #[tokio::test]
    async fn classifies_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/widgets/pulls/1"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/widgets/pulls/2"))
            .respond_with(
                ResponseTemplate::new(403)
                    .insert_header("x-ratelimit-remaining", "0")
                    .insert_header(
                        "x-ratelimit-reset",
                        (Utc::now().timestamp() + 120).to_string().as_str(),
                    ),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/widgets/pulls/3"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "30"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/widgets/pulls/4"))
            .respond_with(ResponseTemplate::new(502))
            .mount(&server)
            .await;

        let client = read_client(&server);
        assert!(matches!(
            client.pull_request("tok", "acme/widgets", 1).await,
            Err(GitHubError::Unreadable { status: 404 })
        ));
        match client.pull_request("tok", "acme/widgets", 2).await {
            Err(GitHubError::RateLimited { retry_after }) => {
                assert!(retry_after.as_secs() > 100 && retry_after.as_secs() <= 120);
            }
            other => panic!("expected rate limit, got {other:?}"),
        }
        match client.pull_request("tok", "acme/widgets", 3).await {
            Err(GitHubError::RateLimited { retry_after }) => {
                assert_eq!(retry_after.as_secs(), 30);
            }
            other => panic!("expected rate limit, got {other:?}"),
        }
        assert!(matches!(
            client.pull_request("tok", "acme/widgets", 4).await,
            Err(GitHubError::Transient(_))
        ));
    }
}
