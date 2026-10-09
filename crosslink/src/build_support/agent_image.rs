// How a build chooses its default agent image. Pure functions, no I/O:
// `build.rs` includes this file to make the choice, and the kickoff tests
// include it to table-test the rules. It is not a module of the crate.

/// The published agent image repository, unless a build overrides it.
const DEFAULT_AGENT_IMAGE_REPOSITORY: &str = "ghcr.io/corvidae-coding-projects/crosslink-agent";

/// Tags a build default may never be: `latest` floats across releases, and
/// `local` is what `crosslink container build` writes.
const RESERVED_AGENT_IMAGE_TAGS: [&str; 2] = ["latest", "local"];

/// An image tag per the OCI grammar `[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}`.
fn validate_agent_image_tag(tag: &str) -> Result<(), String> {
    let mut chars = tag.chars();
    let first_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
    let rest_ok = chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
    if !first_ok || !rest_ok || tag.len() > 128 {
        return Err(format!(
            "{tag:?} is not a valid image tag ([A-Za-z0-9_][A-Za-z0-9_.-]{{0,127}})"
        ));
    }
    if RESERVED_AGENT_IMAGE_TAGS.contains(&tag) {
        return Err(format!("{tag:?} cannot be a build's default image tag"));
    }
    Ok(())
}

/// An image repository: lowercase registry path components, an optional
/// registry port, no tag or digest.
fn validate_agent_image_repository(repository: &str) -> Result<(), String> {
    let (host, path) = repository
        .split_once('/')
        .ok_or_else(|| format!("{repository:?} has no registry path"))?;
    let host_ok = !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-' | ':'));
    let path_ok = !path.is_empty()
        && path.split('/').all(|part| {
            !part.is_empty()
                && part.chars().all(|c| {
                    c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-')
                })
        });
    if host_ok && path_ok {
        Ok(())
    } else {
        Err(format!(
            "{repository:?} is not a valid image repository (lowercase registry/path, no tag)"
        ))
    }
}

/// What the build knows about where it is being built.
struct AgentImageBuild<'a> {
    /// `CROSSLINK_AGENT_IMAGE_TAG` at build time, if set and non-empty.
    override_tag: Option<&'a str>,
    /// The source is a published package (`cargo package` wrote
    /// `.cargo_vcs_info.json`), as for `cargo install crosslink`.
    packaged: bool,
    /// The build runs inside crosslink's own git checkout.
    own_checkout: bool,
    /// The tag `v<version>` points at that checkout's HEAD.
    release_tag_at_head: bool,
    version: &'a str,
}

/// The default agent image tag: an explicit override (which must be valid),
/// then the version for published packages and for checkouts at their
/// release tag, and `nightly` (the image built from `develop`) for every
/// other build. A guess never yields a version: only positive evidence does.
fn choose_agent_image_tag(build: &AgentImageBuild<'_>) -> Result<String, String> {
    if let Some(tag) = build.override_tag {
        validate_agent_image_tag(tag)
            .map_err(|error| format!("CROSSLINK_AGENT_IMAGE_TAG: {error}"))?;
        return Ok(tag.to_string());
    }
    if build.packaged || (build.own_checkout && build.release_tag_at_head) {
        validate_agent_image_tag(build.version)
            .map_err(|error| format!("crate version as image tag: {error}"))?;
        return Ok(build.version.to_string());
    }
    Ok("nightly".to_string())
}

/// The default agent image repository: an explicit, valid override (for
/// forks that publish their own image) or the published one.
fn choose_agent_image_repository(override_repository: Option<&str>) -> Result<String, String> {
    match override_repository {
        Some(repository) => {
            validate_agent_image_repository(repository)
                .map_err(|error| format!("CROSSLINK_AGENT_IMAGE_REPOSITORY: {error}"))?;
            Ok(repository.to_string())
        }
        None => Ok(DEFAULT_AGENT_IMAGE_REPOSITORY.to_string()),
    }
}
