//! Pure PR tracking logic.
//!
//! The poller observes a pull request on GitHub (merge/close flags, reviews,
//! check runs), [`reduce`]s that observation to a derived [`PrState`] and
//! [`ChecksState`], and compares the result with the stored snapshot via
//! [`transitions`]. Only the derived state is ever persisted; raw reviews and
//! comments are discarded after reduction.

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use super::model::{ChecksState, PrState, PrStatus};

/// A review's verdict as GitHub reports it in the review `state` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewVerdict {
    Approved,
    ChangesRequested,
    /// A comment-only review. Never replaces an earlier verdict.
    Commented,
    /// The verdict was dismissed; the reviewer no longer counts.
    Dismissed,
}

impl ReviewVerdict {
    /// Parse GitHub's `state` string. Pending or unknown states yield `None`.
    pub fn parse(state: &str) -> Option<Self> {
        match state.to_ascii_uppercase().as_str() {
            "APPROVED" => Some(Self::Approved),
            "CHANGES_REQUESTED" => Some(Self::ChangesRequested),
            "COMMENTED" => Some(Self::Commented),
            "DISMISSED" => Some(Self::Dismissed),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewObservation {
    pub reviewer: String,
    pub verdict: ReviewVerdict,
    pub submitted_at: DateTime<Utc>,
    /// The commit the review was submitted against, when GitHub reports it.
    pub commit_sha: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckRunObservation {
    pub name: String,
    /// GitHub `status`: "queued" | "in_progress" | "completed" | ...
    pub status: String,
    /// GitHub `conclusion`, set once the run completes.
    pub conclusion: Option<String>,
}

impl CheckRunObservation {
    pub fn is_failing(&self) -> bool {
        self.status == "completed"
            && matches!(
                self.conclusion.as_deref(),
                Some("failure")
                    | Some("timed_out")
                    | Some("cancelled")
                    | Some("action_required")
                    | Some("startup_failure")
                    | Some("stale")
            )
    }
}

/// Everything the poller learns about a PR in one pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrObservation {
    pub merged: bool,
    pub closed: bool,
    pub head_sha: String,
    pub reviews: Vec<ReviewObservation>,
    pub check_runs: Vec<CheckRunObservation>,
}

/// Reduce GitHub's review list to one state.
///
/// Rules, in priority order:
/// 1. The latest verdict-bearing review per reviewer wins. Comment-only
///    reviews never replace a verdict; a dismissal clears it.
/// 2. Any outstanding `changes_requested` beats every approval. Like GitHub,
///    a request for changes stays in force across pushes until the reviewer
///    re-reviews or the review is dismissed.
/// 3. Approvals only count against the current head: an approval left on an
///    earlier commit is stale after a force-push.
pub fn reduce_reviews(head_sha: &str, reviews: &[ReviewObservation]) -> PrState {
    let mut sorted: Vec<&ReviewObservation> = reviews.iter().collect();
    sorted.sort_by_key(|review| review.submitted_at);

    let mut latest: HashMap<&str, &ReviewObservation> = HashMap::new();
    for review in sorted {
        match review.verdict {
            ReviewVerdict::Commented => {}
            ReviewVerdict::Dismissed => {
                latest.remove(review.reviewer.as_str());
            }
            ReviewVerdict::Approved | ReviewVerdict::ChangesRequested => {
                latest.insert(review.reviewer.as_str(), review);
            }
        }
    }

    let changes_requested = latest
        .values()
        .any(|review| review.verdict == ReviewVerdict::ChangesRequested);
    if changes_requested {
        return PrState::ChangesRequested;
    }

    let approved = latest.values().any(|review| {
        review.verdict == ReviewVerdict::Approved
            && review
                .commit_sha
                .as_deref()
                .is_none_or(|sha| sha == head_sha)
    });
    if approved {
        PrState::Approved
    } else {
        PrState::ReviewPending
    }
}

/// Reduce the head commit's check runs to one state. `None` when there are
/// no check runs at all: nothing is pending, nothing has passed.
pub fn reduce_checks(check_runs: &[CheckRunObservation]) -> Option<ChecksState> {
    if check_runs.is_empty() {
        return None;
    }
    if check_runs.iter().any(CheckRunObservation::is_failing) {
        return Some(ChecksState::Failing);
    }
    if check_runs.iter().any(|run| run.status != "completed") {
        return Some(ChecksState::Pending);
    }
    Some(ChecksState::Passing)
}

/// Reduce a full observation. GitHub's own merged/closed flags decide the
/// terminal states; reviews only matter while the PR is open.
pub fn reduce(observation: &PrObservation) -> (PrState, Option<ChecksState>) {
    let checks = reduce_checks(&observation.check_runs);
    let state = if observation.merged {
        PrState::Merged
    } else if observation.closed {
        PrState::Closed
    } else {
        reduce_reviews(&observation.head_sha, &observation.reviews)
    };
    (state, checks)
}

/// A change between the stored snapshot and a fresh observation. Serialised
/// into the session's event stream by [`PrTransition::to_event_payload`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrTransition {
    State {
        from: Option<PrState>,
        to: PrState,
    },
    Checks {
        from: Option<ChecksState>,
        to: Option<ChecksState>,
    },
}

impl PrTransition {
    pub fn to_event_payload(&self) -> serde_json::Value {
        match self {
            PrTransition::State { from, to } => serde_json::json!({
                "type": "pr_state_changed",
                "from": from.map(PrState::as_str),
                "to": to.as_str(),
            }),
            PrTransition::Checks { from, to } => serde_json::json!({
                "type": "checks_state_changed",
                "from": from.map(ChecksState::as_str),
                "to": to.map(ChecksState::as_str),
            }),
        }
    }
}

/// Compare the stored snapshot with a new observation. Empty when nothing
/// changed, so a quiet poll tick never produces an event.
pub fn transitions(
    previous: Option<&PrStatus>,
    state: PrState,
    checks: Option<ChecksState>,
) -> Vec<PrTransition> {
    let mut out = Vec::new();

    let previous_state = previous.map(|p| p.state);
    if previous_state != Some(state) {
        out.push(PrTransition::State {
            from: previous_state,
            to: state,
        });
    }

    match previous {
        Some(previous) if previous.checks != checks => out.push(PrTransition::Checks {
            from: previous.checks,
            to: checks,
        }),
        None if checks.is_some() => out.push(PrTransition::Checks {
            from: None,
            to: checks,
        }),
        _ => {}
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 12, 10, minute, 0).unwrap()
    }

    fn review(
        reviewer: &str,
        verdict: ReviewVerdict,
        minute: u32,
        commit: Option<&str>,
    ) -> ReviewObservation {
        ReviewObservation {
            reviewer: reviewer.into(),
            verdict,
            submitted_at: at(minute),
            commit_sha: commit.map(String::from),
        }
    }

    fn run(name: &str, status: &str, conclusion: Option<&str>) -> CheckRunObservation {
        CheckRunObservation {
            name: name.into(),
            status: status.into(),
            conclusion: conclusion.map(String::from),
        }
    }

    use ReviewVerdict::*;

    #[test]
    fn reduces_reviews_table() {
        let head = "head";
        let cases: Vec<(&str, Vec<ReviewObservation>, PrState)> = vec![
            ("no reviews", vec![], PrState::ReviewPending),
            (
                "single approval on head",
                vec![review("alice", Approved, 1, Some(head))],
                PrState::Approved,
            ),
            (
                "approval without commit info counts",
                vec![review("alice", Approved, 1, None)],
                PrState::Approved,
            ),
            (
                "same reviewer requests changes then approves",
                vec![
                    review("alice", ChangesRequested, 1, Some(head)),
                    review("alice", Approved, 2, Some(head)),
                ],
                PrState::Approved,
            ),
            (
                "same reviewer approves then requests changes",
                vec![
                    review("alice", Approved, 1, Some(head)),
                    review("alice", ChangesRequested, 2, Some(head)),
                ],
                PrState::ChangesRequested,
            ),
            (
                "out-of-order input: latest still wins",
                vec![
                    review("alice", Approved, 2, Some(head)),
                    review("alice", ChangesRequested, 1, Some(head)),
                ],
                PrState::Approved,
            ),
            (
                "mixed approvals with one outstanding changes_requested",
                vec![
                    review("alice", Approved, 1, Some(head)),
                    review("bob", ChangesRequested, 2, Some(head)),
                    review("carol", Approved, 3, Some(head)),
                ],
                PrState::ChangesRequested,
            ),
            (
                "stale approval after force-push",
                vec![review("alice", Approved, 1, Some("old"))],
                PrState::ReviewPending,
            ),
            (
                "stale changes_requested still blocks after force-push",
                vec![review("alice", ChangesRequested, 1, Some("old"))],
                PrState::ChangesRequested,
            ),
            (
                "fresh approval alongside a stale one",
                vec![
                    review("alice", Approved, 1, Some("old")),
                    review("bob", Approved, 2, Some(head)),
                ],
                PrState::Approved,
            ),
            (
                "comment-only review does not clear changes_requested",
                vec![
                    review("alice", ChangesRequested, 1, Some(head)),
                    review("alice", Commented, 2, Some(head)),
                ],
                PrState::ChangesRequested,
            ),
            (
                "comment-only reviews alone are pending",
                vec![review("alice", Commented, 1, Some(head))],
                PrState::ReviewPending,
            ),
            (
                "dismissed changes_requested no longer blocks",
                vec![
                    review("alice", ChangesRequested, 1, Some(head)),
                    review("alice", Dismissed, 2, Some(head)),
                    review("bob", Approved, 3, Some(head)),
                ],
                PrState::Approved,
            ),
        ];

        for (name, reviews, expected) in cases {
            assert_eq!(reduce_reviews(head, &reviews), expected, "{name}");
        }
    }

    #[test]
    fn reduces_checks_table() {
        let cases: Vec<(&str, Vec<CheckRunObservation>, Option<ChecksState>)> = vec![
            ("zero check runs", vec![], None),
            (
                "all success",
                vec![
                    run("test", "completed", Some("success")),
                    run("lint", "completed", Some("skipped")),
                    run("docs", "completed", Some("neutral")),
                ],
                Some(ChecksState::Passing),
            ),
            (
                "one queued",
                vec![
                    run("test", "completed", Some("success")),
                    run("lint", "queued", None),
                ],
                Some(ChecksState::Pending),
            ),
            (
                "one failure beats pending",
                vec![
                    run("test", "completed", Some("failure")),
                    run("lint", "in_progress", None),
                ],
                Some(ChecksState::Failing),
            ),
            (
                "timed out counts as failing",
                vec![run("test", "completed", Some("timed_out"))],
                Some(ChecksState::Failing),
            ),
            (
                "cancelled counts as failing",
                vec![run("test", "completed", Some("cancelled"))],
                Some(ChecksState::Failing),
            ),
        ];

        for (name, runs, expected) in cases {
            assert_eq!(reduce_checks(&runs), expected, "{name}");
        }
    }

    #[test]
    fn github_flags_decide_terminal_states() {
        let base = PrObservation {
            merged: false,
            closed: false,
            head_sha: "head".into(),
            reviews: vec![review("alice", ChangesRequested, 1, Some("head"))],
            check_runs: vec![run("test", "completed", Some("success"))],
        };

        assert_eq!(
            reduce(&base),
            (PrState::ChangesRequested, Some(ChecksState::Passing))
        );

        let merged = PrObservation {
            merged: true,
            closed: true,
            ..base.clone()
        };
        assert_eq!(reduce(&merged).0, PrState::Merged);

        let closed = PrObservation {
            closed: true,
            ..base
        };
        assert_eq!(reduce(&closed).0, PrState::Closed);
    }

    #[test]
    fn parses_review_states() {
        assert_eq!(ReviewVerdict::parse("APPROVED"), Some(Approved));
        assert_eq!(
            ReviewVerdict::parse("changes_requested"),
            Some(ChangesRequested)
        );
        assert_eq!(ReviewVerdict::parse("COMMENTED"), Some(Commented));
        assert_eq!(ReviewVerdict::parse("DISMISSED"), Some(Dismissed));
        assert_eq!(ReviewVerdict::parse("PENDING"), None);
    }

    fn snapshot(state: PrState, checks: Option<ChecksState>) -> PrStatus {
        PrStatus {
            state,
            checks,
            last_synced_at: at(0),
        }
    }

    #[test]
    fn first_observation_reports_initial_state() {
        let events = transitions(None, PrState::ReviewPending, Some(ChecksState::Pending));
        assert_eq!(
            events,
            vec![
                PrTransition::State {
                    from: None,
                    to: PrState::ReviewPending
                },
                PrTransition::Checks {
                    from: None,
                    to: Some(ChecksState::Pending)
                },
            ]
        );
    }

    #[test]
    fn first_observation_without_checks_has_no_checks_event() {
        let events = transitions(None, PrState::ReviewPending, None);
        assert_eq!(
            events,
            vec![PrTransition::State {
                from: None,
                to: PrState::ReviewPending
            }]
        );
    }

    #[test]
    fn unchanged_observation_is_a_noop() {
        let previous = snapshot(PrState::Approved, Some(ChecksState::Passing));
        assert!(
            transitions(
                Some(&previous),
                PrState::Approved,
                Some(ChecksState::Passing)
            )
            .is_empty()
        );

        let previous = snapshot(PrState::ReviewPending, None);
        assert!(transitions(Some(&previous), PrState::ReviewPending, None).is_empty());
    }

    #[test]
    fn detects_state_and_checks_changes_independently() {
        let previous = snapshot(PrState::ReviewPending, Some(ChecksState::Pending));

        assert_eq!(
            transitions(
                Some(&previous),
                PrState::ChangesRequested,
                Some(ChecksState::Pending)
            ),
            vec![PrTransition::State {
                from: Some(PrState::ReviewPending),
                to: PrState::ChangesRequested
            }]
        );

        assert_eq!(
            transitions(
                Some(&previous),
                PrState::ReviewPending,
                Some(ChecksState::Failing)
            ),
            vec![PrTransition::Checks {
                from: Some(ChecksState::Pending),
                to: Some(ChecksState::Failing)
            }]
        );

        assert_eq!(
            transitions(Some(&previous), PrState::Merged, None).len(),
            2,
            "both changed"
        );
    }

    #[test]
    fn event_payloads_are_typed() {
        let state = PrTransition::State {
            from: Some(PrState::ReviewPending),
            to: PrState::ChangesRequested,
        }
        .to_event_payload();
        assert_eq!(
            state,
            serde_json::json!({
                "type": "pr_state_changed",
                "from": "review_pending",
                "to": "changes_requested"
            })
        );

        let checks = PrTransition::Checks {
            from: None,
            to: Some(ChecksState::Failing),
        }
        .to_event_payload();
        assert_eq!(
            checks,
            serde_json::json!({
                "type": "checks_state_changed",
                "from": null,
                "to": "failing"
            })
        );
    }
}
