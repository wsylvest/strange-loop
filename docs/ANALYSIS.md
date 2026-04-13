# Ouroboros: Architectural Analysis

A cold-eyed review of the predecessor system. Every design decision is evaluated against a single question: **does it survive into strange-loop?**

## 1. What Ouroboros Actually Is

Stripped of its philosophy, Ouroboros is a **long-running LLM tool-loop** with four loadbearing features:

1. **Persistent identity** — two markdown files (`BIBLE.md`, `identity.md`) and a scratchpad are injected into every prompt so the model "remembers" who it is across restarts.
2. **Self-modification** — the tool set includes git commit/push and a Claude Code CLI bridge, so the LLM edits its own source and restarts itself.
3. **Budget-governed loop** — every LLM round accumulates USD cost against a hard cap; soft nudges at 30%, hard stop at 50% of remaining budget per task.
4. **Background consciousness** — a lightweight cheap-model loop that wakes every N seconds between tasks and can write the owner unprompted.

Everything else — the Telegram bot, the Colab bootstrap, the multiprocessing workers, the Google Drive state — is delivery machinery, not the thing itself.

## 2. What Works (keep)

### 2.1 LLM-first control flow (Bible P3)

No if/else for behavior routing. No regex intent detection. The LLM owns the decision graph; Python is transport. **This is the single most important design property to preserve.** It's why a message like `/status` can be handled by the supervisor *and* still reach the LLM ("dual-path") — the LLM gets to decide whether to comment on the status.

Concretely this means the tool loop has:
- no task type dispatch table
- no keyword matching for intents
- no hand-coded "if this then that" flows

Dedup, compaction, command parsing — all delegated to a light model call.

### 2.2 Three-block prompt caching

`context.py` builds the system prompt as three content blocks with `cache_control` set independently:

- **static** (1h TTL): SYSTEM.md + BIBLE.md (+ README for evolution/review tasks)
- **semi-stable** (ephemeral): identity.md, scratchpad, knowledge index
- **dynamic** (uncached): state.json, runtime, recent logs, health invariants

On Anthropic, this collapses ~100k cached tokens into ~$0.30 per call instead of ~$3. It is the single largest cost optimization in the system and **should survive verbatim**.

### 2.3 Selective tool schemas

A core set of ~29 always-on tools plus gated "discoverable" tools accessed through `list_available_tools` / `enable_tools` meta-tools. Saves ~40% of schema tokens per round on Anthropic pricing. The LLM decides which extras to load.

### 2.4 Per-round event log as source of truth

`events.jsonl` is append-only. Budget, task metrics, health invariants, duplicate-processing detection all read from this one file. This is the right call — it survives process crashes, is diffable, and can be replayed. The invariant "llm_usage events emit per-round, never per-task aggregate" was learned the hard way (v6.2.0 fix for 2x budget drift).

### 2.5 Health invariants as text in context (Bible P0 × P3)

Instead of hardcoding recovery logic, `_build_health_invariants()` scans state and appends text like `CRITICAL: VERSION DESYNC` into the prompt. The LLM reads it and decides what to do. This is the cleanest expression of LLM-first self-detection in the codebase. **Keep.**

### 2.6 Per-task owner mailbox

While a worker is in a multi-round tool loop, the supervisor can write messages into `owner_mailbox/{task_id}.jsonl`. The worker drains them at the top of each round and injects them as user messages. Dedup via `msg_id`. This lets the owner steer in flight without killing the task.

### 2.7 Soft checkpoints every 50 rounds

Every 50 rounds, a system message asks the LLM: *"Am I making progress? Should I stop? Should I compact context?"* No hard kill. The LLM decides. This is a cognitive feature, not an enforcement feature. **Exactly right.**

### 2.8 Tool execution timeouts with thread-sticky stateful pool

Generic tools run in ephemeral `ThreadPoolExecutor(max_workers=1)` with per-tool timeout. Playwright-based browser tools run in a *sticky* executor pinned to one thread (Playwright sync API requires greenlet thread-affinity). On timeout the sticky pool is shut down and recreated. **This pattern generalizes** — anything stateful that touches a native handle needs thread affinity.

### 2.9 Budget drift detection

Every 50 LLM calls, the supervisor calls OpenRouter's ground-truth usage endpoint and compares session-delta to internally-tracked session-delta. Drift > 50% fires an alert. **Critical for a system that autonomously spends money.**

## 3. What Hurts (fix or delete)

### 3.1 Google Drive as the state layer

Drive-mounted FUSE is the worst database. It's slow, not atomic, doesn't respect flock, and dies on Colab disconnect. The state-locking dance in `supervisor/state.py` exists solely to paper over this. A whole class of bugs (`STATE_LOCK held during HTTP call`, state snapshot races, stale `.lock` cleanup) is pure Drive tax.

**Replace with:** embedded SQLite (Rust: `rusqlite`; Node: `better-sqlite3`) + a flat `logs/` directory for append-only JSONL. SQLite gives atomic writes, WAL, and kills the lock file entirely.

### 3.2 Multiprocessing workers (revised: hybrid model)

The launcher spawns N OS processes via `multiprocessing.fork()` on Linux (because `spawn` re-imports `colab_launcher.py` which has top-level side effects — that's a launcher bug, not a requirement). Each worker has a duplicated `OuroborosAgent` instance, its own `ToolRegistry`, its own LLM client. Crash storms, zombie detection, SHA verification after spawn, per-task mailbox on Drive — all of this exists because workers can't share memory.

**What was the point?** Four things: process isolation (a worker crash doesn't take the agent down), parallel task execution, GIL escape for CPU-bound tool work, and clean cancellation via SIGTERM. An earlier draft of this document proposed a pure single-process replacement; on reflection that was too aggressive. Two of the four benefits (isolation, cancellation) are genuinely valuable; one (GIL) is a Python-specific concern that does not survive the rewrite into Rust or Node; one (parallelism) matters for long-running tool work but can be achieved with async tasks in one process.

**Revised replacement:** a **hybrid model** — one long-running parent process that hosts the scheduler, the event bus, the store, and the LLM client, plus three isolation classes for tool execution:

- **InProc** — tools that run as async tasks in the parent. Fast, trusted, have direct SQLite access. fs, git plumbing, memory, LLM fan-out.
- **Edge** — tools that run in a workerd subprocess with declared capabilities only. Pure JSON-in/JSON-out. URL fetch-and-strip, schema validators, third-party API wrappers. Good for processing untrusted input because V8 isolates cannot escape their declared capabilities.
- **Cell** — tools that run in a Docker container (v0.1) or Firecracker microVM (v0.2 on Linux). Anything that touches a real filesystem, runs binaries, or handles potentially hostile material. `proc`, `run_tests`, Playwright. Per-invocation or warm-pooled; killed and respawned on timeout.

This preserves isolation (tool crashes can't take the parent down, because the tool wasn't in the parent) and gains stronger isolation than Ouroboros had (Cell tier has hardware-level boundaries, not just process boundaries). It preserves parallelism (async tasks in the parent plus detached tool invocations). It drops the Ouroboros worker code — `workers.py`, crash-storm detection, SHA-verify-after-spawn, the direct-chat fallback, the per-task mailbox-on-Drive — because those exist to work around problems a single parent process with tiered tool hosts does not have.

See SYSTEM_SPEC §3 for the full process topology.

### 3.3 Telegram as the only frontend

Telegram is fine as *a* frontend. Baking it in as *the* frontend means the control plane, the identity claim ("first writer is the owner"), the message routing, and the command surface all live in `telegram.py`. Anything non-Telegram (web UI, CLI, IDE extension) requires surgery.

**Replace with:** a thin transport abstraction. Core emits `OwnerMessage` and consumes `AgentMessage`; adapters (Telegram, CLI, web, Slack) implement one trait/interface. First-class CLI adapter for development — you should be able to run `strange-loop chat` and talk to the agent with no Telegram at all.

### 3.4 Colab bootstrap shim

`colab_bootstrap_shim.py` exists to re-point git origin to the user's fork, mount Drive, install deps. It's ~100 lines of bootstrapping that only works in one environment. Modern deployment (Docker, systemd, a single binary) makes it obsolete.

**Replace with:** `cargo build --release` or `pnpm build` → one artifact. Config via env/TOML. Run anywhere with a filesystem.

### 3.5 SYSTEM.md as a 440-line god prompt

The system prompt contains file-path documentation, tool usage recipes, version-bump checklists, code-editing strategy, drift detectors, message dispatch rules. It's doing five jobs: identity, policy, tool docs, release runbook, operational playbook. This bloats every request and is hard to evolve — small edits risk regressions in the tool loop.

**Replace with:** split into:
- `IDENTITY.md` (agent self-concept — small, durable, user-editable)
- `CONSTITUTION.md` (principles — stable)
- `POLICIES.md` (do/don't rules — domain-specific, swappable per deployment)
- Tool docs embedded in tool schemas themselves (the LLM already sees them there)
- Runbook text lives outside the hot prompt path and is loaded only for `release`/`evolution` tasks

### 3.6 Four overlapping JSONL logs

`chat.jsonl`, `progress.jsonl`, `tools.jsonl`, `events.jsonl`, `supervisor.jsonl`. Each has its own reader, its own summarizer, its own rotation policy. Context builder reads all five and concatenates summaries into the prompt.

**Replace with:** one `events` table (SQLite) with an event-type column and structured fields. Summarization becomes a SQL query. Rotation becomes `DELETE WHERE ts < ?`. Test surface shrinks massively.

### 3.7 Worker-0 edge cases and None-safe checks everywhere

`int(x or -1)` treated worker 0 as -1, causing hard-timeout not to fire. Every `or default` in the code was audited in v6.2.0. This is a signal that the type system is too loose. Rust's `Option<WorkerId>` and Node/TS `number | null` with strict checks eliminate this class of bug at compile time.

### 3.8 `apply_patch.py` + Claude Code CLI bridge

Shells out to a CLI that emits a custom patch format, then parses it back, then applies it. Two failure modes, two parsers, one subprocess per edit. The CLI is also unavailable in some environments.

**Replace with:** direct file writes (the LLM already has `repo_write_commit`), plus a structured edit tool backed by the host language (Rust: `similar` for diff; Node: `diff` package). No subprocess bridge. Claude Code CLI becomes an *optional* alternate path, not the primary.

### 3.9 Fork-bomb protection via task_depth

`ToolContext.task_depth` tracks recursion to prevent the LLM from infinitely scheduling subtasks. Works, but it's a runtime check for what should be a scheduler invariant: the scheduler refuses to enqueue children beyond depth N.

### 3.10 Hard-coded pricing table

`_MODEL_PRICING_STATIC` lives in `loop.py` and drifts from reality. There is already a fallback `fetch_openrouter_pricing()` — just always use it, cache for 24h in SQLite, and delete the static table.

## 4. Counted Complexity

| Module | Lines | Keep (%) |
|---|---:|---:|
| `ouroboros/loop.py` | 979 | 60% |
| `ouroboros/agent.py` | 655 | 40% |
| `ouroboros/context.py` | 770 | 50% |
| `ouroboros/consciousness.py` | 478 | 70% |
| `ouroboros/llm.py` | 295 | 80% |
| `ouroboros/memory.py` | 244 | 30% |
| `ouroboros/tools/*` | ~2500 | 60% |
| `supervisor/state.py` | 661 | 10% (replaced by SQLite) |
| `supervisor/queue.py` | 421 | 30% |
| `supervisor/workers.py` | 588 | 10% (hybrid model; see §3.2) |
| `supervisor/telegram.py` | 477 | 20% (becomes adapter) |
| `supervisor/events.py` | 480 | 50% |
| `supervisor/git_ops.py` | 430 | 50% |
| `colab_launcher.py` | 727 | 10% |
| `apply_patch.py` | 178 | 0% (deleted) |

Roughly **9,000 Python lines → target ~4,000 Rust lines or ~5,000 TS lines** for equivalent capability.

## 5. Things That Must Not Regress

These are earned in production. Any rewrite must preserve them on day one:

1. **Per-round budget events, never per-task aggregate.** v6.2.0 ate this lesson twice.
2. **HTTP calls stay outside state locks.** v6.0.0 deadlock.
3. **Explicit executor lifecycle** — no `with ThreadPoolExecutor` context managers for long-lived tool pools. In Rust this is free; in Node use explicit `.close()`.
4. **Single-consumer message routing.** Every owner message hits exactly one handler. No broadcast.
5. **Fallback chain handles primary == first-fallback.** Common failure mode.
6. **`shlex.split` (or equivalent)** for shell commands — never string split.
7. **Version sources in sync** — `VERSION`, `Cargo.toml`/`package.json`, changelog, git tag. Enforced in CI.
8. **Identity core untouchable.** The rewrite must have a mechanism to mark specific files/paths as protected from the agent's own write tools.
9. **Duplicate message dedup** via msg_id across mailbox drains.
10. **Owner message during task** marker is high priority and must reach the LLM on the next round, not the next task.

## 6. What We're Consciously Throwing Away

- "First writer becomes owner" auth. Replace with a config-file owner id.
- `/panic` as SystemExit. Replace with graceful shutdown on SIGTERM.
- Crash-storm detection via rolling crash timestamps. A single-process model doesn't have this problem.
- Auto-resume after restart via synthetic message injection. The LLM should read its own scratchpad on boot and decide to continue; no synthetic user messages.
- `promote_to_stable` as a branch. Replace with git tags — strictly more standard.
- The `evolution`/`review`/`user`/`scheduled` task-type enum. Task type is LLM-visible metadata, not a dispatch key.
- The hardcoded `_MODEL_PRICING_STATIC` table.
- The three README changelog limit ("2 major, 5 minor, 5 patch") — it's arbitrary and ChangeLog.md is a better home.
