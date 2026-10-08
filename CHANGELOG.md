# Changelog

All notable changes to Crosslink will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).

## [Unreleased]

### Added

- Container commands default to the agent image that matches the crosslink
  build: released versions use `ghcr.io/corvidae-coding-projects/crosslink-agent:<version>`
  and development builds use `:nightly`, replacing the `:latest` default that
  could lag behind or run ahead of the CLI (gh#125). A build counts as a
  release only on positive evidence: a published package, a checkout at its
  `v<version>` tag, or an explicit `CROSSLINK_AGENT_IMAGE_TAG`, which must be a
  valid tag or the build fails. `CROSSLINK_AGENT_IMAGE_REPOSITORY` lets fork
  builds default to their own registry. `:nightly` and `:latest` are refreshed
  before every launch, falling back to a local copy with a warning when the
  registry is unreachable. Every container command (kickoff, `container
  start`, `container auth`, swarm and sentinel) resolves its image the same
  way: `--image`, then `CROSSLINK_CONTAINER_IMAGE`, then the build default, and
  reports where the image came from.
- Releases are one gated flow (gh#126, gh#122, gh#123). A `v*` tag must match
  `crosslink/Cargo.toml` and must not already be published (versions are never
  republished). It pushes only `:<version>`; the smoke test pulls that exact
  digest anonymously and checks that every pushed tag resolves to it, so a
  private package or a missing tag fails CI; `:latest` then moves to the
  verified digest, one promotion at a time, and only when the version is the
  highest stable release. The GitHub release is created only after the image
  passed, with Linux (x86_64, aarch64), macOS (aarch64, x86_64) and Windows
  binaries, the Codex plugin archives, `SHA256SUMS`, and build attestations.
  Images carry their crosslink version as a label and a build attestation.
  Write permissions are granted only to the publishing jobs, whose actions are
  pinned by commit. Manual publishes run only from `develop`, and a manual run
  can rehearse the promotion against `:promotion-rehearsal`. A daily canary
  checks, anonymously, `:nightly`, every release's `:<version>` and
  `:latest`, and that the frozen `ghcr.io/forecast-bio/crosslink-agent:latest`
  0.9.0-beta.1 uses still names the same image.
- The agent image pins its base image by digest and its tools by version
  (GitHub CLI, Codex, Claude Code, uv, gosu), and verifies gosu by checksum, so
  two builds of the same commit contain the same tools (gh#128).
