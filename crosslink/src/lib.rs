pub mod agent_flags;
pub mod agent_requests;
pub mod agents;
pub mod checkpoint;
pub mod clock_skew;
pub mod compaction;
pub mod dashboard;
pub mod db;
pub mod events;
pub mod external;
pub mod findings;
pub mod git_compat;
pub mod hub_source;
pub mod hub_v3 {
    #[deprecated(note = "use push_ref_with_lease for coordinated writes")]
    pub fn push_ref_force(
        repo_dir: &std::path::Path,
        remote: &str,
        ref_name: &str,
    ) -> anyhow::Result<PushOutcome> {
        let refspec = format!("+{ref_name}:{ref_name}");
        let output = std::process::Command::new("git")
            .current_dir(repo_dir)
            .args(["push", "--force", remote, &refspec])
            .output()
            .with_context(|| format!("failed to run git push --force for ref '{ref_name}'"))?;
        Ok(classify_push_output(&output))
    }

    include!("hub_v3.rs");
}
#[cfg(test)]
mod hub_v3_operation_tests;
pub mod hydration;
pub mod identity;
pub mod issue_file;
pub mod issue_filing;
pub mod knowledge;
pub mod lock_check;
pub mod locks;
pub mod models;
pub mod orchestrator;
pub mod pipeline;
pub mod reconcile;
pub mod seam;
pub mod server;
pub mod shared_writer;
pub mod signing;
pub mod sync;
pub mod token_usage;
pub mod trust_model;
pub mod utils;
