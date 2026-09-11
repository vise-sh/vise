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
