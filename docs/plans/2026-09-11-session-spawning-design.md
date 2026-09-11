# Session Spawning — Design & Implementation Guide

**Goal:** make `POST /sessions` actually run a coding agent, instead of just inserting a `Pending` row.

## Decisions (settled in design discussion)

| Question | Decision |
|---|---|
| Execution model | K8s-style: sessions are declarative objects; multiple host types can execute them |
| Dispatch | **Pull-based** — hosts watch/poll vise-server, claim work, report back. No inbound network to hosts |
| Session lifecycle | **One-shot job first** (like a k8s Job): one prompt, run to completion. Interactive turns layer on later |
| Runtime contract | **ACP (Agent Client Protocol)** — the host is an ACP *client* that spawns any ACP-capable agent (Claude Code via `claude-code-acp`, Gemini CLI, etc.) over stdio |
| Agent definition | Inline in the create request (OpenAI agents/sessions style), not a separate registry entity yet |

The k8s mapping, for intuition:

- Session = Pod (spec + status, stored in Postgres)
- ACP = CRI (one runtime contract, many harnesses)
- Host = Node/kubelet (pulls work, reconciles, reports)
- vise-server = API server (authoritative store, no execution)

---

## 1. Object model & API

### Session spec

The create request carries everything needed to run. Stored verbatim as an immutable `jsonb` `spec` column.

```jsonc
POST /sessions
{
  "agent": {
    "harness": "claude-code",        // resolved by the host to an ACP agent command
    "model": "claude-fable-5",
    "instructions": "...",           // system-prompt-ish guidance
    "mcp_servers": []                // passed through to ACP session/new
  },
  "environment": {
    "type": "self_hosted",           // later: "docker", "cloud"
    "workspace": { "git_url": "...", "ref": "main" },   // or { "path": "/abs/path" }
    "selector": {}                   // optional host targeting, nodeSelector-style
  },
  "input": "Fix the failing tests in crates/vise-core"
}
```

### Session status

Keep the existing enum; add attribution fields:

```
Pending → Running → Completed | Failed | Cancelled
```

New columns on `sessions`: `spec jsonb`, `host_id text null`, `lease_expires_at timestamptz null`,
`started_at`, `finished_at`, `stop_reason text null`, `error text null`.

`agent_id` on the session goes away (or becomes optional sugar later when an agent registry exists).

### Events

Append-only table — stores ACP `session/update` notifications **verbatim**. ACP does the event modeling; vise doesn't invent its own event schema.

```sql
CREATE TABLE session_events (
  session_id text NOT NULL REFERENCES sessions(id),
  seq        bigint NOT NULL,          -- per-session, monotonically increasing
  payload    jsonb NOT NULL,           -- raw ACP SessionNotification
  created_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (session_id, seq)
);
```

### Public API surface

| Endpoint | Purpose |
|---|---|
| `POST /sessions` | Create (existing, new body shape) |
| `GET /sessions`, `GET /sessions/{id}` | Existing |
| `GET /sessions/{id}/events?after_seq=N` | Paginated event history |
| `GET /sessions/{id}/events/stream` | SSE live tail (history from `after_seq`, then live) |
| `POST /sessions/{id}/cancel` | Request cancellation (sets a flag; host acts on it) |

---

## 2. Control plane — host pull protocol

Hosts are untrusted-network friendly: all connections are outbound from host to server. The protocol is four endpoints under `/hosts`:

### Claim

`POST /hosts/{host_id}/claim` — body: host capabilities (`harnesses: ["claude-code"]`, `environment_types: ["self_hosted"]`, labels).

Server-side, the claim is the critical concurrency point. Use Postgres row locking so two hosts never claim the same session:

```sql
UPDATE sessions
SET status = 'running', host_id = $1,
    lease_expires_at = now() + interval '60 seconds',
    started_at = now(), updated_at = now()
WHERE id = (
  SELECT id FROM sessions
  WHERE status = 'pending'
    -- AND selector matches host labels / capabilities
  ORDER BY created_at
  LIMIT 1
  FOR UPDATE SKIP LOCKED
)
RETURNING *;
```

Returns `200` with the full session (spec included) or `204` if no work. Host polls this on an interval (2–5s is fine to start; long-polling is a later optimization).

### Heartbeat

`POST /hosts/{host_id}/sessions/{id}/heartbeat` — extends `lease_expires_at`. A server-side sweeper (tokio interval task) marks sessions `failed` (`error = "lease expired"`) when the lease lapses — this is how host crashes are detected. The heartbeat response carries `{ "cancel_requested": bool }` so cancellation rides the existing poll, no push channel needed.

### Report events

`POST /hosts/{host_id}/sessions/{id}/events` — body: batch of `{ seq, payload }`. Host assigns `seq` locally (it's the only writer for the session), batches every ~500ms. Insert with `ON CONFLICT DO NOTHING` so retries are idempotent.

### Finish

`POST /hosts/{host_id}/sessions/{id}/finish` — body: `{ "status": "completed" | "failed" | "cancelled", "stop_reason": ..., "error": ... }`. Server validates the reporting host still holds the session.

**Trust model note:** for now `host_id` is self-asserted. Real host auth (registration + tokens) is deliberately out of scope for v1; add before any multi-tenant use.

---

## 3. Host runtime — ACP

New binary: `bins/vise-host`. Its job: poll → claim → provision workspace → run one ACP prompt turn → stream events up → finish.

### Runtime trait (the "CRI" seam)

```rust
#[async_trait]
pub trait SessionRuntime {
    async fn run(
        &self,
        spec: &SessionSpec,
        workspace: &Path,
        events: mpsc::Sender<serde_json::Value>,  // raw ACP notifications
        cancel: CancellationToken,
    ) -> anyhow::Result<RunOutcome>;              // stop_reason or error
}
```

First implementation: `AcpProcessRuntime` (self_hosted). Later: `DockerRuntime` wraps the same ACP flow in a container. The host binary is generic over this trait the same way `SessionService` is generic over `SessionRepository`.

### The ACP flow (one-shot turn)

Use the official `agent-client-protocol` Rust crate (Zed maintains it) rather than hand-rolling JSON-RPC.

1. **Resolve harness → command.** Host config maps `"claude-code"` → `npx claude-code-acp` (Zed's adapter), `"gemini"` → `gemini --experimental-acp`. This map lives in host config, not the server — hosts advertise what they support in their claim body.
2. **Spawn** the agent as a child process, stdio piped. Working directory = the provisioned workspace.
3. **`initialize`** — negotiate protocol version and capabilities.
4. **`session/new`** — pass `cwd` and `mcp_servers` from the spec.
5. **`session/prompt`** — send `input` (prepend `instructions` as context, or use the harness's native mechanism where the adapter supports it).
6. **Stream:** every `session/update` notification (agent message chunks, tool calls, tool call updates, plan updates, thought chunks) goes onto the events channel verbatim. A separate task batches and POSTs them.
7. **Permissions:** the agent will send `session/request_permission` for tool use. V1 policy: **auto-approve everything**, and record the request + auto-response as events so there's an audit trail. This is the honest v1 posture for a one-shot job runner; per-spec permission policy is future work.
8. **Turn ends** when `session/prompt` returns a `stopReason` → map to `completed`. Agent process exit with error / protocol failure → `failed`.
9. **Cancellation:** on `cancel_requested`, send ACP `session/cancel`, give the agent a grace period (~10s), then kill the child process. Report `cancelled`.

### Workspace provisioning

- `{ "path": ... }` — use the host-local directory as-is (dev convenience).
- `{ "git_url": ..., "ref": ... }` — clone into a per-session temp dir (`$VISE_DATA/sessions/{id}/workspace`), shallow clone is fine. Keep the directory after finish (debuggability); a GC policy is future work.

---

## 4. Lifecycle & failure handling

Single source of truth for transitions (enforce in `SessionService`, not scattered across handlers):

| From | To | Trigger |
|---|---|---|
| Pending | Running | host claim |
| Pending | Cancelled | cancel before claim |
| Running | Completed | host finish (stopReason) |
| Running | Failed | host finish (error) **or** lease expiry sweeper |
| Running | Cancelled | cancel → host acks via finish |

Failure cases to handle explicitly:

- **Host dies mid-run** → lease expires → sweeper marks `Failed`. (No retry in v1 — sessions have side effects on workspaces; blind re-run is wrong. Retry policy is future work.)
- **Server unreachable from host** → host keeps buffering events, retries with backoff; if the lease has expired by the time it reconnects, the finish call is rejected and the host discards (session already `Failed`).
- **Agent process crashes** → host reports `Failed` with stderr tail in `error`.
- **Double claim** → impossible by construction (`FOR UPDATE SKIP LOCKED`).

---

## Implementation order

Each milestone is independently testable and leaves the system working. Suggested commits map 1:1.

**M1 — Status transitions & events plumbing (no execution).**
Migrations: new `sessions` columns + `session_events` table. Extend `SessionRepository` + postgres impl: `claim_pending`, `heartbeat`, `finish`, `append_events`, `list_events`. Unit-test the transition table against a real Postgres (sqlx test).

**M2 — New create API.**
`CreateSessionRequest` becomes the spec shape (§1). Store as `spec jsonb`. Update openapi + `vise-client`.

**M3 — Host protocol endpoints.**
`/hosts/...` claim / heartbeat / events / finish routes (§2), plus the lease-expiry sweeper task in `vise-server`. Test with curl: create a session, claim it by hand, post fake events, finish it.

**M4 — `vise-host` binary with a fake runtime.**
Poll-claim loop, workspace provisioning, heartbeating, event batching — but the runtime is an `EchoRuntime` that emits a couple of fake events and completes. This proves the whole control loop end-to-end before ACP enters the picture.

**M5 — `AcpProcessRuntime`.**
Swap in the real ACP client (§3) behind the `SessionRuntime` trait. Start by running `claude-code-acp` against a scratch repo with a trivial prompt ("create hello.txt"). Auto-approve permissions, record them as events.

**M6 — Read-side polish.**
`GET /sessions/{id}/events` pagination + SSE tail. `POST /sessions/{id}/cancel` wired through heartbeat → ACP `session/cancel`.

**M7 — CLI.**
`vise session create / list / watch` in `vise-cli` — `watch` tails the SSE stream and renders agent message chunks. This is your daily driver for exercising everything above.

## Deliberately out of scope (v1)

- Host registration/auth (host_id is self-asserted)
- Retry policy for failed sessions
- Docker / cloud environment types (the `SessionRuntime` trait and `environment.type` field are the seams)
- Interactive multi-turn sessions (ACP already supports it — needs an input inbox + `AwaitingInput` status)
- Agent registry (named, reusable agent configs referenced by `agent_id`)
- Permission policies beyond auto-approve
- Scheduler smarter than FIFO + capability match

## References

- ACP spec & docs: https://agentclientprotocol.com
- `agent-client-protocol` Rust crate (official ACP client/agent library)
- `claude-code-acp` — Zed's ACP adapter for Claude Code
