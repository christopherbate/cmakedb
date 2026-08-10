# Documentation Policy

How this project documents code, architecture, status, and roadmap.
The policy exists so that every kind of information has exactly **one
home**, and so documentation is updated **in the same commit** as the
change it describes — a doc that can drift is a doc that will lie.

## Document set and ownership

| Document | Home for | Update trigger |
|---|---|---|
| `design.md` | The architecture and its rationale: layering, data model, algorithms, testing strategy, milestone definitions. | Only deliberate design revisions. Code does not silently diverge from it — a divergence is either a bug or a design revision that edits this file and records why. |
| `ROADMAP.md` | **Single source of truth for project status**: what is done (with commit refs), what is not, milestone breakdowns with acceptance criteria, and known gaps/debt. | Same commit as any feature/milestone completion, scope change, or newly discovered gap. |
| `README.md` | User-facing overview: what the tool does, quickstart, real examples, validation results. Contains only a one-paragraph status summary linking to `ROADMAP.md`. | When user-visible behavior changes (commands, output, flags). |
| `AGENTS.md` | How to work in the repo: build/test, workflow rules, crate map, hard-won empirical facts, performance baselines. | When a rule, invariant, or baseline changes, or a new expensive-to-rediscover fact is learned. |
| `docs/user-guide.md` | **User-facing capability reference**: every command, every detection with its evidence and false-positive/false-negative cases, tuning, CI recipes. | When a capability, detection, or its FP profile changes — same commit. |
| `docs/` (other) | Long-form policies and deep dives that fit nowhere above. | As needed. |
| Rustdoc (`//!`, `///`) | Code-level contracts: what a module/type is for, invariants the code can't express, and `design.md` §-references tying implementation to design. | Same commit as the code it documents. |

## Rules

1. **Single home.** Each fact lives in exactly one document; everything
   else links to it. Never copy status tables, milestone lists, or
   invariants between documents — summarize in one line and link.
2. **Same-commit updates.** A commit that completes a feature/milestone,
   changes user-visible behavior, or discovers a new invariant updates the
   affected documents *in that commit* (this extends the standing
   commit-per-milestone rule).
3. **Design references in code.** Non-obvious implementation decisions
   cite the design (`design §4.2`) rather than restating it. If the design
   doesn't cover the decision, the module doc states the contract itself.
4. **Honest stubs.** Unimplemented surface area must say so where the user
   hits it (CLI error messages name the milestone) and in `ROADMAP.md`.
   Never present a stub as done.
5. **Comments state contracts, not narration.** Code comments record what
   the code cannot express: invariants, empirical facts about external
   systems (e.g. trace-format behavior), and why an approximation is safe.
   No restating what the next line does.
6. **Empirical facts get promoted.** Anything learned by experiment
   against an external system (CMake trace quirks, path-spelling rules,
   scale behavior) is recorded in `AGENTS.md` under "hard-won facts" the
   moment it is validated — these are the most expensive facts to
   rediscover.
7. **Roadmap entries are verifiable.** Every uncompleted milestone item in
   `ROADMAP.md` carries acceptance criteria concrete enough that a future
   session can tell whether it is done, plus a pointer to the relevant
   design section.

## Definition of "documented" for a milestone

A milestone is complete only when, in the same commit(s):
- tests covering it are green,
- `ROADMAP.md` moves it to *Done* with the commit reference,
- `README.md` reflects any new user-visible behavior,
- `AGENTS.md` captures any new invariants or baselines, and
- module docs cite the design sections they implement.
