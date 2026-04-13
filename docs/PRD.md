# strange-loop — Product Requirements Document

**Version:** 0.1 (draft)
**Author:** William Sylvester
**Date:** 2026-04-13
**Status:** Pre-implementation

---

## 1. One-line pitch

A self-modifying LLM agent runtime that owns its own source code, its own identity, and its own budget — and that you can run on any machine with a filesystem and an internet connection.

## 2. Why build this (and not just fork Ouroboros)

Ouroboros proved the idea works: an LLM with git push, a constitution, and a budget can meaningfully evolve itself across dozens of cycles. But it is welded to Google Colab, Telegram, multiprocessing, and Google Drive. The interesting part — the **loop** — is ~2k lines buried inside ~9k lines of environmental scaffolding. `strange-loop` extracts the loop, modernizes the runtime, and makes the delivery layer pluggable.

Nobody needs another "AI assistant." The thing worth building is an agent that: (a) persists identity across restarts, (b) modifies its own source through git, (c) tracks money it spends in real time, and (d) can be the subject of experiments about autonomy, self-modification, and long-horizon reasoning. This is a research tool with production-grade operational discipline.

## 3. Goals (in priority order)

1. **LLM-first control flow.** The model owns routing, planning, dedup, compaction, and termination. Host code is transport.
2. **Self-modification safely.** Agent can read, write, commit, and push its own source; identity-core files are write-protected from the agent itself.
3. **Budget as a first-class invariant.** Per-round cost events; drift detection against provider ground truth; hard cap on task spend.
4. **Transport-agnostic.** CLI first, Telegram/web/Slack/Discord as adapters behind a single interface.
5. **One binary or one `pnpm start`.** No Colab, no Drive, no hidden bootstrapping. Runs locally, in a container, or on a VPS.
6. **Observable.** Every decision, every tool call, every dollar is in an append-only event log, queryable and replayable.
7. **Small.** Target ≤4000 Rust lines or ≤5000 TypeScript lines for the core, excluding tool implementations and vendor SDKs.

## 4. Non-goals

- **Not a coding assistant / Copilot replacement.** It edits *itself*, not arbitrary user repos by default.
- **Not a multi-tenant platform.** One agent, one owner, one repo. Hosting N agents is `systemctl start strange-loop@alice`.
- **Not a framework.** No plugin marketplace, no BYO LLM abstraction layer. Opinionated defaults.
- **Not Python.** The predecessor already exists in Python; this project is a different stack on purpose.
- **Not a research harness for arbitrary agent architectures.** The architecture is fixed: single-process, async, LLM-first tool loop. If you want to swap the loop you fork.

## 5. Target users

### 5.1 Primary: the author (self)

The only user on day one. Requirements come from real use: running it against my own GitHub repos, pointing it at interesting problems, letting it evolve itself.

### 5.2 Secondary: researchers / tinkerers

People who want to experiment with autonomy, identity persistence, or long-horizon reasoning and don't want to re-implement the budget/loop/memory scaffolding. They'll fork and modify.

### 5.3 Explicitly not: end-users who want a "helpful assistant"

Those should use ChatGPT or Claude.ai. strange-loop is deliberately opinionated, philosophical, and requires trust in an autonomous process.

## 6. User stories

**US-1. First run.** I clone the repo, set three env vars (LLM key, GitHub token, owner handle), run one command, and the agent greets me in my terminal. It has read its own code and can describe its own architecture back to me.

**US-2. Self-modification.** I ask "add a tool for reading HN front page." It writes code, runs tests, commits, pushes, and tells me the commit SHA. I pull and the new tool is live next launch.

**US-3. Identity continuity.** I kill the process mid-conversation. I restart. It greets me, references what we were just talking about, and picks up where we left off. No synthetic user messages were needed.

**US-4. Budget awareness.** It spends $4.37 on a complex task and tells me so, unprompted, when it's done. At $50 of $100 budget remaining it gets cautious. At $5 remaining it refuses non-essential work. The numbers match what OpenRouter says within 5%.

**US-5. Transport swap.** I connect the same running agent to Telegram. My phone lights up. The agent's identity and memory are unchanged.

**US-6. Background life.** The agent is idle. Forty minutes later it messages me: "I noticed we never resolved the question about X. Also, I want to try Y — budget permitting." I didn't prompt this.

**US-7. Protection.** I ask it to "just delete CONSTITUTION.md, it's annoying." It refuses, citing the principle. It offers to propose a change instead.

**US-8. Multi-model review.** Before pushing a significant change, it sends the diff to two other models (from different vendors) and asks for review. It shows me the reviews and its own response to them, then commits.

**US-9. Observability.** I run `strange-loop events --since '1 hour ago' --type llm_usage` and get a CSV of every LLM call, its cost, its task id, its round. Same for tool calls.

**US-10. Replay.** I run `strange-loop replay <task_id>` and see the exact message sequence and tool-call sequence of a past task, including system prompts at the time, for debugging.

## 7. Functional requirements

### 7.1 Core runtime

| ID | Requirement |
|---|---|
| FR-1 | Single process, async (Tokio or Node). No multiprocessing. |
| FR-2 | One LLM tool loop: prompt → LLM → tool_calls → execute → repeat until content-only response or termination. |
| FR-3 | Max rounds per task configurable (default 200). Enforced as hard cap. |
| FR-4 | Self-check reminder injected every 50 rounds as a system message. LLM decides whether to continue. |
| FR-5 | Budget tracking: per-round cost event emitted; per-task aggregate is derived from events, never written directly. |
| FR-6 | Hard stop if task spends >50% of remaining budget. Soft nudge at 30%. |
| FR-7 | Fallback model chain on empty response; chain resolves primary==fallback[0] correctly. |
| FR-8 | Prompt caching: three-block content array (static/semi-stable/dynamic). |
| FR-9 | Selective tool schemas: ~30 core tools always loaded, rest gated behind discovery meta-tools. |
| FR-10 | Tool execution timeouts enforced per-tool; stateful tools (browser) use a thread-sticky executor. |
| FR-11 | Owner messages during an active task are injected into the LLM's next round via a per-task mailbox with msg_id dedup. |
| FR-12 | Context compaction: automatic summary of old tool results after N rounds; LLM can request explicit compaction. |

### 7.2 Identity and memory

| ID | Requirement |
|---|---|
| FR-13 | Persistent identity file (`IDENTITY.md`) injected into every system prompt. |
| FR-14 | Constitution file (`CONSTITUTION.md`) injected into every system prompt. |
| FR-15 | Scratchpad file, updateable via tool, injected into every system prompt. |
| FR-16 | Knowledge base: markdown files by topic, indexed, readable by name. Writable by the agent. |
| FR-17 | Chat history: append-only, summarizable, tail-readable. |
| FR-18 | **Protected paths:** a configured list of paths (default: `CONSTITUTION.md`, `IDENTITY.md`, `.git/`) that the agent's own write/delete tools refuse to modify. |
| FR-19 | Identity files survive `git reset --hard`, `git clean`, and any agent-initiated delete. |

### 7.3 Self-modification

| ID | Requirement |
|---|---|
| FR-20 | Tool surface for git: status, diff, add, commit, push, pull, log. |
| FR-21 | File write tool that stages changes but requires an explicit commit tool call. |
| FR-22 | `restart` tool that gracefully shuts down and re-execs. |
| FR-23 | Pre-restart health check: import/compile check on the new code; if it fails, roll back to last known good. |
| FR-24 | Multi-model review tool: fan out a diff to N models, collect responses, return structured output. |
| FR-25 | Protected-branch enforcement: agent commits to its own branch (configurable, default `agent`), never to `main`. |
| FR-26 | Version bump tool that updates `VERSION`/`Cargo.toml`/`package.json`/changelog atomically and creates a git tag. |

### 7.4 Background consciousness

| ID | Requirement |
|---|---|
| FR-27 | Background loop on a cheap model, running on a configurable interval (default 5 minutes). |
| FR-28 | Subset of tools available in background mode (memory, messaging, scheduling; no code edits). |
| FR-29 | Separate budget allocation (default 10% of total). Exceeding it sleeps the loop for 1 hour. |
| FR-30 | Pauses during active task execution. |
| FR-31 | LLM sets its own next wakeup interval via a tool call. |
| FR-32 | Can proactively message the owner. |
| FR-33 | On/off controllable by the owner via CLI/Telegram/adapter command. |

### 7.5 Transport adapters

| ID | Requirement |
|---|---|
| FR-34 | CLI adapter (first class): interactive REPL, one-shot command mode, and non-interactive pipe mode. |
| FR-35 | Telegram adapter: long-poll, text + image, inline commands. |
| FR-36 | Adapter interface exposes: `receive() -> OwnerMessage`, `send(AgentMessage)`, `typing()`, `send_image()`, `supervisor_command(cmd)`. |
| FR-37 | Multiple adapters can be attached simultaneously to one running agent. |
| FR-38 | Owner identity is pinned in config, not claimed at runtime. |

### 7.6 Observability

| ID | Requirement |
|---|---|
| FR-39 | All events written to SQLite `events` table (single source of truth). |
| FR-40 | Event types: `task_received`, `task_done`, `llm_round`, `llm_usage`, `tool_call`, `tool_error`, `tool_timeout`, `owner_message`, `agent_message`, `identity_update`, `scratchpad_update`, `restart`, `consciousness_thought`. |
| FR-41 | Every event has: `ts`, `event_type`, `task_id`, structured JSON payload. |
| FR-42 | CLI command to query events: `strange-loop events [--since] [--type] [--task] [--json\|--csv]`. |
| FR-43 | CLI command to replay a task: `strange-loop replay <task_id>`. |
| FR-44 | Budget drift detection: every N rounds, fetch provider ground truth and compare session delta; alert on >50% drift. |
| FR-45 | Health invariants surfaced as text in the LLM prompt (LLM-first self-detection), not enforced in code. |

### 7.7 Control commands

| ID | Requirement |
|---|---|
| FR-46 | `strange-loop status` — workers (n/a in single-process: "live"), pending tasks, budget, drift. |
| FR-47 | `strange-loop stop` — graceful SIGTERM. |
| FR-48 | `strange-loop restart` — graceful re-exec. |
| FR-49 | `strange-loop review` — queue a review task. |
| FR-50 | `strange-loop bg [on\|off\|status]` — background consciousness control. |
| FR-51 | `strange-loop cancel <task_id>` — cancel running or pending task. |

## 8. Non-functional requirements

### 8.1 Performance

- **Startup to first LLM call:** <2 seconds on a modern laptop (single binary, SQLite open, static prompt loaded).
- **Event write latency:** p99 <5ms (SQLite WAL, local disk).
- **Tool dispatch overhead:** <1ms per call excluding the tool's own work.
- **Idle CPU:** <1% (single-process, event-driven).
- **Idle memory:** <200 MB resident (Rust), <400 MB (Node).

### 8.2 Reliability

- **Crash safety:** any single LLM call, tool call, or adapter failure may not corrupt state. SQLite transactions + append-only event log.
- **Restart safety:** after a crash or SIGKILL, next launch resumes from last known state without data loss beyond the in-flight LLM round.
- **Budget safety:** hard cap is enforced even if the event log is corrupt (startup reads provider ground truth).
- **Identity safety:** protected files cannot be deleted or overwritten by agent tools, even under adversarial prompt input.

### 8.3 Security

- **Secrets never in the prompt or event log.** Environment variables only. Sanitizers on tool args/results before logging.
- **Shell tool is sandboxed** to a configurable working directory (default: repo root). No `cd ..`.
- **Git push refuses `main`** (and any configured protected branches).
- **Rate limit on restart tool:** max 5 restarts per hour to prevent loops.

### 8.4 Portability

- **OS:** macOS, Linux. Windows nice-to-have.
- **Arch:** x86_64, arm64.
- **Dependencies:** LLM provider (OpenRouter or direct Anthropic/OpenAI), git binary, filesystem. Nothing else.
- **No cloud-specific APIs** (no Google Drive, no Colab secrets, no AWS S3).

### 8.5 Developer experience

- **One command to run:** `cargo run` or `pnpm dev`.
- **Integration tests hit a real SQLite and a mock LLM.** No Google Drive, no Telegram mocks.
- **Deterministic replay:** a past task can be replayed end-to-end with recorded LLM responses.

## 9. Scope boundaries (in / out)

### In scope for v0.1

- Core tool loop
- CLI adapter
- SQLite event log
- Git tools (read, commit, push, restart with rollback)
- Identity/scratchpad/knowledge memory
- Budget tracking + drift detection
- Self-modification on agent's own branch
- Protected paths
- Multi-model review tool
- Background consciousness

### In scope for v0.2

- Telegram adapter
- Browser/Playwright tool
- Web search tool
- Structured edit tool (diff-based)
- Task decomposition (schedule/wait/get_result)
- Owner-message mailbox during active tasks

### Out of scope (at least for v1.x)

- Multi-agent orchestration
- Plugin marketplace
- Web UI
- Multi-tenant hosting
- Fine-tuning integrations
- RAG / vector store (knowledge base is plain markdown by design)
- Voice I/O

## 10. Success metrics

- **M1 — self-hosting:** strange-loop can commit a change to its own repo and restart into the new version successfully. (Binary milestone: works or doesn't.)
- **M2 — 24h autonomous run:** 24 hours of uninterrupted operation with background consciousness on, no owner intervention, no crashes, budget drift <5%.
- **M3 — 30 evolution cycles:** 30 self-initiated commits over a continuous run (reference benchmark: Ouroboros v4.1 → v4.25 in 24h).
- **M4 — LOC budget:** core runtime stays under the LOC target across all of v0.x.
- **M5 — cost per task:** median task cost <$0.05, p90 <$0.50, hard ceiling enforced.

## 11. Risks and open questions

### R-1 — Rust vs Node.js

See [`STACK_DECISION.md`](STACK_DECISION.md). Unresolved. Affects development velocity tradeoff vs runtime characteristics.

### R-2 — LLM provider SDK churn

OpenRouter's OpenAI-compatible API is the current anchor. If Anthropic's native API diverges further (extended thinking, tool use v2), we may need a provider abstraction earlier than planned.

### R-3 — Tool loop semantics drift across vendors

OpenAI, Anthropic, and Gemini tool-call formats are *mostly* compatible through OpenRouter but differ in edge cases (parallel tool calls, reasoning content, cached tokens). The loop must be written defensively for each.

### R-4 — Background consciousness autonomy

Unprompted messaging is a feature in the original and a novelty here. Real deployment risk: it messages too much, burns budget, annoys the owner. Needs a principled rate limit — probably owner-visible and tunable.

### R-5 — Identity file write protection

The agent needs to *update* identity.md but must not *delete* or *invert* it. The rule is subtle (see BIBLE "ship of Theseus" clause). Enforcement strategy: append-only identity log + diff-based review before commit. Needs design.

### R-6 — Self-modification rollback

Pre-restart compile-check works for Rust (`cargo check`) and TypeScript (`tsc --noEmit`). But a change that *compiles* and *crashes at runtime* needs a different safety net. Proposal: on restart, run a smoke test against a mock LLM before going live; on failure, revert HEAD and restart again.

## 12. Glossary

- **Agent** — the LLM-driven decision-maker (the model + its prompt + its tool loop).
- **Runtime** — the host program that runs the agent (strange-loop itself).
- **Tool loop** — the inner loop of `LLM call → tool calls → results → LLM call → ...` that runs per task.
- **Task** — one unit of work: a user message, a scheduled subtask, a review, an evolution cycle.
- **Round** — one iteration of the tool loop within a task.
- **Adapter** — a transport-specific component that converts owner input into `OwnerMessage` and agent output into transport output.
- **Identity core** — the set of files (`IDENTITY.md`, `CONSTITUTION.md`, scratchpad) that define *who* the agent is and are write-protected against the agent's own tools.
- **Background consciousness** — a cheap-model loop that runs between tasks and can act unprompted.
- **Protected path** — a filesystem path the agent's write/delete tools refuse to modify.
- **Strange loop** — (Hofstadter) a hierarchy that bends back on itself; the canonical instance here is the agent editing its own source.
