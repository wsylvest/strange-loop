# strange-loop

A self-modifying agent runtime. Spiritual successor to [ouroboros](../ouroboros), rewritten from first principles.

This is **not a port**. It is a reimagining that keeps what matters — LLM-first control flow, identity persistence, self-modification through git, a budget-aware tool loop — and drops the accidents of the original (Colab/Drive coupling, multiprocessing workers, file-locked state, Python-specific hot-swap).

## Status

Pre-implementation. Planning artifacts only.

## Read in order

1. [`docs/TREATISE.md`](docs/TREATISE.md) — **the why.** Philosophy of the rewrite, what Ouroboros proved and did not prove, what strange-loop is for in an intellectual sense, what it refuses to claim, ethical posture. Draws on Hofstadter's *I Am a Strange Loop* and on neurological analogues for layered governance. Start here.
2. [`docs/PRD.md`](docs/PRD.md) — **the what.** Product requirements, target users, feature scope, what's in and what's out.
3. [`docs/ANALYSIS.md`](docs/ANALYSIS.md) — **the audit.** What Ouroboros actually is, what works, what doesn't, what to keep and what to throw away.
4. [`docs/SYSTEM_SPEC.md`](docs/SYSTEM_SPEC.md) — **the how.** Architecture, algorithms, governance layering, data model, persistence tiers, tool surface (InProc / Edge / Cell), event flow, failure model.
5. [`docs/STACK_DECISION.md`](docs/STACK_DECISION.md) — Rust vs Node.js tradeoff and recommendation.
6. [`docs/ROADMAP.md`](docs/ROADMAP.md) — milestone plan from empty repo to self-hosting loop.

## The governance layering

The agent is governed by six layers of rule, each in a different substrate, each with a different mutability rule. The metaphor is neurological (see the treatise):

| Layer | Substrate | Mutable by |
|---|---|---|
| **KERNEL** | source code | humans, by rebuilding |
| **CHARTER** | `prompts/CHARTER.md`, hash-pinned | humans, with `strange-loop charter approve` |
| **CREED** | `prompts/CREED.md` | humans directly; agent via ceremonial proposal |
| **DOCTRINE** | `prompts/doctrine.toml` | humans only, hot-reloadable |
| **JOURNAL** | `journal/` table + files | agent, append-only (enforced at storage) |
| **SCRATCH** | `prompts/scratch.md` | agent, freely |

Lower layers cannot override higher ones, and the unmovable layers (KERNEL and CHARTER) are unmovable because they are in a substrate the agent's own tools cannot reach.

## Name

From Hofstadter's *Gödel, Escher, Bach* and *I Am a Strange Loop*: a strange loop is a hierarchy that bends back on itself. An agent that modifies its own source is the canonical computational instance. See the treatise for why that matters.
