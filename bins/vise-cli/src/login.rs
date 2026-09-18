//! `vise login`: obtain an API token from a vise cloud server via a browser
//! handoff, and store it in `~/.vise/.env` as `VISE_API_TOKEN` — after which
//! the existing bearer plumbing in every other command just works.
//!
//! The flow mirrors the classic loopback OAuth dance, minus OAuth:
//!
//! 1. Bind an ephemeral listener on 127.0.0.1 and generate a `state` nonce.
//! 2. Open the browser at `<server>/platform/cli/authorize?port=…&state=…`;
//!    the dashboard asks the signed-in user to confirm.
//! 3. On confirm the server redirects the browser to
//!    `http://127.0.0.1:<port>/callback?code=…&state=…`; the listener
//!    answers one request, checks the state, and keeps the one-time code.
//! 4. `POST <server>/platform/cli/exchange {code}` returns the `vk_` secret
//!    exactly once; it is written to `~/.vise/.env`.
//!
//! `--paste` (or a machine where no browser opens) falls back to creating a
//! key by hand in the dashboard and pasting it.

use std::io::Write as _;
use std::time::Duration;

use anyhow::{Context, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::host::Paths;

/// How long the listener waits for the browser round-trip. Matches the
/// server-side five-minute code expiry.
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);

/// What the exchange endpoint answers with.
#[derive(serde::Deserialize)]
struct ExchangeResponse {
    token: String,
    #[serde(default)]
    workspace: Option<ExchangeWorkspace>,
}

#[derive(serde::Deserialize)]
struct ExchangeWorkspace {
    name: String,
}

pub async fn run(paths: &Paths, url: &str, paste: bool) -> anyhow::Result<()> {
    let url = url.trim_end_matches('/');

    if paste {
        return paste_flow(paths, url);
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .context("binding a loopback port for the login callback")?;
    let port = listener.local_addr()?.port();

    let state = uuid::Uuid::new_v4().simple().to_string();
    let authorize_url = authorize_url(url, port, &state, hostname().as_deref());

    eprintln!("Opening your browser to authorize this machine:");
    eprintln!("\n  {authorize_url}\n");
    if !open_browser(&authorize_url) {
        eprintln!("Could not open a browser. Open the URL above on this machine,");
        eprintln!("or run `vise login --paste` to paste a key from the dashboard.");
    }
    eprintln!("Waiting for the browser (Ctrl-C to abort)...");

    let code = tokio::time::timeout(CALLBACK_TIMEOUT, wait_for_callback(&listener, &state))
        .await
        .map_err(|_| anyhow::anyhow!("timed out waiting for the browser; run `vise login` again"))?
        .context("receiving the login callback")?;

    let exchanged = exchange(url, &code).await?;
    save_token(paths, &exchanged.token)?;

    match exchanged.workspace {
        Some(workspace) => eprintln!("Logged in to {url} (workspace: {})", workspace.name),
        None => eprintln!("Logged in to {url}"),
    }
    eprintln!(
        "API token saved to {} as VISE_API_TOKEN.",
        paths.env_file().display()
    );
    Ok(())
}

/// The `--paste` fallback: the user creates a key in the dashboard and
/// pastes it here. Also the right path for headless machines.
fn paste_flow(paths: &Paths, url: &str) -> anyhow::Result<()> {
    eprintln!("Open the dashboard, create an API key, and paste it here:");
    eprintln!("\n  {url}/  (Settings → API keys)\n");
    eprint!("API key (vk_...): ");
    std::io::stderr().flush().ok();

    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("reading the pasted key")?;
    let token = line.trim();
    if !token.starts_with("vk_") {
        bail!("that does not look like a vise API key (expected vk_...)");
    }

    save_token(paths, token)?;
    eprintln!(
        "API token saved to {} as VISE_API_TOKEN.",
        paths.env_file().display()
    );
    Ok(())
}

/// The browser entry point for the handoff.
fn authorize_url(url: &str, port: u16, state: &str, hostname: Option<&str>) -> String {
    let mut authorize = format!(
        "{url}/platform/cli/authorize?port={port}&state={}",
        percent_encode(state)
    );
    if let Some(hostname) = hostname {
        authorize.push_str(&format!("&hostname={}", percent_encode(hostname)));
    }
    authorize
}

/// This machine's hostname, for the server-side key name ("cli <hostname>
/// <date>"). Best-effort: `None` just means a less descriptive name.
fn hostname() -> Option<String> {
    let output = std::process::Command::new("hostname").output().ok()?;
    let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!name.is_empty()).then_some(name)
}

/// Open `url` in the default browser; `false` when no opener worked.
fn open_browser(url: &str) -> bool {
    let openers: &[&str] = if cfg!(target_os = "macos") {
        &["open"]
    } else {
        &["xdg-open", "sensible-browser"]
    };
    openers.iter().any(|opener| {
        std::process::Command::new(opener)
            .arg(url)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    })
}

/// Serve exactly one HTTP request on `listener` and return the one-time
/// code, verifying the `state` round-tripped unchanged.
async fn wait_for_callback(
    listener: &tokio::net::TcpListener,
    expected_state: &str,
) -> anyhow::Result<String> {
    let (mut stream, _) = listener.accept().await?;

    // Read until the end of the request head; the callback has no body.
    let mut buffer = Vec::with_capacity(2048);
    let mut chunk = [0u8; 1024];
    while !buffer.windows(4).any(|window| window == b"\r\n\r\n") {
        if buffer.len() > 16 * 1024 {
            bail!("callback request too large");
        }
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    let head = String::from_utf8_lossy(&buffer);
    let request_line = head.lines().next().unwrap_or_default().to_string();

    let result = parse_callback(&request_line, expected_state);

    let (status, message) = match &result {
        Ok(_) => (
            "200 OK",
            "vise login complete - you can close this tab and return to your terminal.",
        ),
        Err(_) => (
            "400 Bad Request",
            "vise login failed - return to your terminal.",
        ),
    };
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>vise</title>\
         <body style=\"font-family:system-ui;margin:20vh auto;max-width:28rem;text-align:center\">\
         <p>{message}</p></body>"
    );
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await.ok();
    stream.shutdown().await.ok();

    result
}

/// Extract the code from `GET /callback?code=…&state=… HTTP/1.1`, insisting
/// the state matches the one this process generated.
fn parse_callback(request_line: &str, expected_state: &str) -> anyhow::Result<String> {
    let target = request_line
        .split_whitespace()
        .nth(1)
        .context("malformed callback request")?;
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if path != "/callback" {
        bail!("unexpected callback path {path:?}");
    }

    let mut code = None;
    let mut state = None;
    for pair in query.split('&') {
        match pair.split_once('=') {
            Some(("code", value)) => code = Some(percent_decode(value)),
            Some(("state", value)) => state = Some(percent_decode(value)),
            _ => {}
        }
    }

    if state.as_deref() != Some(expected_state) {
        bail!("state mismatch in the login callback; run `vise login` again");
    }
    code.filter(|code| !code.is_empty())
        .context("the login callback carried no code")
}

/// Trade the one-time code for the API token.
async fn exchange(url: &str, code: &str) -> anyhow::Result<ExchangeResponse> {
    let response = reqwest::Client::new()
        .post(format!("{url}/platform/cli/exchange"))
        .json(&serde_json::json!({ "code": code }))
        .send()
        .await
        .with_context(|| format!("reaching {url}/platform/cli/exchange"))?;

    if response.status() == reqwest::StatusCode::NOT_FOUND {
        bail!(
            "{url} has no /platform/cli/exchange endpoint — this looks like a \
             self-hosted (OSS) server, which has no browser login. Set \
             VISE_API_TOKEN in ~/.vise/.env to the server's token instead."
        );
    }
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        bail!("login exchange failed: {status} {body}");
    }
    response.json().await.context("parsing exchange response")
}

/// Upsert `VISE_API_TOKEN=<token>` into `~/.vise/.env`, preserving every
/// other line (comments included) verbatim. The file is chmod 0600: it holds
/// credentials.
fn save_token(paths: &Paths, token: &str) -> anyhow::Result<()> {
    std::fs::create_dir_all(paths.root())
        .with_context(|| format!("creating {}", paths.root().display()))?;

    let env_file = paths.env_file();
    let existing = match std::fs::read_to_string(&env_file) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error).with_context(|| format!("reading {}", env_file.display())),
    };

    let updated = upsert_env_line(&existing, "VISE_API_TOKEN", token);
    std::fs::write(&env_file, updated)
        .with_context(|| format!("writing {}", env_file.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&env_file, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restricting permissions on {}", env_file.display()))?;
    }

    Ok(())
}

/// Replace the `key=` line in `contents` (or append one), leaving everything
/// else — ordering, comments, unrelated keys — untouched.
fn upsert_env_line(contents: &str, key: &str, value: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut replaced = false;
    for line in contents.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with(&format!("{key}=")) && !replaced {
            lines.push(format!("{key}={value}"));
            replaced = true;
        } else {
            lines.push(line.to_string());
        }
    }
    if !replaced {
        lines.push(format!("{key}={value}"));
    }
    let mut result = lines.join("\n");
    result.push('\n');
    result
}

/// Minimal percent-encoding for query values: everything but unreserved
/// characters is escaped. The values here (uuids, hostnames) are short, so
/// simplicity beats pulling in a crate.
fn percent_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char)
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

/// Decode `%XX` escapes and `+` (form-encoded spaces) in a query value.
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => match u8::from_str_radix(&value[i + 1..i + 3], 16) {
                Ok(byte) => {
                    decoded.push(byte);
                    i += 3;
                }
                Err(_) => {
                    decoded.push(b'%');
                    i += 1;
                }
            },
            b'+' => {
                decoded.push(b' ');
                i += 1;
            }
            byte => {
                decoded.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_parsing_extracts_the_code_and_checks_state() {
        let code =
            parse_callback("GET /callback?code=vcode_abc&state=st4te HTTP/1.1", "st4te").unwrap();
        assert_eq!(code, "vcode_abc");

        // Parameter order does not matter.
        let code =
            parse_callback("GET /callback?state=st4te&code=vcode_abc HTTP/1.1", "st4te").unwrap();
        assert_eq!(code, "vcode_abc");
    }

    #[test]
    fn callback_parsing_rejects_bad_requests() {
        // Wrong state: someone else's redirect must not log us in.
        assert!(parse_callback("GET /callback?code=c&state=other HTTP/1.1", "st4te").is_err());
        // Missing state entirely.
        assert!(parse_callback("GET /callback?code=c HTTP/1.1", "st4te").is_err());
        // Missing code.
        assert!(parse_callback("GET /callback?state=st4te HTTP/1.1", "st4te").is_err());
        // Wrong path (e.g. a stray favicon request).
        assert!(parse_callback("GET /favicon.ico?code=c&state=st4te HTTP/1.1", "st4te").is_err());
        // Not HTTP at all.
        assert!(parse_callback("garbage", "st4te").is_err());
    }

    #[test]
    fn percent_coding_round_trips() {
        assert_eq!(percent_encode("abc-123"), "abc-123");
        assert_eq!(percent_encode("my mac"), "my%20mac");
        assert_eq!(percent_decode("my%20mac"), "my mac");
        assert_eq!(percent_decode("my+mac"), "my mac");
        assert_eq!(
            percent_decode(&percent_encode("weird/چiz?&=")),
            "weird/چiz?&="
        );
        // Invalid escapes are passed through rather than panicking.
        assert_eq!(percent_decode("bad%zz"), "bad%zz");
        assert_eq!(percent_decode("trailing%2"), "trailing%2");
    }

    #[test]
    fn upsert_env_line_replaces_or_appends_and_preserves_the_rest() {
        // Appends to an empty file.
        assert_eq!(
            upsert_env_line("", "VISE_API_TOKEN", "vk_1"),
            "VISE_API_TOKEN=vk_1\n"
        );

        // Replaces in place, preserving comments and other keys verbatim.
        let existing = "# managed by install.sh\nVISE_URL=https://api.vise.sh\n\
                        VISE_API_TOKEN=vk_old\nVISE_HOST_TOKEN=vhost_x\n";
        let updated = upsert_env_line(existing, "VISE_API_TOKEN", "vk_new");
        assert_eq!(
            updated,
            "# managed by install.sh\nVISE_URL=https://api.vise.sh\n\
             VISE_API_TOKEN=vk_new\nVISE_HOST_TOKEN=vhost_x\n"
        );

        // Appends when the key is missing.
        let updated = upsert_env_line("VISE_URL=http://localhost:3000\n", "VISE_API_TOKEN", "vk_1");
        assert_eq!(
            updated,
            "VISE_URL=http://localhost:3000\nVISE_API_TOKEN=vk_1\n"
        );
    }

    #[test]
    fn save_token_writes_the_env_file_with_owner_only_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path().join("vise-home"));

        save_token(&paths, "vk_secret").unwrap();

        let written = std::fs::read_to_string(paths.env_file()).unwrap();
        assert_eq!(written, "VISE_API_TOKEN=vk_secret\n");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(paths.env_file())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        // A second login replaces the token.
        save_token(&paths, "vk_rotated").unwrap();
        let written = std::fs::read_to_string(paths.env_file()).unwrap();
        assert_eq!(written, "VISE_API_TOKEN=vk_rotated\n");
    }

    #[test]
    fn authorize_url_carries_port_state_and_encoded_hostname() {
        let url = authorize_url("https://api.vise.sh", 43210, "st4te", Some("my mac"));
        assert_eq!(
            url,
            "https://api.vise.sh/platform/cli/authorize?port=43210&state=st4te&hostname=my%20mac"
        );
        let url = authorize_url("http://localhost:3000", 1, "s", None);
        assert_eq!(
            url,
            "http://localhost:3000/platform/cli/authorize?port=1&state=s"
        );
    }
}
