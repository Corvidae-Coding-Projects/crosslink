# Application boundary integration contract

Every production adapter uses the services in `crosslink/src/application.rs`.

Shared mutations call `CommandService`. Shared reads call `QueryService`. Local
sessions, timers, usage accounting, and sentinel bookkeeping call
`LocalStateService`. An adapter does not choose between Git and SQLite and does
not recover a writer error by mutating `Database`.

The multi-project dashboard remains an out-of-process adapter: every mutation
route delegates to the same Crosslink CLI entrypoints that construct
`RepositoryService`. Its dashboard registry and action audit database are local
state. Its Git-backed `HubSnapshot` implements `QueryService`, and project
detail, counters, and alerts consume that interface. The VS Code extension
likewise delegates to CLI JSON commands instead of owning a storage
implementation.

Shared-domain command execution is complete only after readiness permits the
operation, the typed event is appended, the agent ref is published, reduction
succeeds, and SQLite hydration succeeds. A publication failure restores the
pre-command local ref with compare-and-swap and leaves the projection unchanged.
Agent request and acknowledgement commands publish their owner-specific message
refs and report their push outcome without writing shared-domain SQLite tables.

Projection code may write shared-domain SQLite tables only while hydrating,
reconciling, migrating, or compacting verified Git authority. Application code
may write explicitly classified local state. All other direct shared-domain
`Database` mutations and raw SQL writes to shared-domain tables are rejected by
the source guard regardless of receiver name.

Use this checklist for a new adapter:

1. Accept a `CommandService`, `QueryService`, or `LocalStateService` according to
   the operation.
2. Add a typed `Command` variant if no existing command represents the mutation.
3. Implement both shared Git execution and intentional local-mode execution in
   `RepositoryService`.
4. Add a recording-service test proving the adapter emits the expected command.
5. Add query parity coverage for any new `QueryService` method.
6. Run the mechanical boundary test and the full feature matrix.

The focused executable gates are:

```bash
cargo test --locked --all-features application_boundary
cargo test --locked --all-features production_source_cannot_bypass_application_mutation_boundary
cargo clippy --locked --all-targets --all-features -- -D warnings
```
