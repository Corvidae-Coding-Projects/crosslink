# Feature: Write-path cost and lock semantics (umbrella)

Umbrella design document, written 2026-10-08 against `origin/develop` at 3440c0d05. Status: reviewed. It was revised after review rounds 1 and 2, and was to be frozen when the first increment's scaffold commit landed. It has since been amended, by dated operator decisions only (the 2026-10-09 reorder and the reduce invariant); later changes follow the same rule. It states the shared problem, the evidence, the increments named by feature and their order, and the decisions that cross increments. Each increment gets its own design document, written when that increment is next to build. Only the first exists: `.design/lock-claim-outcome-and-kickoff-rollback.md`. The entries for later increments are provisional. This document follows `.design/crosslink-architecture-overhead-map.md` and amends `.design/hub-v3-per-agent-refs.md` where the increment documents say so. A bare `#N` is a tracker item, and GitHub records are written in full. Handles are listed in the key at the end.

## Problem

- **Writes are slow.** One hub write, such as `crosslink session work <id>`, spawns hundreds of git processes. On macOS, `/usr/bin/git` is a shim that is slow to start, so a write takes tens of seconds.
- **Claims are misreported.** A 30 s guard reports a lock claim as failed after the claim has already been published. The lock is stranded, and the caller records nothing.
- **Kickoff leaves debris.** It does not undo what it created when it fails. `kickoff cleanup` never releases locks.
- **Stranded locks cannot be cleared.** On v3 hubs nobody but the holder can clear a lock: steal and force-release do nothing.
- **Heartbeats ride the slow path.** While a slow write runs, heartbeats wait behind it, so locks read stale.
- **The knowledge cache has no owner.** Since reads became side-effect-free, nothing creates or refreshes it.

## Evidence

Each figure is marked as measured, estimated or a target.

- **Measured**, 2026-09-29, on macOS on develop, with a `PATH` shim counting spawns (Corvidae-Coding-Projects/crosslink#118):
  - about 615 git spawns to bring a fresh repository to ready;
  - about 350 git spawns per write on a four-agent hub;
  - `/usr/bin/git` at about 35 ms per spawn, against about 9 ms for the Command Line Tools git;
  - writes took 16–30 s.
- **Measured**, 2026-10-08, in a downstream project (comment on Corvidae-Coding-Projects/crosslink#118): lock claims took 58 s, 72 s and 86 s.
- **Measured**, 2026-10-09 UTC, in this checkout: two claims timed out at 51 s and 52 s, and both locks landed anyway (tracker #817).
- **Estimated** by reading the code, not measured. The write-cost increment's spawn-count test replaces these with a baseline.
  - About 40 spawns per readiness validation.
  - Four to five validations per write.
  - Three to four full reduces per claim.
  - The `cat-file -t` and `ls-remote` counts in the Corvidae-Coding-Projects/crosslink#118 measurement come from daemon reconciliation running alongside the write. It validates the same immutable objects three to four times per pass.
- **Found** by reading the code, 2026-10-08:
  - The reducer ignores claim and release events from anyone but the holder.
  - Steal and force-release edit local files that a v3 reduce never reads.
  - The smoke test for steal passes whether or not the steal worked (tracker #818).

## Increments, in build order

1. **Lock-claim outcome and kickoff rollback** (Corvidae-Coding-Projects/crosslink#110, and the timeout half of Corvidae-Coding-Projects/crosslink#118). The design document is written: `.design/lock-claim-outcome-and-kickoff-rollback.md`.
   - It also carries the reducer rule that a reduce from any checkpoint equals a replay of every event in total order (late arrivals are replayed), which REQ-6(b) depends on. Write cost and lock clearing build on that invariant. Recorded 2026-10-09.
2. **Write cost** (Corvidae-Coding-Projects/crosslink#118). Provisional.
   - Moved to second on 2026-10-09 (operator decision, from the #820 review): bootstrap and write latency now break the first increment's undo and cleanup on large hubs. The reducer's late-arrival rule (#820) lands before it, and its caches must keep the property test against full replay.
   - A spawn-count test lands first and records the baseline.
   - Then readiness validated once per command, with `SyncManager` setup and `remote_exists` computed once.
   - At most two reduces per write.
   - One `for-each-ref` per reduce, and a shared history cache.
   - Batched `cat-file --batch-check` validation, with a memo per reconciler.
   - One `ls-remote` per publication step.
   - The git binary resolved once (see the git-binary decision below).
   - Budget: no more than a quarter of the baseline and no more than 100 spawns per `session work`.
   - Constraints found:
     - The readiness contract forbids "in-memory-only readiness" and requires every write to reject a non-ready repository. So a cached validation must still re-check record age and daemon liveness on every acquisition.
     - Every new cache needs a property test against full recomputation, proved by removing its invalidation and watching the test fail.
     - The reducer's late-arrival replay is not persisted: until a write publishes a covering checkpoint, every read-only reduce replays again (confirmation, reads, the daemon tick, dashboard polls). Persist or memoise the rebuilt state, or rebuild from the newest checkpoint before the late event.
3. **Knowledge cache ownership** (Corvidae-Coding-Projects/crosslink#120, tracker #807). Provisional.
   - The main checkout's daemon creates the cache after its first ready activation and refreshes it on its sync tick when the remote tip moves. Creation is atomic.
   - A failure is logged and never affects readiness or the reconcile budget.
   - The hook and the knowledge MCP server name `crosslink knowledge sync` when the cache is missing.
   - Constraints found:
     - The step must run only in the ready normal loop, per the readiness contract's state gating (tracker #794).
     - Running it outside the operation permit departs from Corvidae-Coding-Projects/crosslink#120's suggestion, and the increment document must say why.
     - The overhead map asks that the cache never be made authoritative.
   - It was second because it is small and independent; moved to third on 2026-10-09 (operator decision) because bootstraps after a wake now exceed the 120 s `ensure` wait (Corvidae-Coding-Projects/crosslink#118), which breaks claims, kickoff undo and cleanup. Downstream projects rely on it: pages attached to an issue reach agents at session start only if the cache exists.
4. **Heartbeats off the write path.** Provisional.
   - A heartbeat writes only the agent's own ref, without a reduce or full validation.
   - It still runs only while readiness grants writes; the readiness contract lists heartbeats among the operations that require ready.
   - The daemon publishes heartbeats even while a CLI write holds the permit. Today the deferral skips the heartbeat counter.
   - Constraints found:
     - The hook heartbeat (`heartbeat.py`, upstream #275) and the daemon heartbeat share the agent ref and the hub write lock.
     - On v3 a stale lock is never cleared by time. Staleness only lets the next claim proceed, and the reducer then ignores that claim, so a stranded lock stays until its holder releases it. Aging heartbeat-less locks from `claimed_at` changes only when a lock is reported stale, so decide it together with lock clearing.
5. **Lock clearing on v3 hubs** (tracker #818). Provisional.
   - Steal and force-release produce an event the reducer applies deterministically and idempotently, naming the previous holder and that holder's claim.
   - The steal is recorded for audit.
   - Increment 1 does not depend on this increment. Until it lands, a lock that increment 1's undo fails to release stays until its holder releases it.
   - Constraints found:
     - The holder-only release rule is deliberate (event-sourced design §4.2 and §6.2).
     - Steal worked on v2 only by editing cache files, and stopped working when v3 reads moved to refs (upstream #754).
     - The hub-v3 design's motivation lists a "live-holder lock force-steal" defect, so the emitter must re-check the holder's heartbeat immediately before emitting, and the tests need a live-holder negative case.
     - The reader-version mechanism must reuse an existing gate (see the steal decision below).
     - A binary that skips an unknown event must never publish a checkpoint covering it.
   - It is last because it changes the hub protocol and needs the upstream owners' agreement.

Each increment is dispatched as one issue. The number of merges each takes is decided in its own document.

## Cross-increment decisions

- **Reduce equals a total-order replay** (2026-10-09, from the first increment's review). A reduce from any checkpoint must give the same state as replaying every event after the authority baseline in total order. Late arrivals trigger that replay, and `compaction::tests::prop_reduce_from_any_checkpoint_equals_full_replay` is its oracle. Every later increment that caches or shortcuts reduction keeps this property.

- **Steal wire format** (2026-10-08, @magnificentlycursed). The direction was a tolerant decoder that skips unknown event types with a warning, plus a minimum-reader-version gate before any new lock-clearing event is emitted. Rejected: an optional field on `LockReleased`, which old binaries would reduce differently, and a staged rollout without a gate. The precedent check the same day found that a gate already exists:
  - the generation descriptor's `protocol_version` on `crosslink/reconciliation/current`, checked by exact equality, which every readiness-era binary refuses to become ready on;
  - the checkpoint schema's fail-closed watermark guard (#798);
  - the design history's stated rule that "mixed-version operation is refused".

  The lock-clearing increment's document must use or extend that gate rather than add a new meta-ref field. If that changes the decision's substance, the document must take it back to the operator. This is a hub protocol change: the upstream owners must agree before it merges.
- **Git binary** (2026-10-08, @magnificentlycursed). `CROSSLINK_GIT` from the environment overrides the git binary. Otherwise, on macOS, when `PATH` gives `/usr/bin/git`, crosslink runs `xcrun --find git` once per process and falls back to `/usr/bin/git`. It never reads a repository's config, so a cloned repository cannot redirect it. Rejected: override only, which leaves every Mac slow by default; and plain `PATH`. No library (git2, gix) is added, consistent with the readiness work's no-new-dependency boundary.
- **Each increment builds from a scaffold commit** of types, stubs and failing tests. Bug fixes are failing-first: the regression test fails on the base revision and passes after the fix.
- **Work-in-progress limit.** While this umbrella's increments wait to be built, no other design cycle opens unless it blocks them. The hook design (Corvidae-Coding-Projects/crosslink#104, tracker #814, with Corvidae-Coding-Projects/crosslink#116 and tracker #816) waits until the first increment is built.

## Out of scope

- The work-check hook and commit gate, agent command policy, and the post-wake readiness block: these are the hook design.
- Integration drift in `init --update`: Corvidae-Coding-Projects/crosslink#105, #13, #15, #20 and #99.
- Replacing git subprocesses with a library.
- The global checkpoint watermark (tracker #798).
- Removing dead kickoff agents' identities, key files and trust approvals (tracker #819).
- Clearing the existing stale `dashboard-bootstrap` locks. That is the upstream maintainer's driver identity, so it is an operator action to coordinate with them once lock clearing lands.
- Knowledge features beyond creating and refreshing the cache, such as page status, supersede, a generated index and export.

## Key

Tracker records (`crosslink issue show N`), with title and state as of 2026-10-08:
- #794: the readiness contract (its plan comment holds the contract).
- #798 "Replace global checkpoint watermark with per-agent causal frontiers", open.
- #807 "Knowledge cache is never created or refreshed since the readiness model", open.
- #814 "Commit gate and work-check hook misfires (upstream #104 item 2 and related)", open.
- #816 "Readiness blocks all work for ~30 s plus a reconcile after every wake", open.
- #817 "Design: write-path cost and lock-confirmation semantics (upstream #118, #110, #120)", open.
- #818 "v3 hubs cannot steal or force-release another agent's lock", open.
- #819 "Dead kickoff agents leave identities, host key files and trust approvals behind", open.

GitHub records, with state as of 2026-10-08:
- Corvidae-Coding-Projects/crosslink#104 "Provider hooks: deployed hooks can predate the readiness model unnoticed; work-check blocks on a 3 s timeout and mislabels out-of-root paths", open.
- Corvidae-Coding-Projects/crosslink#110 "kickoff: a failed launch keeps the issue lock and worktree, and kickoff cleanup leaves the lock and branch behind", open.
- Corvidae-Coding-Projects/crosslink#116 "Container agents with a GitHub login can push, open PRs and read the token; the agent hook policy prices colleague trust, not agent trust", open.
- Corvidae-Coding-Projects/crosslink#118 "Each write spawns hundreds of git processes (~615 on bootstrap, ~350 per write), which trips fixed timeouts where git spawns are slow", open.
- Corvidae-Coding-Projects/crosslink#120 "Knowledge cache is never created in a fresh checkout or refreshed since the readiness model; reads fail and the session hook and MCP server go silent", open.
- Corvidae-Coding-Projects/crosslink#105, #13, #15, #20 and #99: the `init --update` integration-drift issues, open.
- Upstream #275, #754 and the event-sourced coordination design are cited from git history. That design was last present at the parent of commit 8e054a3e, as `DESIGN-EVENT-SOURCED-COORDINATION.md`.
