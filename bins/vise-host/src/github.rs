use std::path::{Path, PathBuf};

pub struct PreparedRepo {
    pub repo_dir: PathBuf,
    pub token_file: PathBuf,
    pub base_commit: String,
    pub base_branch: String,
}

pub fn write_token_file(token_file: &Path, token: &str) -> anyhow::Result<()> {
    // Write-then-rename so the credential helper never reads a torn file.
    let tmp = token_file.with_extension("tmp");
    write_secret(&tmp, token)?;
    std::fs::rename(&tmp, token_file)?;
    Ok(())
}

#[cfg(unix)]
fn write_secret(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(contents.as_bytes())
}

#[cfg(not(unix))]
fn write_secret(path: &Path, contents: &str) -> std::io::Result<()> {
    std::fs::write(path, contents)
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
///
/// The token never appears in the URL or on any git command line: the clone
/// authenticates through the credential helper, which reads the token file.
pub async fn prepare_workspace(
    workdir: &Path,
    clone_url: &str,
    base_branch: Option<&str>,
    initial_token: &str,
) -> anyhow::Result<PreparedRepo> {
    std::fs::create_dir_all(workdir)?;
    let repo_dir = workdir.join("repo");
    let token_file = workdir.join("github-token");

    // The helper reads this file, so it must exist before the clone fetches.
    write_token_file(&token_file, initial_token)?;

    let helper = format!(
        "!f() {{ test \"$1\" = get && echo username=x-access-token && echo \"password=$(cat '{}')\"; }}; f",
        token_file.display()
    );
    let helper_config = format!("credential.helper={helper}");

    // `git clone -c` writes the config into the new repository before fetching,
    // so it both authenticates the clone and stays configured for later pushes.
    let mut args = vec!["clone", "-c", helper_config.as_str(), "--depth", "50"];
    if let Some(branch) = base_branch {
        args.extend(["--branch", branch]);
    }
    let repo_dir_s = repo_dir.to_string_lossy().into_owned();
    args.extend([clone_url, repo_dir_s.as_str()]);
    git(workdir, &args).await?;

    git(&repo_dir, &["config", "user.name", "vise[bot]"]).await?;
    git(&repo_dir, &["config", "user.email", "vise-bot@users.noreply.github.com"]).await?;

    let base_commit = git(&repo_dir, &["rev-parse", "HEAD"]).await?.trim().to_string();
    let base_branch = git(&repo_dir, &["rev-parse", "--abbrev-ref", "HEAD"])
        .await?
        .trim()
        .to_string();

    Ok(PreparedRepo { repo_dir, token_file, base_commit, base_branch })
}

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

    // Committed work exists; is it (fully) on the remote?
    let remote_sha = git(dir, &["ls-remote", "--heads", "origin", &branch])
        .await?
        .split_whitespace()
        .next()
        .map(str::to_string);

    if remote_sha.as_deref() != Some(head.as_str()) {
        // Missing remote ref or unpushed local commits: nothing (fully) on GitHub.
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
        // credential helper persisted into the clone via `git clone -c`
        assert!(sh(&prepared.repo_dir, "git config credential.helper").contains("github-token"));
        // token file exists and is refreshable
        assert_eq!(std::fs::read_to_string(&prepared.token_file).unwrap(), "tok_initial");
        write_token_file(&prepared.token_file, "tok_refreshed").unwrap();
        assert_eq!(std::fs::read_to_string(&prepared.token_file).unwrap(), "tok_refreshed");
        // base commit recorded
        assert_eq!(
            prepared.base_commit,
            sh(&prepared.repo_dir, "git rev-parse HEAD").trim()
        );
    }

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

    #[tokio::test]
    async fn detects_unpushed_base_branch_commits_as_uncommitted_kind() {
        let tmp = tempfile::tempdir().unwrap();
        let p = prepared(tmp.path()).await;
        sh(&p.repo_dir, "git commit -qm work --allow-empty");
        let outcome = detect_outcome(&p, None).await.unwrap();
        assert_eq!(outcome.kind, "uncommitted_changes");
        assert_eq!(outcome.branch.as_deref(), Some("main"));
    }
}
