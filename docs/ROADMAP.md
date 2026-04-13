# strange-loop — Roadmap

Milestone-ordered. Each milestone is a demonstrable capability; no milestone is complete until it's demonstrable on the author's machine.

---

## M0 — Scaffolding (week 1)

**Demonstrable:** `strange-loop --version` prints a version. `strange-loop self-test` exits 0.

- Cargo workspace with stub crates.
- `strange-loop.toml` config loader with `cell_backend = "auto"` detection (apple on macOS, firecracker on Linux, docker elsewhere).
- SQLite migrations (events, tasks, messages, kv, task_mailbox, knowledge, journal, creed_proposals, pricing tables).
- Journal append-only triggers verified in migration tests.
- `tracing` setup with JSON output to `data/logs/strange-loop.log`.
- CI matrix: `cargo check`, `cargo test`, `cargo clippy -- -D warnings` on macOS (Apple backend) *and* Linux (Firecracker backend). Both must pass before any PR merges.
- **Exit criterion:** empty binary + empty DB + `self-test` subcommand that runs a no-op and writes one event. Backend detection reports correctly on both macOS and Linux dev hosts.

## M1 — Minimum viable tool loop (weeks 2–3)

**Demonstrable:** CLI adapter chats with the agent. The agent can `fs_read` and respond.

- `LlmClient` trait + `OpenRouterClient` implementation with prompt caching.
- Mock LLM client for tests.
- Tool trait + registry + dispatcher.
- Event bus and SQLite writer.
- Core tool set: `fs_read`, `fs_list`, `fs_write` (with protected paths), `chat_history`, `scratchpad_update`.
- Tool loop with: hard round cap, budget tracking (per-round `llm_usage` events), retry + fallback chain.
- Context builder with three-block caching.
- CLI adapter with interactive mode.
- **Exit criterion:** `echo "read VERSION" | strange-loop chat` produces a correct response and `strange-loop events --type llm_usage` shows the cost.

## M2 — Self-modification (weeks 4–5)

**Demonstrable:** the agent can commit a change to its own repo.

- `git_status`, `git_diff`, `git_commit`, `git_push`, `git_log` tools.
- Protected branches enforcement.
- `version_bump` tool.
- `restart` tool with pre-restart compile check (`cargo check`) + smoke test (`self-test --mock-llm`).
- Restart rate limit (5/hour).
- `multi_model_review` tool (fan-out to configured models).
- **Exit criterion:** a chat session where the owner says "add a log line to event dispatch" results in a real commit on the `agent` branch that compiles and passes `self-test`.

## M3 — Identity and memory (week 6)

**Demonstrable:** the agent survives a restart mid-conversation and picks up where it left off.

- `IDENTITY.md`, `CONSTITUTION.md`, `scratchpad.md` plumbed into context builder.
- `identity_update` tool with audit events + append-only diff log.
- Knowledge base: `knowledge_read`, `knowledge_write`, `knowledge_list`.
- Health invariants in context (VERSION sync, stale identity, budget drift text).
- Startup verification: check protected files exist and are non-empty; refuse to boot otherwise.
- **Exit criterion:** kill -9 mid-task, restart, agent describes the last conversation and resumes.

## M4 — Background consciousness (week 7)

**Demonstrable:** the agent messages the owner unprompted.

- BG loop state machine (idle → wake → think → sleep).
- Budget cap at 10% of total.
- Tool whitelist in BG mode.
- Proactive message rate limit (3/hour).
- `set_next_wakeup` tool.
- Pause/resume on active task.
- **Exit criterion:** start the agent, walk away for an hour, come back to find the agent has messaged at least once with a non-trivial thought, and budget is within allocation.

## M5 — 24-hour autonomy run (week 8)

**Demonstrable:** 24 hours of uninterrupted operation, no owner intervention, budget drift <5%.

- Budget drift check against OpenRouter every 50 calls.
- `strange-loop events` query subcommand.
- `strange-loop replay <task_id>` subcommand.
- Cancellation flag plumbing.
- Crash-recovery pass on startup (`state='running'` → `state='failed'`).
- **Exit criterion:** a recorded 24h run with the event log inspected afterward. No crashes, drift <5%, at least 10 self-initiated events.

## M6 — Self-hosting loop (weeks 9–10)

**Demonstrable:** the agent commits a non-trivial change (new tool or new policy) to itself, restarts into it, and the new capability works.

- `structured_edit` tool (diff-based).
- `schedule_task`, `wait_for_task`, `get_task_result` tools.
- Fork-bomb protection via depth.
- Owner message mailbox for in-flight injection.
- **Exit criterion:** the agent, given a feature request, writes code, runs tests, commits, pushes, requests restart, preflight passes, restart completes, new tool is live.

## M7 — Second adapter (week 11)

**Demonstrable:** the same running agent responds on Telegram.

- Telegram adapter crate.
- Adapter trait finalization (`receive`, `send`, `typing`, `send_image`, `supervisor_command`).
- Multi-adapter concurrent attach.
- **Exit criterion:** start agent, attach CLI + Telegram, send messages on both, responses route correctly.

## M8 — Browser + web tools (weeks 12–14)

**Demonstrable:** the agent can fetch a web page, analyze a screenshot, and search the web.

- Decide Rust-native vs Node-sidecar for Playwright.
- `browse_page`, `browser_action`, `analyze_screenshot`, `vlm_query` tools.
- `web_search` tool (OpenAI Responses API or similar).
- Stateful tool pool (thread-sticky executor).
- **Exit criterion:** "find the current top story on Hacker News" yields a correct answer.

## M9 — Polish and release v0.1 (week 15)

- `CHANGELOG.md`.
- Install docs.
- Minimal seeded `CONSTITUTION.md`, `IDENTITY.md`, `POLICIES.md`.
- A recorded demo.
- Tag `v0.1.0`.

---

## Beyond v0.1

**v0.2:** Slack/Discord adapters, parallel subtasks (scheduler `max_parallel > 1`), owner-facing budget dashboard, VLM-native image support in user messages.

**v0.3:** Optional web adapter (HTTP API over the same event stream), CSV/JSON export, remote event tail.

**v0.x experiments:** alternative identity persistence models (structured vs narrative), long-context memory strategies beyond scratchpad + knowledge base, multi-agent swarms where strange-loop instances talk to each other.

---

## Cut-line rules

If we're running behind at any milestone, here's what gets cut in priority order:

1. **Drop browser tool from v0.1 entirely.** Ship without it. (Already planned.)
2. **Drop `multi_model_review` from M2.** The agent can self-commit without it; review is a quality layer, not a correctness layer.
3. **Drop Telegram adapter from v0.1.** CLI-only is fine for a first release.
4. **Drop `structured_edit` in favor of `fs_write` only.** The LLM can do full-file rewrites; structured edits are an optimization.
5. **Drop `schedule_task`/subtask machinery.** Tasks are sequential; the LLM can still decompose in-context.

Never cut, at any milestone:
- Protected paths
- Budget tracking per round
- The event log
- Pre-restart compile check
- 50-round self-check
- Three-block prompt caching
- LLM-first control flow (no intent regex, no dispatch tables)
