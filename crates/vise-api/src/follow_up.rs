//! Server-side composition of a follow-up session's input: the PR's current
//! review threads and failing checks, fetched with the server's credential
//! and frozen into the session record at dispatch time.

use crate::github::{CheckRun, GitHubError, GitHubReadClient, PullRequest, Review, ReviewComment};

#[derive(Debug, Clone)]
pub struct ReviewThread {
    pub root: ReviewComment,
    pub replies: Vec<ReviewComment>,
}

#[derive(Debug, Clone)]
pub struct FollowUpContext {
    pub pr: PullRequest,
    /// Top-level review submissions with a body (summaries, not inline comments).
    pub reviews: Vec<Review>,
    pub threads: Vec<ReviewThread>,
    pub failing_checks: Vec<CheckRun>,
}

/// Fetch everything `compose_input` needs for `repo` PR `number`.
pub async fn fetch_context(
    github: &GitHubReadClient,
    token: &str,
    repo: &str,
    number: u64,
) -> Result<FollowUpContext, GitHubError> {
    let pr = github.pull_request(token, repo, number).await?;
    let reviews = github.reviews(token, repo, number).await?;
    let comments = github.review_comments(token, repo, number).await?;
    let check_runs = github.check_runs(token, repo, &pr.head.sha).await?;

    Ok(FollowUpContext {
        pr,
        reviews: reviews
            .into_iter()
            .filter(|review| review.body.as_deref().is_some_and(|b| !b.trim().is_empty()))
            .collect(),
        threads: group_threads(comments),
        failing_checks: check_runs
            .into_iter()
            .filter(|run| {
                run.status == "completed"
                    && matches!(
                        run.conclusion.as_deref(),
                        Some(
                            "failure"
                                | "timed_out"
                                | "cancelled"
                                | "action_required"
                                | "startup_failure"
                        )
                    )
            })
            .collect(),
    })
}

/// Group inline comments into threads by `in_reply_to_id`, preserving
/// GitHub's chronological order. Replies whose root is missing (deleted)
/// become their own thread so nothing is silently dropped.
pub fn group_threads(comments: Vec<ReviewComment>) -> Vec<ReviewThread> {
    let mut threads: Vec<ReviewThread> = Vec::new();
    let mut orphans: Vec<ReviewComment> = Vec::new();

    for comment in comments {
        match comment.in_reply_to_id {
            None => threads.push(ReviewThread {
                root: comment,
                replies: Vec::new(),
            }),
            Some(parent) => match threads.iter_mut().find(|t| t.root.id == parent) {
                Some(thread) => thread.replies.push(comment),
                None => orphans.push(comment),
            },
        }
    }

    for orphan in orphans {
        match threads
            .iter_mut()
            .find(|t| t.root.id == orphan.in_reply_to_id.unwrap_or(0))
        {
            Some(thread) => thread.replies.push(orphan),
            None => threads.push(ReviewThread {
                root: orphan,
                replies: Vec::new(),
            }),
        }
    }

    threads
}

fn login(user: Option<&crate::github::GitHubUser>) -> &str {
    user.map(|u| u.login.as_str()).unwrap_or("unknown")
}

fn quote(body: &str) -> String {
    body.lines()
        .map(|line| format!("> {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Render the agent-facing prompt for a follow-up session.
pub fn compose_input(context: &FollowUpContext, instructions: Option<&str>) -> String {
    let pr = &context.pr;
    let mut out = String::new();

    out.push_str(&format!(
        "Address the review feedback on pull request {} (\"{}\").\n\
         The PR's branch is `{}`, which is the branch you are checked out on. Push follow-up \
         commits to that branch so the existing pull request updates. Do not open a new pull \
         request and do not force-push.\n",
        pr.html_url, pr.title, pr.head.name
    ));

    if let Some(instructions) = instructions.map(str::trim).filter(|s| !s.is_empty()) {
        out.push_str("\n## Instructions from the requester\n\n");
        out.push_str(instructions);
        out.push('\n');
    }

    if !context.reviews.is_empty() {
        out.push_str("\n## Review summaries\n\n");
        for review in &context.reviews {
            let state = review.state.to_ascii_lowercase().replace('_', " ");
            out.push_str(&format!(
                "### @{} ({state})\n\n",
                login(review.user.as_ref())
            ));
            out.push_str(&quote(review.body.as_deref().unwrap_or_default()));
            out.push_str("\n\n");
        }
    }

    if !context.threads.is_empty() {
        out.push_str("\n## Review comment threads\n\n");
        for thread in &context.threads {
            let root = &thread.root;
            let location = match (root.line, root.original_line) {
                (Some(line), _) => format!("`{}` line {line}", root.path),
                (None, Some(line)) => {
                    format!("`{}` (outdated; originally line {line})", root.path)
                }
                (None, None) => format!("`{}`", root.path),
            };
            out.push_str(&format!(
                "### {location} — @{}\n\n",
                login(root.user.as_ref())
            ));
            if let Some(hunk) = root.diff_hunk.as_deref().filter(|h| !h.trim().is_empty()) {
                out.push_str("```diff\n");
                out.push_str(hunk.trim_end());
                out.push_str("\n```\n\n");
            }
            out.push_str(&quote(&root.body));
            out.push('\n');
            for reply in &thread.replies {
                out.push_str(&format!("\n↳ @{} replied:\n", login(reply.user.as_ref())));
                out.push_str(&quote(&reply.body));
                out.push('\n');
            }
            out.push('\n');
        }
    }

    if !context.failing_checks.is_empty() {
        out.push_str("\n## Failing checks\n\n");
        for run in &context.failing_checks {
            let link = run
                .details_url
                .as_deref()
                .or(run.html_url.as_deref())
                .map(|url| format!(" — {url}"))
                .unwrap_or_default();
            out.push_str(&format!(
                "- {} ({}){link}\n",
                run.name,
                run.conclusion.as_deref().unwrap_or("failed")
            ));
        }
    }

    if context.reviews.is_empty() && context.threads.is_empty() && context.failing_checks.is_empty()
    {
        out.push_str(
            "\nNo open review comments or failing checks were found when this session was \
             created.\n",
        );
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::{GitHubUser, GitRef};

    fn user(login: &str) -> Option<GitHubUser> {
        Some(GitHubUser {
            login: login.into(),
        })
    }

    fn comment(
        id: u64,
        reply_to: Option<u64>,
        path: &str,
        line: Option<u64>,
        body: &str,
    ) -> ReviewComment {
        ReviewComment {
            id,
            in_reply_to_id: reply_to,
            user: user("ana"),
            path: path.into(),
            line,
            original_line: Some(10),
            body: body.into(),
            diff_hunk: Some("@@ -1 +1 @@\n-old\n+new".into()),
            html_url: None,
            created_at: None,
        }
    }

    fn pr() -> PullRequest {
        PullRequest {
            number: 7,
            state: "open".into(),
            merged: false,
            html_url: "https://github.com/acme/widgets/pull/7".into(),
            title: "Add widgets".into(),
            head: GitRef {
                name: "vise/widgets".into(),
                sha: "abc".into(),
            },
            base: GitRef {
                name: "main".into(),
                sha: "def".into(),
            },
            user: user("vise[bot]"),
        }
    }

    #[test]
    fn groups_replies_under_their_root() {
        let threads = group_threads(vec![
            comment(1, None, "a.rs", Some(3), "root a"),
            comment(2, Some(1), "a.rs", Some(3), "reply a"),
            comment(3, None, "b.rs", None, "root b"),
            comment(4, Some(99), "c.rs", Some(1), "orphan"),
        ]);
        assert_eq!(threads.len(), 3);
        assert_eq!(threads[0].replies.len(), 1);
        assert_eq!(threads[0].replies[0].body, "reply a");
        assert_eq!(threads[1].root.body, "root b");
        assert_eq!(threads[2].root.body, "orphan");
    }

    #[test]
    fn composes_threads_checks_and_instructions() {
        let context = FollowUpContext {
            pr: pr(),
            reviews: vec![Review {
                id: 1,
                user: user("bo"),
                state: "CHANGES_REQUESTED".into(),
                body: Some("Please add tests".into()),
                commit_id: None,
                submitted_at: None,
                html_url: None,
            }],
            threads: group_threads(vec![
                comment(1, None, "src/lib.rs", Some(42), "rename this"),
                comment(2, Some(1), "src/lib.rs", Some(42), "agreed"),
                comment(3, None, "src/old.rs", None, "outdated remark"),
            ]),
            failing_checks: vec![CheckRun {
                name: "clippy".into(),
                status: "completed".into(),
                conclusion: Some("failure".into()),
                details_url: Some("https://ci/1".into()),
                html_url: None,
            }],
        };

        let input = compose_input(&context, Some("  Keep the public API stable.  "));

        assert!(input.contains("https://github.com/acme/widgets/pull/7"));
        assert!(input.contains("`vise/widgets`"));
        assert!(input.contains("Do not open a new pull request"));
        assert!(
            input.contains("## Instructions from the requester\n\nKeep the public API stable.\n")
        );
        assert!(input.contains("### @bo (changes requested)\n\n> Please add tests"));
        assert!(input.contains("### `src/lib.rs` line 42 — @ana"));
        assert!(input.contains("```diff\n@@ -1 +1 @@\n-old\n+new\n```"));
        assert!(input.contains("> rename this\n\n↳ @ana replied:\n> agreed"));
        assert!(input.contains("`src/old.rs` (outdated; originally line 10)"));
        assert!(input.contains("- clippy (failure) — https://ci/1"));
        assert!(!input.contains("No open review comments"));
    }

    #[test]
    fn composes_empty_context_without_instructions() {
        let context = FollowUpContext {
            pr: pr(),
            reviews: vec![],
            threads: vec![],
            failing_checks: vec![],
        };
        let input = compose_input(&context, None);
        assert!(input.contains("No open review comments or failing checks"));
        assert!(!input.contains("## Instructions"));
        assert!(!input.contains("## Review"));
        assert!(!input.contains("## Failing"));
    }
}
