# Releasing crosslink

This is the runbook for cutting a crosslink release and for moving the project
to a new repository or owner. It describes what the workflows do today; when a
workflow changes, change this file in the same pull request.

## What a release publishes

A `v<version>` tag pushed to `main` starts the release flow in
`.github/workflows/container-image.yml`:

| Stage | Job | What happens | Fails when |
|---|---|---|---|
| Gate | `gate` | Decides what the run may publish and the version string every binary reports. | The tag does not match `crosslink/Cargo.toml`, the tagged commit is not on `main`, or the registry does not confirm `:<version>` is unpublished (only an explicit "not found" proceeds). |
| Image | `build-binary`, `publish-image` | Builds Linux amd64/arm64 binaries that default to `:<version>`, pushes `ghcr.io/<owner>/crosslink-agent:<version>` built without the registry cache (which `develop` pushes write), with `dev.crosslink.version`, revision and source labels, and attests the build. | The build fails. |
| Smoke test | `smoke-verify` | Without logging in, checks that every pushed tag resolves to the built digest, pulls that digest on both architectures, runs `crosslink --version` and the bundled tools, and checks the version label and that the binary defaults to `:<version>`. | The package is private, a tag did not land, a tool is missing, or the binary is not the tagged version. |
| `:latest` | `promote-latest` | Points `:latest` at the verified digest. One promotion runs at a time. | Skipped for prereleases and for any version that is not the highest stable `v*` tag. |
| Binaries | `release-binaries` (`release-builds.yml`) | Builds Linux x86_64/aarch64, macOS aarch64/x86_64 and Windows x86_64 binaries plus the Codex plugin archives, and checks each binary defaults to `:<version>` and reports the gate's version. | A build or a check fails. |
| GitHub release | `publish-release` | Creates the release with the binaries, plugin archives, `SHA256SUMS` and build attestations. Prereleases are marked as such. | The smoke test or a build failed. |

The tag push also runs `publish.yml` (crates.io). That workflow is not part of
the gated flow yet and needs the crate's ownership and credential settled first;
until then it fails on tag pushes from this repository.

Every released build defaults to the agent image of its own version, so a
release is only usable once `:<version>` is published and public. The order
above guarantees that the GitHub release never points at an unverified image.

## Before you tag

1. Release from a `release/v<version>` branch cut from `develop`, opened as a
   pull request to `main`.
2. Bump `version` in `crosslink/Cargo.toml` (and `Cargo.lock`). A version that
   was ever published anywhere, including crates.io, cannot be reused.
3. Move the `[Unreleased]` entries in `CHANGELOG.md` under the new version.
4. Make sure CI on the release pull request is green, then merge it.
5. Optional, before the first release from a new repository or after changing
   the workflow: run **Container Image** manually on `develop` with
   *rehearse_promotion*. It exercises the gate, smoke test and promotion
   against `:promotion-rehearsal` without touching `:latest`.

## Tag and watch

Create an annotated tag `v<version>` on the merged `main` commit and push it to
the repository.

Watch the **Container Image** run for the tag. When it is green:

- `ghcr.io/<owner>/crosslink-agent:<version>` exists and pulls anonymously;
- for a stable release, `:latest` names the same digest;
- the GitHub release has the binaries and `SHA256SUMS`.

Then merge `main` back into `develop` so the next release does not conflict
(skipping this back-merge is what made the v0.9.0-beta.1 cut painful).

## If something fails

- **Gate fails:** nothing was published. Fix the version or the commit and tag
  again. Delete the failed tag (an admin can, even once tags are protected):
  promotion takes "highest stable" from git tags, so a stray higher tag would
  stop `:latest` from moving and turn the canary red.
- **A later stage fails for a transient reason:** use **Re-run failed jobs**.
  A re-run of the image job reuses the digest that was already pushed instead
  of rebuilding it. Re-running all jobs starts at the gate again, which
  refuses because `:<version>` now exists. Build artifacts are kept for one
  day, so re-run within a day or release the next patch version.
- **Smoke test fails:** `:<version>` exists but `:latest` did not move and no
  GitHub release was created. Find the cause, fix it, and release the next
  patch version. Do not republish the same version: the gate refuses, on
  purpose, because pinned tags are cached by users.
- **Bad release after promotion (rollback):** point `:latest` at the previous
  release's digest by hand:

  ```
  docker buildx imagetools create --tag ghcr.io/<owner>/crosslink-agent:latest \
    ghcr.io/<owner>/crosslink-agent@<previous digest>
  ```

  Then mark the GitHub release as a prerelease or delete its assets, and ship a
  fixed patch release.

## Images and their guarantees

| Tag | Moves? | Who uses it |
|---|---|---|
| `:<version>` | Not by the release flow (the gate refuses to republish and re-runs reuse the pushed digest); any repository writer can still push one from another workflow until rulesets restrict that | Released builds of that version |
| `:latest` | To each stable release, after its smoke test | Anyone who asks for it with `--image` |
| `:nightly` | On every push to `develop` | Development builds; refreshed before each launch |
| `:nightly-<sha>` | Not by design, but registry tags can be moved | Pinning a development build |

Only a digest pin is byte-for-byte reproducible. Images and binaries carry
build attestations (`gh attestation verify --owner <owner> <file or oci://image>`).

Because development builds follow `:nightly`, anything merged to `develop` runs
in every development build's next container, which mounts the user's provider
and GitHub logins. Protect `develop` accordingly.

Known limit: `cargo install --git … --tag v<version>` cannot see its release tag
and defaults to `:nightly`; such users should pass `--image` with their version.

The daily **Agent Image Canary** checks, anonymously, that `:nightly`, every
release's `:<version>` and `:latest`, and the frozen
`ghcr.io/forecast-bio/crosslink-agent:latest` (which 0.9.0-beta.1 and older
builds default to, pinned to its digest) can still be pulled. Never delete or
make private an image that a released version defaults to.

## Updating pinned tools in the image

`crosslink/resources/container/Dockerfile` pins the base image by digest and gh,
Codex, Claude Code, uv and gosu by version (gosu by checksum too). Still
floating: Ubuntu's apt packages, the gh apt keyring, Codex's npm dependencies,
and the Claude Code and uv installer scripts. To update a pin, change its `ARG`
(and checksum) in a pull request; the new tools reach users in the next
`:nightly` and the next release. Released `:<version>` images are not rebuilt
with newer tools: ship a patch release instead.

## Required repository settings

- **Packages:** `crosslink-agent` is public and its Actions access lets this
  repository write. A package created by a workflow starts private; make it
  public before the first release from a new owner.
- **Permissions:** workflows default to read-only tokens; the publishing jobs
  request `packages: write`, `contents: write`, `id-token: write` and
  `attestations: write` themselves.
- **Environments:** `github-pages` for the docs; `cargo` (tag-only deployment
  policy) once crates.io publishing is wired up.
- **Rulesets (intended; not configured as of 2026-10-08):** `v*` tags may be
  created only by maintainers and never moved (deletion by an admin only, for
  tags whose release failed at the gate); `main` and `develop` require pull
  requests and passing CI and refuse force pushes and deletion. Until they
  exist, the release checks guard against accidents only: the gate runs from
  the tagged commit's own workflow, so anyone with write access can tag any
  commit, edit the checks out, and publish a release, and can push to
  `develop`, which every development build's `:nightly` follows. The rulesets
  are the only control against that.

## Moving the repository or owner

Each past move (dollspace-gay/chainlink → forecast-bio/crosslink →
dollspace-gay/crosslink → Corvidae-Coding-Projects/crosslink) dropped part of
publishing. Repository redirects do not cover GHCR, crates.io, Pages or
settings. When moving:

1. Update `repository`/`homepage` in `crosslink/Cargo.toml`, README links, the
   VS Code extension metadata, `docs_src/_quarto.yml`, and the default image
   repository in `crosslink/src/build_support/agent_image.rs`. Leave historical
   references (CHANGELOG entries, old issue links) pointing where they pointed.
2. Make the new owner's `crosslink-agent` package public and link it to the
   repository after its first publish; check with an anonymous pull.
3. Add the new owner to the crates.io crate (or set up trusted publishing for
   the new repository) and recreate the `cargo` environment and credential.
4. Recreate Actions secrets, environments (including `github-pages`) and the
   rulesets above, and enable Pages with the workflow source.
5. Keep the old owner's images public and frozen (released builds still pull
   them); mark them deprecated in the package description and add them to the
   canary with their digests.
6. Cut a patch release from the new repository so crates.io metadata, the
   release assets and `:<version>` all exist under the new owner.
