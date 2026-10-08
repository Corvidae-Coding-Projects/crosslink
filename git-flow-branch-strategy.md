---
title: "Git Flow Branch Strategy"
tags: ["git", "workflow", "ci-cd", "branching"]
sources:
  - url: "https://github.com/forecast-bio/crosslink/issues/129"
    title: ""
    accessed_at: "2026-03-02"
contributors: ["maxine-at-forecast--noether--ci-cd-restructure", "magnificentlycursed"]
created: 2026-03-02
updated: 2026-10-08
---



# Git Flow Branch Strategy

Crosslink uses a tiered git flow pattern to balance agent autonomy with quality gates.

## Branch Layout

```
main <---- release/v0.x.y <---- develop <---- feature/some-work
  |                                 ^
  +-- hotfix/critical-fix ----------+
```

## Branch Tiers

| Branch | Protection | CI | Purpose |
|--------|-----------|-----|---------|
| main | Strict (require PR, CI pass, no force push, no deletion) | Full suite + release publish | Production releases only |
| develop | Moderate (require PR, CI pass, no force push, no deletion) | Full test suite + clippy + fmt + cross-platform | Integration branch, all features merge here |
| feature/* | None | Fast checks only (build + unit tests) | Agent working branches, free to push |
| release/* | Moderate (CI pass, no force push, no deletion) | Full suite + dry-run publish | Release candidates |
| hotfix/* | Moderate (CI pass, no force push, no deletion) | Full suite | Emergency fixes branched from main |

## Default Branch

main is the default branch of Corvidae-Coding-Projects/crosslink (since the 2026 repository moves; forecast-bio used develop). Feature pull requests still target develop; releases go develop -> release/vX.Y.Z -> main. See RELEASING.md at the repository root for the release runbook (updated 2026-10-08).

## Agent Workflow

1. /feature creates branches from develop
2. /kickoff launches agents that work on feature/* branches
3. Agents push freely to feature/* (no protection)
4. PRs from feature/* to develop require CI pass
5. /release creates release/* from develop, opens PR to main
6. Tag on main triggers crates.io publish + GitHub release

## CI Tiering

- Feature branches (ci-feature.yml): Build + unit tests only (~2 min)
- develop/release/hotfix (ci.yml): Full suite -- lint, security audit, tests on ubuntu + macos, proptests, fuzz, release builds
- main + tags (publish.yml): Full suite + crates.io publish + GitHub release with binary artifacts

## GitHub Rulesets (configured)

The rulesets below were configured on forecast-bio/crosslink (2026-03-02). As of 2026-10-08 Corvidae-Coding-Projects/crosslink has no rulesets and no branch protection; RELEASING.md lists the intended settings (maintainer-only, immutable v* tags; pull requests and CI required on main and develop).

- main: PR required, CI (Lint + Tests x2), no force push, no deletion
- develop: PR required, CI (Lint + Tests x2), no force push, no deletion
- release/*: CI (Lint + Tests x2), no force push, no deletion
- hotfix/*: CI (Lint + Tests x2), no force push, no deletion
- feature/*: no rules (agents push freely)

## Reference

- GitHub Issue: https://github.com/forecast-bio/crosslink/issues/129
- Setup completed: 2026-03-02
