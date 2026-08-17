---
name: design
description: "Develop a codebase-grounded feature design and save it under .design/ for review or kickoff."
---

# Feature design

Accept a feature description, `--issue <id>`, `--gh-issue <number>`, or `--continue <slug>`.

## Explore

Read relevant issues, manifests, architecture documents, implementations, tests, and prior designs. Search for the concrete types, functions, storage, and interfaces the feature would change. Record compatibility constraints and existing patterns.

Ask only questions whose answers materially change scope, behavior, data shape, security, or architecture. Tie each question to evidence from the repository.

## Draft

Create `.design/<slug>.md` with:

```markdown
# Feature: <name>

## Summary
## User-visible behavior
## Requirements
## Acceptance criteria
## Current architecture
## Proposed design
## Data and compatibility
## Failure handling
## Security considerations
## Verification
## Rollout and rollback
## Open questions
## Out of scope
```

Requirements must be testable. Acceptance criteria must state observable completion conditions. Name the source files and interfaces involved without inventing paths.

## Resolve and iterate

Present open decisions one at a time when user input is required, update the document with the answer, and remove resolved questions. With `--continue`, compare the draft against current code and revise stale assumptions.

## Validate

Check that every requirement maps to acceptance evidence, migrations are explicit, failure paths are covered, and out-of-scope boundaries are clear. Search Crosslink knowledge for relevant records and cite only material actually used.

Initialize the design pipeline state when the CLI supports it, then report the document path, resolved choices, remaining questions, and the suitable `crosslink kickoff --doc` command.
