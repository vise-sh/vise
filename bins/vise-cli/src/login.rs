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
//!    answers requests until that one arrives, checks the state, and keeps
//!    the one-time code.
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

    if is_plaintext_remote(url) {
        eprintln!(
            "Warning: {url} uses plain http on a non-local host; the API key \
             will travel over the network unencrypted."
        );
    }

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

/// True when `url` is plain http to a host other than the local machine —
/// i.e. the exchanged key would cross the network unencrypted.
fn is_plaintext_remote(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = match authority.strip_prefix('[') {
        // `[::1]:3000` — the port sits outside the brackets.
        Some(v6) => v6.split(']').next().unwrap_or(v6),
        None => authority.split(':').next().unwrap_or(authority),
    };
    !matches!(host, "localhost" | "127.0.0.1" | "::1")
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

/// Serve HTTP requests on `listener` until the browser delivers the one-time
/// code. Stray connections — port scans, speculative connects, favicon
/// fetches, redirects with the wrong state — get a 404 and the listener
/// keeps waiting; the enclosing timeout in `run` bounds the whole wait.
async fn wait_for_callback(
    listener: &tokio::net::TcpListener,
    expected_state: &str,
) -> anyhow::Result<String> {
    loop {
        let (mut stream, _) = listener.accept().await?;

        let Some(request_line) = read_request_line(&mut stream).await else {
            // Hung up early or flooded us: a scan, not the browser.
            continue;
        };

        match parse_callback(&request_line, expected_state) {
            Callback::Code(code) => {
                respond(
                    &mut stream,
                    "200 OK",
                    "vise login complete - you can close this tab and return to your terminal.",
                )
                .await;
                return Ok(code);
            }
            Callback::Broken => {
                respond(
                    &mut stream,
                    "400 Bad Request",
                    "vise login failed - return to your terminal.",
                )
                .await;
                bail!("the login callback carried no code");
            }
            Callback::Stray => {
                respond(&mut stream, "404 Not Found", "not found").await;
            }
        }
    }
}

/// Read the request head from `stream` and return its first line; `None`
/// when the peer hangs up before sending one or sends something oversized.
async fn read_request_line(stream: &mut tokio::net::TcpStream) -> Option<String> {
    let mut buffer = Vec::with_capacity(2048);
    let mut chunk = [0u8; 1024];
    while !buffer.windows(4).any(|window| window == b"\r\n\r\n") {
        if buffer.len() > 16 * 1024 {
            return None;
        }
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    let head = String::from_utf8_lossy(&buffer);
    head.lines().next().map(str::to_string)
}

/// Send a minimal HTML response and close the connection. Best-effort: the
/// peer may already be gone.
async fn respond(stream: &mut tokio::net::TcpStream, status: &str, message: &str) {
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
}

/// What one loopback request turned out to be.
#[derive(Debug, PartialEq, Eq)]
enum Callback {
    /// `/callback` with the expected state and a one-time code: done.
    Code(String),
    /// `/callback` with the expected state but no code: the handoff itself
    /// is broken, so waiting for another request cannot help.
    Broken,
    /// Anything else — wrong path, wrong or missing state. Not our redirect.
    Stray,
}

/// Classify `GET /callback?code=…&state=… HTTP/1.1`, insisting the state
/// matches the one this process generated.
fn parse_callback(request_line: &str, expected_state: &str) -> Callback {
    let Some(target) = request_line.split_whitespace().nth(1) else {
        return Callback::Stray;
    };
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if path != "/callback" {
        return Callback::Stray;
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
        return Callback::Stray;
    }
    match code.filter(|code| !code.is_empty()) {
        Some(code) => Callback::Code(code),
        None => Callback::Broken,
    }
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
/// other line (comments included) verbatim. The new contents are written to
/// a 0600 temp file in the same directory and renamed into place, so the
/// secret is never on disk with looser permissions and a crash mid-write
/// cannot leave a truncated file.
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

    let tmp_file = paths.root().join(".env.tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(&tmp_file)
        .with_context(|| format!("creating {}", tmp_file.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        // `mode` only applies when the file is created; also tighten a
        // leftover temp file from an interrupted earlier run.
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restricting permissions on {}", tmp_file.display()))?;
    }
    file.write_all(updated.as_bytes())
        .with_context(|| format!("writing {}", tmp_file.display()))?;
    file.sync_all()
        .with_context(|| format!("flushing {}", tmp_file.display()))?;
    drop(file);

    std::fs::rename(&tmp_file, &env_file)
        .with_context(|| format!("replacing {}", env_file.display()))?;
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
/// Works byte-by-byte: the input is attacker-controllable (it arrives on the
/// loopback listener), so nothing here may assume UTF-8 char boundaries.
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3])
                    .ok()
                    .and_then(|hex| u8::from_str_radix(hex, 16).ok());
                match hex {
                    Some(byte) => {
                        decoded.push(byte);
                        i += 3;
                    }
                    None => {
                        decoded.push(b'%');
                        i += 1;
                    }
                }
            }
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
        assert_eq!(
            parse_callback("GET /callback?code=vcode_abc&state=st4te HTTP/1.1", "st4te"),
            Callback::Code("vcode_abc".into())
        );

        // Parameter order does not matter.
        assert_eq!(
            parse_callback("GET /callback?state=st4te&code=vcode_abc HTTP/1.1", "st4te"),
            Callback::Code("vcode_abc".into())
        );
    }

    #[test]
    fn callback_parsing_classifies_bad_requests() {
        // Wrong state: someone else's redirect must not log us in.
        assert_eq!(
            parse_callback("GET /callback?code=c&state=other HTTP/1.1", "st4te"),
            Callback::Stray
        );
        // Missing state entirely.
        assert_eq!(
            parse_callback("GET /callback?code=c HTTP/1.1", "st4te"),
            Callback::Stray
        );
        // Wrong path (e.g. a stray favicon request).
        assert_eq!(
            parse_callback("GET /favicon.ico?code=c&state=st4te HTTP/1.1", "st4te"),
            Callback::Stray
        );
        // Not HTTP at all.
        assert_eq!(parse_callback("garbage", "st4te"), Callback::Stray);
        // Right state but no code: the handoff is broken, not a stray.
        assert_eq!(
            parse_callback("GET /callback?state=st4te HTTP/1.1", "st4te"),
            Callback::Broken
        );
        assert_eq!(
            parse_callback("GET /callback?code=&state=st4te HTTP/1.1", "st4te"),
            Callback::Broken
        );
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
        // Multibyte UTF-8 right after `%` must not panic: the two bytes
        // after the escape are not a char boundary.
        assert_eq!(percent_decode("%aé"), "%aé");
        assert_eq!(percent_decode("%é"), "%é");
        assert_eq!(percent_decode("é%41é"), "éAé");
    }

    #[test]
    fn plaintext_remote_detection_spares_local_and_https_urls() {
        assert!(!is_plaintext_remote("https://api.vise.sh"));
        assert!(!is_plaintext_remote("http://localhost:3000"));
        assert!(!is_plaintext_remote("http://127.0.0.1:3000"));
        assert!(!is_plaintext_remote("http://[::1]:3000"));
        assert!(is_plaintext_remote("http://api.vise.sh"));
        assert!(is_plaintext_remote("http://10.0.0.7:3000"));
        assert!(is_plaintext_remote("http://example.com/path"));
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

        // The temp file used for the atomic rewrite is gone.
        assert!(!paths.root().join(".env.tmp").exists());

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

    /// Read one full HTTP request (head plus `Content-Length` body) from a
    /// blocking stream. Test-only counterpart of the client side.
    fn read_http_request(stream: &mut std::net::TcpStream) -> String {
        use std::io::Read as _;
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 1024];
        while !buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = stream.read(&mut chunk).unwrap();
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
        }
        let head_end = buffer
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|position| position + 4)
            .unwrap_or(buffer.len());
        let content_length: usize = String::from_utf8_lossy(&buffer[..head_end])
            .lines()
            .find_map(|line| {
                let lower = line.to_ascii_lowercase();
                let value = lower.strip_prefix("content-length:")?;
                value.trim().parse().ok()
            })
            .unwrap_or(0);
        while buffer.len() < head_end + content_length {
            let read = stream.read(&mut chunk).unwrap();
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
        }
        String::from_utf8_lossy(&buffer).into_owned()
    }

    #[tokio::test]
    async fn exchange_posts_the_code_and_parses_the_secret() {
        // A one-shot mock server for the exchange endpoint.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_http_request(&mut stream);
            let body = r#"{"token":"vk_fresh","workspace":{"name":"acme"}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            std::io::Write::write_all(&mut stream, response.as_bytes()).unwrap();
            request
        });

        let exchanged = exchange(&format!("http://{addr}"), "vcode_123")
            .await
            .unwrap();
        assert_eq!(exchanged.token, "vk_fresh");
        assert_eq!(exchanged.workspace.unwrap().name, "acme");

        let request = server.join().unwrap();
        assert!(
            request.starts_with("POST /platform/cli/exchange HTTP/1.1"),
            "{request}"
        );
        assert!(request.contains(r#""code":"vcode_123""#), "{request}");
    }

    #[tokio::test]
    async fn wait_for_callback_outlives_stray_connections() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let browser = tokio::spawn(async move {
            // A port scan that connects and immediately hangs up.
            drop(tokio::net::TcpStream::connect(addr).await.unwrap());

            // A stray request on the wrong path gets a 404...
            let mut stray = tokio::net::TcpStream::connect(addr).await.unwrap();
            stray
                .write_all(b"GET /favicon.ico HTTP/1.1\r\n\r\n")
                .await
                .unwrap();
            let mut response = String::new();
            stray.read_to_string(&mut response).await.unwrap();
            assert!(response.starts_with("HTTP/1.1 404"), "{response}");

            // ...as does a forged redirect with the wrong state.
            let mut forged = tokio::net::TcpStream::connect(addr).await.unwrap();
            forged
                .write_all(b"GET /callback?code=evil&state=wrong HTTP/1.1\r\n\r\n")
                .await
                .unwrap();
            let mut response = String::new();
            forged.read_to_string(&mut response).await.unwrap();
            assert!(response.starts_with("HTTP/1.1 404"), "{response}");

            // The real callback still gets through afterwards.
            let mut real = tokio::net::TcpStream::connect(addr).await.unwrap();
            real.write_all(b"GET /callback?code=vcode_ok&state=st4te HTTP/1.1\r\n\r\n")
                .await
                .unwrap();
            let mut response = String::new();
            real.read_to_string(&mut response).await.unwrap();
            assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        });

        let code = tokio::time::timeout(
            Duration::from_secs(10),
            wait_for_callback(&listener, "st4te"),
        )
        .await
        .expect("wait_for_callback should survive stray connections")
        .unwrap();
        assert_eq!(code, "vcode_ok");
        browser.await.unwrap();
    }
}
