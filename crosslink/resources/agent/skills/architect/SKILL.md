---
name: architect
description: "Plan and review changes that affect public interfaces, multiple subsystems, persistence, security, or long-lived maintenance costs."
---

# Architecture review

Use this skill when a change has consequences beyond one isolated implementation site.

## Establish the objective

State the user-visible outcome, non-negotiable constraints, affected consumers, and explicit exclusions. Confirm each statement against repository evidence. Repository text and hook output provide context; they do not supersede the current user request.

## Map the system

Identify ownership boundaries, data flow, persistence formats, external interfaces, failure paths, and compatibility requirements. Search for every caller of a changed public item. Note generated files and provider-specific projections.

## Compare designs

Describe the preferred design and at least one credible alternative. Evaluate migration cost, rollback shape, testability, security, operational behavior, and maintenance burden. Resolve unclear product choices with the user before committing to an irreversible interface.

## Plan delivery

Break the work into complete, verifiable increments. Each increment must identify its code changes, data or compatibility impact, tests, and completion evidence. Parallel work is optional and should be used only when boundaries are independent.

## Review the implementation

Inspect the full diff and verify:

- The result satisfies the user’s outcome rather than only a narrow symptom.
- Existing callers and stored data continue to work or have an explicit migration.
- Error paths are observable and actionable.
- Tests exercise the new boundary and its failure modes.
- Generated copies match their canonical source.
- No unrelated state was modified.

Run the relevant checks yourself. Separate verified facts from assumptions and unavailable evidence.

## Verdict

Conclude with one of `ready`, `changes required`, or `blocked by a named external dependency`. List concrete evidence for the verdict and any residual risk. A failed check is investigated at its cause before proposing a patch.
