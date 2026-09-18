//! `vise host`: manage the local `vise-host` process.
//!
//! The host runs natively because it spawns the locally installed agent
//! harness (Claude Code), so this is a small pidfile supervisor rather than a
//! launchd/systemd unit. All state lives under `~/.vise` (override with
//! `VISE_HOME`), which `scripts/install.sh` also populates:
//!
//! - `.env`: `KEY=VALUE` config (`VISE_URL`, `VISE_API_TOKEN`, `VISE_HOST_TOKEN`, ...)
//! - `bin/`: installed `vise` and `vise-host` binaries
//! - `host.pid`: pid of the running `vise-host`
//! - `logs/host.log`: combined stdout/stderr of `vise-host`
//!
//! Signals are sent through the `kill` utility (present on macOS and every
//! Linux distribution) so the CLI needs neither libc nor `unsafe`.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};

/// Server URL used when neither `--url`, `VISE_URL` nor `~/.vise/.env` set one.
pub const DEFAULT_URL: &str = "http://localhost:3000";

const HOST_BINARY: &str = "vise-host";
const STOP_TIMEOUT: Duration = Duration::from_secs(10);
const START_GRACE: Duration = Duration::from_millis(500);
const FOLLOW_INTERVAL: Duration = Duration::from_millis(500);

/// Locations of everything `vise host` touches, rooted at the vise home dir.
pub struct Paths {
    root: PathBuf,
}

impl Paths {
    /// `$VISE_HOME`, else `$HOME/.vise`.
    pub fn from_env() -> anyhow::Result<Self> {
        if let Some(home) = std::env::var_os("VISE_HOME") {
            return Ok(Self::new(PathBuf::from(home)));
        }
        let home = std::env::var_os("HOME")
            .context("HOME is not set; set VISE_HOME to the vise state directory")?;
        Ok(Self::new(PathBuf::from(home).join(".vise")))
    }

    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn env_file(&self) -> PathBuf {
        self.root.join(".env")
    }

    pub fn bin_dir(&self) -> PathBuf {
        self.root.join("bin")
    }

    pub fn pid_file(&self) -> PathBuf {
        self.root.join("host.pid")
    }

    pub fn log_file(&self) -> PathBuf {
        self.root.join("logs").join("host.log")
    }
}

/// Parse dotenv-style text: one `KEY=VALUE` per line, `#` comments and blank
/// lines ignored, optional `export ` prefix, optional matching single or
/// double quotes around the value. A key that appears twice keeps the last
/// value. No variable expansion.
pub fn parse_env(contents: &str) -> BTreeMap<String, String> {
    let mut vars = BTreeMap::new();

    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").map(str::trim).unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }

        let value = value.trim();
        let value = match value.as_bytes() {
            [b'"', .., b'"'] | [b'\'', .., b'\''] if value.len() >= 2 => &value[1..value.len() - 1],
            _ => value,
        };

        vars.insert(key.to_string(), value.to_string());
    }

    vars
}

/// Read `~/.vise/.env`; a missing file is an empty config, not an error.
pub fn load_env(paths: &Paths) -> anyhow::Result<BTreeMap<String, String>> {
    match fs::read_to_string(paths.env_file()) {
        Ok(contents) => Ok(parse_env(&contents)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(error) => Err(error).with_context(|| format!("reading {}", paths.env_file().display())),
    }
}

/// Server URL precedence: explicit (`--url` flag or `VISE_URL` env var), then
/// `VISE_URL` in `~/.vise/.env`, then [`DEFAULT_URL`].
pub fn resolve_url(explicit: Option<&str>, env: &BTreeMap<String, String>) -> String {
    explicit
        .map(str::to_string)
        .or_else(|| env.get("VISE_URL").cloned())
        .filter(|url| !url.is_empty())
        .unwrap_or_else(|| DEFAULT_URL.to_string())
}

/// API token precedence: explicit (`--api-token` flag or `VISE_API_TOKEN` env
/// var), then `VISE_API_TOKEN` in `~/.vise/.env`. `None` when neither is set:
/// the server only requires one when it was started with `VISE_API_TOKEN`.
pub fn resolve_api_token(explicit: Option<&str>, env: &BTreeMap<String, String>) -> Option<String> {
    explicit
        .map(str::to_string)
        .or_else(|| env.get("VISE_API_TOKEN").cloned())
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty())
}

/// Find the `vise-host` binary: an explicit path (`--bin` / `VISE_HOST_BIN`),
/// then next to the running `vise` executable, then `~/.vise/bin`, then `PATH`.
pub fn resolve_host_binary(explicit: Option<&Path>, paths: &Paths) -> anyhow::Result<PathBuf> {
    if let Some(path) = explicit {
        if path.is_file() {
            return Ok(path.to_path_buf());
        }
        bail!("vise-host binary not found at {}", path.display());
    }

    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        candidates.push(dir.join(HOST_BINARY));
    }
    candidates.push(paths.bin_dir().join(HOST_BINARY));
    if let Some(path_var) = std::env::var_os("PATH") {
        candidates.extend(std::env::split_paths(&path_var).map(|dir| dir.join(HOST_BINARY)));
    }

    candidates
        .into_iter()
        .find(|candidate| candidate.is_file())
        .with_context(|| {
            format!(
                "vise-host binary not found (looked next to vise, in {} and on PATH); \
                 install it with scripts/install.sh or pass --bin",
                paths.bin_dir().display()
            )
        })
}

pub struct StartOptions {
    /// Explicit path to the `vise-host` binary.
    pub binary: Option<PathBuf>,
    /// Host bearer token; falls back to `VISE_HOST_TOKEN` in `~/.vise/.env`.
    pub token: Option<String>,
    /// Server URL; falls back to `VISE_URL` in `~/.vise/.env`.
    pub url: Option<String>,
    /// Extra arguments appended to the `vise-host` command line.
    pub extra_args: Vec<String>,
}

/// Daemonize `vise-host`. A no-op when the pidfile points at a live process.
pub fn start(paths: &Paths, options: StartOptions) -> anyhow::Result<()> {
    // The child handle is dropped without waiting: the CLI exits right after
    // and init reaps the host when it eventually stops.
    launch(paths, options).map(drop)
}

/// [`start`], returning the spawned child (`None` when already running).
fn launch(paths: &Paths, options: StartOptions) -> anyhow::Result<Option<Child>> {
    if let Some(pid) = live_pid(paths)? {
        println!("vise-host already running (pid {pid})");
        return Ok(None);
    }

    let binary = resolve_host_binary(options.binary.as_deref(), paths)?;
    let env = load_env(paths)?;
    let token = options
        .token
        .or_else(|| env.get("VISE_HOST_TOKEN").cloned())
        .filter(|token| !token.is_empty())
        .with_context(|| {
            format!(
                "no host token: pass --token, set VISE_HOST_TOKEN, or add VISE_HOST_TOKEN=... to {} \
                 (enroll a host with `vise hosts create <name>`)",
                paths.env_file().display()
            )
        })?;
    let url = resolve_url(options.url.as_deref(), &env);

    fs::create_dir_all(paths.root())
        .with_context(|| format!("creating {}", paths.root().display()))?;
    let log_path = paths.log_file();
    if let Some(dir) = log_path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("opening {}", log_path.display()))?;
    writeln!(
        log,
        "=== vise host start {} ===",
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    )?;

    let mut command = Command::new(&binary);
    command
        .arg("--url")
        .arg(&url)
        .args(&options.extra_args)
        .env("VISE_HOST_TOKEN", &token)
        .current_dir(paths.root())
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log));

    // Own process group: the host is not a job of the calling shell, so it
    // survives the CLI exiting and the terminal closing.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }

    let mut child = command
        .spawn()
        .with_context(|| format!("spawning {}", binary.display()))?;
    let pid = child.id();
    write_pid(paths, pid)?;

    // Catch immediate failures (bad token, unreachable server, missing libs)
    // so `start` fails loudly instead of leaving a dead pidfile behind.
    std::thread::sleep(START_GRACE);
    if let Some(status) = child.try_wait()? {
        remove_pid(paths);
        let recent = fs::read_to_string(&log_path)
            .map(|text| tail_lines(&text, 20))
            .unwrap_or_default();
        bail!(
            "vise-host exited immediately ({status}); last log lines from {}:\n{recent}",
            log_path.display()
        );
    }

    println!("started vise-host (pid {pid})");
    println!("  url: {url}");
    println!("  log: {}", log_path.display());
    Ok(Some(child))
}

/// Terminate the process in the pidfile (SIGTERM, then SIGKILL after
/// [`STOP_TIMEOUT`]). A no-op when nothing is running.
pub fn stop(paths: &Paths) -> anyhow::Result<()> {
    let Some(pid) = live_pid(paths)? else {
        println!("vise-host is not running");
        return Ok(());
    };

    signal(pid, "TERM")?;
    if !wait_for_exit(pid, STOP_TIMEOUT) {
        eprintln!("vise-host (pid {pid}) did not exit after SIGTERM; sending SIGKILL");
        signal(pid, "KILL")?;
        if !wait_for_exit(pid, Duration::from_secs(2)) {
            bail!("vise-host (pid {pid}) is still running after SIGKILL");
        }
    }

    remove_pid(paths);
    println!("stopped vise-host (pid {pid})");
    Ok(())
}

/// Print whether the host is running. Returns `true` when it is.
pub fn status(paths: &Paths) -> anyhow::Result<bool> {
    match live_pid(paths)? {
        Some(pid) => {
            println!("vise-host: running (pid {pid})");
            println!("  log: {}", paths.log_file().display());
            Ok(true)
        }
        None => {
            println!("vise-host: not running");
            Ok(false)
        }
    }
}

/// Print the last `lines` lines of the host log, optionally following it.
pub fn logs(paths: &Paths, lines: usize, follow: bool) -> anyhow::Result<()> {
    let log_path = paths.log_file();
    let mut file = fs::File::open(&log_path).with_context(|| {
        format!(
            "no host log at {} (start the host with `vise host start`)",
            log_path.display()
        )
    })?;

    let mut contents = String::new();
    file.read_to_string(&mut contents)?;
    let mut stdout = io::stdout().lock();
    stdout.write_all(tail_lines(&contents, lines).as_bytes())?;
    stdout.flush()?;

    if !follow {
        return Ok(());
    }

    let mut offset = contents.len() as u64;
    loop {
        std::thread::sleep(FOLLOW_INTERVAL);
        let len = fs::metadata(&log_path).map(|m| m.len()).unwrap_or(0);
        if len < offset {
            // Truncated or rotated: start over from the beginning.
            offset = 0;
        }
        if len == offset {
            continue;
        }
        file.seek(SeekFrom::Start(offset))?;
        let mut chunk = Vec::new();
        file.read_to_end(&mut chunk)?;
        offset += chunk.len() as u64;
        stdout.write_all(&chunk)?;
        stdout.flush()?;
    }
}

/// The last `n` lines of `text`, each terminated by a newline.
pub fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    let mut out = String::new();
    for line in &lines[start..] {
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Pid from the pidfile if that process is alive. A stale pidfile (process
/// gone) is removed on the way through.
fn live_pid(paths: &Paths) -> anyhow::Result<Option<u32>> {
    let Some(pid) = read_pid(paths)? else {
        return Ok(None);
    };
    if process_alive(pid) {
        Ok(Some(pid))
    } else {
        remove_pid(paths);
        Ok(None)
    }
}

fn read_pid(paths: &Paths) -> anyhow::Result<Option<u32>> {
    let path = paths.pid_file();
    match fs::read_to_string(&path) {
        Ok(text) => {
            let text = text.trim();
            if text.is_empty() {
                return Ok(None);
            }
            let pid = text
                .parse()
                .with_context(|| format!("{} does not contain a pid: {text:?}", path.display()))?;
            Ok(Some(pid))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

fn write_pid(paths: &Paths, pid: u32) -> anyhow::Result<()> {
    let path = paths.pid_file();
    fs::write(&path, format!("{pid}\n")).with_context(|| format!("writing {}", path.display()))
}

fn remove_pid(paths: &Paths) {
    let _ = fs::remove_file(paths.pid_file());
}

/// `kill -0 <pid>`: true when the process exists and we may signal it.
fn process_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn signal(pid: u32, name: &str) -> anyhow::Result<()> {
    let status = Command::new("kill")
        .arg(format!("-{name}"))
        .arg(pid.to_string())
        .status()
        .context("running kill")?;
    if !status.success() {
        bail!("kill -{name} {pid} failed ({status})");
    }
    Ok(())
}

fn wait_for_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while process_alive(pid) {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_env_handles_comments_quotes_and_export() {
        let text = "\
# comment
VISE_URL=http://localhost:3000
export VISE_HOST_TOKEN='vhost_abc'

POSTGRES_PASSWORD=\"p=ss word\"
  SPACED  =  value
INVALID LINE
EMPTY=
VISE_URL=http://override:1
";
        let env = parse_env(text);
        assert_eq!(env["VISE_URL"], "http://override:1");
        assert_eq!(env["VISE_HOST_TOKEN"], "vhost_abc");
        assert_eq!(env["POSTGRES_PASSWORD"], "p=ss word");
        assert_eq!(env["SPACED"], "value");
        assert_eq!(env["EMPTY"], "");
        assert!(!env.contains_key("INVALID LINE"));
        assert_eq!(env.len(), 5);
    }

    #[test]
    fn resolve_url_precedence() {
        let mut env = BTreeMap::new();
        assert_eq!(resolve_url(None, &env), DEFAULT_URL);
        env.insert("VISE_URL".to_string(), "http://file:1".to_string());
        assert_eq!(resolve_url(None, &env), "http://file:1");
        assert_eq!(resolve_url(Some("http://flag:2"), &env), "http://flag:2");
        env.insert("VISE_URL".to_string(), String::new());
        assert_eq!(resolve_url(None, &env), DEFAULT_URL);
    }

    #[test]
    fn resolve_api_token_precedence() {
        let mut env = BTreeMap::new();
        assert_eq!(resolve_api_token(None, &env), None);
        env.insert("VISE_API_TOKEN".to_string(), " file-token ".to_string());
        assert_eq!(resolve_api_token(None, &env).as_deref(), Some("file-token"));
        assert_eq!(
            resolve_api_token(Some("flag-token"), &env).as_deref(),
            Some("flag-token")
        );
        env.insert("VISE_API_TOKEN".to_string(), String::new());
        assert_eq!(resolve_api_token(None, &env), None);
    }

    #[test]
    fn tail_lines_keeps_last_n() {
        assert_eq!(tail_lines("a\nb\nc\n", 2), "b\nc\n");
        assert_eq!(tail_lines("a\nb\nc", 5), "a\nb\nc\n");
        assert_eq!(tail_lines("", 3), "");
        assert_eq!(tail_lines("a\nb\n", 0), "");
    }

    #[test]
    fn load_env_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        assert!(load_env(&paths).unwrap().is_empty());
    }

    #[test]
    fn resolve_host_binary_explicit_and_bin_dir() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());

        let missing = dir.path().join("nope");
        let error = resolve_host_binary(Some(&missing), &paths).unwrap_err();
        assert!(error.to_string().contains("not found"), "{error}");

        fs::create_dir_all(paths.bin_dir()).unwrap();
        let installed = paths.bin_dir().join(HOST_BINARY);
        fs::write(&installed, "").unwrap();
        // No explicit path, nothing next to the test binary, so bin/ wins
        // unless a vise-host happens to be on PATH before it (it is not: bin/
        // is searched before PATH).
        assert_eq!(resolve_host_binary(None, &paths).unwrap(), installed);
        assert_eq!(
            resolve_host_binary(Some(&installed), &paths).unwrap(),
            installed
        );
    }

    /// Serializes the tests that write a script and then exec it, or that
    /// fork at all (`kill -0` in `status`). Cargo runs tests in parallel
    /// threads, and a fork in one thread while another still has its script
    /// open for writing leaves the child holding that write descriptor until
    /// it execs; exec'ing the script in the meantime fails with ETXTBSY
    /// ("Text file busy"). Holding this lock across each such test removes
    /// the overlap.
    fn process_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[cfg(unix)]
    fn write_script(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        fs::write(path, format!("#!/bin/sh\n{body}")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn start_status_stop_lifecycle() {
        let _guard = process_lock();
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        fs::create_dir_all(paths.bin_dir()).unwrap();
        let fake = paths.bin_dir().join(HOST_BINARY);
        write_script(
            &fake,
            "echo \"args: $*\"\necho \"token: $VISE_HOST_TOKEN\"\nexec sleep 30\n",
        );
        fs::write(
            paths.env_file(),
            "VISE_URL=http://example.test:1234\nVISE_HOST_TOKEN=vhost_fromfile\n",
        )
        .unwrap();

        assert!(!status(&paths).unwrap());

        let mut child = launch(
            &paths,
            StartOptions {
                binary: Some(fake.clone()),
                token: None,
                url: None,
                extra_args: vec!["--keep-workspaces".to_string()],
            },
        )
        .unwrap()
        .expect("spawned");
        // This test process is the parent, so reap the child once it is
        // killed; otherwise `kill -0` keeps seeing the zombie. (The real CLI
        // exits and leaves that to init.)
        let reaper = std::thread::spawn(move || child.wait());

        let pid = read_pid(&paths).unwrap().expect("pidfile written");
        assert!(process_alive(pid));
        assert!(status(&paths).unwrap());

        let log = fs::read_to_string(paths.log_file()).unwrap();
        assert!(log.contains("=== vise host start "), "{log}");
        assert!(
            log.contains("args: --url http://example.test:1234 --keep-workspaces"),
            "{log}"
        );
        assert!(log.contains("token: vhost_fromfile"), "{log}");

        // Second start is a no-op against the same process.
        let again = launch(
            &paths,
            StartOptions {
                binary: Some(fake),
                token: None,
                url: None,
                extra_args: vec![],
            },
        )
        .unwrap();
        assert!(again.is_none());
        assert_eq!(read_pid(&paths).unwrap(), Some(pid));

        stop(&paths).unwrap();
        assert!(!reaper.join().unwrap().unwrap().success());
        assert!(!process_alive(pid));
        assert!(!paths.pid_file().exists());
        assert!(!status(&paths).unwrap());

        // Stopping again is fine.
        stop(&paths).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn start_reports_immediate_exit() {
        let _guard = process_lock();
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        let fake = dir.path().join("failing-host");
        write_script(&fake, "echo 'boom: bad token' >&2\nexit 3\n");

        let error = start(
            &paths,
            StartOptions {
                binary: Some(fake),
                token: Some("vhost_x".to_string()),
                url: None,
                extra_args: vec![],
            },
        )
        .unwrap_err();

        let message = error.to_string();
        assert!(message.contains("exited immediately"), "{message}");
        assert!(message.contains("boom: bad token"), "{message}");
        assert!(!paths.pid_file().exists());
    }

    #[cfg(unix)]
    #[test]
    fn start_without_token_fails_before_spawning() {
        let _guard = process_lock();
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        let fake = dir.path().join("host");
        write_script(&fake, "exec sleep 30\n");

        let error = start(
            &paths,
            StartOptions {
                binary: Some(fake),
                token: None,
                url: None,
                extra_args: vec![],
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("no host token"), "{error}");
        assert!(!paths.pid_file().exists());
    }

    #[test]
    fn stale_pidfile_is_cleaned_up() {
        let _guard = process_lock();
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        // Highest possible pid on Linux is 4194304; nothing should own this.
        fs::write(paths.pid_file(), "4194303\n").unwrap();
        assert!(!status(&paths).unwrap());
        assert!(!paths.pid_file().exists());
    }
}
