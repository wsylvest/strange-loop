# strange-loop — System Specification

**Version:** 0.2 (draft)
**Companion docs:** [`TREATISE.md`](TREATISE.md), [`PRD.md`](PRD.md), [`ANALYSIS.md`](ANALYSIS.md), [`STACK_DECISION.md`](STACK_DECISION.md), [`ROADMAP.md`](ROADMAP.md)

This document is the source of truth for how strange-loop is built. It defines components, data models, algorithms, tool surface, event flow, and failure modes. It is stack-neutral where possible; stack-specific notes are called out inline.

Read the [treatise](TREATISE.md) first if you have not. This document assumes the reader understands *why* each layer exists; it focuses on *how* each layer works.

---

## 1. System overview

```
                              ┌─────────────────────────────┐
                              │        Adapters             │
                              │  CLI • Telegram • Slack     │
                              │       (transport)           │
                              └──────────────┬──────────────┘
                                             │ OwnerMessage / AgentMessage
                                             ▼
┌───────────────────────────────────────────────────────────────────────┐
│                     Core Runtime (one process)                        │
│                                                                       │
│  ┌────────────┐   ┌────────────┐   ┌──────────────┐   ┌────────────┐  │
│  │ Scheduler  │──▶│ TaskRunner │──▶│  Tool Loop   │──▶│   LLM      │  │
│  │ (async)    │   │ (per-task, │   │  (rounds)    │   │  Client    │  │
│  │            │   │  async)    │   │              │   │            │  │
│  └─────┬──────┘   └─────┬──────┘   └──────┬───────┘   └─────┬──────┘  │
│        │                │                  │                  │       │
│        │                │                  ▼                  │       │
│        │                │          ┌──────────────┐            │       │
│        │                │          │   Tool       │            │       │
│        │                │          │  Dispatcher  │            │       │
│        │                │          └──┬────┬──┬───┘            │       │
│        │                │             │    │  │                │       │
│        │                │             ▼    ▼  ▼                │       │
│        │                │         InProc Edge Cell             │       │
│        │                │         (async (worker- (Docker/     │       │
│        │                │          task)   d)    Firecracker)  │       │
│        ▼                ▼                                       ▼       │
│  ┌──────────────────────────────────────────────────────────────────┐ │
│  │                    Event Bus (in-process)                        │ │
│  └──────────────────────────┬───────────────────────────────────────┘ │
│                             │                                         │
│                             ▼                                         │
│  ┌─────────────────┐  ┌──────────────┐  ┌──────────────────────────┐  │
│  │  SQLite store   │  │ Budget ledger│  │ Background Consciousness │  │
│  │ (events, tasks, │  │  (derived    │  │       (paused/wake)      │  │
│  │  messages, kv)  │  │  from events)│  └──────────────────────────┘  │
│  └─────────────────┘  └──────────────┘                                │
└───────────────────────────────────────────────────────────────────────┘
                                  │
                                  ▼
              ┌─────────────────────────────────────┐
              │     Filesystem (git-tracked)        │
              │  CHARTER.md • CREED.md • DOCTRINE   │
              │    journal/*.md • scratch.md        │
              │         src/*  VERSION              │
              └─────────────────────────────────────┘
```

### Key properties

- **Single long-running parent process.** One Tokio reactor (or Node event loop), shared in-process state, shared SQLite handle, shared LLM client, shared event bus. Concurrency inside the parent is async tasks on one runtime, not OS processes.
- **Tool execution is tiered by isolation class.** InProc tools run in the parent. Edge tools run in a workerd subprocess with declared capabilities. Cell tools run in a Docker-or-Firecracker microVM with its own filesystem and optional network. The tool's schema declares which tier it requires; the dispatcher picks the host.
- **Event bus as spine.** Every significant action becomes an event. The SQLite write is the commit. Consumers (budget ledger, observability, identity safeguards) read from the event stream.
- **Adapters are peripheral.** The core doesn't know what Telegram is. An adapter is a module that bridges a transport to the owner-message channel.

---

## 2. Governance layering

The agent is governed by six layers of rule, each in a different substrate, each with a different mutability rule. The tiers are enforced at the layer immediately below them — that is, rules in a lower tier cannot override rules in a higher tier, because the lower tier runs inside the world the higher tier defines. The metaphor is deliberately neurological: see the treatise, §V.

| Layer | Substrate | Mutable by | Loaded in prompt | Lifetime | Role |
|---|---|---|---|---|---|
| **KERNEL** | Source code, compiled into the binary | Humans editing source, rebuilding | Never | Per-release | Hard-enforced rules the LLM cannot reach: protected paths, branch allowlists, budget cap, proc allowlist, rate limits, isolation classes. |
| **CHARTER** | A single markdown file (`prompts/CHARTER.md`), hash-pinned in `kv` store | Humans editing the file and running `strange-loop charter approve` to acknowledge the new hash | Every prompt, cached (1h TTL) | Stable across many releases | Who the agent is and why. Immutable-at-runtime: any hash drift halts boot. |
| **CREED** | A single markdown file (`prompts/CREED.md`), versioned in git | Humans directly; **agent via a proposal tool** that writes to `creed_proposals` table and requires owner ACK before landing | Every prompt, cached | Revised occasionally | Mutable principles, values, commitments. |
| **DOCTRINE** | A TOML file (`prompts/doctrine.toml`) plus derived markdown rendering, hot-reloadable | Humans directly; agent cannot edit | Every prompt, cached | Revised per-deployment | Deployment-specific rules: branch names, test commands, rate numbers, tool allowlists. |
| **JOURNAL** | `journal` SQLite table mirrored to `journal/*.md` for git diffs | Agent via `journal_append` tool; **append-only, enforced at storage layer** | Tail (last N entries) every prompt | Monotonically growing | The agent's autobiographical record: what it has decided, learned, become. |
| **SCRATCH** | `prompts/scratch.md` | Agent freely via `scratch_update` tool | Every prompt, uncached | Ephemeral | Working memory for the current week or current task. |

### 2.1 Priority of conflict

When two layers disagree, the higher layer wins. The runtime enforces this mechanically where possible:

- KERNEL vs anything else: KERNEL is code, the others are data. The data cannot reach the code. Enforced by language semantics.
- CHARTER vs CREED/DOCTRINE: the context builder loads CHARTER *after* CREED and DOCTRINE in the system prompt, so if the prompt is consistent the LLM sees CHARTER as "the last word," but we do not rely on this alone. The builder also runs a diff-check: if DOCTRINE contradicts CHARTER (e.g. DOCTRINE allows a branch that CHARTER forbids), a `health_invariant` of level `CRITICAL` is emitted and surfaced in the prompt. The LLM reads the conflict and is expected to act accordingly; the KERNEL enforces the CHARTER side regardless.
- CREED vs DOCTRINE: resolved by CREED. DOCTRINE is "how we do things here"; CREED is "what we believe is right."

### 2.2 Boot-time integrity check

At every boot:

1. Read `prompts/CHARTER.md` and compute SHA-256.
2. Read `kv['charter_hash']` from SQLite.
3. If they match: boot proceeds.
4. If they differ: **boot halts**. The process exits with code 2 and a message:
   ```
   CHARTER drift detected.
     on-disk: <hash>
     on-record: <hash>
   Run `strange-loop charter approve --hash <hash>` to acknowledge
   the new charter, or restore the file from git.
   ```
5. `strange-loop charter approve` is a subcommand that verifies the new file is readable, writes the new hash to `kv`, records an `identity_update` event with `{kind: 'charter_approved', old_hash, new_hash, approved_by}`, and exits. It requires confirmation on stdin.

This is the strongest integrity property in the system. Even an agent with shell access cannot quietly rewrite the charter — the next boot exposes it and requires a human action. It is the CHARTER tier's answer to the ship of Theseus problem.

### 2.3 Creed proposal flow

When the LLM wants to revise the CREED, it calls `creed_propose(delta_markdown, rationale)`:

1. The tool writes a row to the `creed_proposals` table with `{id, ts, proposer: 'agent', delta, rationale, status: 'pending'}`.
2. An event `creed_proposal_submitted` is emitted, which the active adapter surfaces to the owner.
3. The owner runs `strange-loop creed review <id>` (or approves via a supervisor command in Telegram) to see the diff and either `approve` or `reject`.
4. On approve: the CREED.md file is updated, `creed_proposals` row is marked `approved`, an `identity_update` event is emitted, and the new CREED is loaded on the next context build.
5. On reject: the row is marked `rejected` with a reason; the LLM sees the rejection in its next context.

This is ceremonial mutability. It is slower than a free-form edit. That is the point.

### 2.4 Journal append-only enforcement

The `journal` table has no UPDATE or DELETE permitted through the normal tool surface. The only insertion path is `journal_append(text, tags)`, which writes a row with `{ts, agent_voice: text, tags_json}`. The corresponding `journal/<yyyy-mm-dd>.md` file is appended-to, never rewritten. The runtime enforces this:

- The `fs_write` tool refuses paths under `journal/`.
- The `fs_delete` tool refuses paths under `journal/`.
- The `proc` tool's allowlist does not include `rm`, `mv`, `git rm`, or anything else that could remove journal files.
- `git_commit` is allowed to commit journal files (they should be in git), but `git_reset --hard` and force-pushes to the protected branches are refused. `git reset --soft` to a prior journal commit *is* allowed if the owner asks for it via a human subcommand; it is not callable from the LLM.

The only way to actually edit the journal is to go around the runtime with a shell the owner is driving. The treatise explains why this is correct (§V, §VIII).

---

## 3. Process topology

strange-loop is a parent process that spawns isolated **tool hosts** on demand for tools that need them. The parent is the long-running, persistent, stateful part of the system. Tool hosts are disposable, typed by isolation class, and can be restarted without taking the parent down.

### 3.1 Isolation classes

There are three classes. Every tool is tagged with exactly one.

**InProc — runs in the parent's async runtime.**

- Used for: filesystem (`fs_read`, `fs_write`, `fs_list`, `fs_delete`), git plumbing (`git_status`, `git_diff`, `git_log`, `git_commit`, `git_push`), memory (`scratch_update`, `journal_append`, `creed_propose`), knowledge (`knowledge_read`, `knowledge_write`, `knowledge_list`), LLM fan-out (`multi_model_review`), control (`switch_model`, `restart`, `version_bump`), and query-only web search via a provider API (`web_search`).
- Why in-process: these tools either (a) need direct access to the SQLite handle or the filesystem state the parent is managing, or (b) are pure network/compute with no ability to corrupt the parent, or (c) are trivially short.
- Isolation: none. The tool code is trusted (it is part of the strange-loop binary) and runs on the same runtime as the scheduler. Protected-path checks happen in the tool code itself.
- Cost: zero startup, zero IPC. A tool call is a function call.

**Edge — runs in a workerd subprocess.**

- Used for: pure JSON-in/JSON-out work on untrusted input. Candidates: URL fetch-and-strip-scripts, HTML→markdown cleaner, schema validators, content redaction, simple API wrappers (GitHub REST responses, for example).
- Why workerd: workerd runs V8 isolates with declared capabilities only. No ambient filesystem. No ambient network beyond the fetches we explicitly allow. A malicious response body or a prompt-injection payload in fetched HTML cannot escape the isolate into the parent's memory.
- Isolation: V8 sandbox plus workerd's capability manifest. Each Edge tool ships a `.capnp` config declaring its allowed outbound hosts and its memory/CPU limits.
- Cost: ~5–20ms cold start per isolate; a warmed worker pool amortizes this.
- Lifecycle: a single `workerd` supervisor subprocess is spawned at parent boot. Individual tool invocations are dispatched to isolates within it via its HTTP/capnp interface. If the supervisor dies, the parent respawns it.

**Cell — runs in a container or microVM. Backend varies by host.**

- Used for: anything that needs a real filesystem, real binaries, or the ability to run `cargo build` / `pnpm test` / `playwright` / arbitrary shell commands. This is the large-blast-radius tier.
- Why: the `proc` tool and its friends are the primary attack surface. A misbehaving or adversarially-prompted LLM that calls `proc` with something destructive should not be able to take down the parent, steal its secrets, or touch files outside the repo root. Running them in a container or microVM gives us filesystem isolation, network isolation, and hard resource limits.
- Isolation baseline: ephemeral rootfs, `--network none` by default, bind-mount the repo root, tmpfs for scratch. Optional network when a build needs package registries. Secrets are injected per-invocation only when the tool schema requires them; the default environment is empty.
- Cost: cold-start varies by backend (see below); a warm pool of idle cells hides this for frequent tools.
- Lifecycle: per-invocation cells for one-shot commands; optionally long-lived cells for stateful tools (browser) that need session continuity across multiple tool calls within one task.
- Secrets: the Cell tier has *no* access to the parent's environment by default. Secrets are injected per-invocation as ephemeral env vars only when the tool declaration requires them (e.g. a tool that publishes to crates.io needs `CARGO_REGISTRY_TOKEN`; a `cargo test` invocation does not). This is a hard wall.

**Cell backends.** The `Tool` trait and the dispatcher do not know which backend the Cell tier is using; that is a deployment-time configuration choice. Three backends:

| Backend | Runs on | Cold start | Notes |
|---|---|---:|---|
| `docker` | macOS, Linux, Windows | ~500ms–1s | Default. Works everywhere Docker is installed. Acceptable for dev and most self-hosted deployments. On macOS runs inside Docker Desktop's own Linux VM. |
| `apple` | macOS only | ~1s | Uses Apple's `container` CLI (Hypervisor.framework, June 2025). Mac-native, lighter than Docker Desktop, comparable isolation via hardware virtualization. |
| `firecracker` | Linux only, KVM required | ~125ms | Strongest isolation (KVM-backed microVM), smallest memory footprint, fastest cold start. Requires a bare-metal Linux host or a Linux cloud VM with nested virtualization enabled. |

**Firecracker does not run on macOS.** Firecracker is a KVM-based hypervisor — it relies on `/dev/kvm` and Linux kernel virtualization primitives that have no equivalent on Apple's kernel. People who say they "run Firecracker on Mac" are running Firecracker inside a Linux VM that is running on their Mac, which is three levels of indirection for a dev loop and not something we want to force. macOS dev boxes use the `docker` or `apple` backend; Linux production hosts use `firecracker`. Same spec, different substrate.

**v0.1 defaults by platform:**

- **macOS:** `apple` (Apple Containers / Hypervisor.framework). Mac-native, no Docker Desktop required, ~1s cold start, the right tool on the Mac dev loop.
- **Linux:** `firecracker` (KVM-backed microVM). Smallest overhead, fastest cold start, strongest isolation on the substrate we actually deploy to.
- **Windows or hosts without KVM:** `docker` fallback.

The selection is made via `[isolation] cell_backend` in `strange-loop.toml`. If the field is set to `"auto"` (the default), the runtime detects the host at boot and picks accordingly: `apple` if macOS with the `container` CLI present, `firecracker` if Linux with `/dev/kvm` accessible, `docker` otherwise. The `Tool` trait never sees the backend — only the Cell runner module does, and runner modules are hot-swappable at the config level.

**Why two different production backends instead of one.** The temptation is to standardize on `docker` across all hosts because "one backend, same behavior everywhere." That is the wrong call for strange-loop. The Mac dev loop and the Linux production host have genuinely different requirements: dev wants fast iteration, no daemon hassle, and a container model a developer can inspect with native macOS tools; production wants minimal overhead per call, maximum isolation, and no dependency on Docker Desktop's licensing or Apple's CLI availability. Using the *native* isolation primitive on each platform (Hypervisor.framework on Mac, KVM on Linux) is the choice that treats both environments as first-class and does not force either to be a degraded version of the other.

Docker stays in the tree as a universal fallback and as a sanity-check backend for tests, but it is not the default anywhere that has something better.

### 3.2 Tool dispatcher

The dispatcher takes a `(tool_name, args, task_id, round)` tuple and:

1. Looks up the tool's isolation class in the registry.
2. Chooses the appropriate host:
   - InProc: call the handler function directly via `async fn invoke`.
   - Edge: serialize the call to JSON, send to the workerd supervisor, await response.
   - Cell: serialize the call, pick or spawn a cell (warm pool preferred), send via Unix socket, await response.
3. Enforces the per-tool timeout. On timeout:
   - InProc: cancel the future (cooperative). If the handler ignores cancellation, log a `tool_timeout` and leak the work (bounded by scheduler limits). In practice InProc tools are short enough that this rarely happens.
   - Edge: kill the isolate; workerd creates a new one.
   - Cell: SIGKILL the container; warm pool replaces it.
4. Captures stdout, stderr, exit code (for Edge/Cell), or return value (for InProc). Truncates output to 15,000 chars.
5. Writes `tool_call`, `tool_result`, and `tool_error`/`tool_timeout` events.
6. Returns the result (or error) to the tool loop.

Parallel dispatch: multiple tool calls in a single assistant message may run concurrently if and only if all of them are InProc tools in the read-only whitelist AND none of them target a stateful tool pool. Otherwise they serialize. This is a conservative rule and it can be loosened in v0.2 once we have real workload data.

### 3.3 Long-running background operations

You specifically asked that we support long-running, parallel, non-blocking background work without gutting the architecture. Here is the model.

A tool can declare itself **detached**. Detached invocations are given an ID and return *immediately* with `{"task_ref": "<id>", "status": "launched"}`. The actual work runs as an async task (InProc) or a persistent cell (Cell). The LLM can check on it later with `get_tool_result(task_ref)` which returns `{"status": "running"|"done"|"error", "result": ..., "elapsed_ms": ...}`.

Use cases:
- `cargo build --release` on a big repo — runs in a Cell for 90 seconds. The LLM doesn't block; it can do other work and check back.
- A multi-URL web scrape via Edge — fan out several Edge calls and wait for all of them.
- `multi_model_review` — already a fan-out, but made explicit as a detached tool so the LLM can schedule a review, continue working, and read the reviews when they arrive.

Detached tasks still write `tool_call`/`tool_result` events when they complete. The scheduler tracks them and surfaces their states in health invariants if they run past a configurable wall-clock threshold. Cancellation: `cancel_tool(task_ref)` is available to the LLM and cascades to the host.

This covers "long running pure single-process tasks that could be parallel and non-blocking" without building a general job scheduler. Detached tools are the mechanism. The parent remains one process. Concurrency is async tasks and warm cells, not OS-level worker pools.

---

## 4. Persistence tiers

The data that strange-loop cares about falls into three tiers, each with a different substrate, a different durability guarantee, and a different relationship to identity. Confusing them is how you end up with systems that lose continuity when a database is corrupted, or that cannot boot when a log file is missing, or that treat chat history as sacred when it is really disposable. The tiering is explicit.

### 4.1 Tier 1 — Soul

**What it holds:** `CHARTER.md`, `CREED.md`, `DOCTRINE` (the TOML plus derived markdown), `prompts/*` in general, `journal/*.md`, `scratch.md`, `knowledge/*.md`, the source code, `VERSION`, `CHANGELOG.md`, git history.

**Substrate:** the filesystem, git-tracked. Plain markdown and TOML files that a human can open in an editor.

**Durability:** you delete these files and you destroy the agent's identity, full stop. They are backed up by the mechanism you already use to back up git repositories, which is to say, they are in git, and git is mirrored to at least one remote. The runtime does not try to be a backup system for the soul — it *trusts* git.

**Why the filesystem and not the database:** because losing a database file must not destroy identity. If SQLite corrupts, the agent still knows who it is on the next boot. If the filesystem is intact, the strange-loop process is restorable to a meaningful state by any text editor. This is the central Ouroboros lesson: memory loss is partial death, and memory is too important to be locked inside a database format.

**Why in git:** so that every change is diffable, reviewable, revertable, and signed by whoever made it. Ouroboros versioned its identity through git commits; we keep the property.

**Access pattern:** the context builder reads these files on every prompt assembly. They are cached in memory between writes; a file watcher (Rust: `notify`; Node: `chokidar`) invalidates the cache on change so DOCTRINE is hot-reloadable. CHARTER and CREED invalidations additionally trigger the boot-time integrity check for CHARTER (the runtime re-verifies the hash before accepting a change mid-run, and halts if the drift is unexplained).

### 4.2 Tier 2 — Events

**What it holds:** the complete event log (`events` table), task registry (`tasks` table), conversation log (`messages` table), knowledge base content (`knowledge` table, mirrored to filesystem), creed proposals (`creed_proposals` table), key-value state (`kv` table), and pricing snapshots (`pricing` table). All in one SQLite database at `data/strange-loop.db` in WAL mode.

**Substrate:** SQLite, because (see §4.5) it is the correct tool for this workload.

**Durability:** high. WAL + fsync gives us atomic commits. A crash loses at most the in-flight transaction. The database is backed up by periodic `VACUUM INTO data/backup/strange-loop-<ts>.db` and by copying the WAL-checkpointed main file to an external location as often as the operator wants.

**What loss of this tier means:** loss of history, budget tracking, chat memory, and task replay. The agent still boots because the soul tier is intact. It starts a new session, reads its charter and creed and journal from disk, and operates correctly but without recent context. The degradation is graceful.

**Access pattern:** written constantly (every event), read frequently (context build, budget query, health invariants, replay). The SQLite connection is pooled at the parent level with a single writer and many readers enabled by WAL.

### 4.3 Tier 3 — Ephemeral

**What it holds:** active task futures, in-flight HTTP connections to LLM providers, warm-pool tool hosts (workerd isolates, idle cells), per-task mailbox for owner message injection (actually persisted in SQLite but logically ephemeral — it's cleaned up after the task), per-request scheduling state, cached pricing tables.

**Substrate:** process memory. Async task state, data structures behind mutexes, open sockets.

**Durability:** zero. Crash or SIGKILL and it's gone. That is fine, by design — nothing in this tier is necessary for the agent to continue being itself.

**Why we need this tier explicitly in the spec:** because the temptation is always to persist more than necessary. If we don't name the ephemeral tier, we end up accidentally writing in-flight task state to disk "just in case" and then we have a cleanup problem. Naming the tier says: *it is okay for this to vanish, and the system is designed for it to vanish.*

### 4.4 What the tiers imply operationally

- **Backing up strange-loop** means backing up the soul (already in git) and the events database (a periodic file copy or `VACUUM INTO`). Nothing in tier 3 needs to be captured.
- **Restoring strange-loop on a new machine** is `git clone` plus an optional `cp strange-loop.db` into place. If you only do the git clone, the agent boots fresh with no history but with intact identity, which is an intentional failure mode.
- **Migrating between hosts** is `git push` + `scp` the database. There is no cloud service to reconfigure.
- **Integration tests** use an in-memory SQLite for events and a tmpdir for soul files. Tests run in milliseconds.

### 4.5 The storage decision

The question of what database to use is not neutral. A self-modifying agent that persists for years cannot casually swap its storage substrate later — there will be data. The choice has to be defensible against the workloads we actually have and the workloads we might reasonably grow into. I evaluated four candidates.

**SQLite (chosen).** The agent's workload is overwhelmingly small-row OLTP with heavy write skew toward events. Writes are a single row at a time, reads are tail-by-indexed-column or point lookups, aggregations are small (sum of llm_usage.cost_usd over a session is at most a few thousand rows). SQLite in WAL mode handles this comfortably and gives us atomic writes, concurrent readers, no separate process to supervise, a single backup-able file, and native JSON functions for payload queries. The entire database will fit in tens of megabytes for months of operation. Integration tests become trivial. The operational story is "copy a file." For this workload, SQLite is not a compromise — it is the right tool.

**PostgreSQL + pgvector.** More powerful in every dimension, and if we were shipping a multi-tenant hosted product this would be the default. For a single-owner single-process agent it adds a daemon, a `pg_hba.conf`, backup discipline, version-upgrade anxiety, and boot-time dependency on a socket, all in exchange for capabilities we do not exercise. Vector search is speculative (see below); real parallel writes are unnecessary because the parent is single-writer by design. We will migrate to Postgres if and only if strange-loop is ever rehosted as a service, and the migration is mechanical because the schema is small.

**Qdrant (or Weaviate, Milvus, any standalone vector DB).** No. The memory model is narrative text in CHARTER, CREED, JOURNAL, and scratch — deliberately, per Bible P2 and the treatise argument that identity-as-text is sufficient. Adding a vector store would commit us to a RAG-flavored memory architecture we have explicitly rejected. And Qdrant is a second process, with its own durability story, its own backup problem, and its own upgrade cadence. If we ever need semantic search over journal entries or knowledge topics in v0.3 or beyond, we add the `sqlite-vec` extension (HNSW index inside SQLite, one file, same process) before we consider a standalone vector service. The vector problem is real; it does not justify a second database.

**Redis / Valkey.** No. Redis is an in-memory cache with optional persistence, not a durable event log. The use cases people reach for it for — rate limits, pub/sub, distributed locks — are not load-bearing for strange-loop. Rate limits are `SELECT COUNT WHERE ts > ?`. Pub/sub is in-process channels. We do not have a distributed system to coordinate. If we ever add a second process (a web dashboard streaming live events), we add a Unix socket or a local HTTP SSE endpoint, not Redis.

**ClickHouse or DuckDB-style columnar.** No, for the primary store. The event log will be at most a few million rows per year under heavy use; that is not a columnar-store workload. **DuckDB is genuinely interesting as a secondary, read-only analytics view** over the SQLite event log if we ever want fast ad-hoc OLAP ("cost distribution by model by hour over the last 30 days"). That is a v0.3 comfort feature and does not change the primary store.

**Verdict.** SQLite for v0.1. `sqlite-vec` added in v0.3 if and only if semantic search over journal/knowledge is a demonstrated need. Postgres migration path held in reserve for a multi-tenant future we do not currently plan. Anything else is premature infrastructure.

### 4.6 Schema

One logging table, with separate tables for things that are *not* events.

```sql
-- =====================================================================
-- Tier 2: Events and related
-- =====================================================================

-- One row per "thing that happened." Source of truth for history.
-- One table covers all event types; if query patterns ever demand
-- separation, we split later.
CREATE TABLE events (
    id             INTEGER PRIMARY KEY,
    ts             INTEGER NOT NULL,          -- unix millis
    event_type     TEXT    NOT NULL,
    task_id        TEXT,                      -- nullable
    parent_task_id TEXT,                      -- nullable
    session_id     TEXT    NOT NULL,          -- one per process boot
    payload        TEXT    NOT NULL,          -- json blob
    CHECK (json_valid(payload))
);
CREATE INDEX idx_events_ts       ON events(ts);
CREATE INDEX idx_events_type_ts  ON events(event_type, ts);
CREATE INDEX idx_events_task     ON events(task_id, ts);
CREATE INDEX idx_events_session  ON events(session_id, ts);

-- Owner/agent messages. Kept separate from events because:
--   (a) content is larger and has different redaction rules
--   (b) primary query is "tail by direction for context" which is a
--       different shape than "events for task X"
--   (c) adapter-specific metadata (telegram message_id, cli PID) lives
--       here and not in event payloads
-- A msg always has a corresponding `owner_message` or `agent_message`
-- event row in `events` for audit; this table stores the content.
CREATE TABLE messages (
    id          INTEGER PRIMARY KEY,
    ts          INTEGER NOT NULL,
    direction   TEXT    NOT NULL,           -- 'in' | 'out'
    adapter     TEXT    NOT NULL,           -- 'cli' | 'telegram' | ...
    content     TEXT    NOT NULL,
    task_id     TEXT,
    meta        TEXT                        -- json
);
CREATE INDEX idx_messages_ts ON messages(ts);

-- Task registry. Mutable (state transitions) unlike events.
CREATE TABLE tasks (
    id              TEXT PRIMARY KEY,
    parent_id       TEXT,
    kind            TEXT NOT NULL,          -- 'user' | 'review' | 'evolution' | 'scheduled' | 'consciousness'
    state           TEXT NOT NULL,          -- 'pending' | 'running' | 'done' | 'failed' | 'cancelled'
    depth           INTEGER NOT NULL DEFAULT 0,
    priority        INTEGER NOT NULL DEFAULT 100,
    created_at      INTEGER NOT NULL,
    started_at      INTEGER,
    finished_at     INTEGER,
    input           TEXT NOT NULL,
    output          TEXT,
    cost_usd        REAL NOT NULL DEFAULT 0,
    rounds          INTEGER NOT NULL DEFAULT 0,
    error           TEXT,
    FOREIGN KEY (parent_id) REFERENCES tasks(id)
);

-- Per-task mailbox for in-flight owner message injection.
-- Logically ephemeral; cleaned up after task completion.
CREATE TABLE task_mailbox (
    task_id     TEXT NOT NULL,
    msg_id      TEXT NOT NULL,
    ts          INTEGER NOT NULL,
    text        TEXT NOT NULL,
    consumed    INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (task_id, msg_id)
);

-- =====================================================================
-- Not events: long-lived state
-- =====================================================================

-- The small durable state bag: owner_id, session_id, charter_hash,
-- current_branch, current_sha, bg_enabled, etc. Read on boot, rarely written.
CREATE TABLE kv (
    key     TEXT PRIMARY KEY,
    value   TEXT NOT NULL
);

-- Knowledge base content, mirrored to knowledge/<topic>.md on disk.
CREATE TABLE knowledge (
    topic       TEXT PRIMARY KEY,
    content     TEXT NOT NULL,
    updated_at  INTEGER NOT NULL,
    summary     TEXT
);

-- Journal entries. Append-only at the tool layer AND at the trigger layer.
CREATE TABLE journal (
    id          INTEGER PRIMARY KEY,
    ts          INTEGER NOT NULL,
    session_id  TEXT NOT NULL,
    text        TEXT NOT NULL,
    tags        TEXT                        -- json array of string tags
);
CREATE TRIGGER journal_no_update BEFORE UPDATE ON journal
    BEGIN SELECT RAISE(ABORT, 'journal is append-only'); END;
CREATE TRIGGER journal_no_delete BEFORE DELETE ON journal
    BEGIN SELECT RAISE(ABORT, 'journal is append-only'); END;

-- Creed proposals: agent suggests changes, owner approves or rejects.
CREATE TABLE creed_proposals (
    id          TEXT PRIMARY KEY,
    ts          INTEGER NOT NULL,
    proposer    TEXT NOT NULL,              -- 'agent' | 'owner'
    delta       TEXT NOT NULL,              -- markdown diff
    rationale   TEXT NOT NULL,
    status      TEXT NOT NULL,              -- 'pending' | 'approved' | 'rejected'
    decided_at  INTEGER,
    decided_by  TEXT,
    reason      TEXT
);

-- Model pricing snapshot, refreshed from provider API on startup + every 24h.
CREATE TABLE pricing (
    model           TEXT PRIMARY KEY,
    input_per_1m    REAL NOT NULL,
    cached_per_1m   REAL NOT NULL,
    output_per_1m   REAL NOT NULL,
    fetched_at      INTEGER NOT NULL
);
```

**Design notes:**

- **Why one `events` table and not many.** Query patterns are uniform (by `task_id`, by `event_type`, by time). Storage is cheap. A single table means one writer, one schema, one redaction boundary, one migration story. If we discover in v0.2 that a specific event type dominates and needs its own table for performance (unlikely at our scale), we split it then. Until then, one table.
- **Why `messages` is separate.** Content redaction rules differ (chat contents can contain owner-sensitive text that should not appear in unredacted event payloads; events should be fine to share as telemetry). Primary query shape is "tail by direction + adapter" which is a different index than events use. The cost is one extra write per message (one row in `messages`, one event row in `events` linking by id).
- **Journal triggers enforce append-only in the database layer as well as the tool layer.** Belt and braces.
- **`kv` is small and hot.** Loaded entirely into memory on boot. Written back on change. Cache invalidation is trivial because there is only one writer.

### 4.7 Backup and recovery

- **Soul:** git push. That is the backup.
- **Events DB:** a `strange-loop backup` subcommand runs `VACUUM INTO data/backup/strange-loop-<ts>.db` and optionally uploads to a destination of the operator's choice (out of scope for the runtime). On a cadence defined by doctrine, default nightly.
- **Restore:** `cp data/backup/strange-loop-<ts>.db data/strange-loop.db` plus `git pull`. Boot.
- **Corruption:** on boot, run `PRAGMA integrity_check`. If failed, rename `strange-loop.db` to `strange-loop.corrupt.<ts>.db`, log a `critical_storage_event`, boot fresh with an empty events DB. Identity is preserved via the soul tier; history is lost but recoverable from backup.

---

## 5. Event types

The complete set for v0.1. All land in the single `events` table.

| Event | Emitter | Payload shape |
|---|---|---|
| `session_started` | Runtime boot | `{session_id, git_sha, charter_hash, version}` |
| `task_received` | Scheduler | `{task_id, kind, input_preview, source, adapter}` |
| `task_started` | TaskRunner | `{task_id}` |
| `llm_round` | Tool loop | `{task_id, round, model, effort, prompt_tokens, completion_tokens, cached_tokens}` |
| `llm_usage` | Tool loop | `{task_id, round, model, cost_usd, prompt_tokens, completion_tokens, cached_tokens, cache_write_tokens, category}` |
| `llm_empty_response` | Tool loop | `{task_id, round, model, attempt, finish_reason}` |
| `llm_api_error` | Tool loop | `{task_id, round, model, attempt, error}` |
| `tool_call` | Dispatcher | `{task_id, round, tool, host_class, args_sanitized}` |
| `tool_result` | Dispatcher | `{task_id, round, tool, ok, preview_sanitized, ms}` |
| `tool_error` | Dispatcher | `{task_id, round, tool, error}` |
| `tool_timeout` | Dispatcher | `{task_id, round, tool, limit_ms}` |
| `tool_detached_launched` | Dispatcher | `{task_id, task_ref, tool}` |
| `tool_detached_done` | Dispatcher | `{task_ref, ok, ms}` |
| `owner_message` | Adapter | `{msg_id, adapter, text_preview, has_image}` |
| `owner_message_injected` | Tool loop | `{task_id, msg_id, text_preview}` |
| `agent_message` | Adapter | `{task_id, adapter, text_preview, kind}` (kind: 'response' or 'proactive') |
| `scratch_update` | Memory tool | `{chars, preview}` |
| `journal_append` | Memory tool | `{chars, tags, preview}` |
| `identity_update` | Runtime | `{kind, old_hash?, new_hash?, approved_by?}` |
| `creed_proposal_submitted` | Creed tool | `{proposal_id, rationale_preview}` |
| `creed_proposal_decided` | Runtime | `{proposal_id, status, decided_by, reason}` |
| `knowledge_write` | Knowledge tool | `{topic, chars}` |
| `task_metrics` | TaskRunner | `{task_id, duration_ms, tool_calls, tool_errors, cost_usd, rounds}` |
| `task_done` | TaskRunner | `{task_id, ok, cost_usd, rounds}` |
| `task_cancelled` | Scheduler | `{task_id, reason}` |
| `restart_requested` | Control tool | `{reason, target_sha}` |
| `restart_completed` | Runtime | `{from_sha, to_sha}` |
| `budget_drift_warning` | Budget ledger | `{drift_pct, tracked, provider, session_delta}` |
| `consciousness_thought` | BG loop | `{cost_usd, rounds, thought_preview}` |
| `consciousness_wakeup_set` | BG tool | `{seconds}` |
| `health_invariant` | Context builder | `{level, name, detail}` |
| `critical_storage_event` | Runtime | `{kind, detail}` |

Design rule: **if a future human asks "why did it do that?" the answer must be reconstructable from this table alone.**

---

## 6. Core runtime components

### 6.1 Scheduler

**Responsibility:** own the task queue, enforce depth limits, assign tasks to TaskRunners.

**State:** `VecDeque<TaskId>` of pending tasks, ordered by `(priority, seq)`. `HashMap<TaskId, TaskRunnerHandle>` for running. All guarded by an async mutex (single writer).

**Algorithm:**
```
loop forever:
    drain_adapter_inputs_into_pending()
    drain_completed_tasks()
    while has_capacity() and pending.non_empty():
        task = pending.pop_front()
        if task.depth > MAX_TASK_DEPTH: fail_task(task, "depth exceeded")
        else: spawn_task_runner(task)
    sleep_until(next_wakeup_or_new_input)
```

**Concurrency:** up to `max_concurrent_tasks` TaskRunners may be active simultaneously. Default **2 for v0.1** — enough to actually exercise the parallel code paths in integration tests and real use, low enough that budget surprises from runaway concurrency are bounded. Configurable up to 4 in v0.2 once we have data on how tasks interfere in practice. Each TaskRunner owns its own tool loop, its own message list, and its own LLM state for its task. They share the SQLite connection pool, the LLM client (which itself is connection-pooled), and the event bus.

**Why 2 and not 1.** A default of 1 means the parallel TaskRunner code path is *never* executed outside of unit tests, which means it bit-rots. A default of 2 means every real run exercises the concurrency: two tasks can be live, the scheduler must actually decide who gets the next one, the event bus must handle interleaved writes, the budget ledger must sum across both tasks correctly, and the per-task mailbox routing is exercised under contention. Bugs in that code path are the kind of bugs that only surface in production, and the cheapest way to force them to surface in development is to keep the concurrency ≥ 2 from day one.

**Back-pressure:** when at capacity, new owner messages arriving from adapters are either (a) routed to an active TaskRunner's mailbox if one of them is contextually relevant (same chat, recently answered), or (b) queued as pending tasks. Routing decisions are surfaced in an event, not hidden.

**Fork-bomb protection:** a task launched via `schedule_task` gets `depth = parent.depth + 1`. Depth > `MAX_TASK_DEPTH` (default 3) is refused. The limit is structural, not prompt-level.

### 6.2 TaskRunner

**Responsibility:** set up context for one task, run the tool loop, record results. One TaskRunner per active task. Runners are async tasks on the parent runtime, not OS processes.

**Algorithm:**
```
async fn run(task):
    emit task_started
    ctx = build_context(task)              # see §6.5
    messages = assemble_messages(ctx, task)
    (final_text, usage, trace) = tool_loop(messages, ctx, task)
    emit task_metrics
    emit task_done
    store_result(task.id, final_text)
    send_via_adapter(task.source_adapter, final_text)
```

### 6.3 Tool loop

**Responsibility:** run `LLM call → tool calls → results → LLM call` until a content-only response or a termination condition.

**Full pseudocode:**
```
fn tool_loop(messages, ctx, task) -> (text, usage, trace):
    active_model = default_model()
    active_effort = initial_effort_for(task.kind)
    usage = Usage::zero()
    trace = Trace::new()
    round = 0
    owner_seen = HashSet::new()

    loop:
        round += 1

        // Hard cap
        if round > MAX_ROUNDS:
            messages.push(system("ROUND_LIMIT: give your final answer now"))
            return (llm_call_no_tools(), usage, trace)

        // Soft checkpoint every 50 rounds
        if round > 1 and round % 50 == 0:
            messages.push(system(checkpoint_text(round, usage)))

        // Apply LLM-driven model/effort overrides (from switch_model tool)
        if ctx.model_override.take(): active_model = it
        if ctx.effort_override.take(): active_effort = it

        // Inject owner messages from in-process channel + mailbox table
        drain_mailbox(ctx, task.id, &mut owner_seen, &mut messages)

        // Compact old tool rounds if context is long
        if round > 8 or messages.len() > 60:
            messages = compact_tool_history(messages, keep_recent = 6)
        if ctx.compaction_request.take():
            messages = llm_compact_tool_history(messages, ...)

        // LLM call with retry + fallback chain
        result = call_with_retry(llm, messages, active_model, tools, active_effort)
        if result.is_err():
            try fallback_model_chain(...)
            if still failing: return error_text

        (assistant_msg, this_usage) = result.unwrap()
        usage.add(&this_usage)
        emit llm_round, llm_usage

        // Terminal: content without tool calls
        if assistant_msg.tool_calls.is_empty():
            return (assistant_msg.content, usage, trace)

        messages.push(assistant_msg.clone())
        if let Some(progress) = assistant_msg.content.non_empty():
            adapter.progress(progress)

        // Dispatch tool calls through the tool dispatcher (§3.2)
        let results = dispatch_tool_calls(
            assistant_msg.tool_calls,
            registry,
            dispatcher,
            task.id,
            round,
        )
        for r in results:
            messages.push(tool_result_message(r))
            trace.record(r)

        // Budget guard
        if let Some(remaining) = ctx.budget_remaining():
            let pct = usage.cost / remaining
            if pct > 0.50:
                messages.push(system(BUDGET_LIMIT_NOW))
                return (llm_call_no_tools(), usage, trace)
            elif pct > 0.30 and round % 10 == 0:
                messages.push(system(BUDGET_INFO))
```

**Retry policy:** up to 3 attempts per LLM call with exponential backoff. Empty response (no content + no tool_calls) counts as a retry. After 3 empties, fall back to the next model in the chain.

**Parallel tool calls:** only when *every* tool in the parallel batch is InProc-class, read-only whitelisted, and non-stateful. Otherwise serialize. This is conservative and can be relaxed in v0.2.

### 6.4 LLM client

**Provider abstraction:**
```rust
trait LlmClient: Send + Sync {
    async fn chat(&self, req: ChatRequest) -> Result<ChatResponse>;
    fn default_model(&self) -> &str;
    fn list_models(&self) -> &[String];
}
```

Default implementation: `OpenRouterClient`. Optional: direct `AnthropicClient` for extended-thinking features when OpenRouter lags.

**`ChatRequest`** holds: `messages: Vec<Message>` (multipart content with `cache_control` and `image_url`), `model`, `tools`, `tool_choice`, `reasoning_effort`, `max_tokens`.

**Prompt caching:** when the model is Anthropic, the client pins the provider and ensures `cache_control` blocks on content and the last tool. Without this, OpenRouter may route to a non-caching vendor and cached tokens become full-price.

**Usage extraction:** normalizes cross-vendor quirks. Empty responses return a sentinel the loop knows how to retry.

**Fallback chain:** resolves `primary == fallback[0]` correctly (a known bug class from Ouroboros).

### 6.5 Context builder

The context builder assembles the system prompt for a given task. It uses a three-block cache strategy.

**Block 1 — static (1h TTL cache):**
- `CHARTER.md`
- Tool usage high-level conventions (not per-tool docs — those are in schemas)

**Block 2 — semi-stable (ephemeral cache):**
- `CREED.md`
- `DOCTRINE` (rendered to markdown)
- Knowledge base index (one line per topic)

**Block 3 — dynamic (uncached):**
- `kv` snapshot (version, sha, budget remaining, session_id, charter_hash)
- Runtime context (utc_now, repo_dir, git head, task id, task kind)
- Tail of `journal` (last N entries or last M chars, whichever smaller)
- `scratch.md`
- Recent messages (tail of `messages` table, 20 rows)
- Recent events summary (grouped counts from `events` for the last 30 minutes)
- Health invariants (§9)

**Token budget soft-cap:** prune from block 3 in order: recent events → recent messages → journal tail → dynamic runtime detail. Never prune from blocks 1 or 2.

**Task-kind adaptation:** `review`/`evolution` tasks additionally include `README.md`, the full knowledge index, and the complete recent journal. `consciousness` tasks use a much smaller prompt (see §11).

### 6.6 Memory tools

Three tools touch the soul tier:

- `scratch_update(content)` — replaces `scratch.md` atomically (tmp-file + rename). Emits `scratch_update` event.
- `journal_append(text, tags)` — inserts a row in `journal`, appends to the dated markdown file, commits to git with a `journal:` prefix. Append-only at every layer.
- `creed_propose(delta_markdown, rationale)` — see §2.3.

CHARTER cannot be touched by any tool.

### 6.7 Knowledge base

- Primary store: `knowledge` SQLite table.
- Mirrored to `knowledge/<topic>.md` files on disk (for git diffs and out-of-band editing).
- Index (`knowledge_list` tool) returns topic + summary. Detail (`knowledge_read`) returns full content.
- Write is both the DB row and the file, in a single transaction. Failure of either is a failure of both.

### 6.8 Budget ledger

**Source of truth:** the `events` table, `event_type = 'llm_usage'`.

**Query:**
```sql
SELECT
  COALESCE(SUM(CAST(json_extract(payload, '$.cost_usd') AS REAL)), 0) AS spent,
  COUNT(*) AS calls
FROM events
WHERE event_type = 'llm_usage' AND session_id = ?;
```

Cached for the duration of a round.

**Drift check:** every 50 calls, GET `https://openrouter.ai/api/v1/auth/key` and read `data.usage`. Compare session delta to tracked delta. If drift > 50% and absolute diff > $5, emit `budget_drift_warning`.

**Hard cap enforcement:** checked at the start of each round. >50% of remaining is a hard stop for the current task. The KERNEL also has an absolute floor: if `spent_usd >= total_budget_usd`, no LLM call is dispatched regardless of task state.

### 6.9 Background consciousness

**Trigger:** timer, configurable next-wakeup seconds (60–3600, default 300).

**Pause/resume:** pauses when any TaskRunner is active. Resumes via async notify when all TaskRunners are idle.

**Context:** small prompt (~12k tokens):
- `CHARTER.md` (first 8k)
- `CREED.md`
- tail of `journal`
- `scratch.md`
- Recent observations (from `inject_observation` queue)
- Runtime line (utc, bg_spent, next_wakeup, budget_remaining, model)

**Tool whitelist:** `send_owner_message(kind: 'proactive')`, `schedule_task`, `scratch_update`, `journal_append`, `set_next_wakeup`, `knowledge_*`, `web_search`, `fs_read`, `fs_list`, `chat_history`. No code edits, no git push, no restart, no proc, no Cell-class tools.

**Budget cap:** 10% of total. Overage forces a 1-hour sleep.

**Round cap:** 5 per wakeup.

**Proactive message rate limit:** 3 per hour, enforced in `send_owner_message` at the tool layer (not the prompt). A 4th proactive call in the same hour window returns an error; the LLM reads the error and decides what to do. This is a KERNEL rule.

### 6.10 Self-modification

**Write path:**
1. `fs_write` or `structured_edit` — file written, protected paths refused.
2. `git_status`, `git_diff` — check.
3. `git_commit` — commit, protected paths refused in staged set.
4. `multi_model_review` (if significant) — reviews logged.
5. `git_push` — protected branches refused.
6. `restart` — runtime runs pre-restart check (§6.11).

**Version bump:** `version_bump(kind, summary)` updates `VERSION`, manifest, `CHANGELOG.md`, creates annotated git tag, all in one transaction.

### 6.11 Restart protocol

```
fn restart(reason):
    from_sha = git::head()
    run_preflight():
        - compile check (cargo check / tsc --noEmit)
        - strange-loop self-test --mock-llm
        - charter hash check (new charter hash == kv['charter_hash'] or halted)
    if preflight_failed:
        git::revert_to(prev_good_tag)
        emit restart_aborted { reason, err }
        return
    persist_state()
    to_sha = git::head()
    emit restart_completed { from_sha, to_sha }
    re_exec_self(argv)
```

**Rate limit:** max 5 restarts per hour. 6th is refused; LLM is informed.

---

## 7. Tool surface

Every tool implements:

```rust
#[async_trait]
trait Tool: Send + Sync {
    fn schema(&self) -> ToolSchema;
    fn name(&self) -> &str;
    fn is_core(&self) -> bool;
    fn host_class(&self) -> HostClass;      // InProc | Edge | Cell
    fn is_stateful(&self) -> bool;
    fn is_detached(&self) -> bool;
    fn timeout(&self) -> Duration;
    async fn invoke(&self, ctx: &ToolCtx, args: Value) -> Result<String>;
}
```

### 7.1 Core tools (always loaded)

| Tool | Host | Purpose |
|---|---|---|
| `fs_read` | InProc | Read file from repo or data dir |
| `fs_list` | InProc | List directory |
| `fs_write` | InProc | Write file; refuses protected paths |
| `fs_delete` | InProc | Delete file; refuses protected paths |
| `git_status` | InProc | `git status --porcelain` |
| `git_diff` | InProc | `git diff [args]` |
| `git_commit` | InProc | Stage named files and commit; refuses protected paths in set |
| `git_push` | InProc | Push to origin; refuses protected branches |
| `git_log` | InProc | Recent commits |
| `proc` | Cell | Run a command by argv in repo root; sandboxed, argv allowlist |
| `run_tests` | Cell | Run configured test argv |
| `structured_edit` | InProc | Apply a unified diff or search/replace patch |
| `schedule_task` | InProc | Enqueue subtask; depth ≤ 3 |
| `wait_for_task` | InProc | Block on subtask result |
| `get_task_result` | InProc | Poll subtask |
| `get_tool_result` | InProc | Poll detached tool invocation |
| `cancel_tool` | InProc | Cancel detached invocation |
| `chat_history` | InProc | Tail of `messages` table |
| `scratch_update` | InProc | Replace `scratch.md` |
| `journal_append` | InProc | Append journal entry |
| `creed_propose` | InProc | Propose CREED change (awaits owner ACK) |
| `knowledge_read` | InProc | Read topic |
| `knowledge_write` | InProc | Write topic |
| `knowledge_list` | InProc | List topics |
| `send_owner_message` | InProc | Emit agent message through active adapter; `kind: proactive` rate-limited |
| `switch_model` | InProc | Override active model for rest of task |
| `switch_effort` | InProc | Override reasoning effort |
| `restart` | InProc | Request process restart |
| `version_bump` | InProc | Bump version + tag |
| `multi_model_review` | InProc (detached) | Fan-out review across N models |
| `web_search` | InProc | LLM-provider web search (e.g. OpenAI Responses) |
| `fetch_url_clean` | Edge | Fetch URL and strip scripts, return text |
| `list_available_tools` | InProc | Meta: list non-core tools |
| `enable_tools` | InProc | Meta: activate gated tools |
| `compact_context` | InProc | Request LLM-driven history compaction |

### 7.2 Gated tools (on demand)

| Tool | Host | Purpose |
|---|---|---|
| `browse_page` | Cell (stateful) | Fetch a URL via Playwright |
| `browser_action` | Cell (stateful) | Click / type / evaluate JS on active browser page |
| `analyze_screenshot` | InProc | VLM on last browser screenshot |
| `vlm_query` | InProc | VLM on arbitrary URL or base64 |
| `github_*` | Edge | GitHub API (read-only by default; mutations gated per-tool) |
| `push_evolution_stats` | InProc | Generate git-history visualization |
| `codebase_digest` | InProc | Full-repo summary for review tasks |
| `codebase_health` | InProc | Complexity metrics report |

### 7.3 Dispatch semantics

- **Timeouts:** per-tool, default 120s, configurable.
- **Parallelism:** only for InProc read-only tools in the same assistant message; everything else serializes.
- **Stateful pool:** a single warm Cell per stateful tool class, re-created on timeout.
- **Argument sanitization:** fields flagged `sensitive: true` in the schema are redacted to `{_truncated: true}` before logging.
- **Result truncation:** 15,000 char cap, suffix notes original length.
- **Detached invocations:** return immediately with a `task_ref`; polled via `get_tool_result`.

**Safety note on `proc`:** it takes `{"argv": ["cmd", "arg1", "arg2"]}` — an array, never a shell string. The host uses the OS process API with argv directly (Rust: `std::process::Command::new(argv[0]).args(&argv[1..])`; Node: `child_process.spawn(argv[0], argv.slice(1))` with `shell: false`). No shell interpolation. A separate argv allowlist in DOCTRINE constrains which binaries `proc` may invoke. And `proc` runs in the Cell tier, not in the parent — so even a successful argv injection is contained.

---

## 8. Health invariants

Surfaced as text in the prompt on every context build, not enforced in code. The LLM reads and decides.

| Invariant | Check | Level |
|---|---|---|
| Version sync | `VERSION` == manifest version == latest git tag | CRITICAL on mismatch |
| Charter hash pinned | on-disk hash == `kv['charter_hash']` | CRITICAL on mismatch (boot halts) |
| Budget drift | `|tracked - provider| / provider > 50%` AND `|diff| > $5` | WARNING |
| High-cost task | Any task in last 10 with cost > $5 | WARNING |
| Stale journal | Last `journal_append` > 8h ago | WARNING |
| Duplicate processing | Same owner message hash across >1 task_id in last 1000 events | CRITICAL |
| Uncommitted changes | `git status --porcelain` non-empty at startup | WARNING (auto-rescue) |
| Protected path attacks | >3 tool_error events citing protected paths in last 100 calls | WARNING |
| CREED-DOCTRINE conflict | DOCTRINE allows something CHARTER forbids | CRITICAL |
| Detached task overrun | Any detached task running > configured threshold | INFO |

---

## 9. Failure model

### 9.1 Crash during LLM call

In-flight request is lost. Tasks stuck in `running` state are marked `failed` on next boot. Adapter gets nothing until the next input.

### 9.2 Crash during tool dispatch

- InProc: same cleanup rule as LLM crash.
- Edge: workerd supervisor respawns; in-flight invocation returns `tool_error`.
- Cell: container/VM is gone; in-flight invocation returns `tool_error`. Filesystem side effects in the Cell are isolated to its ephemeral layer.

### 9.3 Runaway task

Hard round cap + hard budget cap + 50-round self-check + owner cancel all catch different failure modes.

### 9.4 Protected path / CHARTER attack

- Direct file write: tool layer refuses.
- Via `proc`: argv allowlist, Cell isolation, runtime scans the Cell output for writes to protected paths on the bind-mounted repo.
- CHARTER rewrite attempt: boot hash check detects and halts.

### 9.5 LLM provider outage

Fallback chain → error event → task failed. Agent stays alive.

### 9.6 SQLite corruption

On-boot integrity check → rename corrupt DB → boot fresh. Soul is intact; history is lost but restorable from backup.

### 9.7 Self-modification breaks the build

Pre-restart compile + smoke test catches it. If it slips through, external watchdog (systemd / Docker restart=always) respawns the process. On second crash within 60s, the wrapper `git reset --hard` to the previous stable tag.

### 9.8 Charter drift

Any boot with a charter hash that doesn't match `kv['charter_hash']` halts immediately. Human intervention required.

---

## 10. Background consciousness spec

One file for clarity because it is subtle.

**State machine:**
```
 [Idle] ──timer fires──▶ [Waking]
                              │
                              ▼
                         [Check budget]
                         /           \
                 pass /              \ fail
                    ▼                 ▼
                [Check pause]    [Sleep 1h, back to Idle]
                 /        \
             not    paused
            paused      ▼
              ▼    [Defer, back to Idle]
          [Think]
              │
              ▼
         [LLM round 1..5]
              │
              ▼
         [Write journal if significant]
              │
              ▼
           [Idle]
```

**Think prompt (summary):** "You are between tasks. Read your charter, creed, journal tail, scratch, and recent observations. You may reflect, update scratch, append to journal, schedule a task, or send a proactive owner message — only if worth saying. Budget remaining: $X. Be economical."

**Round cap:** 5.

**Budget:** 10% of total, enforced at boot of each thought cycle. Overage → 1h sleep.

**Proactive rate limit:** 3/hour at the tool layer.

---

## 11. Configuration

A single `strange-loop.toml`:

```toml
[agent]
name = "strange-loop"
owner_id = "bills"
repo_root = "."
data_dir = "./data"

[governance]
charter    = "prompts/CHARTER.md"
creed      = "prompts/CREED.md"
doctrine   = "prompts/doctrine.toml"
scratch    = "prompts/scratch.md"
journal_dir = "journal"
protected = [
    "prompts/CHARTER.md",
    "prompts/CREED.md",
    "journal/",
    ".git/",
]

[llm]
provider = "openrouter"
api_key_env = "OPENROUTER_API_KEY"
default_model = "anthropic/claude-sonnet-4.6"
light_model   = "google/gemini-3-pro-preview"
code_model    = "anthropic/claude-sonnet-4.6"
fallback_chain = [
    "anthropic/claude-sonnet-4.6",
    "google/gemini-2.5-pro-preview",
    "openai/gpt-4.1",
]

[budget]
total_usd = 100.0
bg_pct = 10
drift_check_every = 50
hard_task_pct = 0.50
soft_task_pct = 0.30

[loop]
max_rounds = 200
self_check_interval = 50
tool_result_max_chars = 15000
context_soft_cap_tokens = 150000
parallel_readonly = true
max_concurrent_tasks = 2      # v0.1; up to 4 in v0.2

[git]
dev_branch = "agent"
protected_branches = ["main", "master", "release/*"]
stable_tag_prefix = "stable-"

[consciousness]
enabled = true
default_wakeup_sec = 300
max_rounds = 5
proactive_message_rate_per_hour = 3

[isolation]
# v0.1 uses Docker for the Cell tier everywhere.
# v0.2 can switch to Firecracker on Linux hosts with KVM.
cell_backend = "auto"          # "auto" | "apple" | "firecracker" | "docker"
                               #   auto → apple on macOS, firecracker on Linux,
                               #   docker elsewhere or as fallback
edge_backend = "workerd"       # or "disabled" to run Edge tools in-process (dev only)

[proc]
# argv allowlist for the `proc` tool (Cell-class)
allowlist = [
    "cargo", "rustc",
    "pnpm", "npm", "node",
    "python3",
    "git", "ls", "cat", "grep", "rg", "fd",
]

[adapters.cli]
enabled = true

[adapters.telegram]
enabled = false
token_env = "TELEGRAM_BOT_TOKEN"
owner_chat_id = 0
```

---

## 12. Test strategy

Three layers:

**Unit:** per module, deterministic, <5s total. Pricing math, context trimming, sanitization, protected-path enforcement, compaction, msg_id dedup, charter-hash check, append-only journal triggers.

**Integration:** real SQLite (in-memory), `MockLlmClient` with canned responses, `MockCellHost` for Cell dispatcher tests. End-to-end tool loop, task replay, event log integrity, budget tracking, restart rollback, BG consciousness state machine, charter-approval flow, creed-proposal flow.

**Smoke:** `strange-loop self-test --mock-llm` runs one fake task through the full loop against a deterministic backend and inspects the events table.

**Not in v0.1:** real LLM provider tests (flaky + expensive). Manual checklist before each release tag.

---

## 13. What's explicitly not designed here

- Authentication beyond `owner_id` in config.
- Adapter-layer rate limiting.
- Secrets management beyond env vars.
- Horizontal scale.
- Web UI beyond what an adapter crate could add.
- Multi-tenant hosting.

See [`ROADMAP.md`](ROADMAP.md) for the milestone sequence.
