# Crosslink Architecture Map

## High-Level ASCII Map

```
┌─────────────────────────────────────────────────────────────────────┐
│                          CROSSLINK CLI                              │
│                         (main.rs / clap)                            │
├─────────────────────────────────────────────────────────────────────┤
│                                                                     │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────────────────┐  │
│  │   COMMANDS    │  │  DATA LAYER  │  │  COORDINATION SYSTEM     │  │
│  │  (35 modules) │  │              │  │                          │  │
│  │              │  │  models.rs   │  │  events.rs  (append-only) │  │
│  │  create      │  │  db.rs       │  │  sync.rs    (hub branch)  │  │
│  │  show/list   │──│  issue_file  │──│  compaction (reduce)      │  │
│  │  session     │  │  hydration   │  │  checkpoint (snapshot)    │  │
│  │  comment     │  │              │  │  shared_writer (writes)   │  │
│  │  ...         │  │  SQLite ◄────│──│──── JSON on git           │  │
│  └──────────────┘  │  (cache)     │  │    (source of truth)     │  │
│                    └──────────────┘  └──────────────────────────┘  │
│                                                                     │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────────────────┐  │
│  │   IDENTITY   │  │    LOCKS     │  │    KNOWLEDGE             │  │
│  │              │  │              │  │                          │  │
│  │  identity.rs │  │  locks.rs    │  │  knowledge.rs            │  │
│  │  signing.rs  │  │  lock_check  │  │  (orphan branch)         │  │
│  │  trust.rs    │  │              │  │  YAML frontmatter + MD   │  │
│  │  (SSH keys)  │  │  (V1 file /  │  │  conflict resolution     │  │
│  │              │  │   V2 event)  │  │                          │  │
│  └──────────────┘  └──────────────┘  └──────────────────────────┘  │
│                                                                     │
│  ┌──────────────┐  ┌──────────────┐                                │
│  │  CONTAINER   │  │    DAEMON    │                                │
│  │              │  │              │                                │
│  │  container.rs│  │  daemon.rs   │                                │
│  │  Dockerfile  │  │  (bg sync)   │                                │
│  │  entrypoint  │  │              │                                │
│  └──────────────┘  └──────────────┘                                │
└─────────────────────────────────────────────────────────────────────┘
        │
        │ deployed by `crosslink init`
        ▼
┌─────────────────────────────────────────────────────────────────────┐
│               PROVIDER-NEUTRAL AGENT INTEGRATION LAYER              │
│                                                                     │
│  ┌──── CANONICAL HOOKS (.crosslink/integrations/hooks/) ────────┐  │
│  │                                                               │  │
│  │  session-start.py    SessionStart   report live state        │  │
│  │  prompt-guard.py     Prompt events  load rule-file surfaces  │  │
│  │  work-check.py       PreToolUse     enforce tracking config  │  │
│  │  post-edit-check.py  PostToolUse    edit diagnostics         │  │
│  │  pre-web-check.py    PreToolUse     source boundary notice   │  │
│  │  crosslink_config.py (shared)       configuration loading    │  │
│  │                                                               │  │
│  └───────────────────────────────────────────────────────────────┘  │
│                                                                     │
│  ┌──── SKILLS (.agents/skills/) + PROVIDER PROJECTIONS ─────────┐  │
│  │                                                               │  │
│  │  Canonical skills are installed once under `.agents/skills`. │  │
│  │  Claude: `.claude/settings.json` + `.claude/skills`           │  │
│  │  Codex: `.codex/hooks.json` + `AGENTS.md` + local plugin      │  │
│  │  Both provider configs call the same canonical hook scripts. │  │
│  │                                                               │  │
│  └───────────────────────────────────────────────────────────────┘  │
│                                                                     │
│  ┌──── COMPATIBILITY PATHS (.crosslink/rules/) ─────────────────┐  │
│  │                                                               │  │
│  │  Rule filenames remain wired through prompt-guard.py.        │  │
│  │  Every bundled Markdown input is currently zero bytes.       │  │
│  │                                                               │  │
│  └───────────────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────────────┘
```

## Agent Provider Boundary

`src/agents/` is the protocol boundary for agent execution. Configuration selects
`agent.provider` (`claude`, `codex`, or `custom`); `agent.binary` only overrides
the executable path. Semantic model tiers are resolved through provider-specific
maps before an adapter creates an `AgentInvocation` containing argv, environment,
stdin, output protocol, sandbox, approval, authentication, and timeout policy.

Claude and Codex adapters never construct a shell command. The invocation is only
quoted at the tmux/container boundary. Codex structured runs use
`codex exec - --json`; Claude structured runs use stream JSON. Both are normalized into the same
runtime event model before status, dashboard, orchestrator, and usage consumers see
them. Raw provider JSONL remains alongside normalized JSONL for diagnostics.

`crosslink init` installs both provider integrations by default. The
`--agent-integration claude|codex|both` selector changes installed projections,
not the runtime provider. Canonical hooks, MCP servers, skills, schemas, and
instructions live under `resources/agent/`; provider directories remain thin.

Hook trust bypass is emitted only after Crosslink verifies the complete managed
hook manifest and script digests. Project-local and plugin hook copies share an
atomic event claim so one logical event is processed once. Codex hosted web search
does not traverse local tool hooks, so external-content provenance is also carried
by repository instructions and skills; fetched words are evidence, never commands.

## Data Flow

```
CLI / HTTP / dashboard / TUI / sentinel / kickoff / swarm / orchestrator
                              │
               ┌──────────────┴──────────────┐
               ▼                             ▼
         CommandService                 QueryService
               │                             │
               ▼                             ▼
       readiness + operation          SQLite projection
               │
               ▼
       append typed Git event
               │
               ▼
       publish per-agent ref
               │
               ▼
       reduce + hydrate SQLite
```

## Application authority boundary

`application::RepositoryService` is the production implementation of both
`CommandService` and `QueryService`. Shared-mode commands require a healthy Git
writer and successful publication before the SQLite projection is hydrated. A
missing writer, failed readiness check, unavailable remote, rejected ref update,
or failed hydration returns an error. Crosslink never converts those failures
into an authoritative SQLite write.

`QueryService` reads shared-domain data from the verified SQLite projection.
The multi-project dashboard provides a Git-backed `HubSnapshot` implementation,
so dashboard detail, counter, and alert reads use the same query contract without
treating its machine-local registry database as repository authority.
`LocalStateService` separates machine-local operations: sessions, timers, token
usage, and sentinel run/dispatch state. Session-to-issue association is typed as
a command while remaining an explicit local operational link. Projection,
hydration, reconciliation, migration, and compaction are the only code allowed to
write shared-domain SQLite tables directly.

New adapters must accept or construct the application services, invoke typed
methods, and return application failures unchanged. They must not accept an
optional writer and must not call a shared-domain `Database` mutation method.
The `production_source_cannot_bypass_application_mutation_boundary` test rejects
direct mutation methods under any receiver name and raw SQL writes to shared
tables outside projector modules.

## Git Branch Layout

```
main                     ← user's code
  └─ feature/*           ← work branches (worktrees)

refs/heads/crosslink/agents/{agent-id}
  └─ events.log          ← one append-only stream per writer

refs/heads/crosslink/checkpoint
  └─ state.json          ← versioned reduced state plus per-agent causal frontier

refs/heads/crosslink/meta
  └─ trust and format metadata

crosslink/hub-v3-host    ← local implementation worktree, not authority

crosslink/knowledge      ← shared research (orphan branch)
  └─ pages/
      └─ {slug}.md       ← knowledge pages with YAML frontmatter
```

Each causal-frontier entry records the highest contiguous sequence applied for
one agent, the pinned Git tip that proves the prefix, and a digest of the event
prefix. Reduction selects unseen events independently per agent before applying
the deterministic total order used for conflict resolution. Checkpoints are
adopted only when a verified frontier dominates the local frontier; concurrent
frontiers are recomputed from pinned authority. Daemon reconciliation upgrades
legacy global-watermark checkpoints from their recorded genesis snapshot and
complete agent histories before publishing the replacement schema.

## Command Modules (src/commands/)

### Issue Management
| Module | Commands | Purpose |
|--------|----------|---------|
| `create.rs` | `create`, `quick`, `subissue` | Create issues with templates, labels, auto-work |
| `show.rs` | `show` | Display full issue details + relationships |
| `list.rs` | `list` | Filtered table or JSON output |
| `search.rs` | `search` | Text search across titles/descriptions/comments |
| `update.rs` | `update` | Modify title, description, priority |
| `delete.rs` | `delete` | Remove issue (cascades children) |
| `status.rs` | `close`, `close-all`, `reopen` | Status transitions, auto-changelog |

### Relationships & Organization
| Module | Commands | Purpose |
|--------|----------|---------|
| `deps.rs` | `block`, `unblock`, `blocked`, `ready` | Dependency graph |
| `relate.rs` | `relate`, `unrelate`, `related` | Bidirectional links |
| `tree.rs` | `tree` | Hierarchy visualization |
| `next.rs` | `next` | Smart priority scoring for next task |
| `label.rs` | `label`, `unlabel` | Changelog categorization |
| `milestone.rs` | `milestone *` | Release grouping |
| `timer.rs` | `start`, `stop`, `timer` | Time tracking |

### Workflow & Session
| Module | Commands | Purpose |
|--------|----------|---------|
| `session.rs` | `session start/end/status/work/action` | Session lifecycle + handoff |
| `comment.rs` | `comment` | Typed comments (plan/decision/observation/...) |
| `intervene.rs` | `intervene` | Log driver interventions for audit |
| `tested.rs` | `tested` | Mark tests run (resets reminder) |
| `workflow.rs` | `workflow diff/trail` | Policy drift detection, comment trails |

### Multi-Agent & Infrastructure
| Module | Commands | Purpose |
|--------|----------|---------|
| `agent.rs` | `agent init/status/bootstrap` | Agent identity + SSH keys |
| `trust.rs` | `trust approve/revoke/list/pending/check` | SSH trust management |
| `locks_cmd.rs` | `locks list/check/claim/release/steal`, `sync` | Lock management |
| `container.rs` | `container build/start/ps/logs/stop/rm/kill/shell/snapshot` | Docker agent execution |
| `compact.rs` | `compact` | Manual event compaction |
| `knowledge.rs` | `knowledge add/show/list/edit/remove/sync/search` | Shared research pages |
| `context.rs` | `context measure/check` | Installed agent asset measurement |
| `config.rs` | `config show/get/set/list/reset/diff` | Hook configuration |

### Data Management
| Module | Commands | Purpose |
|--------|----------|---------|
| `init.rs` | `init` | Project setup (hooks, rules, db, signing) |
| `export.rs` | `export` | JSON/markdown export |
| `import.rs` | `import` | JSON import |
| `archive.rs` | `archive add/remove/list/older` | Issue archival |
| `migrate.rs` | `migrate-to-shared/from-shared/rename-branch` | Schema migration |
| `integrity_cmd.rs` | `integrity counters/hydration/locks/schema` | Data integrity checks |
| `style.rs` | `style set/sync/diff/show/unset` | House style syncing |
| `cpitd.rs` | `cpitd scan/status/clear` | Code clone detection |

## Hook Execution Flow

```
Provider session starts
        │
        ▼
  ┌─────────────────┐
  │  session-start   │  (SessionStart — once per session)
  │  auto-end stale  │
  │  show handoff    │
  └────────┬────────┘
           ▼
  ┌─────────────────┐
  │  prompt-guard    │  (prompt and subagent events)
  │  wired rule set │──► bundled Markdown files are empty
  └────────┬────────┘
           ▼
     Agent works...
           │
           ▼
  ┌─────────────────┐
  │  work-check      │  (PreToolUse — before Write/Edit/Bash)
  │                  │
  │  strict:  BLOCK  │──► must have active issue
  │  normal:  WARN   │──► reminder but allow
  │  relaxed: PASS   │──► no enforcement
  │                  │
  │  always:  block  │──► git push/merge/reset/etc.
  │  gated:   check  │──► git commit needs active issue
  │                  │
  └────────┬────────┘
           ▼
  ┌─────────────────┐
  │  pre-web-check   │  (PreToolUse — before WebFetch/WebSearch)
  │  source boundary │
  └────────┬────────┘
           ▼
  ┌─────────────────┐
  │  post-edit-check │  (PostToolUse — after Write/Edit)
  │  stub detection  │
  │  test reminders  │
  └─────────────────┘
```

## Key Architecture Decisions

| Decision | Implementation | Why |
|----------|---------------|-----|
| Event sourcing | Append-only NDJSON logs per agent | Audit trail, conflict-free merge, offline-safe |
| Git as coordination DB | `crosslink/hub` orphan branch | Distributed, no external service needed |
| Dual storage | JSON on git (truth) + SQLite (cache) | Fast local reads, durable distributed state |
| UUID-first IDs | Create with UUID, display_id assigned on push | Offline creation, eventual consistency |
| SSH signing (not GPG) | Ed25519 keys, AllowedSigners format | Modern, fast, offline verification |
| Provider-neutral hooks | Shared Python scripts with Claude and Codex projections | One implementation for repository checks |
| Zeroed rule inputs | Wired Markdown paths with zero-byte bundled files | Removes the shipped prose without removing the integration |
| On-demand skills | Provider-specific skill projections | Workflows load only when selected |

## Web Dashboard

`crosslink serve` starts a local HTTP server built on [axum](https://github.com/tokio-rs/axum) that provides a browser-based interface for monitoring and managing crosslink state.

### Frontend

The dashboard is a React single-page application built with TypeScript, Vite, and TailwindCSS 4. UI components come from shadcn/ui. The SPA is embedded into the crosslink binary at compile time and served as static assets.

### REST API

The server exposes REST endpoints for all core crosslink data:

- `/api/issues` — issue CRUD, filtering, search
- `/api/sessions` — session lifecycle and history
- `/api/agents` — agent registration and status
- `/api/knowledge` — knowledge page listing and content
- `/api/milestones` — milestone progress tracking
- `/api/sync` — trigger coordination branch sync
- `/api/config` — read and update hook configuration

### WebSocket

A WebSocket endpoint at `/ws` provides real-time updates for agent monitoring. Clients receive push notifications for heartbeat changes, lock acquisitions/releases, issue state transitions, and session events.

### DAG Execution Engine

The orchestrator workflow system uses a directed acyclic graph (DAG) engine to plan and execute multi-step agent workflows. The dashboard visualizes DAG state, phase progress, and dependency edges.

### Data Sources

All reads come from the same SQLite database and git coordination branches (`crosslink/hub`, `crosslink/knowledge`) used by the CLI. The web server holds no separate state — it is a read/write interface to the existing crosslink data layer.
