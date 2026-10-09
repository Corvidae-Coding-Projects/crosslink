# Feature: Lock-claim outcome and kickoff rollback

Increment design document, written 2026-10-08 against `origin/develop` at 3440c0d05, for the first increment of the umbrella `.design/write-path-cost.md`. Status: reviewed. It was revised after review round 1 (16 findings) and round 2 (verdict: ready to build after fixes), and round 2's fixes are applied. It amends `.design/hub-v3-per-agent-refs.md`: the confirmation timeout now bounds confirmation only, and its 30 s value and REQ-6(b) stand. It is to be amended as the code lands. A bare `#N` is a tracker item; GitHub records are written in full. Handles are listed in the key at the end.

## Summary

Today a lock claim is published, then reported as failed if the whole write took more than 30 s. The lock stays on the hub, and the caller records nothing. On v3, confirmation cannot fail at all: a failed fetch is logged and the claim is confirmed against the stale local view. Kickoff then exits, or later fails to launch, without undoing what it created, and `kickoff cleanup` never releases locks.

This increment does three things:
- makes a claim's reported outcome match what was confirmed on the remote;
- makes kickoff undo what it created when it fails before the agent starts;
- makes cleanup release the agent's lock and session.

## User-visible behavior

- `crosslink session work <id>` succeeds only once the claim is confirmed against a fresh fetch of the remote. Otherwise it exits non-zero with "claim published but not confirmed: <cause>; run `crosslink session work <id>` again to confirm". The rerun publishes no second claim.
- A kickoff (`run` or `plan`) that fails before its agent starts undoes everything it created in this invocation:
  - it releases the issue lock and ends the worktree session;
  - it stops the worktree daemon;
  - it removes any container or tmux session it started;
  - it restores the pipeline state;
  - it removes the worktree and the branch, if this invocation created them.

  Each step that fails, or that finds nothing to undo when it expected something, is named along with the command that clears it.
- `crosslink kickoff cleanup` brings the worktree up enough to release the agent's locks and end its session. It then removes the worktree, and deletes the agent's branch only when the branch has no commits beyond the recorded base, unless `--keep-branch` is given.

## Requirements

Claim outcome:

- REQ-1: A claim's outcome comes from confirmation, never from the elapsed time of publication. Confirmation is a fetch of every agent ref and the checkpoint, followed by a reduce. The elapsed-time `bail!` in `claim_lock_v2_inner` is removed.
- REQ-2: Confirmation is fallible. The result is `Unconfirmed { cause }` in three cases:
  - the confirmation fetch fails;
  - the hub-lock wait plus the fetch does not finish within 30 s (`LOCK_CONFIRM_TIMEOUT_SECS`);
  - any step errors after the claim's push has succeeded: adopt, reduce, checkpoint or hydration.

  The bound covers only the network part, the hub-lock wait and the fetch. The local reduce of the fetched snapshot runs to completion: freshness comes from the fetch, and a slow reduce must not make a confirmed claim fail. No caller treats `Unconfirmed` as ownership, which keeps hub-v3 REQ-6(b): no state lets two agents both verify ownership.
- REQ-3: When the local view already shows this agent as holder, the claim runs the same confirmation before returning `AlreadyHeld`. It publishes nothing. If confirmation shows another holder, the result is `Contended`; if confirmation does not complete, the result is `Unconfirmed`. The contention-cleanup `LockReleased` stays on the new-claim path only.
- REQ-4: `session work` records the active issue and the `.active-issue` sentinel only for `Claimed` and `AlreadyHeld`. For `Unconfirmed` it prints the rerun message and exits non-zero, leaving the published claim for the rerun to confirm.
- REQ-5: If `session work` fails after a confirmed claim, it releases the lock. (The base revision already does this for `set_session_issue`; this increment keeps it.)

Kickoff undo:

- REQ-6: Kickoff `run` and `plan` record each resource as this invocation creates it, and on any error before the agent starts, undo them in reverse order:
  1. the container or tmux session;
  2. the issue lock and the session, by running `crosslink locks release <id>` and `crosslink session end` inside the worktree while its daemon is still up, so the worktree agent's own identity signs them;
  3. the worktree daemon;
  4. the pipeline state, restored to what it was before `mark_running` (`run`) or `mark_planning` (`plan`);
  5. the worktree, only if this invocation created it;
  6. the branch, only if this invocation created it and it has no commits beyond the recorded base.

  A worktree or branch that kickoff reused (`--branch` with an existing directory, a swarm-stored branch) is never removed.
- REQ-7: The agent has started once tmux `send-keys` succeeds (`launch_local`) or `launch_container` returns `Ok`, which happens after its early-exit check. From then on, undo is disarmed, and the lock, worktree and branch belong to the agent.
- REQ-8: `kickoff cleanup` works in this order:
  1. it removes the tmux session or container, so no agent process is still running;
  2. it ensures the worktree daemon is ready (`daemon::ensure`, which waits up to 120 s per agent, plus a readiness bootstrap);
  3. it runs `crosslink locks release <id>` and `crosslink session end` inside the worktree;
  4. it stops the daemon and removes the worktree;
  5. it deletes the branch under the rule in REQ-6 step 6.

  `--keep-branch` skips the branch step. A worktree whose metadata has no recorded base keeps its branch.
- REQ-9: Undo and cleanup verify each lock release with the same bounded, fallible confirmation as REQ-2, run in the worktree after the release:
  - if confirmation shows the worktree agent still holding the lock, the lock is reported as stranded;
  - if confirmation does not complete, the release is reported as unverified.

  Each report includes its remedy. A failed step does not stop the remaining steps. Each failure is reported with the resource left behind and the command that clears it. The process exits non-zero, with the original error first.
- REQ-10: Kickoff metadata gains an optional `base_commit`, the commit the worktree's branch was created from, written by `create_worktree`. It is absent in metadata written before this increment.
- REQ-11: The worktree agent's identity, its host-side key file and the host's trust approval are deliberately left in place by undo and cleanup. Revoking the approval would leave the agent's already-published events signed by a revoked key. Their removal is tracker #819.

## Acceptance criteria

Tests marked "fails first" must fail on 3440c0d05 and pass after the change. They are the regression tests for Corvidae-Coding-Projects/crosslink#110.

- [ ] AC-1 (fails first): publication is delayed past 30 s by a test hook, and confirmation succeeds. `session work` succeeds, records the active issue, and the hub shows the lock held by the agent. (REQ-1, REQ-4)
- [ ] AC-2 (fails first): the remote becomes unreachable after the claim's push. The result is `Unconfirmed`, `session work` exits non-zero with the rerun message, and no active issue is recorded. With the remote back, a rerun records the active issue, and the agent ref holds exactly one `LockClaimed` for the issue. (REQ-2, REQ-3, REQ-4)
- [ ] AC-3: an error is injected into the reduce after the push. The result is `Unconfirmed`, not a plain error. (REQ-2)
- [ ] AC-4: a test sets the confirmation deadline to 2 s, and a fake git holds the fetch open. The result is `Unconfirmed` within 4 s. No process from the fetch's process group remains, and a normal fetch afterwards succeeds, so no lock files were left. On Windows, only the direct child is checked. (REQ-2)
- [ ] AC-4a: the fetch is fast, and a test hook makes the reduce take longer than the deadline. The result is `Claimed` (and `AlreadyHeld` on a rerun), not `Unconfirmed`. (REQ-2, REQ-3)
- [ ] AC-5: the local view shows this agent as holder, and the remote is unreachable. `session work` returns `Unconfirmed`, not `AlreadyHeld`. (REQ-3)
- [ ] AC-6: another agent's earlier-ordered claim arrives during confirmation. The result is `Contended`, and no active issue is recorded. (REQ-3)
- [ ] AC-7 (guard, passes on base): `set_session_issue` fails after a confirmed claim. A `LockReleased` follows, and the hub shows no lock. (REQ-5)
- [ ] AC-8 (fails first for activation): this is a fake-runtime test for each failure point: activation, missing preflight, tmux start, container start, and early container exit. Afterwards the issue is unlocked, the worktree directory and branch are gone, the worktree daemon is not running, no container or tmux session remains, and the pipeline state equals its prior value. This holds for both `run` and `plan`. (REQ-6)
- [ ] AC-9: in a reuse case (`--branch` with an existing worktree), activation fails. The lock is released, but the worktree and branch remain. (REQ-6)
- [ ] AC-10: a launch succeeds. Afterwards the lock is held by the worktree agent, and the worktree, branch and daemon remain. (REQ-7)
- [ ] AC-11: `kickoff cleanup --force` runs on a stopped agent whose daemon is not running. Afterwards the issue is unlocked, the session ended, the daemon stopped, and the worktree removed. A branch with no commits beyond `base_commit` is deleted. A branch with a commit, any branch under `--keep-branch`, and any branch whose metadata has no `base_commit` remain. (REQ-8, REQ-10)
- [ ] AC-12: the lock release does not reach the remote (the remote is rejecting pushes), and confirmation still shows the worktree agent holding the lock. The output names the stranded lock, and the remaining steps run. With the remote unreachable, the output says "release unverified". (REQ-9)
- [ ] AC-13: the agent identity and trust approval remain after undo and after cleanup. (REQ-11)

## Current architecture

All references are to 3440c0d05.

**Claim.**
- `claim_lock_v2_inner` (`crosslink/src/shared_writer/locks.rs`) times `emit_compact_push_inner` (`crosslink/src/shared_writer/core.rs`). That call covers `commit_v3` (append, push, fetch-and-adopt, reduce, checkpoint) and then hydration. The function bails when more than 30 s have passed, after the event is already on the remote.
- `confirm_v3_locks` (`core.rs:682-687`) logs a failed fetch and reduces the local view. The v3 `SyncManager::fetch` (`crosslink/src/sync/cache.rs:500-505`) always returns `Ok`, and `fetch_and_adopt_v3_refs` (`cache.rs:548-556`) returns silently when git fails. So confirmation never fails.
- `AlreadyHeld` is decided from the local view, with no fetch.
- The bail came from commit 4728766c. It cites §8 of the event-sourced coordination design: "if compaction hasn't run within 30s, agent compacts itself". That was a wait-then-act rule, implemented as a bail.
- GitHub PR Corvidae-Coding-Projects/crosslink#103 raised the bound to 120 s. It was closed unmerged because it worked around write cost instead of fixing the semantics.

**session work.** `crosslink/src/commands/session.rs` runs `enforce_lock`, `try_claim_lock`, `set_session_issue`, then writes the sentinel.
- A claim error returns early.
- A stale lock held by another agent lets `enforce_lock` proceed, but the reducer then ignores the new claim (`crosslink/src/compaction.rs:1084-1087`), so the result is `Contended`.
- A stranded lock therefore stays until its holder releases it.

**Kickoff.**
- `run.rs` and `plan.rs` call `create_worktree` (`crosslink/src/commands/kickoff/launch.rs`) from `HEAD`, with `base_branch: None`. `run` reuses an existing worktree when `--branch` names one. `create_worktree` may delete and recreate a merged branch.
- `init_worktree_agent` (`launch.rs`) runs these steps:
  - `crosslink init`;
  - `daemon::ensure`;
  - agent creation, with the key stored on the host;
  - a host-signed trust approval committed to the hub;
  - `sync`, `session start` and `session work`, as subprocesses.
- Since b6dbf785 the daemon is stopped on a bootstrap failure. Nothing else is undone.
- `run` calls `mark_running`; `plan` calls `mark_planning` before activation.
- After tmux `send-keys` succeeds, or `launch_container` returns, no later step can fail: they are `let _ =` or warnings only.
- `KickoffMetadata` (`types.rs:157-171`) records no base.

**Cleanup.** `cleanup.rs` removes tmux, the container, pipeline state, the daemon (stop only) and the worktree. It touches no locks, sessions or branches.

**Release.** `locks release` takes the mutation permit, which requires a ready worktree daemon. It exits 0 and prints "was not locked" whenever the local view does not show this agent as holder (`crosslink/src/commands/locks_cmd.rs:170-174`).

**Callers.** Swarm and sentinel call `kickoff::run` and treat an error as a failed launch.

## Proposed design

1. **The claim.**
   - `LockClaimResult` becomes `Claimed`, `AlreadyHeld`, `Contended { winner_agent_id }` and `Unconfirmed { cause }`.
   - `commit_v3` (`crosslink/src/shared_writer/core.rs`) returns a typed outcome distinguishing "not published" (an error before or during the push, where the ref is rolled back as today) from "published", which carries any later error from adopt, reduce or checkpoint. `emit_compact_push_inner` keeps that distinction through hydration. `claim_lock_v2_inner` maps "published with an error" to `Unconfirmed`. Other callers of `emit_compact_push_inner` keep their current behaviour, by converting "published with an error" back to an error.
   - A new `confirm_v3_locks_bounded(deadline)` works in three steps:
     1. it takes the hub lock with the remaining time, through `acquire_hub_lock_with_timeout` (`crosslink/src/sync/cache.rs`), made `pub(crate)` with the duration taken out of its error text;
     2. it runs the v3 fetch against the deadline and propagates every error;
     3. it reduces the fetched snapshot without a deadline.

     The `AlreadyHeld` path calls the same confirmation.
   - The fetch runs through `command_output_with_timeout` (`crosslink/src/commands/kickoff/helpers.rs`), promoted to a shared module. It is extended for Unix: it spawns the child in its own process group (as `detach_daemon_command` in `crosslink/src/daemon.rs` does), and on the deadline it sends SIGTERM to the group, so git removes its lock files, then waits 500 ms and sends SIGKILL. On Windows it kills the direct child, as it does today. No third timeout helper is written.
2. **`session work`.** `try_claim_lock` (`crosslink/src/lock_check.rs`) returns an error carrying the rerun message for `Unconfirmed`. Every exhaustive match on `LockClaimResult` is updated.
3. **Kickoff undo.** Add `KickoffRollback` in `crosslink/src/commands/kickoff/`.
   - It records created resources as they are created, with a flag saying whether the worktree and branch were created or reused.
   - `run` and `plan` wrap everything after `create_worktree` in one `match`. They call `unwind(&err)` on error and `disarm()` at the boundary in REQ-7.
   - Each undo step returns a report entry.
   - The lock and session steps run before the daemon stop, while the daemon `init_worktree_agent` ensured is still up.
4. **Cleanup.** It reuses the lock, session and branch steps. Before them it calls `daemon::ensure` on the worktree. If the worktree cannot become ready, those steps are reported as skipped, with `crosslink locks release <id>` (run from the worktree) as the remedy.
5. **Metadata.** `create_worktree` returns the base commit, and the caller writes it to `base_commit` in `KickoffMetadata` (`#[serde(default)]`).

The build starts from a scaffold commit containing the new `LockClaimResult` variant, the bounded-confirmation and `KickoffRollback` signatures with stubbed bodies, the metadata field, and the failing tests for the acceptance criteria.

## Decisions

- **An unconfirmed claim is not ownership.** hub-v3 REQ-6(b) forbids a state in which two agents both verify ownership. Treating `Unconfirmed` as held, or confirming against a stale local view, could create one. Rejected: "held with a warning", which the first draft of this design proposed; it was withdrawn after the precedent check.
- **Undo is an explicit `unwind`, not `Drop`.** It runs subprocesses, can fail and must report. `Drop` can do none of that visibly, and it would also run during a panic.
- **Release under the worktree agent's identity.** Running the release inside the worktree keeps the signed log truthful. Rejected: signing from the host with the worktree agent's key.
- **The deadline bounds only the network.** The hub-lock wait and the fetch are bounded; the local reduce is not. A slow reduce of a fresh snapshot is still a correct confirmation, and bounding it would make claims fail on slow machines until the write-cost increment lands. Rejected: bounding the whole confirmation, which round 2 of the review showed would turn today's slow success into a permanent failure.
- **The 30 s value stays; what it measures changes.** Raising it, as Corvidae-Coding-Projects/crosslink#103 did, would treat write cost, and that is the write-cost increment's job.
- **Identity and trust approval are left in place.** Revoking them would make the agent's published events unverifiable.

## Data and compatibility

- **One additive field.** `base_commit` in kickoff metadata, optional, with a default. Metadata without it means "keep the branch".
- `LockClaimResult` and rollback state are internal, so no other persisted format changes.
- Older binaries on the same hub are unaffected.

## Failure handling

- **Confirmation does not complete.** The claim stays published, the command exits non-zero, and a rerun confirms it. If nobody reruns, kickoff undo releases it: the release comes from the holder, so the reducer accepts it. Otherwise the lock stays until its holder releases it. A stale timeout does not clear it on v3. Clearing another agent's lock arrives with the lock-clearing increment (#818).
- **An undo or cleanup step fails.** The failure is reported with its remedy, and the remaining steps run.
- **The worktree cannot become ready during cleanup.** The lock and session steps are reported as skipped, with the remedy.

## Security considerations

- Releases are signed by the agent that claimed, so no new authority is added.
- Branch deletion requires a recorded base and an equal tip, so no work is lost.
- Leaving trust approvals for dead kickoff agents keeps existing exposure as it is. Removing them is out of scope.

## Verification

- **Tests:**
  - `cargo test --manifest-path crosslink/Cargo.toml --lib shared_writer::tests`, `lock_check::tests` and `commands::kickoff::tests`;
  - `cargo test --manifest-path crosslink/Cargo.toml --test smoke coordination`.
- **Before every commit:** `cargo fmt --check` and `cargo clippy -- -D warnings -W clippy::unwrap_used -W clippy::expect_used`.
- **Full suite:** CI only.
- **Fake runtimes and fake git:** the kickoff tests reuse those from Corvidae-Coding-Projects/crosslink#129.
- **Status wording:** "CI passed" for the gates, "reviewed" for an independent review. Neither stands in for the other.
- **Manual check on this Mac:** run `crosslink session work <id>` with `/usr/bin/git` on `PATH`. It succeeds, or it prints the rerun message and a rerun succeeds. A run that never succeeds on this machine is a failure of this increment. Record the timings on #817.

## Rollout and rollback

- This increment merges when its tests and review pass.
- Reverting it restores the elapsed-time bail.
- `base_commit` values already written are ignored by older binaries.

## Open questions

None.

## Out of scope

- Write cost, heartbeats, the knowledge cache, and lock clearing on v3 hubs. These are later increments of the umbrella.
- Removing dead kickoff agents' identities, key files and trust approvals: tracker #819.
- Swarm- and sentinel-specific tests. Both call `run` and inherit its undo.

## Key

- Tracker (`crosslink issue show N`):
  - #817 "Design: write-path cost and lock-confirmation semantics (upstream #118, #110, #120)", open. It holds the 2026-10-09 claim timings.
  - #818 "v3 hubs cannot steal or force-release another agent's lock", open.
  - #819 "Dead kickoff agents leave identities, host key files and trust approvals behind", open.
- Corvidae-Coding-Projects/crosslink#110 "kickoff: a failed launch keeps the issue lock and worktree, and kickoff cleanup leaves the lock and branch behind", open.
- Corvidae-Coding-Projects/crosslink#118 "Each write spawns hundreds of git processes (~615 on bootstrap, ~350 per write), which trips fixed timeouts where git spawns are slow", open.
- Corvidae-Coding-Projects/crosslink#103, the 120 s timeout increase, closed unmerged.
- Corvidae-Coding-Projects/crosslink#129 "Build-matched agent image, launch-time version check, and one gated, verifiable release flow", merged.
