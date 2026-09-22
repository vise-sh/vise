# Routine Primitive Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Build the Routine primitive — API-managed, cron-scheduled session spawns with an inline session spec — in the OSS `vise` repo, then bump the platform pin and mirror the scheduler.

**Architecture:** A new `routines` module in `vise-core` (model / repository / service / postgres), mirroring the existing `hosts` and `sessions` modules. A `RoutineScheduler` background task (modeled on `vise-api`'s `PrPoller`) runs in both `vise-server` and the platform `vise-cloud-server`, claims due routines across all workspaces with `FOR UPDATE SKIP LOCKED`, and spawns sessions in-process via `SessionService::create`, stamping each with the routine's id. Schedule math lives behind a small pure `schedule` seam (cron + IANA timezone → next UTC occurrence).

**Tech Stack:** Rust, axum 0.8, sqlx 0.9 (Postgres, `#[sqlx::test]` throwaway DBs), chrono, `croner` (cron parsing) + `chrono-tz` (IANA timezones), utoipa (OpenAPI).

**Design doc:** `platform/docs/plans/2026-09-22-routine-primitive-design.md` (this repo's sibling). Ticket: VISE-237.

---

## Conventions for every task

- **TDD:** write the failing test first, watch it fail, implement minimally, watch it pass, commit. @superpowers:test-driven-development
- **Test harness:** `#[sqlx::test]` gives each test a throwaway database with all migrations applied. Tests need `DATABASE_URL` set to a reachable Postgres. Run a single test with `cargo test -p <crate> --test <file> <name> -- --exact`.
- **sqlx offline cache — CRITICAL GOTCHA:** CI builds with `SQLX_OFFLINE=true` against `.sqlx/`. Any new/changed `sqlx::query!`/`query_as!` macro means you MUST regenerate the cache before committing: `cargo sqlx prepare --workspace` (needs `DATABASE_URL` + migrations applied). The `just check` target runs `sqlx-check` and will fail on a stale cache. Commit the `.sqlx/*.json` changes alongside the code.
- **Full check before the final commit of each task group:** `just check` (fmt, lint, test, sqlx-check, spec-check).
- **IDs:** `vise_core::id::new_id("rtn")` for routine ids (TypeID-style, uuidv7).
- **Commit style:** `feat: …` / `test: …`, small and frequent.

---

## Task 1: Schedule seam — cron + IANA timezone → next occurrence

Pure, DB-free module that isolates the cron crate behind one function. This is the testable seam; everything else treats scheduling as a black box.

**Files:**
- Modify: `Cargo.toml` (root workspace `[workspace.dependencies]`) — add `croner` and `chrono-tz`.
- Modify: `crates/vise-core/Cargo.toml` — add `croner`, `chrono-tz`.
- Create: `crates/vise-core/src/routines/mod.rs` (declares `pub mod schedule;` and, later, the other submodules)
- Create: `crates/vise-core/src/routines/schedule.rs`
- Modify: `crates/vise-core/src/lib.rs` — add `pub mod routines;`

**Step 1: Add dependencies.** In root `Cargo.toml` `[workspace.dependencies]`:
```toml
croner = "2"
chrono-tz = { version = "0.10", features = ["serde"] }
```
In `crates/vise-core/Cargo.toml` `[dependencies]`:
```toml
croner = { workspace = true }
chrono-tz = { workspace = true }
```
Run `cargo build -p vise-core` to resolve versions. If `croner` 2.x API differs from Step 3, adjust the wrapper — the seam exists precisely to contain this.

**Step 2: Write the failing test.** Create `crates/vise-core/src/routines/schedule.rs` with an inline `#[cfg(test)] mod tests`:
```rust
use chrono::{TimeZone, Utc};

#[test]
fn next_after_computes_daily_utc() {
    // "every day at 02:00 America/New_York". 2026-01-10 is EST (UTC-5),
    // so 02:00 local == 07:00 UTC.
    let after = Utc.with_ymd_and_hms(2026, 1, 10, 0, 0, 0).unwrap();
    let next = super::next_after("0 2 * * *", "America/New_York", after).unwrap();
    assert_eq!(next, Utc.with_ymd_and_hms(2026, 1, 10, 7, 0, 0).unwrap());
}

#[test]
fn next_after_is_strictly_after() {
    // If `after` is exactly on an occurrence, return the NEXT one, not `after`.
    let after = Utc.with_ymd_and_hms(2026, 1, 10, 7, 0, 0).unwrap();
    let next = super::next_after("0 2 * * *", "America/New_York", after).unwrap();
    assert_eq!(next, Utc.with_ymd_and_hms(2026, 1, 11, 7, 0, 0).unwrap());
}

#[test]
fn rejects_bad_cron() {
    assert!(super::next_after("not a cron", "UTC", Utc::now()).is_err());
}

#[test]
fn rejects_bad_timezone() {
    assert!(super::next_after("0 2 * * *", "Mars/Phobos", Utc::now()).is_err());
}
```

**Step 3: Run test to verify it fails.** `cargo test -p vise-core schedule::` → FAIL (function missing).

**Step 4: Minimal implementation** (above the tests):
```rust
//! The one place that understands cron syntax and timezones. Everything else
//! treats a schedule as opaque and asks only "what is the next run after t?".

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use croner::Cron;

/// The next occurrence of `cron` strictly after `after`, in UTC.
///
/// `cron` is standard 5-field (min hour dom month dow). `timezone` is an IANA
/// name (e.g. "America/New_York") so DST is handled at the wall-clock the user
/// wrote. Errors on a malformed cron or unknown timezone — callers validate at
/// create time so this never fires at run time.
pub fn next_after(
    cron: &str,
    timezone: &str,
    after: DateTime<Utc>,
) -> anyhow::Result<DateTime<Utc>> {
    let tz: Tz = timezone
        .parse()
        .map_err(|_| anyhow::anyhow!("unknown timezone {timezone:?}"))?;
    let parsed = Cron::new(cron)
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid cron {cron:?}: {e}"))?;
    let local_after = after.with_timezone(&tz);
    let next = parsed
        .find_next_occurrence(&local_after, false)
        .map_err(|e| anyhow::anyhow!("no next occurrence for {cron:?}: {e}"))?;
    Ok(next.with_timezone(&Utc))
}
```
> Verify `croner` 2.x: `Cron::new(pat).parse()` and `find_next_occurrence(&DateTime<Tz>, inclusive: bool)`. If the API differs, adapt here only. `find_next_occurrence(.., false)` = strictly after.

**Step 5: A 15-minute-floor validator (same file), test-first.**
```rust
#[test]
fn rejects_sub_floor_interval() {
    assert!(super::validate_min_interval("* * * * *").is_err());     // every minute
    assert!(super::validate_min_interval("*/5 * * * *").is_err());   // every 5 min
    assert!(super::validate_min_interval("*/15 * * * *").is_ok());   // every 15 min
    assert!(super::validate_min_interval("0 2 * * *").is_ok());      // daily
}
```
Implement by sampling: compute the first N (e.g. 5) occurrences after a fixed epoch in UTC and assert the smallest gap ≥ 15 min. This is crate-agnostic and robust to step syntax:
```rust
/// Reject schedules that fire more often than every 15 minutes. Sampling the
/// first few occurrences avoids parsing step syntax ourselves.
pub fn validate_min_interval(cron: &str) -> anyhow::Result<()> {
    const FLOOR_SECS: i64 = 15 * 60;
    let mut t = chrono::Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let mut prev = t;
    for i in 0..6 {
        t = next_after(cron, "UTC", t)?;
        if i > 0 && (t - prev).num_seconds() < FLOOR_SECS {
            anyhow::bail!("schedule fires more often than every 15 minutes");
        }
        prev = t;
    }
    Ok(())
}
```

**Step 6: Run, then commit.**
```bash
cargo test -p vise-core routines::schedule
git add Cargo.toml Cargo.lock crates/vise-core/Cargo.toml crates/vise-core/src/lib.rs crates/vise-core/src/routines/
git commit -m "feat: routine schedule seam (cron + IANA tz next-occurrence, 15-min floor)"
```

---

## Task 2: Migration — `routines` table + `sessions.routine_id`

**Files:**
- Create: `crates/vise-core/migrations/0004_routines.sql`
- Test: `crates/vise-core/tests/migrations.rs` already asserts migrations apply cleanly; it will exercise this one.

**Step 1: Write the migration.**
```sql
-- Routines: API-managed scheduled session spawns (VISE-237). A routine holds
-- a cron schedule and an inline session spec; a background scheduler spawns a
-- session per due routine, in the routine's own workspace.
CREATE TABLE routines (
    id            TEXT PRIMARY KEY,
    workspace_id  TEXT NOT NULL REFERENCES workspaces(id),
    name          TEXT NOT NULL,
    cron          TEXT NOT NULL,
    timezone      TEXT NOT NULL,
    spec          JSONB NOT NULL,            -- { agent, environment, input }
    enabled       BOOLEAN NOT NULL DEFAULT true,
    next_run_at   TIMESTAMPTZ NOT NULL,
    last_fired_at TIMESTAMPTZ,
    last_session_id TEXT,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- The scheduler's claim query: enabled routines that are due, oldest first.
CREATE INDEX routines_due_idx ON routines (next_run_at) WHERE enabled;

-- Routines are listed per workspace.
CREATE INDEX routines_workspace_idx ON routines (workspace_id, created_at DESC);

-- Link a spawned session back to its routine. NULL for hand-created sessions.
ALTER TABLE sessions ADD COLUMN routine_id TEXT;
CREATE INDEX sessions_routine_idx ON sessions (routine_id, created_at DESC)
    WHERE routine_id IS NOT NULL;
```

**Step 2: Apply + regenerate offline cache.**
```bash
sqlx migrate run --source crates/vise-core/migrations   # or: just db-migrate
```

**Step 3: Run the migration test.** `cargo test -p vise-core --test migrations` → PASS.

**Step 4: Commit.**
```bash
git add crates/vise-core/migrations/0004_routines.sql
git commit -m "feat: 0004 routines table + sessions.routine_id"
```

---

## Task 3: Thread `routine_id` through the Session model + repository

The session spawn must stamp `routine_id`, and reads must surface it. Do this in `sessions` before the routine service needs it.

**Files:**
- Modify: `crates/vise-core/src/sessions/model.rs` (add `routine_id: Option<String>` to `Session`)
- Modify: `crates/vise-core/src/sessions/service.rs` (`create` gains a `routine_id` param)
- Modify: `crates/vise-core/src/sessions/repository.rs` (new method `list_by_routine`; `create` already takes a `Session`)
- Modify: `crates/vise-core/src/sessions/postgres.rs` (INSERT + all `SessionRow`/SELECTs include `routine_id`)
- Test: `crates/vise-core/tests/sessions.rs` (new) or extend an existing session test.

**Step 1: Failing test** (new `crates/vise-core/tests/routine_sessions.rs`):
```rust
// A session created with a routine_id is retrievable and lists under that routine.
#[sqlx::test]
async fn session_carries_routine_id(pool: sqlx::PgPool) {
    // create workspace + a session with Some("rtn_x"); assert get() returns it,
    // and list_by_routine("rtn_x") includes it while list_by_routine("rtn_y") does not.
}
```
(Model the setup helpers on `crates/vise-core/tests/enrollment.rs`.)

**Step 2–4:** Add the field (defaulting to `None` everywhere it's constructed — grep for `Session {` and `outcome: None` sites, including `events.rs` echo and any test builders), extend `create`’s signature with `routine_id: Option<String>` (update the sole existing caller in `routes/sessions.rs::create_session` to pass `None`), add `list_by_routine(&self, workspace, routine_id) -> Vec<Session>` to the trait + Postgres impl, and add `routine_id` to every `SessionRow` SELECT/INSERT in `postgres.rs`. Run `cargo sqlx prepare --workspace`.

**Step 5: Commit.**
```bash
git add crates/vise-core/src/sessions crates/vise-core/tests/routine_sessions.rs .sqlx
git commit -m "feat: thread routine_id through Session model, service, repository"
```

---

## Task 4: Routine domain model

**Files:**
- Create: `crates/vise-core/src/routines/model.rs`
- Modify: `crates/vise-core/src/routines/mod.rs` — `pub mod model;`

**Step 1: Define the types** (no test needed for plain structs; they're exercised by later tasks):
```rust
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::sessions::model::{Agent, Environment};
use crate::workspaces::model::WorkspaceId;

/// The inline session spec a routine instantiates. Mirrors
/// `CreateSessionRequest` exactly so sibling primitives (deliverable policy,
/// …) flow in for free when they land there.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SessionSpec {
    pub agent: Agent,
    pub environment: Environment,
    pub input: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Routine {
    pub id: String,
    pub workspace_id: WorkspaceId,
    pub name: String,
    pub cron: String,
    pub timezone: String,
    pub spec: SessionSpec,
    pub enabled: bool,
    pub next_run_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fired_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_session_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
```

**Step 2: Commit.** `git commit -m "feat: routine domain model + inline SessionSpec"`

---

## Task 5: Routine repository trait + Postgres implementation

**Files:**
- Create: `crates/vise-core/src/routines/repository.rs`
- Create: `crates/vise-core/src/routines/postgres.rs`
- Modify: `crates/vise-core/src/routines/mod.rs`
- Test: `crates/vise-core/tests/routines.rs`

**Trait surface:**
```rust
#[async_trait]
pub trait RoutineRepository: Send + Sync {
    async fn create(&self, routine: Routine) -> anyhow::Result<Routine>;
    async fn get(&self, ws: &WorkspaceId, id: &str) -> anyhow::Result<Option<Routine>>;
    async fn list(&self, ws: &WorkspaceId) -> anyhow::Result<Vec<Routine>>;
    async fn update(&self, routine: Routine) -> anyhow::Result<Option<Routine>>; // by (ws,id)
    async fn delete(&self, ws: &WorkspaceId, id: &str) -> anyhow::Result<()>;

    /// Cross-workspace: claim up to `limit` enabled, due routines with
    /// `FOR UPDATE SKIP LOCKED` so replicas never double-fire. Returns them
    /// inside a held transaction the caller commits via `advance_next_run`.
    /// Simplest correct v1: one method that returns due routines and a second
    /// that advances them; see note below on transaction boundary.
    async fn claim_due(&self, now: DateTime<Utc>, limit: i64) -> anyhow::Result<Vec<Routine>>;

    /// Set next_run_at (+ last_fired_at, last_session_id) after a fire/skip.
    async fn record_fire(
        &self,
        id: &str,
        next_run_at: DateTime<Utc>,
        last_fired_at: Option<DateTime<Utc>>,
        last_session_id: Option<String>,
    ) -> anyhow::Result<()>;
}
```

**Transaction-boundary note (design decision to encode):** the clean single-statement approach is to make `claim_due` itself the mutex: `UPDATE routines SET next_run_at = <far-future sentinel or leave> ... RETURNING` won't work cleanly because we don't yet know the next occurrence. Instead, claim inside a transaction with `SELECT ... FOR UPDATE SKIP LOCKED`, and have the scheduler compute the spawn + next_run_at, then `record_fire` in the same logical pass. For v1 simplicity and because the poll interval (60s) ≫ spawn latency, implement `claim_due` as a plain `SELECT ... WHERE enabled AND next_run_at <= now() ORDER BY next_run_at FOR UPDATE SKIP LOCKED` in a short transaction that the repository holds only long enough to also stamp a **lease** (`next_run_at = now() + interval '1 minute'`) so a second replica skips it; the scheduler then does the real `record_fire`. **Encode the chosen mechanism explicitly in code comments.** (Recommended: claim = advance `next_run_at` provisionally by the poll interval under `SKIP LOCKED`, then `record_fire` overwrites it with the true next occurrence. A crash between the two just means the routine re-fires after the provisional window — acceptable, and the overlap-skip guard prevents a duplicate concurrent run.)

**Tests (`crates/vise-core/tests/routines.rs`, `#[sqlx::test]`):**
1. `create_then_get_roundtrips` — spec JSON survives.
2. `list_scoped_to_workspace` — a routine in ws A is invisible to ws B.
3. `claim_due_returns_only_enabled_and_due` — disabled or future routines are not claimed.
4. `claim_due_skips_locked` — two concurrent `claim_due` calls never return the same routine (spawn two transactions).
5. `record_fire_advances_next_run` — next_run_at, last_fired_at, last_session_id updated.

Regenerate `.sqlx` after writing the queries. Commit.

---

## Task 6: RoutineService — validation, CRUD, fire logic, run-now

**Files:**
- Create: `crates/vise-core/src/routines/service.rs`
- Modify: `crates/vise-core/src/routines/mod.rs`
- Test: extend `crates/vise-core/tests/routines.rs`

**Surface:**
```rust
pub struct RoutineService<R, S> { routines: R, sessions: Arc<SessionService<S>> }

impl RoutineService {
    /// Validate cron (parse + 15-min floor), timezone, and spec.environment,
    /// compute the first next_run_at, then persist.
    pub async fn create(&self, ws, name, cron, timezone, spec) -> Result<Routine>;
    pub async fn get / list / update / delete ...;

    /// Manual "run now": spawn immediately, stamped with routine_id, BYPASSING
    /// the overlap guard, WITHOUT advancing next_run_at. Returns the Session.
    pub async fn run_now(&self, ws, id) -> Result<Option<Session>>;

    /// One scheduler pass: claim due routines, and for each apply the fire
    /// decision (overlap-skip vs spawn), then record_fire (always advance).
    pub async fn tick(&self, now: DateTime<Utc>) -> Result<TickReport>;
}
```

**Fire decision inside `tick` (per design):**
- Compute `next = schedule::next_after(cron, timezone, now)`.
- **Overlap:** `sessions.list_by_routine(ws, id)` — if any is `Pending`/`Running`, skip spawning (log `skipped_overlap`); still `record_fire(next, last_fired_at=unchanged, last_session_id=unchanged)`.
- **Else spawn:** `sessions.create(ws, spec.agent, spec.environment, spec.input, routine_id=Some(id))`; `record_fire(next, Some(now), Some(session.id))`.

**Validation test:** `create` rejects a sub-15-min cron, an unknown timezone, and a `github_repo` spec missing `repo` (reuse `Environment::validate`).

**Fire tests (`#[sqlx::test]`):**
1. `tick_spawns_due_session_with_routine_id` — due routine → one session stamped with the routine id; `next_run_at` advanced; `last_session_id` set.
2. `tick_skips_when_prior_run_active` — seed a Running session for the routine; tick spawns nothing but still advances `next_run_at`.
3. `tick_ignores_future_and_disabled` — no spawn.
4. `run_now_spawns_regardless_of_overlap_and_leaves_next_run_at` — seed a Running session; `run_now` still spawns; `next_run_at` unchanged.

Regenerate `.sqlx`, `just check`, commit.

---

## Task 7: HTTP API — `routes/routines.rs`, AppState, router, OpenAPI

**Files:**
- Create: `crates/vise-api/src/routes/routines.rs`
- Modify: `crates/vise-api/src/routes/mod.rs` (`pub mod routines; pub use routines::routes as routines;`)
- Modify: `crates/vise-api/src/lib.rs` (`.merge(routes::routines())`)
- Modify: `crates/vise-api/src/state.rs` (add `pub routines: Arc<RoutineService<PostgresRoutineRepository, PostgresSessionRepository>>`)
- Modify: `crates/vise-api/src/openapi.rs` (register the new paths + `Routine`, `SessionSpec`, request/response schemas)
- Modify: `bins/vise-server/src/main.rs` (construct `RoutineService`, add to `AppState`)
- Test: `crates/vise-api/tests/routines.rs` (HTTP-level, model on `tests/claim.rs` / `tests/common/mod.rs`)

**Routes** (all behind `AuthedCaller`, workspace-scoped exactly like `/sessions`):
```
POST   /routines              create   (422 on bad cron/tz/spec)
GET    /routines              list
GET    /routines/{id}         get      (404)
PATCH  /routines/{id}         update   (recompute next_run_at if cron|tz changed; 404)
DELETE /routines/{id}         delete
POST   /routines/{id}/run     run_now  (201 Session; 404)
GET    /routines/{id}/runs    list_runs (sessions.list_by_routine)
```

**Request bodies:** `CreateRoutineRequest { name, schedule: { cron, timezone }, spec }`, `UpdateRoutineRequest { name?, schedule?, spec?, enabled? }` (all optional, PATCH semantics). Responses: `Routine`, `ListRoutinesResponse { routines }`, `ListRunsResponse { sessions }`.

**Tests (HTTP):**
1. `create_returns_routine_with_next_run_at`.
2. `create_rejects_bad_cron` → 422.
3. `list_and_get_scoped_to_caller_workspace`.
4. `patch_disable_then_enable`.
5. `run_now_returns_session` → 201 with `routine_id` set; appears in `/routines/{id}/runs`.
6. `delete_then_get` → 404.

**spec-check gotcha:** `just spec-check` diffs the committed OpenAPI JSON against the code. Regenerate it (the repo's generation command — check the `justfile` `spec` recipe) and commit the updated spec file. Regenerate `.sqlx`. `just check`. Commit.

---

## Task 8: RoutineScheduler background task + wire into `vise-server`

**Files:**
- Create: `crates/vise-api/src/routine_scheduler.rs` (modeled on `pr_tracking.rs`'s `PrPoller`)
- Modify: `crates/vise-api/src/lib.rs` (`pub mod routine_scheduler;`)
- Modify: `bins/vise-server/src/main.rs` (spawn it, like the lease sweeper)
- Test: `crates/vise-api/tests/routine_scheduler.rs` (drive one `tick` end-to-end through the service; the loop itself is a thin `tokio::interval` wrapper and needs no test)

**Scheduler:**
```rust
pub struct RoutineScheduler<R, S> {
    routines: Arc<RoutineService<R, S>>,
    interval: Duration,
}
impl RoutineScheduler {
    pub fn new(routines, interval) -> Self { ... }
    pub async fn run_forever(self) {
        let mut tick = tokio::time::interval(self.interval);
        loop {
            tick.tick().await;
            match self.routines.tick(chrono::Utc::now()).await {
                Ok(report) if report.spawned > 0 || report.skipped > 0 =>
                    tracing::info!(report.spawned, report.skipped, "routine tick"),
                Ok(_) => {}
                Err(error) => tracing::error!(%error, "routine scheduler failed"),
            }
        }
    }
}
```

**Wire into `bins/vise-server/src/main.rs`** (next to the lease sweeper, ~line 47):
```rust
// Routine scheduler: spawns a session per due routine, in the routine's
// own workspace. FOR UPDATE SKIP LOCKED makes running >1 server safe.
{
    let interval_secs: u64 = std::env::var("VISE_ROUTINE_POLL_INTERVAL_SECS")
        .ok().and_then(|v| v.parse().ok()).unwrap_or(60);
    let scheduler = vise_api::routine_scheduler::RoutineScheduler::new(
        routines.clone(),
        std::time::Duration::from_secs(interval_secs.max(1)),
    );
    tokio::spawn(scheduler.run_forever());
}
```
(Construct `routines` alongside `sessions` earlier in `main`.)

**Test:** `#[sqlx::test]` — build the full service stack, insert a due routine, call `scheduler`'s underlying `routines.tick(now)` (or a single manual `run_forever` iteration), assert a session was spawned. Run `just check`, commit.

---

## Task 9 (platform repo, separate worktree): pin bump + mirror scheduler

Switch to the platform worktree: `/Users/ericpsimon/workspace/vise-sh/platform` (branch `ericpsimon/vise-237-routine-primitive`). This task only runs after Tasks 1–8 are merged/pushed on the OSS side and you have a commit SHA to pin.

**Files:**
- Modify: `Cargo.toml` — bump `vise-api` / `vise-core` `rev` to the new OSS commit (the test that enforces equal revs will guard this).
- Modify: `bins/vise-cloud-server/src/main.rs` — add the RoutineScheduler spawn, mirroring `vise-server/src/main.rs` line-for-line (the file is deliberately kept close to upstream). Construct `routines` in the `AppState` build, and add the same `tokio::spawn` block.
- Modify: wherever `AppState { … }` is constructed in the platform (`build_app` call site) — add the `routines` field.

**Steps:**
1. Push the OSS branch, note the SHA.
2. Bump the pin; `cargo update -p vise-core -p vise-api`.
3. Add the `routines` service construction + scheduler spawn to `vise-cloud-server/src/main.rs`.
4. `cargo test -p vise-cloud-server` (the rev-match test + a smoke build).
5. Commit: `feat: adopt routine primitive (bump OSS pin, run routine scheduler)`.

---

## Definition of done

- All OSS tests green under `just check` (fmt, lint, test, sqlx-check, spec-check).
- Design-doc acceptance sketch items 1–6 covered by tests.
- Platform builds against the new pin and spawns the scheduler.
- **Known limitation intact:** no runaway guardrail (Budget descoped) — documented, not silently closed.
```
