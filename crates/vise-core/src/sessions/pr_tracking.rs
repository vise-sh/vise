//! Pure reduction of GitHub observations into the derived [`PrStatus`] and the
//! session events that record its transitions. No I/O lives here: the poller
//! in `vise-api` fetches, this module decides.

use chrono::{DateTime, Utc};
use serde_json::json;

use super::model::{ChecksState, PrState, PrStatus};

/// Number of consecutive 401/403/404 polls before a PR is marked `sync_error`.
pub const SYNC_ERROR_THRESHOLD: i32 = 3;

/// GitHub's own open/merged/closed flags for a pull request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrLifecycle {
    Open,
    Merged,
    Closed,
}

/// One submitted review, as GitHub reports it. `COMMENTED` and `PENDING`
/// reviews carry no decision and never change a reviewer's standing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewDecision {
    Approved,
    ChangesRequested,
    Commented,
    Dismissed,
    Pending,
}

impl ReviewDecision {
    /// Parse GitHub's review `state` string (case-insensitive). Unknown values
    /// are treated as comments so they never affect the derived state.
    pub fn parse(state: &str) -> Self {
        match state.to_ascii_uppercase().as_str() {
            "APPROVED" => Self::Approved,
            "CHANGES_REQUESTED" => Self::ChangesRequested,
            "DISMISSED" => Self::Dismissed,
            "PENDING" => Self::Pending,
            _ => Self::Commented,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewObservation {
    /// Reviewer login; decisions are collapsed per reviewer.
    pub reviewer: String,
    pub decision: ReviewDecision,
    /// The commit the review was submitted against. `None` when GitHub omits it.
    pub commit_id: Option<String>,
    pub submitted_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckRunObservation {
    pub name: String,
    /// "queued" | "in_progress" | "completed" | "waiting" | "requested" | "pending"
    pub status: String,
    /// Set once completed: "success" | "failure" | "neutral" | "cancelled" |
    /// "skipped" | "timed_out" | "action_required" | "stale" | "startup_failure"
    pub conclusion: Option<String>,
}

impl CheckRunObservation {
    pub fn is_failing(&self) -> bool {
        self.status == "completed"
            && matches!(
                self.conclusion.as_deref(),
                Some("failure" | "timed_out" | "cancelled" | "action_required" | "startup_failure")
            )
    }
}

/// Everything the poller learned about a PR in one tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrObservation {
    pub lifecycle: PrLifecycle,
    pub head_sha: String,
    pub reviews: Vec<ReviewObservation>,
    pub check_runs: Vec<CheckRunObservation>,
}

/// Reduce a full observation to the derived `(state, checks)` pair.
pub fn reduce(observation: &PrObservation) -> (PrState, Option<ChecksState>) {
    let state = match observation.lifecycle {
        PrLifecycle::Merged => PrState::Merged,
        PrLifecycle::Closed => PrState::Closed,
        PrLifecycle::Open => reduce_reviews(&observation.reviews, &observation.head_sha),
    };
    (state, reduce_checks(&observation.check_runs))
}

/// Collapse a review list to one state for an open PR.
///
/// Rules, in order:
/// 1. Per reviewer, the latest *decisive* review (approve / request changes /
///    dismiss) wins; plain comments never change a reviewer's standing.
/// 2. Any outstanding `changes_requested` beats every approval. A request for
///    changes stays outstanding across pushes until that reviewer re-reviews
///    or the review is dismissed, matching GitHub's merge-gate semantics.
/// 3. Approvals only count against the current head: an approval submitted
///    on a commit that was since force-pushed away is stale and ignored.
/// 4. Otherwise the PR is still waiting for review.
pub fn reduce_reviews(reviews: &[ReviewObservation], head_sha: &str) -> PrState {
    let mut ordered: Vec<&ReviewObservation> = reviews.iter().collect();
    // GitHub returns reviews chronologically; sorting is a guard, and it is
    // stable so unordered timestamps keep their input order.
    ordered.sort_by_key(|review| review.submitted_at);

    let mut latest: Vec<(&str, &ReviewObservation)> = Vec::new();
    for review in ordered {
        match review.decision {
            ReviewDecision::Commented | ReviewDecision::Pending => continue,
            ReviewDecision::Dismissed => {
                latest.retain(|(reviewer, _)| *reviewer != review.reviewer);
            }
            ReviewDecision::Approved | ReviewDecision::ChangesRequested => {
                latest.retain(|(reviewer, _)| *reviewer != review.reviewer);
                latest.push((review.reviewer.as_str(), review));
            }
        }
    }

    if latest
        .iter()
        .any(|(_, review)| review.decision == ReviewDecision::ChangesRequested)
    {
        return PrState::ChangesRequested;
    }

    let approved_at_head = latest.iter().any(|(_, review)| {
        review.decision == ReviewDecision::Approved
            && review
                .commit_id
                .as_deref()
                .is_none_or(|commit| commit == head_sha)
    });

    if approved_at_head {
        PrState::Approved
    } else {
        PrState::ReviewPending
    }
}

/// Collapse the head commit's check runs. Zero runs means "no checks", which
/// is distinct from all-passing and reported as `None`.
pub fn reduce_checks(runs: &[CheckRunObservation]) -> Option<ChecksState> {
    if runs.is_empty() {
        return None;
    }
    if runs.iter().any(CheckRunObservation::is_failing) {
        return Some(ChecksState::Failing);
    }
    if runs.iter().any(|run| run.status != "completed") {
        return Some(ChecksState::Pending);
    }
    Some(ChecksState::Passing)
}

/// Compare the stored snapshot with a fresh observation and produce the
/// session events that record what changed. Empty when nothing changed, so a
/// poll tick that observes the same state appends nothing.
pub fn detect_transitions(
    previous: Option<&PrStatus>,
    state: PrState,
    checks: Option<ChecksState>,
) -> Vec<serde_json::Value> {
    let mut events = Vec::new();

    let previous_state = previous.map(|status| status.state);
    if previous_state != Some(state) {
        events.push(json!({
            "type": "pr_state_changed",
            "from": previous_state.map(PrState::as_str),
            "to": state.as_str(),
        }));
    }

    let previous_checks = previous.and_then(|status| status.checks);
    if previous_checks != checks {
        events.push(json!({
            "type": "checks_state_changed",
            "from": previous_checks.map(ChecksState::as_str),
            "to": checks.map(ChecksState::as_str),
        }));
    }

    events
}

/// Split a GitHub PR URL (`https://github.com/owner/name/pull/123`) into its
/// `owner/name` repo and PR number.
pub fn parse_pr_url(url: &str) -> Option<(String, u64)> {
    let path = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?
        .split_once('/')
        .map(|(_, rest)| rest)?;
    let path = path.split(['?', '#']).next()?.trim_end_matches('/');
    let mut parts = path.split('/');
    let owner = parts.next().filter(|s| !s.is_empty())?;
    let name = parts.next().filter(|s| !s.is_empty())?;
    if parts.next()? != "pull" {
        return None;
    }
    let number: u64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((format!("{owner}/{name}"), number))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(minute: u32) -> Option<DateTime<Utc>> {
        Some(Utc.with_ymd_and_hms(2026, 9, 11, 12, minute, 0).unwrap())
    }

    fn review(
        reviewer: &str,
        decision: ReviewDecision,
        commit: &str,
        minute: u32,
    ) -> ReviewObservation {
        ReviewObservation {
            reviewer: reviewer.into(),
            decision,
            commit_id: Some(commit.into()),
            submitted_at: at(minute),
        }
    }

    fn run(name: &str, status: &str, conclusion: Option<&str>) -> CheckRunObservation {
        CheckRunObservation {
            name: name.into(),
            status: status.into(),
            conclusion: conclusion.map(String::from),
        }
    }

    use ReviewDecision::*;

    #[test]
    fn review_reducer_table() {
        let head = "head";
        let cases: Vec<(&str, Vec<ReviewObservation>, PrState)> = vec![
            ("no reviews", vec![], PrState::ReviewPending),
            (
                "single approval at head",
                vec![review("ana", Approved, head, 1)],
                PrState::Approved,
            ),
            (
                "same reviewer requests changes then approves",
                vec![
                    review("ana", ChangesRequested, head, 1),
                    review("ana", Approved, head, 2),
                ],
                PrState::Approved,
            ),
            (
                "same reviewer approves then requests changes",
                vec![
                    review("ana", Approved, head, 1),
                    review("ana", ChangesRequested, head, 2),
                ],
                PrState::ChangesRequested,
            ),
            (
                "mixed approvals with one outstanding changes_requested",
                vec![
                    review("ana", Approved, head, 1),
                    review("bo", ChangesRequested, head, 2),
                    review("cy", Approved, head, 3),
                ],
                PrState::ChangesRequested,
            ),
            (
                "stale approval after force-push",
                vec![review("ana", Approved, "old", 1)],
                PrState::ReviewPending,
            ),
            (
                "changes_requested stays outstanding across force-push",
                vec![review("ana", ChangesRequested, "old", 1)],
                PrState::ChangesRequested,
            ),
            (
                "stale approval plus fresh approval from another reviewer",
                vec![
                    review("ana", Approved, "old", 1),
                    review("bo", Approved, head, 2),
                ],
                PrState::Approved,
            ),
            (
                "comments never change standing",
                vec![
                    review("ana", Approved, head, 1),
                    review("ana", Commented, head, 2),
                    review("bo", Commented, head, 3),
                ],
                PrState::Approved,
            ),
            (
                "dismissed changes_requested clears the block",
                vec![
                    review("ana", ChangesRequested, head, 1),
                    review("bo", Approved, head, 2),
                    review("ana", Dismissed, head, 3),
                ],
                PrState::Approved,
            ),
            (
                "dismissed approval leaves the PR pending",
                vec![
                    review("ana", Approved, head, 1),
                    review("ana", Dismissed, head, 2),
                ],
                PrState::ReviewPending,
            ),
            (
                "pending (unsubmitted) reviews are ignored",
                vec![review("ana", Pending, head, 1)],
                PrState::ReviewPending,
            ),
            (
                "approval without commit id counts",
                vec![ReviewObservation {
                    reviewer: "ana".into(),
                    decision: Approved,
                    commit_id: None,
                    submitted_at: at(1),
                }],
                PrState::Approved,
            ),
            (
                "out-of-order input is sorted by submission time",
                vec![
                    review("ana", Approved, head, 2),
                    review("ana", ChangesRequested, head, 1),
                ],
                PrState::Approved,
            ),
        ];

        for (name, reviews, expected) in cases {
            assert_eq!(reduce_reviews(&reviews, head), expected, "{name}");
        }
    }

    #[test]
    fn checks_reducer_table() {
        let cases: Vec<(&str, Vec<CheckRunObservation>, Option<ChecksState>)> = vec![
            ("zero check runs", vec![], None),
            (
                "all success",
                vec![
                    run("build", "completed", Some("success")),
                    run("lint", "completed", Some("success")),
                ],
                Some(ChecksState::Passing),
            ),
            (
                "neutral and skipped count as passing",
                vec![
                    run("a", "completed", Some("neutral")),
                    run("b", "completed", Some("skipped")),
                ],
                Some(ChecksState::Passing),
            ),
            (
                "one queued makes pending",
                vec![
                    run("a", "completed", Some("success")),
                    run("b", "queued", None),
                ],
                Some(ChecksState::Pending),
            ),
            (
                "in_progress is pending",
                vec![run("a", "in_progress", None)],
                Some(ChecksState::Pending),
            ),
            (
                "failure beats pending",
                vec![
                    run("a", "completed", Some("failure")),
                    run("b", "in_progress", None),
                ],
                Some(ChecksState::Failing),
            ),
            (
                "timed_out, cancelled, action_required fail",
                vec![run("a", "completed", Some("timed_out"))],
                Some(ChecksState::Failing),
            ),
            (
                "cancelled fails",
                vec![run("a", "completed", Some("cancelled"))],
                Some(ChecksState::Failing),
            ),
            (
                "action_required fails",
                vec![run("a", "completed", Some("action_required"))],
                Some(ChecksState::Failing),
            ),
        ];

        for (name, runs, expected) in cases {
            assert_eq!(reduce_checks(&runs), expected, "{name}");
        }
    }

    #[test]
    fn reduce_uses_github_flags_for_terminal_states() {
        let base = PrObservation {
            lifecycle: PrLifecycle::Open,
            head_sha: "head".into(),
            reviews: vec![review("ana", ChangesRequested, "head", 1)],
            check_runs: vec![run("ci", "completed", Some("success"))],
        };
        assert_eq!(
            reduce(&base),
            (PrState::ChangesRequested, Some(ChecksState::Passing))
        );

        let merged = PrObservation {
            lifecycle: PrLifecycle::Merged,
            ..base.clone()
        };
        assert_eq!(reduce(&merged).0, PrState::Merged);

        let closed = PrObservation {
            lifecycle: PrLifecycle::Closed,
            ..base
        };
        assert_eq!(reduce(&closed).0, PrState::Closed);
    }

    fn snapshot(state: PrState, checks: Option<ChecksState>) -> PrStatus {
        PrStatus {
            state,
            checks,
            last_synced_at: at(0).unwrap(),
        }
    }

    #[test]
    fn no_events_when_nothing_changed() {
        let previous = snapshot(PrState::ReviewPending, Some(ChecksState::Pending));
        let events = detect_transitions(
            Some(&previous),
            PrState::ReviewPending,
            Some(ChecksState::Pending),
        );
        assert!(events.is_empty(), "{events:?}");
    }

    #[test]
    fn first_observation_emits_from_null() {
        let events = detect_transitions(None, PrState::ReviewPending, Some(ChecksState::Pending));
        assert_eq!(
            events,
            vec![
                json!({"type": "pr_state_changed", "from": null, "to": "review_pending"}),
                json!({"type": "checks_state_changed", "from": null, "to": "pending"}),
            ]
        );
    }

    #[test]
    fn first_observation_without_checks_emits_only_state() {
        let events = detect_transitions(None, PrState::Approved, None);
        assert_eq!(
            events,
            vec![json!({"type": "pr_state_changed", "from": null, "to": "approved"})]
        );
    }

    #[test]
    fn state_change_only() {
        let previous = snapshot(PrState::ReviewPending, Some(ChecksState::Passing));
        let events = detect_transitions(
            Some(&previous),
            PrState::ChangesRequested,
            Some(ChecksState::Passing),
        );
        assert_eq!(
            events,
            vec![
                json!({"type": "pr_state_changed", "from": "review_pending", "to": "changes_requested"})
            ]
        );
    }

    #[test]
    fn checks_change_only() {
        let previous = snapshot(PrState::Approved, Some(ChecksState::Pending));
        let events = detect_transitions(
            Some(&previous),
            PrState::Approved,
            Some(ChecksState::Failing),
        );
        assert_eq!(
            events,
            vec![json!({"type": "checks_state_changed", "from": "pending", "to": "failing"})]
        );
    }

    #[test]
    fn checks_disappearing_is_a_change() {
        let previous = snapshot(PrState::Approved, Some(ChecksState::Passing));
        let events = detect_transitions(Some(&previous), PrState::Approved, None);
        assert_eq!(
            events,
            vec![json!({"type": "checks_state_changed", "from": "passing", "to": null})]
        );
    }

    #[test]
    fn both_change_in_order() {
        let previous = snapshot(PrState::Approved, Some(ChecksState::Passing));
        let events =
            detect_transitions(Some(&previous), PrState::Merged, Some(ChecksState::Failing));
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["type"], "pr_state_changed");
        assert_eq!(events[1]["type"], "checks_state_changed");
    }

    #[test]
    fn parses_pr_urls() {
        assert_eq!(
            parse_pr_url("https://github.com/vise-sh/vise/pull/42"),
            Some(("vise-sh/vise".into(), 42))
        );
        assert_eq!(
            parse_pr_url("https://github.com/vise-sh/vise/pull/42/"),
            Some(("vise-sh/vise".into(), 42))
        );
        assert_eq!(
            parse_pr_url("https://github.com/vise-sh/vise/pull/42?diff=split"),
            Some(("vise-sh/vise".into(), 42))
        );
        for bad in [
            "",
            "https://github.com/vise-sh/vise",
            "https://github.com/vise-sh/vise/issues/42",
            "https://github.com/vise-sh/vise/pull/abc",
            "https://github.com/vise-sh/vise/pull/42/files",
            "vise-sh/vise/pull/42",
        ] {
            assert_eq!(parse_pr_url(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn review_decision_parse_is_case_insensitive_and_defaults_to_comment() {
        assert_eq!(ReviewDecision::parse("APPROVED"), Approved);
        assert_eq!(ReviewDecision::parse("approved"), Approved);
        assert_eq!(ReviewDecision::parse("CHANGES_REQUESTED"), ChangesRequested);
        assert_eq!(ReviewDecision::parse("DISMISSED"), Dismissed);
        assert_eq!(ReviewDecision::parse("PENDING"), Pending);
        assert_eq!(ReviewDecision::parse("COMMENTED"), Commented);
        assert_eq!(ReviewDecision::parse("something_new"), Commented);
    }

    #[test]
    fn pr_state_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&PrState::ChangesRequested).unwrap(),
            "\"changes_requested\""
        );
        assert_eq!(
            serde_json::to_string(&ChecksState::Failing).unwrap(),
            "\"failing\""
        );
        let status: PrStatus =
            serde_json::from_str(r#"{"state":"approved","last_synced_at":"2026-09-11T12:00:00Z"}"#)
                .unwrap();
        assert_eq!(status.state, PrState::Approved);
        assert_eq!(status.checks, None);
    }
}
