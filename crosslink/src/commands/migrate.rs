use std::path::Path;

use anyhow::{bail, Result};

pub fn to_shared_repository(crosslink_dir: &Path) -> Result<()> {
    crate::reconcile::migration::run_forward_compatibility(crosslink_dir, "migrate to-shared")
}

pub fn from_shared_repository() -> Result<()> {
    bail!(
        "`crosslink migrate from-shared` cannot reverse the canonical repository authority; run `crosslink daemon ensure --wait-ready --json` to reconcile and hydrate the local projection"
    )
}

pub fn rename_branch(_crosslink_dir: &Path) -> Result<()> {
    bail!(
        "`crosslink migrate rename-branch` cannot rename canonical authority refs independently; run `crosslink migrate hub-v3` to reconcile every historical ref family through the verified generation protocol"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reverse_and_rename_adapters_reject_independent_authority_changes() {
        let directory = tempfile::tempdir().unwrap();
        let from = from_shared_repository().unwrap_err().to_string();
        assert!(from.contains("cannot reverse"));
        assert!(from.contains("daemon ensure"));
        let rename = rename_branch(directory.path()).unwrap_err().to_string();
        assert!(rename.contains("cannot rename"));
        assert!(rename.contains("migrate hub-v3"));
    }
}
