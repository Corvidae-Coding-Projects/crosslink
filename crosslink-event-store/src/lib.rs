//! Narrow, payload-neutral storage primitives extracted from Crosslink Hub v3.
//!
//! Each writer owns one Git ref in a bare repository. Appends use plumbing
//! commands and finish with a compare-and-swap `update-ref`, so crashes before
//! the final step can leave only unreachable objects, never a partial stream.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use anyhow::{anyhow, bail, ensure, Context, Result};
use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const WRITER_REF_PREFIX: &str = "refs/heads/crosslink/runtime/writers/";
pub const CHECKPOINT_REF: &str = "refs/heads/crosslink/runtime/checkpoint";
pub const LOG_NAME: &str = "events.ndjson";
pub const CHECKPOINT_NAME: &str = "checkpoint.json";
pub const EXPORT_SCHEMA: &str = "crosslink.event_store_export";
pub const EXPORT_VERSION: u32 = 1;

#[derive(Debug, Clone)]
pub struct GitEventStore {
    repo: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppendOutcome {
    pub writer_id: String,
    pub sequence: u64,
    pub old_commit: Option<String>,
    pub new_commit: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionOutcome {
    pub writer_id: String,
    pub records: u64,
    pub log_sha256: String,
    pub old_commit: String,
    pub new_commit: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredLine {
    pub writer_id: String,
    pub sequence: u64,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamSummary {
    pub writer_id: String,
    pub tip: String,
    pub records: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportManifest {
    pub schema: String,
    pub version: u32,
    pub created_at: DateTime<Utc>,
    pub streams: Vec<StreamSummary>,
    pub checkpoint_sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegrityReport {
    pub valid: bool,
    pub streams: Vec<StreamSummary>,
    pub findings: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AbortAfter {
    Never,
    HashObject,
    Tree,
    Commit,
}

impl GitEventStore {
    /// Open an existing bare repository or initialize a new one.
    pub fn open_or_init(path: impl Into<PathBuf>) -> Result<Self> {
        let repo = path.into();
        if !repo.exists() {
            fs::create_dir_all(&repo)
                .with_context(|| format!("failed to create {}", repo.display()))?;
            let output = Command::new("git")
                .args(["init", "--bare", "--quiet"])
                .arg(&repo)
                .output()
                .context("failed to execute git init --bare")?;
            require_success(output, "git init --bare")?;
        }
        ensure!(
            repo.join("HEAD").is_file(),
            "{} is not a bare Git repository",
            repo.display()
        );
        let bare = git_output(&repo, &["rev-parse", "--is-bare-repository"], None)?;
        ensure!(
            bare.trim() == "true",
            "{} must be a bare Git repository",
            repo.display()
        );
        fs::create_dir_all(repo.join("runtime-locks"))?;
        Ok(Self { repo })
    }

    #[must_use]
    pub fn repo(&self) -> &Path {
        &self.repo
    }

    pub fn append(&self, writer_id: &str, json_line: &[u8]) -> Result<AppendOutcome> {
        self.append_with_abort(writer_id, json_line, AbortAfter::Never)
    }

    fn append_with_abort(
        &self,
        writer_id: &str,
        json_line: &[u8],
        abort: AbortAfter,
    ) -> Result<AppendOutcome> {
        validate_writer_id(writer_id)?;
        validate_json_line(json_line)?;
        let _lock = self.lock_writer(writer_id)?;
        let ref_name = writer_ref(writer_id)?;
        let old_commit = git_rev_parse_optional(&self.repo, &ref_name)?;
        let mut bytes = self.read_log_at(old_commit.as_deref())?;
        let existing = parse_log(&bytes).context("existing writer stream is corrupt")?;
        let sequence = u64::try_from(existing.len())? + 1;
        bytes.extend_from_slice(trim_line_ending(json_line));
        bytes.push(b'\n');

        let blob = git_output(&self.repo, &["hash-object", "-w", "--stdin"], Some(&bytes))?;
        if abort == AbortAfter::HashObject {
            bail!("injected abort after hash-object");
        }
        let tree_input = format!("100644 blob {blob}\t{LOG_NAME}\n");
        let tree = git_output(&self.repo, &["mktree"], Some(tree_input.as_bytes()))?;
        if abort == AbortAfter::Tree {
            bail!("injected abort after mktree");
        }
        let commit = commit_tree(
            &self.repo,
            &tree,
            old_commit.as_deref(),
            &format!("runtime event: writer {writer_id} seq {sequence}"),
            writer_id,
        )?;
        if abort == AbortAfter::Commit {
            bail!("injected abort after commit-tree");
        }
        update_ref_cas(&self.repo, &ref_name, &commit, old_commit.as_deref())?;
        Ok(AppendOutcome {
            writer_id: writer_id.to_owned(),
            sequence,
            old_commit,
            new_commit: commit,
        })
    }

    pub fn writer_tip(&self, writer_id: &str) -> Result<Option<String>> {
        git_rev_parse_optional(&self.repo, &writer_ref(writer_id)?)
    }

    pub fn read_writer(&self, writer_id: &str) -> Result<Vec<Vec<u8>>> {
        let tip = self.writer_tip(writer_id)?;
        parse_log(&self.read_log_at(tip.as_deref())?)
    }

    pub fn read_writer_at(&self, writer_id: &str, tip: &str) -> Result<Vec<Vec<u8>>> {
        validate_writer_id(writer_id)?;
        let expected = self.writer_tip(writer_id)?;
        ensure!(expected.is_some(), "writer {writer_id} does not exist");
        parse_log(&self.read_log_at(Some(tip))?)
    }

    /// Replace one writer's commit ancestry with a parentless commit containing
    /// the exact same append log. This compacts Git history without pruning any
    /// audit record or changing writer sequences.
    pub fn compact_writer_history(&self, writer_id: &str) -> Result<CompactionOutcome> {
        validate_writer_id(writer_id)?;
        let _lock = self.lock_writer(writer_id)?;
        let ref_name = writer_ref(writer_id)?;
        let old_commit = git_rev_parse_optional(&self.repo, &ref_name)?
            .context("cannot compact a writer without records")?;
        let bytes = self.read_log_at(Some(&old_commit))?;
        let records = parse_log(&bytes)?;
        ensure!(!records.is_empty(), "cannot compact an empty writer stream");
        let blob = git_output(&self.repo, &["hash-object", "-w", "--stdin"], Some(&bytes))?;
        let tree_input = format!("100644 blob {blob}\t{LOG_NAME}\n");
        let tree = git_output(&self.repo, &["mktree"], Some(tree_input.as_bytes()))?;
        let new_commit = commit_tree(
            &self.repo,
            &tree,
            None,
            &format!("compact runtime writer {writer_id}"),
            writer_id,
        )?;
        update_ref_cas(&self.repo, &ref_name, &new_commit, Some(&old_commit))?;
        ensure!(
            self.read_log_at(Some(&new_commit))? == bytes,
            "compacted writer bytes differ"
        );
        Ok(CompactionOutcome {
            writer_id: writer_id.to_owned(),
            records: u64::try_from(records.len())?,
            log_sha256: sha256(&bytes),
            old_commit,
            new_commit,
        })
    }

    pub fn list_writers(&self) -> Result<Vec<String>> {
        let output = git_output(
            &self.repo,
            &["for-each-ref", "--format=%(refname)", WRITER_REF_PREFIX],
            None,
        )?;
        let mut writers = output
            .lines()
            .filter_map(|line| line.strip_prefix(WRITER_REF_PREFIX))
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();
        writers.sort();
        Ok(writers)
    }

    /// Return a deterministic union ordered by writer id and writer sequence.
    pub fn read_union(&self) -> Result<Vec<StoredLine>> {
        let mut union = Vec::new();
        for writer_id in self.list_writers()? {
            for (index, bytes) in self.read_writer(&writer_id)?.into_iter().enumerate() {
                union.push(StoredLine {
                    writer_id: writer_id.clone(),
                    sequence: u64::try_from(index)? + 1,
                    bytes,
                });
            }
        }
        Ok(union)
    }

    pub fn write_checkpoint(&self, checkpoint: &[u8]) -> Result<String> {
        serde_json::from_slice::<serde_json::Value>(checkpoint)
            .context("checkpoint must be valid JSON")?;
        let lock = self.lock_named("checkpoint")?;
        let old = git_rev_parse_optional(&self.repo, CHECKPOINT_REF)?;
        let blob = git_output(
            &self.repo,
            &["hash-object", "-w", "--stdin"],
            Some(checkpoint),
        )?;
        let tree_input = format!("100644 blob {blob}\t{CHECKPOINT_NAME}\n");
        let tree = git_output(&self.repo, &["mktree"], Some(tree_input.as_bytes()))?;
        let commit = commit_tree(
            &self.repo,
            &tree,
            old.as_deref(),
            "runtime checkpoint",
            "checkpoint",
        )?;
        update_ref_cas(&self.repo, CHECKPOINT_REF, &commit, old.as_deref())?;
        drop(lock);
        Ok(commit)
    }

    pub fn read_checkpoint(&self) -> Result<Option<Vec<u8>>> {
        let Some(tip) = git_rev_parse_optional(&self.repo, CHECKPOINT_REF)? else {
            return Ok(None);
        };
        git_cat_file_optional(&self.repo, &format!("{tip}:{CHECKPOINT_NAME}"))
    }

    pub fn integrity(&self) -> Result<IntegrityReport> {
        let mut streams = Vec::new();
        let mut findings = Vec::new();
        for writer_id in self.list_writers()? {
            let Some(tip) = self.writer_tip(&writer_id)? else {
                findings.push(format!("writer_without_tip:{writer_id}"));
                continue;
            };
            let bytes = self.read_log_at(Some(&tip))?;
            match parse_log(&bytes) {
                Ok(lines) => streams.push(StreamSummary {
                    writer_id,
                    tip,
                    records: u64::try_from(lines.len())?,
                    sha256: sha256(&bytes),
                }),
                Err(error) => findings.push(format!("corrupt_stream:{writer_id}:{error:#}")),
            }
        }
        Ok(IntegrityReport {
            valid: findings.is_empty(),
            streams,
            findings,
        })
    }

    pub fn export(&self, output: &Path) -> Result<ExportManifest> {
        ensure!(
            !output.starts_with(&self.repo),
            "export must be outside the event repository"
        );
        if output.exists() {
            ensure!(
                output.read_dir()?.next().is_none(),
                "export directory must be empty"
            );
        }
        fs::create_dir_all(output.join("writers"))?;
        let report = self.integrity()?;
        ensure!(
            report.valid,
            "refusing to export an invalid event store: {:?}",
            report.findings
        );
        for summary in &report.streams {
            let tip = &summary.tip;
            let bytes = self.read_log_at(Some(tip))?;
            let dir = output.join("writers").join(&summary.writer_id);
            fs::create_dir_all(&dir)?;
            fs::write(dir.join(LOG_NAME), bytes)?;
        }
        let checkpoint = self.read_checkpoint()?;
        let checkpoint_sha256 = checkpoint.as_ref().map(|bytes| sha256(bytes));
        if let Some(bytes) = checkpoint {
            fs::create_dir_all(output.join("checkpoints"))?;
            fs::write(output.join("checkpoints").join(CHECKPOINT_NAME), bytes)?;
        }
        let manifest = ExportManifest {
            schema: EXPORT_SCHEMA.to_owned(),
            version: EXPORT_VERSION,
            created_at: Utc::now(),
            streams: report.streams,
            checkpoint_sha256,
        };
        fs::write(
            output.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest)?,
        )?;
        Ok(manifest)
    }

    pub fn verify_export(output: &Path) -> Result<ExportManifest> {
        let manifest: ExportManifest =
            serde_json::from_slice(&fs::read(output.join("manifest.json"))?)?;
        ensure!(
            manifest.schema == EXPORT_SCHEMA,
            "unsupported export schema"
        );
        ensure!(
            manifest.version == EXPORT_VERSION,
            "unsupported export version"
        );
        for stream in &manifest.streams {
            validate_writer_id(&stream.writer_id)?;
            let bytes = fs::read(
                output
                    .join("writers")
                    .join(&stream.writer_id)
                    .join(LOG_NAME),
            )?;
            let lines = parse_log(&bytes)?;
            ensure!(
                u64::try_from(lines.len())? == stream.records,
                "record count mismatch for {}",
                stream.writer_id
            );
            ensure!(
                sha256(&bytes) == stream.sha256,
                "digest mismatch for {}",
                stream.writer_id
            );
        }
        match &manifest.checkpoint_sha256 {
            Some(expected) => {
                let bytes = fs::read(output.join("checkpoints").join(CHECKPOINT_NAME))?;
                ensure!(&sha256(&bytes) == expected, "checkpoint digest mismatch");
                serde_json::from_slice::<serde_json::Value>(&bytes)?;
            }
            None => ensure!(
                !output.join("checkpoints").join(CHECKPOINT_NAME).exists(),
                "undeclared checkpoint"
            ),
        }
        Ok(manifest)
    }

    pub fn import(&self, input: &Path) -> Result<ExportManifest> {
        let manifest = Self::verify_export(input)?;
        for summary in &manifest.streams {
            let imported = fs::read(
                input
                    .join("writers")
                    .join(&summary.writer_id)
                    .join(LOG_NAME),
            )?;
            let current = self.read_writer_bytes(&summary.writer_id)?;
            if current == imported {
                continue;
            }
            ensure!(
                imported.starts_with(&current),
                "writer history conflict for {}",
                summary.writer_id
            );
            for line in parse_log(&imported[current.len()..])? {
                self.append(&summary.writer_id, &line)?;
            }
        }
        if let Ok(checkpoint) = fs::read(input.join("checkpoints").join(CHECKPOINT_NAME)) {
            self.write_checkpoint(&checkpoint)?;
        }
        Ok(manifest)
    }

    fn read_writer_bytes(&self, writer_id: &str) -> Result<Vec<u8>> {
        let tip = self.writer_tip(writer_id)?;
        self.read_log_at(tip.as_deref())
    }

    fn read_log_at(&self, tip: Option<&str>) -> Result<Vec<u8>> {
        let Some(tip) = tip else {
            return Ok(Vec::new());
        };
        Ok(git_cat_file_optional(&self.repo, &format!("{tip}:{LOG_NAME}"))?.unwrap_or_default())
    }

    fn lock_writer(&self, writer_id: &str) -> Result<File> {
        self.lock_named(&format!("writer-{writer_id}"))
    }

    fn lock_named(&self, name: &str) -> Result<File> {
        let path = self.repo.join("runtime-locks").join(format!("{name}.lock"));
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)?;
        file.lock_exclusive()?;
        Ok(file)
    }
}

pub fn ssh_sign(private_key: &Path, namespace: &str, bytes: &[u8]) -> Result<String> {
    validate_namespace(namespace)?;
    let dir = tempfile::tempdir()?;
    let payload = dir.path().join("payload");
    fs::write(&payload, bytes)?;
    let output = Command::new("ssh-keygen")
        .args(["-Y", "sign", "-q", "-f"])
        .arg(private_key)
        .args(["-n", namespace])
        .arg(&payload)
        .output()
        .context("failed to execute ssh-keygen -Y sign")?;
    require_success(output, "ssh-keygen -Y sign")?;
    Ok(fs::read_to_string(payload.with_extension("sig"))?)
}

pub fn ssh_verify(
    allowed_signers: &Path,
    principal: &str,
    namespace: &str,
    bytes: &[u8],
    signature: &str,
) -> Result<()> {
    validate_namespace(namespace)?;
    ensure!(
        !principal.is_empty() && !principal.contains(char::is_whitespace),
        "invalid signer principal"
    );
    let dir = tempfile::tempdir()?;
    let signature_path = dir.path().join("signature");
    fs::write(&signature_path, signature)?;
    let mut child = Command::new("ssh-keygen")
        .args(["-Y", "verify", "-q", "-f"])
        .arg(allowed_signers)
        .args(["-I", principal, "-n", namespace, "-s"])
        .arg(&signature_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to execute ssh-keygen -Y verify")?;
    child
        .stdin
        .take()
        .context("missing verifier stdin")?
        .write_all(bytes)?;
    require_success(child.wait_with_output()?, "ssh-keygen -Y verify")?;
    Ok(())
}

fn validate_writer_id(writer_id: &str) -> Result<()> {
    ensure!(
        (3..=64).contains(&writer_id.len()),
        "writer id must be 3..=64 characters"
    );
    ensure!(
        writer_id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_')),
        "writer id contains invalid characters"
    );
    Ok(())
}

fn validate_namespace(namespace: &str) -> Result<()> {
    ensure!(!namespace.is_empty(), "signature namespace cannot be empty");
    ensure!(
        namespace
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.')),
        "invalid signature namespace"
    );
    Ok(())
}

fn validate_json_line(line: &[u8]) -> Result<()> {
    let trimmed = trim_line_ending(line);
    ensure!(!trimmed.is_empty(), "event line cannot be empty");
    serde_json::from_slice::<serde_json::Value>(trimmed)
        .context("event line must be valid JSON")?;
    ensure!(
        !trimmed.contains(&b'\n') && !trimmed.contains(&b'\r'),
        "event must occupy one line"
    );
    Ok(())
}

fn parse_log(bytes: &[u8]) -> Result<Vec<Vec<u8>>> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    ensure!(bytes.ends_with(b"\n"), "stream has a partial final record");
    bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            validate_json_line(line)?;
            Ok(line.to_vec())
        })
        .collect()
}

fn trim_line_ending(mut bytes: &[u8]) -> &[u8] {
    while bytes
        .last()
        .is_some_and(|byte| matches!(byte, b'\n' | b'\r'))
    {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

fn writer_ref(writer_id: &str) -> Result<String> {
    validate_writer_id(writer_id)?;
    Ok(format!("{WRITER_REF_PREFIX}{writer_id}"))
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn git_rev_parse_optional(repo: &Path, ref_name: &str) -> Result<Option<String>> {
    let output = git_command(repo, &["rev-parse", "--verify", ref_name], None)?;
    if output.status.success() {
        return Ok(Some(String::from_utf8(output.stdout)?.trim().to_owned()));
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("Needed a single revision") || stderr.contains("unknown revision") {
        return Ok(None);
    }
    Err(anyhow!("git rev-parse failed: {}", stderr.trim()))
}

fn git_cat_file_optional(repo: &Path, spec: &str) -> Result<Option<Vec<u8>>> {
    let output = git_command(repo, &["cat-file", "blob", spec], None)?;
    if output.status.success() {
        return Ok(Some(output.stdout));
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("does not exist") || stderr.contains("Not a valid object name") {
        return Ok(None);
    }
    Err(anyhow!("git cat-file failed: {}", stderr.trim()))
}

fn commit_tree(
    repo: &Path,
    tree: &str,
    parent: Option<&str>,
    message: &str,
    writer: &str,
) -> Result<String> {
    let mut args = vec!["commit-tree", tree];
    if let Some(parent) = parent {
        args.extend(["-p", parent]);
    }
    args.extend(["-m", message]);
    let mut command = base_git_command(repo);
    command.args(args);
    command
        .env("GIT_AUTHOR_NAME", format!("crosslink-runtime:{writer}"))
        .env("GIT_AUTHOR_EMAIL", "runtime@invalid.local")
        .env("GIT_COMMITTER_NAME", "crosslink-runtime")
        .env("GIT_COMMITTER_EMAIL", "runtime@invalid.local");
    let output = command.output()?;
    require_success(output, "git commit-tree")
}

fn update_ref_cas(repo: &Path, ref_name: &str, new: &str, old: Option<&str>) -> Result<()> {
    let zero = "0000000000000000000000000000000000000000";
    let output = git_command(
        repo,
        &["update-ref", ref_name, new, old.unwrap_or(zero)],
        None,
    )?;
    if output.status.success() {
        return Ok(());
    }
    bail!(
        "ref moved concurrently: {ref_name}: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

fn git_output(repo: &Path, args: &[&str], input: Option<&[u8]>) -> Result<String> {
    let output = git_command(repo, args, input)?;
    require_success(output, &format!("git {}", args.join(" ")))
}

fn git_command(repo: &Path, args: &[&str], input: Option<&[u8]>) -> Result<Output> {
    let mut child = base_git_command(repo)
        .args(args)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to execute git {}", args.join(" ")))?;
    if let Some(input) = input {
        child
            .stdin
            .take()
            .context("missing git stdin")?
            .write_all(input)?;
    }
    Ok(child.wait_with_output()?)
}

fn base_git_command(repo: &Path) -> Command {
    let mut command = Command::new("git");
    command.arg("--git-dir").arg(repo);
    command
}

fn require_success(output: Output, operation: &str) -> Result<String> {
    ensure!(
        output.status.success(),
        "{operation} failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};
    use std::thread;

    use super::*;

    fn store() -> (tempfile::TempDir, GitEventStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = GitEventStore::open_or_init(dir.path().join("events.git")).unwrap();
        (dir, store)
    }

    #[test]
    fn bare_store_appends_without_worktree_or_index() {
        let (_dir, store) = store();
        let first = store.append("writer-one", br#"{"n":1}"#).unwrap();
        let second = store.append("writer-one", br#"{"n":2}"#).unwrap();
        assert_eq!(first.sequence, 1);
        assert_eq!(second.sequence, 2);
        assert_eq!(
            second.old_commit.as_deref(),
            Some(first.new_commit.as_str())
        );
        assert_eq!(
            store.read_writer("writer-one").unwrap(),
            vec![br#"{"n":1}"#.to_vec(), br#"{"n":2}"#.to_vec()]
        );
        assert!(!store.repo().join("index").exists());
        assert!(!store.repo().join(LOG_NAME).exists());
    }

    #[test]
    fn same_writer_concurrency_is_serialized_without_loss() {
        let (_dir, store) = store();
        let store = Arc::new(store);
        let barrier = Arc::new(Barrier::new(17));
        let handles = (0..16)
            .map(|n| {
                let store = Arc::clone(&store);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    store
                        .append("writer-one", format!("{{\"n\":{n}}}").as_bytes())
                        .unwrap()
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        let mut sequences = handles
            .into_iter()
            .map(|handle| handle.join().unwrap().sequence)
            .collect::<Vec<_>>();
        sequences.sort_unstable();
        assert_eq!(sequences, (1..=16).collect::<Vec<_>>());
        let records = store.read_writer("writer-one").unwrap();
        assert_eq!(records.len(), 16);
        let values = records
            .into_iter()
            .map(|bytes| {
                serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["n"]
                    .as_u64()
                    .unwrap()
            })
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(values, (0..16).collect());
    }

    #[test]
    fn crash_before_ref_update_does_not_publish_event() {
        for abort in [AbortAfter::HashObject, AbortAfter::Tree, AbortAfter::Commit] {
            let (_dir, store) = store();
            assert!(store
                .append_with_abort("writer-one", br#"{"n":1}"#, abort)
                .is_err());
            assert!(store.writer_tip("writer-one").unwrap().is_none());
            assert!(store.read_writer("writer-one").unwrap().is_empty());
            let outcome = store.append("writer-one", br#"{"n":2}"#).unwrap();
            assert_eq!(outcome.sequence, 1);
        }
    }

    #[test]
    fn invalid_input_and_corrupt_existing_stream_fail_loudly() {
        let (_dir, store) = store();
        assert!(store.append("../escape", br#"{"n":1}"#).is_err());
        assert!(store.append("writer-one", b"not-json").is_err());
        store.append("writer-one", br#"{"n":1}"#).unwrap();
        let tip = store.writer_tip("writer-one").unwrap().unwrap();
        let blob = git_output(
            store.repo(),
            &["hash-object", "-w", "--stdin"],
            Some(b"partial"),
        )
        .unwrap();
        let tree = git_output(
            store.repo(),
            &["mktree"],
            Some(format!("100644 blob {blob}\t{LOG_NAME}\n").as_bytes()),
        )
        .unwrap();
        let commit = commit_tree(store.repo(), &tree, Some(&tip), "corrupt", "writer-one").unwrap();
        update_ref_cas(
            store.repo(),
            &writer_ref("writer-one").unwrap(),
            &commit,
            Some(&tip),
        )
        .unwrap();
        assert!(store.append("writer-one", br#"{"n":2}"#).is_err());
        let report = store.integrity().unwrap();
        assert!(!report.valid);
        assert_eq!(report.findings.len(), 1);
    }

    #[test]
    fn deterministic_union_and_pinned_reads_preserve_writer_order() {
        let (_dir, store) = store();
        let first_tip = store
            .append("writer-zed", br#"{"n":1}"#)
            .unwrap()
            .new_commit;
        store.append("writer-zed", br#"{"n":2}"#).unwrap();
        store.append("writer-aaa", br#"{"n":3}"#).unwrap();
        let union = store.read_union().unwrap();
        assert_eq!(
            union
                .iter()
                .map(|line| (&line.writer_id, line.sequence))
                .collect::<Vec<_>>(),
            vec![
                (&"writer-aaa".to_owned(), 1),
                (&"writer-zed".to_owned(), 1),
                (&"writer-zed".to_owned(), 2)
            ]
        );
        assert_eq!(
            store
                .read_writer_at("writer-zed", &first_tip)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn checkpoint_export_import_and_verification_roundtrip() {
        let (_dir, source) = store();
        source.append("writer-one", br#"{"n":1}"#).unwrap();
        source.append("writer-two", br#"{"n":2}"#).unwrap();
        source.write_checkpoint(br#"{"covered":2}"#).unwrap();
        let export = tempfile::tempdir().unwrap();
        let manifest = source.export(export.path()).unwrap();
        assert_eq!(manifest.streams.len(), 2);
        assert!(GitEventStore::verify_export(export.path()).is_ok());

        let (_target_dir, target) = store();
        target.import(export.path()).unwrap();
        assert_eq!(target.read_union().unwrap(), source.read_union().unwrap());
        assert_eq!(
            target.read_checkpoint().unwrap(),
            source.read_checkpoint().unwrap()
        );
        target.import(export.path()).unwrap();
        assert_eq!(target.read_union().unwrap().len(), 2);

        fs::write(
            export.path().join("writers/writer-one/events.ndjson"),
            b"{}\n",
        )
        .unwrap();
        assert!(GitEventStore::verify_export(export.path()).is_err());
    }

    #[test]
    fn compaction_squashes_commit_ancestry_without_pruning_records() {
        let directory = tempfile::tempdir().unwrap();
        let store = GitEventStore::open_or_init(directory.path().join("events.git")).unwrap();
        for sequence in 1..=4 {
            store
                .append(
                    "agent-one",
                    format!(r#"{{"sequence":{sequence}}}"#).as_bytes(),
                )
                .unwrap();
        }
        let before = store.read_writer("agent-one").unwrap();
        let reference = writer_ref("agent-one").unwrap();
        assert_eq!(
            git_output(store.repo(), &["rev-list", "--count", &reference], None).unwrap(),
            "4"
        );
        let outcome = store.compact_writer_history("agent-one").unwrap();
        assert_eq!(outcome.records, 4);
        assert_ne!(outcome.old_commit, outcome.new_commit);
        assert_eq!(store.read_writer("agent-one").unwrap(), before);
        assert_eq!(
            git_output(store.repo(), &["rev-list", "--count", &reference], None).unwrap(),
            "1"
        );
        let appended = store.append("agent-one", br#"{"sequence":5}"#).unwrap();
        assert_eq!(appended.sequence, 5);
        assert_eq!(store.read_writer("agent-one").unwrap().len(), 5);
        assert!(store.integrity().unwrap().valid);
    }

    #[test]
    fn ssh_signature_verifies_and_tampering_fails() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("id_ed25519");
        let output = Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-C", "writer-one", "-f"])
            .arg(&key)
            .output()
            .unwrap();
        assert!(output.status.success());
        let allowed = dir.path().join("allowed_signers");
        fs::write(
            &allowed,
            format!(
                "writer-one {}",
                fs::read_to_string(key.with_extension("pub")).unwrap()
            ),
        )
        .unwrap();
        let signature = ssh_sign(&key, "mistake.runtime", b"payload").unwrap();
        ssh_verify(
            &allowed,
            "writer-one",
            "mistake.runtime",
            b"payload",
            &signature,
        )
        .unwrap();
        assert!(ssh_verify(
            &allowed,
            "writer-one",
            "mistake.runtime",
            b"tampered",
            &signature
        )
        .is_err());
    }
}
