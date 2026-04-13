# Stack Decision: Rust vs Node.js

**Status:** RATIFIED 2026-04-13. Rust is the stack. Node.js is not a fallback; it is simply not the choice.
**TL;DR:** **Rust** for the core. The two-week escape hatch described below is preserved as a historical note but is not being exercised — we committed to Rust on the first line of code.

---

## 1. What we're actually choosing

The question is *not* "which language is better." It's: **which stack gets strange-loop to self-hosting (M1) with fewer regrettable decisions?**

Regrettable decisions in an agent runtime look like:
- Subtle concurrency bugs that only show up after a 24-hour autonomous run
- Loose types that let worker_id==0 masquerade as an error sentinel (the original's pain)
- Memory leaks in a long-running process that nobody notices until day three
- Dependency churn that breaks a self-modification cycle because a transitive dep changed its API

The loop logic is not algorithmically hard. The hard part is building something **reliable, observable, and small enough to fit in one head.**

---

## 2. Option A — Rust + Tokio

### Pros

1. **Type system kills a whole class of v6.2.0-style bugs.** `Option<WorkerId>`, exhaustive enums for events, `Result` everywhere. The original Ouroboros has a LOT of `x or -1` / `x or {}` defensive code; most of that disappears into the type system.
2. **One binary.** No runtime install, no `pnpm install` on deploy, no node_modules on Drive. `cargo build --release` → a ~15 MB binary that runs anywhere. This is a huge operational win for a self-modifying agent (one less moving part in the restart cycle).
3. **Performance headroom is basically infinite.** Idle at <50 MB resident, handles thousands of events/sec in SQLite without warming up. Never becomes the bottleneck.
4. **Tokio is the right shape.** The agent is one reactor, many tasks, a handful of blocking-pool operations. Async Rust was literally designed for this.
5. **`rusqlite` + `rusqlite::hooks`** give us direct, synchronous SQLite access that's faster and lower-allocation than any Node binding.
6. **`cargo check` as pre-restart compile gate** is fast and deterministic. TS `tsc --noEmit` is slower and less reliable as a correctness signal.
7. **Small dep tree.** We pick ~15 crates: `tokio`, `rusqlite`, `reqwest`, `serde`, `clap`, `anyhow`, `tracing`, `async-trait`, `chrono`, `uuid`, `dashmap`, `git2` (or shell out to git), `once_cell`, `sha2`, `toml`. We can audit every transitive one.
8. **The ecosystem is maturing exactly in our direction.** `async-openai`, `anthropic-sdk`, `genai`, `llm-chain` — there are now good enough LLM client crates we can borrow from or use directly.

### Cons

1. **Development velocity.** The first 500 lines of tool implementations will take 2–3x as long to write as the Node equivalent. Every JSON payload needs a `serde` struct. Every error needs a variant. This is real friction.
2. **LLM SDKs lag OpenAI/Anthropic by weeks-months.** We will probably end up writing our own thin HTTP client for OpenRouter rather than relying on a crate, at least for v0.1.
3. **Playwright in Rust doesn't really exist.** The browser tool is either (a) run a Node-based Playwright sidecar process, (b) use `fantoccini`/`thirtyfour` + WebDriver, or (c) drop the browser tool from v0.1. **Recommendation: drop from v0.1**, revisit in v0.2.
4. **Hot reload isn't a thing.** Every edit cycle during development is `cargo check` → `cargo run`, ~5–20s. For a prompt-engineering-heavy workflow, that adds up.
5. **Rust concurrency debugging** (panics inside async tasks, `Send` bound errors, lifetime wrangling around shared state) has a learning tax that Node doesn't charge.
6. **`git2` is libgit2 C bindings** and comes with platform-specific quirks. Calling the `git` binary via the `proc` tool infrastructure is simpler and more portable.

---

## 3. Option B — Node.js + TypeScript

### Pros

1. **Velocity.** You can write a working tool loop end-to-end in a day. Tool implementations are trivial: `async (ctx, args) => { ... }`. No type gymnastics.
2. **LLM SDKs are first-class.** `@anthropic-ai/sdk`, `openai`, `@google/generative-ai`, `@openrouter/ai-sdk-provider` all exist, are well-maintained, and track vendor changes within days.
3. **Playwright.** `@playwright/test` is Node-native. The browser tool is a one-day integration.
4. **Prompt iteration is fast.** `tsx watch` or `node --watch` gives you a working REPL in 500ms after edits. Essential for the hot-loop part of agent development.
5. **JSON-first is a feature here.** The LLM produces JSON, tools take JSON args, events are JSON — in TS this is all `unknown` that you narrow as needed, no impedance mismatch.
6. **`better-sqlite3`** is synchronous, fast (faster than most Rust SQLite bindings in realistic workloads because it skips the async runtime), and has a clean API.
7. **Kysely** (type-safe SQL query builder) gives us most of the type-safety benefits of Rust for the DB layer specifically.
8. **More contributors eventually.** If strange-loop ever has a second developer, TypeScript has an order of magnitude more applicable hires than Rust.

### Cons

1. **Type safety is aspirational.** `strict: true` helps, but the moment you touch a JSON response from an LLM provider, you're back in `any`-land until you write a validator (`zod`/`valibot`). This is where v6.2.0-class bugs live.
2. **Deployment is heavier.** `node_modules` is ~200 MB even for a modest project. The runtime is a dependency. On restart, you're praying nothing in `node_modules` got corrupted by a half-written install.
3. **Memory drift.** Long-running Node processes leak in surprising ways (closures holding references, event listeners accumulating, etc.). The original Python agent has the same class of problem; neither is better than Rust here.
4. **The Node event loop is fragile.** One stray synchronous `JSON.parse` of a 10 MB string blocks everything. In Rust, the compiler won't let you accidentally block the reactor.
5. **`tsc` as a pre-restart gate** is slow (2–5s cold) and doesn't catch runtime errors. The smoke-test fallback is more load-bearing in the Node variant.
6. **`Next.js` was mentioned in the original brief but is the wrong shape.** strange-loop is a daemon, not a web app. A Next.js-scaffolded project drags in React, SSR, route handlers, and an opinionated file layout we don't want. If we go Node, we go **plain TS + tsx**, not Next.js.

---

## 4. Side-by-side on the things that matter

| Dimension | Rust | Node/TS | Winner |
|---|---|---|---|
| Time to first working tool loop | 1 week | 2–3 days | **Node** |
| Time to v0.1 (all of PRD FR-1..FR-51) | 4–6 weeks | 3–5 weeks | Node (slight) |
| Type safety for LLM payloads | Native enums, exhaustive | Requires runtime validation | **Rust** |
| Deployment artifact | Single binary | Directory + runtime | **Rust** |
| LLM SDK availability | Thin, 6mo behind | First class | **Node** |
| Playwright browser tool | Painful | Trivial | **Node** |
| Prompt iteration speed | Slow (rebuild) | Fast (watch) | **Node** |
| Concurrency correctness | Compiler-enforced | Dev-enforced | **Rust** |
| Idle memory | <50 MB | 100–400 MB | **Rust** |
| Long-run stability (24h+) | Excellent | Good (with care) | **Rust** |
| Pre-restart gate reliability | `cargo check` is meaningful | `tsc` is weaker | **Rust** |
| SQLite interaction | `rusqlite` direct | `better-sqlite3` direct | Tie |
| Dev team scaling | Smaller Rust pool | Larger TS pool | **Node** |
| Future hire pool | Smaller | Larger | **Node** |
| Fun for the author (this matters) | High if Rust-comfortable | High always | — |

Net: Rust wins on **reliability and deploy story**, Node wins on **velocity and ecosystem fit**.

---

## 5. Decision

**Primary: Rust. Ratified 2026-04-13.**

Rationale:

1. **The PRD explicitly prioritizes reliability, observability, and a small LOC budget.** Rust's type system and binary deployment model are a better match for those goals than Node's velocity.
2. **The predecessor's bug history is concentrated in loose-typing bugs** (`worker_id==0`, budget double-counting, empty-response handling). Rust's compiler makes most of those impossible.
3. **The self-modification cycle benefits from one binary.** No install step in the middle of a restart. One less failure mode.
4. **Long-horizon autonomous runs are the thesis of the project.** 24-hour uptime is a success criterion (M2). Rust's memory behavior here is meaningfully better.
5. **The velocity penalty is front-loaded.** After the first 1,500 lines of scaffolding (event bus, store, tool trait, LLM client), adding new tools and modifying the loop are both fast in Rust. The 2–3x slowdown is not a permanent tax.

### Explicit concessions to the Node case

- **Drop the browser tool from v0.1.** Revisit in v0.2 and decide whether to (a) run a sidecar Node process for Playwright or (b) use Rust WebDriver bindings.
- **Thin HTTP client for LLM providers** in v0.1 (no reliance on third-party crates for request/response shapes). Hand-roll it for OpenRouter's OpenAI-compatible API. This keeps us out of the "wait for the SDK to catch up" trap.
- **Prompt iteration cycles use a dev subcommand** (`strange-loop dev repl`) that loads without re-building tools — just reads prompts from disk each time. Mitigates the rebuild-velocity penalty for the most common dev workflow.

### Escape hatch

If, after **two weeks of actual implementation**, the velocity is meaningfully worse than projected — specifically, if we haven't completed the tool loop, LLM client, and CLI adapter in that window — switch to Node and throw away the Rust scaffolding. That's a reversible mistake as long as we catch it early.

### What this decision is not

- **Not a religious commitment.** If Rust's LLM SDK situation gets worse rather than better, or if the browser tool becomes a hard v0.1 requirement after all, revisit.
- **Not a ban on Node code.** Adapters in v0.2+ can be separate processes in any language that speaks the adapter protocol. A Telegram adapter in Node, talking to the Rust core via a Unix socket, is fine.
- **Not a rejection of Next.js.** It's a rejection of Next.js *as the host of strange-loop*. If someone wants to build a web dashboard on top of the event log later, Next.js is a perfectly good choice for that dashboard — as a separate frontend consuming the Rust core's SQLite/HTTP surface.

---

## 6. If the decision flips

If after the two-week check-in we switch to Node, the system spec stays the same. Translation notes:

| Spec section | Rust mapping | Node mapping |
|---|---|---|
| `Tokio` | `tokio::task::spawn` | Async functions + `Promise` |
| Blocking pool | `tokio::task::spawn_blocking` | `worker_threads` (rare: most Node libs are already async) |
| Tool trait | `async_trait` | Plain interface + async functions |
| `rusqlite` | `rusqlite::Connection` | `better-sqlite3` (sync) or `@libsql/client` |
| `serde` | Derive on types | `zod` schemas at boundaries |
| CLI | `clap` | `commander` or manual `process.argv` |
| Process restart | `execv` via `nix` crate | `child_process.spawn` + exit |
| Config | `toml` crate | `@iarna/toml` or native JSON |
| Smoke test binary | `strange-loop self-test` subcommand | Same, as a CLI flag |

Everything in the system spec above is written language-neutral for this reason.
