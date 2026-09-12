//! Composes the input for a follow-up session from the PR's current review
//! feedback. The result is stored verbatim on the new session, so the agent
//! needs no GitHub read access of its own and the feedback it addressed is
//! frozen in the session record.

use std::collections::BTreeMap;

use crate::github::{ReviewComment, ReviewSummary};

pub struct FollowUpContext<'a> {
    pub pr_url: &'a str,
    pub head_ref: &'a str,
    pub review_summaries: &'a [ReviewSummary],
    pub review_comments: &'a [ReviewComment],
    pub failing_checks: &'a [String],
    pub instructions: Option<&'a str>,
}

/// Group inline comments into threads (root comment followed by replies),
/// ordered by the root comment's position in `comments` (oldest first).
fn threads(comments: &[ReviewComment]) -> Vec<Vec<&ReviewComment>> {
    let mut by_root: BTreeMap<usize, Vec<&ReviewComment>> = BTreeMap::new();
    let mut root_index: BTreeMap<u64, usize> = BTreeMap::new();

    for (index, comment) in comments.iter().enumerate() {
        let root = comment
            .in_reply_to_id
            .and_then(|id| root_index.get(&id).copied());
        match root {
            Some(root) => by_root.entry(root).or_default().push(comment),
            None => {
                root_index.insert(comment.id, index);
                by_root.entry(index).or_default().push(comment);
            }
        }
    }

    by_root.into_values().collect()
}

fn quote(body: &str, prefix: &str) -> String {
    body.trim()
        .lines()
        .map(|line| format!("{prefix}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn compose_input(ctx: &FollowUpContext<'_>) -> String {
    let mut out = String::new();

    out.push_str(&format!(
        "Address the review feedback on pull request {} (branch `{}`).\n\
         Make the requested changes, commit them, and push to the same branch so the \
         existing pull request updates. Do not open a new pull request.\n",
        ctx.pr_url, ctx.head_ref
    ));

    let mut anything = false;

    if !ctx.review_summaries.is_empty() {
        anything = true;
        out.push_str("\n## Review summaries\n");
        for review in ctx.review_summaries {
            out.push_str(&format!(
                "\n- @{} ({}):\n{}\n",
                review.reviewer,
                review.state,
                quote(&review.body, "  > ")
            ));
        }
    }

    let threads = threads(ctx.review_comments);
    if !threads.is_empty() {
        anything = true;
        out.push_str("\n## Review comments\n");
        for thread in threads {
            let root = thread[0];
            let location = match root.line {
                Some(line) => format!("{}:{line}", root.path),
                None => root.path.clone(),
            };
            out.push_str(&format!(
                "\n- {location} — @{}:\n{}\n",
                root.reviewer,
                quote(&root.body, "  > ")
            ));
            for reply in &thread[1..] {
                out.push_str(&format!(
                    "  - reply from @{}:\n{}\n",
                    reply.reviewer,
                    quote(&reply.body, "    > ")
                ));
            }
        }
    }

    if !ctx.failing_checks.is_empty() {
        anything = true;
        out.push_str("\n## Failing checks\n\n");
        for check in ctx.failing_checks {
            out.push_str(&format!("- {check}\n"));
        }
    }

    if !anything {
        out.push_str("\nThere is no outstanding review feedback or failing check on the PR.\n");
    }

    if let Some(instructions) = ctx.instructions.map(str::trim).filter(|s| !s.is_empty()) {
        out.push_str("\n## Additional instructions\n\n");
        out.push_str(instructions);
        out.push('\n');
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn comment(
        id: u64,
        reply_to: Option<u64>,
        who: &str,
        path: &str,
        line: Option<u64>,
        body: &str,
    ) -> ReviewComment {
        ReviewComment {
            id,
            in_reply_to_id: reply_to,
            reviewer: who.into(),
            path: path.into(),
            line,
            body: body.into(),
            created_at: Utc.with_ymd_and_hms(2026, 9, 12, 10, id as u32, 0).unwrap(),
        }
    }

    #[test]
    fn composes_threads_checks_and_instructions() {
        let comments = vec![
            comment(1, None, "alice", "src/lib.rs", Some(42), "rename this"),
            comment(2, None, "bob", "README.md", None, "typo"),
            comment(3, Some(1), "carol", "src/lib.rs", Some(42), "agreed"),
        ];
        let summaries = vec![ReviewSummary {
            reviewer: "alice".into(),
            state: "changes_requested".into(),
            body: "Please address the naming.".into(),
        }];
        let checks = vec!["clippy".to_string()];

        let input = compose_input(&FollowUpContext {
            pr_url: "https://github.com/acme/widgets/pull/17",
            head_ref: "vise/feature",
            review_summaries: &summaries,
            review_comments: &comments,
            failing_checks: &checks,
            instructions: Some("Keep the public API stable."),
        });

        assert!(input.contains("https://github.com/acme/widgets/pull/17"));
        assert!(input.contains("branch `vise/feature`"));
        assert!(input.contains("Do not open a new pull request"));
        assert!(input.contains("@alice (changes_requested)"));
        assert!(input.contains("> Please address the naming."));
        assert!(input.contains("src/lib.rs:42 — @alice"));
        assert!(input.contains("> rename this"));
        assert!(input.contains("reply from @carol"));
        assert!(input.contains("README.md — @bob"));
        assert!(input.contains("- clippy"));
        assert!(input.contains("Keep the public API stable."));

        // the reply is rendered inside its thread, after the root comment
        let root = input.find("> rename this").unwrap();
        let reply = input.find("reply from @carol").unwrap();
        let next_root = input.find("README.md — @bob").unwrap();
        assert!(root < reply && reply < next_root);
    }

    #[test]
    fn empty_feedback_says_so() {
        let input = compose_input(&FollowUpContext {
            pr_url: "https://github.com/acme/widgets/pull/17",
            head_ref: "vise/feature",
            review_summaries: &[],
            review_comments: &[],
            failing_checks: &[],
            instructions: None,
        });
        assert!(input.contains("no outstanding review feedback"));
        assert!(!input.contains("## Additional instructions"));
    }
}
