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

- REQ-1: A claim's outcome comes from confirmation, never from the elapsed time of publication. Confirmation fetches every agent ref and the checkpoint, adopts every other agent's ref tip (not the remote checkpoint), and reduces over the local checkpoint. Any tip that fails to adopt fails the confirmation. On v3, a confirmed view that shows no lock is `Unconfirmed`, not ownership. The elapsed-time `bail!` in `claim_lock_v2_inner` is removed. (Amended 2026-10-09; see Amendments.)
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

  A worktree or branch that kickoff reused (`--branch` with an existing directory, a swarm-stored branch) is never removed. Amended 2026-10-09: if the lock release in step 2 does not complete (failed, stranded, unverified, or readiness not granted), steps 5 and 6 are skipped and reported as kept, with a remedy naming the kept path. Step 3 still runs, so no orphaned daemon holds the hub write lock.
- REQ-7: The agent has started once tmux `send-keys` succeeds (`launch_local`) or `launch_container` returns `Ok`, which happens after its early-exit check. From then on, undo is disarmed, and the lock, worktree and branch belong to the agent.
- REQ-8: `kickoff cleanup` works in this order:
  1. it removes the tmux session or container, so no agent process is still running;
  2. it ensures the worktree daemon is ready (`daemon::ensure`, which waits up to 120 s per agent, plus a readiness bootstrap);
  3. it runs `crosslink locks release <id>` and `crosslink session end` inside the worktree;
  4. it stops the daemon and removes the worktree;
  5. it deletes the branch under the rule in REQ-6 step 6.

  `--keep-branch` skips the branch step. A worktree whose metadata has no recorded base keeps its branch. Amended 2026-10-09: if a release in step 3 does not complete, step 4 still stops the daemon but keeps the worktree, and step 5 is skipped. The warning names the kept path and how to finish (`crosslink locks release` there, then `kickoff cleanup` again) or give up (`git worktree remove --force`).
- REQ-9: Undo and cleanup verify each lock release with the same bounded, fallible confirmation as REQ-2, run in the worktree after the release:
  - if confirmation shows the worktree agent still holding the lock, the lock is reported as stranded;
  - if confirmation does not complete, the release is reported as unverified.

  Each report includes its remedy. A failed step does not stop the remaining steps. Each failure is reported with the resource left behind and the command that clears it. The process exits non-zero, with the original error first.
- REQ-10: Kickoff metadata gains an optional `base_commit`, the commit the worktree's branch was created from, written by `create_worktree`. It is absent in metadata written before this increment.
- REQ-11: The worktree agent's identity, its host-side key file and the host's trust approval are deliberately left in place by undo and cleanup. Revoking the approval would leave the agent's already-published events signed by a revoked key. Their removal is tracker #819.

## Acceptance criteria

Tests marked "fails first" must fail on 3440c0d05 and pass after the change. They are the regression tests for Corvidae-Coding-Projects/crosslink#110.

- [ ] AC-1 (fails first): a test hook delays publication past the confirmation deadline (a 6 s delay against a 5 s deadline), and confirmation succeeds. Amended 2026-10-08 during the build, from "past 30 s": the deadline is what publication time must not count against. `session work` succeeds, records the active issue, and the hub shows the lock held by the agent. (REQ-1, REQ-4)
- [ ] AC-2 (fails first): the remote becomes unreachable after the claim's push. The result is `Unconfirmed`, `session work` exits non-zero with the rerun message, and no active issue is recorded. With the remote back, a rerun records the active issue, and the agent ref holds exactly one `LockClaimed` for the issue. (REQ-2, REQ-3, REQ-4)
- [ ] AC-3: an error is injected into the reduce after the push. The result is `Unconfirmed`, not a plain error. (REQ-2)
- [ ] AC-4: a test sets the confirmation deadline to 2 s, and a fake git holds the fetch open for 30 s. On the already-held path, the result is `Unconfirmed` in well under 30 s (the test asserts under 20 s). Amended 2026-10-08 during the scaffold: "within 4 s" could not hold, because the claim's publication and the local read before confirmation are not part of the bound. No process from the fetch's process group remains (the test's marker is on a grandchild), and a lock file the fake fetch removes only on SIGTERM is gone, so termination was graceful. On Windows, only the direct child is checked. (REQ-2)
- [ ] AC-4a: the fetch is fast, and a test hook makes the reduce take longer than the deadline. The result is `Claimed` (and `AlreadyHeld` on a rerun), not `Unconfirmed`. (REQ-2, REQ-3)
- [ ] AC-5: the local view shows this agent as holder, and the remote is unreachable. `session work` returns `Unconfirmed`, not `AlreadyHeld`. (REQ-3)
- [ ] AC-6: another agent's earlier-ordered claim arrives during confirmation. The result is `Contended`, and no active issue is recorded. (REQ-3)
- [ ] AC-7 (guard, passes on base): `set_session_issue` fails after a confirmed claim. A `LockReleased` follows, and the hub shows no lock. (REQ-5)
- [ ] AC-8 (fails first for activation): this is a fake-runtime test for each failure point: activation, missing preflight, tmux start, container start, and early container exit. Afterwards the issue is unlocked, the worktree directory and branch are gone, the worktree daemon is not running, no container or tmux session remains, and the pipeline state equals its prior value. This holds for both `run` and `plan`. (REQ-6)
- [ ] AC-9: in a reuse case (`--branch` with an existing worktree), activation fails. The worktree and branch remain. A lock, session and daemon that existed before this invocation are left as found; a lock or daemon this invocation created is undone. (REQ-6; amended 2026-10-09 by operator decision)
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
- **Release under the worktree agent's identity.** The release runs with the worktree's `.crosslink` as the crosslink directory, so the event is signed as the worktree agent and the signed log stays truthful. Built in-process, it is the same host process and key file a subprocess would use. Rejected: releasing as the host or driver identity, or through a writer built on the host's `.crosslink`.
- **The deadline bounds only the network.** The hub-lock wait and the fetch are bounded; the local reduce is not. A slow reduce of a fresh snapshot is still a correct confirmation, and bounding it would make claims fail on slow machines until the write-cost increment lands. Rejected: bounding the whole confirmation, which round 2 of the review showed would turn today's slow success into a permanent failure.
- **The 30 s value stays; what it measures changes.** Raising it, as Corvidae-Coding-Projects/crosslink#103 did, would treat write cost, and that is the write-cost increment's job.
- **Identity and trust approval are left in place.** Revoking them would make the agent's published events unverifiable.

## Amendments during the build

Dated 2026-10-08, made while building on `feat/lock-claim-outcome`:

- **AC-1** is relative to the confirmation deadline (a 6 s publication delay against a 5 s deadline), not "past 30 s".
- **AC-4** checks the bound against the hung fetch (under 20 s), not "within 4 s". Publication and the local read before confirmation are outside the bound.
- **Undo and cleanup release the lock in-process.** They use a `SharedWriter` built on the worktree's `.crosslink`, not a `crosslink locks release` subprocess. The signer is the same worktree agent. Running in-process lets the release be checked by `SharedWriter::confirmed_lock_holder`, the bounded confirmation, which a subprocess exit code cannot provide.
- **`session end` runs without notes.** With notes it would post a comment on the issue, an extra hub write for every failed kickoff. Superseded 2026-10-09 (round 2): undo and cleanup no longer end the worktree session at all. The session is local state in the worktree and is discarded with it, and a failed session end printed a remedy for a path that was then removed. REQ-6 step 2, REQ-8 step 3 and AC-11's "session ended" read accordingly.
- **Cleanup releases every lock the worktree agent holds**, because cleanup does not know the agent's issue. Amended 2026-10-09: it reads them from the host's view of the hub, and brings the worktree daemon up only when there is one to release.

Dated 2026-10-09, after the manual checks and the phase 3 review (three reviewers, recorded on #820):

- **Confirmation adopts agent tips, not the remote checkpoint** (`f60712ed`). Verifying a remote checkpoint rebuilds state from every event with an `ssh-keygen` check per event, and a claim took 302 s. Skipping it keeps the lock table correct: each agent's history is validated from sequence 1, so a missing or pruned event fails the reduce instead of hiding a claim, and a stale local checkpoint only lengthens the replay. REQ-1 is restated to match.
- **Undo brings the worktree daemon up before releasing** (`444f28bf`). Readiness can be stale by the time undo runs, for example across a sleep.
- **Spawned daemons are reaped** (`73ed70db`). A daemon stopped by the process that started it lingered as a zombie, and `daemon stop` reported it alive.
- **Review fixes:**
  - a tip that fails to adopt fails the confirmation;
  - a confirmed "no lock" is `Unconfirmed` on v3;
  - the confirmation fetch never prompts (no stdin, `GIT_TERMINAL_PROMPT=0`, ssh `BatchMode=yes` appended to the user's ssh command);
  - the timeout helper drains output while waiting and waits at most a second for it after exit;
  - `locks steal` and auto-steal act on the claim result;
  - the undo release goes through the worktree's command service and is classified from confirmation (`Stranded`, `Unverified` or done) whatever the release reported;
  - the undo report follows the original error in the returned error, so swarm and sentinel keep it;
  - cleanup stops daemons through the shared undo step and exits non-zero when it leaves work behind;
  - on a reused worktree, undo leaves what pre-existed (AC-9).
- **Reducer: late arrivals are replayed in total order** (`9b6a59ed`). The contention test (AC-6) found that a reduce applying unseen events on top of the checkpoint diverges from a total-order replay when another agent's earlier event arrives late. Two clients could then each see themselves as holder, and a checkpoint written from that state failed verification for every reader. REQ-6(b) depends on the reduce matching a total-order replay. Reduce now rebuilds from the authority baseline when the earliest unseen event orders before the latest covered one. `f60712ed` had widened the window by dropping the checkpoint adoption that recomputed on concurrent checkpoints; the gap also existed before this branch when the earlier claim was in no checkpoint. This is not a protocol change: the event and checkpoint formats are unchanged, and checkpoint verification already replays in total order. Older binaries on a mixed-version hub can still compute the wrong holder locally until they upgrade; their checkpoints from that state already fail verification on every reader.
- **Signature checks without a polling floor** (`21ff0aad`). `verify_content` waited for `ssh-keygen` with a 50 ms sleep per event, and the late-arrival replay will run whenever writers overlap. It now uses the shared timeout helper, which polls from 1 ms. Strictly this belongs to the write-cost increment; it is here because the replay above would otherwise make claims slow again.
- **Round 2 review fixes (2026-10-09):**
  - cleanup decides what the agent may hold from the host's view plus the agent's own claims read from its ref, and keeps the worktree if that cannot be read;
  - undo and cleanup no longer end the worktree session (see the superseded entry above);
  - signature verification treats incomplete output as an error, not a bad signature, and a hung `ssh-keygen` now fails after 30 s instead of blocking;
  - the late-arrival replay is logged at debug level.
- **Costs and visible effects of the reducer rule.** The rebuilt state is not persisted by the reduce itself. Until a write publishes a checkpoint covering the late event, each reduce that writes no checkpoint replays the history again: lock confirmation, side-effect-free reads, the daemon's hydration tick and dashboard polls. An agent with a lagging clock keeps producing late arrivals. This is a constraint on the write-cost increment. A late `IssueCreated` that orders earlier also renumbers display ids a client has already shown, and lock events are keyed by display id. That is the canonical total-order semantics, which diverging clients used to hide; whether hydration tolerates two ids swapping is tracked separately (#824).
- **Keep the worktree when a release does not complete** (`4a43adc9`). See the amendments to REQ-6 and REQ-8. This also brings the build back in line with REQ-11: the worktree holds the agent's `agent.json`, half the identity REQ-11 keeps.
- **Permit hold time.** A claim now holds its mutation permit through confirmation: up to 30 s more for the hub-lock wait and fetch, plus one reduce. Writes that hold permits past 90 s let the readiness record expire and the daemon exit (#816, #817), so this narrows that margin on slow machines until the write-cost and heartbeat increments land.

Where each criterion is covered:

- **Claim:** tests in `crosslink/src/commands/hub_v3_operation_tests.rs`, against real v3 hubs with a bare remote.
  - **AC-1:** deadline-relative. It does not catch a literal revert of the old bail against the fixed 30 s, which only review would.
  - **AC-2 and AC-5:** a failpoint, plus a real failing fetch.
  - **AC-3:** after-push and hydration failures.
  - **AC-4:** the grandchild in the fetch's process group and the SIGTERM lock-file cleanup.
  - **AC-4a.**
  - **AC-6:** contention found during confirmation, on the already-held path (`v3_contention_found_during_confirmation_is_contended`).
- **Reducer:** `compaction::tests::prop_reduce_from_any_checkpoint_equals_full_replay` (256 cases: lock claims and releases, issue creation with allocated display ids, two successive checkpoint cuts) and `a_late_earlier_claim_wins_over_a_checkpointed_later_claim`; both failed with the rule disabled (lock-only version). The oracle is `reduce` from empty, the total order checkpoint verification also uses. `prop_` tests run on Ubuntu CI only. The baseline branch of the rebuild is not exercised: the generated hubs have no authority baseline.
- **AC-12:** the real release on a refused push (`Stranded`) and an unreachable remote (`Unverified`), Unix only; it skips itself when run as root.
- **Undo and cleanup:** unit tests in `crosslink/src/commands/kickoff/rollback.rs` and `cleanup.rs`:
  - the step order for each failure point;
  - reuse, including pre-existing state;
  - disarming;
  - failure reporting;
  - the branch rule;
  - the pipeline restore through `undo_failed_kickoff`;
  - a host key file surviving undo (weak evidence for AC-13: undo never touches that directory, and the trust approval and the after-cleanup half are not tested);
  - an incomplete release keeping the worktree and branch, in undo and in cleanup, with the field scenario of a daemon that never becomes ready simulated at the `UndoOps` boundary (a canned `Failed` release), and an unreadable lock view keeping it too;
  - the agent's own claims read from its ref (`v3_agent_own_claims_are_read_from_its_ref`);
  - git operations on a temporary repository.
- **Daemon reaping:** `daemon::tests::a_spawned_child_is_reaped_when_it_exits`.

Not covered by automated tests:

- **The `session work` message for an unconfirmed claim** (AC-2's command-level half). Not checked by hand either.
- **AC-7.** The base revision already releases on that path; not checked by hand.
- **AC-8 end to end,** which needs the `crosslink` binary, tmux and a container runtime. Checked by hand for the activation failure point on `run` only. Kickoff's preflight runs the image first, so a missing image fails before any undo.
- **AC-10 and AC-11 end to end.** AC-11's cleanup flow was checked by hand. `--keep-branch` is untested.
- **The undo's readiness wake-up.** It would start a daemon, so no unit test runs it.
- **Windows:** the AC-4 and AC-12 tests are Unix only, and the Windows CI shards do not run the v3 operation suite.
- **Known local failure, not this branch:** `reconcile::migration::tests::production_ready_observer_completes_fallback_pointer_window` fails on this Mac on develop (`3440c0d0`) as well; its 5 s wait for a migration's pointer push is shorter than a migration takes here.

Manual checks, 2026-10-09, macOS, installed builds of this branch:

- **New claim.** With `dcbe2bd9`, `session work` on an unheld issue succeeded in 302 s, and `locks check` agreed. A stack sample showed the confirmation spending most of its time verifying the fetched remote checkpoint. `signing::verify_content` polls `ssh-keygen` with a 50 ms sleep per event. `f60712ed` confirms against agent refs without adopting the checkpoint. With it, a new claim took 85 s and an already-held lock 34 s. The remaining time is the publication's own cost (the write-cost increment).
- **Activation failure.** The kickoff aimed at an issue another agent held. The undo released, ended the session and removed the worktree and branch, and the other agent's lock was untouched. Two defects were found and fixed:
  - the undo now brings stale worktree readiness up before releasing (`444f28bf`);
  - the daemon stop reported a zombie as alive, because the spawning kickoff never reaped it; spawned daemons are now reaped (`73ed70db`).

  After the fixes, the undo reported "removed everything this kickoff created".
- **Cleanup of a stopped agent.** A local kickoff was stopped at the trust prompt, and `kickoff cleanup --force` ran on it. It stopped the daemon, released the agent's lock, removed the worktree and deleted the unchanged branch. The issue then read "not locked". This run took the path where the worktree daemon became ready; the not-ready path was not exercised by hand.

Field observation, 2026-10-09T20:43Z, from another repository's session on an installed build of this branch before `ea5402a0` (Corvidae-Coding-Projects/crosslink#110): `kickoff cleanup --force` over three timed-out plan worktrees removed the worktrees and stopped their daemons, but the worktree daemons did not become ready, so no lock was released, and each warning's remedy named the directory just removed. The lock stayed stranded with no identity left to release it. This is evidence against AC-11 and REQ-8 as implemented at that build. Fixed by `4a43adc9`, verified by `cleanup_keeps_the_worktree_when_a_release_does_not_complete` and `an_incomplete_release_keeps_the_worktree_and_branch`.
- **Observed, out of scope.** The host daemon exited, as `running: false`, after writes that held permits for more than 90 s: the 130 s cleanup, and the kickoff runs. It also parked once as `blocked_corrupt` after repeated record expiries. These are tracked on #816 and #817, for the heartbeat and write-cost increments.

## Data and compatibility

- **One additive field.** `base_commit` in kickoff metadata, optional, with a default. Metadata without it means "keep the branch".
- `LockClaimResult` and rollback state are internal, so no other persisted format changes.
- Older binaries on the same hub are unaffected by the claim, undo and cleanup changes. They do not have the reducer's late-arrival rule: until they upgrade they can compute a different lock holder locally, and checkpoints they write from that state fail verification on every reader, as they already did.

## Failure handling

- **Confirmation does not complete.** The claim stays published, the command exits non-zero, and a rerun confirms it. If nobody reruns, kickoff undo releases it: the release comes from the holder, so the reducer accepts it. Otherwise the lock stays until its holder releases it. A stale timeout does not clear it on v3. Clearing another agent's lock arrives with the lock-clearing increment (#818).
- **An undo or cleanup step fails.** The failure is reported with its remedy, and the remaining steps run.
- **The worktree cannot become ready during cleanup, or the agent's locks cannot be read.** The daemon is stopped and the worktree and branch are kept, with a remedy naming the kept path.

## Security considerations

- Releases are signed by the agent that claimed, so no new authority is added.
- Branch deletion requires a recorded base and an equal tip, so no work is lost.
- Leaving trust approvals for dead kickoff agents keeps existing exposure as it is. Removing them is out of scope.

## Verification

- **Tests:**
  - `cargo test --manifest-path crosslink/Cargo.toml --bin crosslink -- hub_v3_operation_tests compaction:: kickoff::rollback kickoff::cleanup daemon::tests::a_spawned_child lock_check sync:: utils:: signing::` (the new tests live in these modules);
  - `cargo test --manifest-path crosslink/Cargo.toml --test smoke coordination`.
- **Before every commit:** `cargo fmt --check` and `cargo clippy -- -D warnings -W clippy::unwrap_used -W clippy::expect_used`.
- **Full suite:** CI only.
- **Fake runtimes and fake git:** the kickoff tests reuse those from Corvidae-Coding-Projects/crosslink#129.
- **Status wording:** "CI passed" for the gates, "reviewed" for an independent review. Neither stands in for the other.
- **Manual check on this Mac:** run `crosslink session work <id>` with `/usr/bin/git` on `PATH`. It succeeds, or it prints the rerun message and a rerun succeeds. A run that never succeeds on this machine is a failure of this increment. Record the timings on #817.

## Rollout and rollback

- This increment merges when its tests and review pass.
- Reverting it restores the elapsed-time bail, and also removes the reducer's late-arrival rule, reopening the two-holder gap REQ-6(b) depends on.
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
