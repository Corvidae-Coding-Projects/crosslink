use anyhow::{Context, Result};
use chrono::Utc;
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, Notify};
use tokio::task::JoinHandle;

const REPLAY_BUFFER_BYTES: usize = 64 * 1024;

#[allow(dead_code)]
pub const DEFAULT_GRACE_PERIOD_SECS: u64 = 30 * 60;

pub const DEFAULT_MAX_CONCURRENT: usize = 8;

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ClientFrame {
    Stdin { data: String },

    Resize { rows: u16, cols: u16 },
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ServerFrame {
    Stdout { data: String },

    Exit { code: Option<i32> },
}

pub struct PtySession {
    pub id: String,
    pub project_slug: String,
    pub command: String,
    pub started_at: String,

    master: Arc<Mutex<Option<Box<dyn MasterPty + Send>>>>,

    writer: Arc<Mutex<Option<Box<dyn Write + Send>>>>,

    broadcaster: broadcast::Sender<Vec<u8>>,

    replay: Arc<Mutex<VecDeque<u8>>>,

    pub exit_code: Arc<Mutex<Option<i32>>>,

    #[allow(dead_code)]
    pub exit_notify: Arc<Notify>,

    reader_handle: Mutex<Option<JoinHandle<()>>>,
}

impl PtySession {
    pub fn subscribe(&self) -> (broadcast::Receiver<Vec<u8>>, Vec<u8>) {
        let receiver = self.broadcaster.subscribe();
        let snapshot = self.replay.lock().map_or_else(
            |_| Vec::new(),
            |buf| buf.iter().copied().collect::<Vec<u8>>(),
        );
        (receiver, snapshot)
    }

    pub fn write_stdin(&self, data: &[u8]) -> Result<()> {
        let mut guard = self
            .writer
            .lock()
            .map_err(|_| anyhow::anyhow!("pty writer mutex poisoned"))?;
        let writer = guard
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("pty already closed"))?;
        writer.write_all(data).context("write to pty")?;
        Ok(())
    }

    pub fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        let guard = self
            .master
            .lock()
            .map_err(|_| anyhow::anyhow!("pty master mutex poisoned"))?;
        let master = guard
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("pty already closed"))?;
        master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("resize pty")?;
        Ok(())
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        if let Ok(mut g) = self.reader_handle.lock() {
            if let Some(h) = g.take() {
                h.abort();
            }
        }
        if let Ok(mut g) = self.writer.lock() {
            *g = None;
        }

        if let Ok(mut g) = self.master.lock() {
            *g = None;
        }
    }
}

#[derive(Clone, Default)]
pub struct SessionRegistry {
    inner: Arc<tokio::sync::RwLock<std::collections::HashMap<String, Arc<PtySession>>>>,
}

impl SessionRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn insert(&self, session: Arc<PtySession>) {
        self.inner.write().await.insert(session.id.clone(), session);
    }

    pub async fn get(&self, id: &str) -> Option<Arc<PtySession>> {
        self.inner.read().await.get(id).cloned()
    }

    #[allow(dead_code)]
    pub async fn remove(&self, id: &str) -> Option<Arc<PtySession>> {
        self.inner.write().await.remove(id)
    }

    pub async fn list_ids(&self) -> Vec<String> {
        self.inner.read().await.keys().cloned().collect()
    }

    pub async fn len(&self) -> usize {
        self.inner.read().await.len()
    }

    #[allow(dead_code)]
    pub async fn is_empty(&self) -> bool {
        self.inner.read().await.is_empty()
    }
}

pub fn spawn_pty(
    cwd: &std::path::Path,
    command: &str,
    args: &[String],
    rows: u16,
    cols: u16,
) -> Result<Arc<PtySession>> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("open pty")?;

    let mut cmd = CommandBuilder::new(command);
    for a in args {
        cmd.arg(a);
    }
    cmd.cwd(cwd);

    cmd.env("CROSSLINK_DASHBOARD", "1");
    cmd.env("TERM", "xterm-256color");

    let mut child = pair.slave.spawn_command(cmd).context("spawn pty child")?;
    drop(pair.slave);

    let id = format!("pty-{}", uuid::Uuid::new_v4());
    let started_at = Utc::now().to_rfc3339();
    let (tx, _) = broadcast::channel::<Vec<u8>>(64);
    let replay = Arc::new(Mutex::new(VecDeque::with_capacity(REPLAY_BUFFER_BYTES)));
    let exit_code = Arc::new(Mutex::new(None::<i32>));
    let exit_notify = Arc::new(Notify::new());

    let mut reader = pair.master.try_clone_reader().context("clone pty reader")?;
    let writer = pair.master.take_writer().context("take pty writer")?;

    let tx_for_reader = tx.clone();
    let replay_for_reader = Arc::clone(&replay);
    let (exit_ready_tx, exit_ready_rx) = std::sync::mpsc::sync_channel::<()>(0);

    let reader_handle = tokio::task::spawn_blocking(move || {
        let mut buf = [0u8; 4096];
        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let chunk = buf[..n].to_vec();
            if let Ok(mut replay_guard) = replay_for_reader.lock() {
                for &b in &chunk {
                    if replay_guard.len() == REPLAY_BUFFER_BYTES {
                        replay_guard.pop_front();
                    }
                    replay_guard.push_back(b);
                }
            }

            let _ = tx_for_reader.send(chunk);
        }

        let _ = exit_ready_rx.recv();
        let _ = tx_for_reader.send(Vec::new());
    });

    let master = Arc::new(Mutex::new(Some(pair.master)));
    let writer = Arc::new(Mutex::new(Some(writer)));
    let master_for_waiter = Arc::clone(&master);
    let writer_for_waiter = Arc::clone(&writer);
    let exit_code_for_waiter = Arc::clone(&exit_code);
    let exit_notify_for_waiter = Arc::clone(&exit_notify);

    let waiter_handle = tokio::task::spawn_blocking(move || {
        let code = child.wait().map_or(-1, |status| status.exit_code() as i32);
        if let Ok(mut g) = exit_code_for_waiter.lock() {
            *g = Some(code);
        }
        if let Ok(mut g) = writer_for_waiter.lock() {
            *g = None;
        }
        if let Ok(mut g) = master_for_waiter.lock() {
            *g = None;
        }
        exit_notify_for_waiter.notify_waiters();
        let _ = exit_ready_tx.send(());
    });

    let reader_handle: JoinHandle<()> = tokio::spawn(async move {
        let _ = tokio::join!(reader_handle, waiter_handle);
    });

    Ok(Arc::new(PtySession {
        id,
        project_slug: cwd.to_string_lossy().into_owned(),
        command: command.to_string(),
        started_at,
        master,
        writer,
        broadcaster: tx,
        replay,
        exit_code,
        exit_notify,
        reader_handle: Mutex::new(Some(reader_handle)),
    }))
}

#[derive(Debug, Clone, Serialize)]
pub struct PtySessionView {
    pub id: String,
    pub project_slug: String,
    pub command: String,
    pub started_at: String,
    pub exit_code: Option<i32>,
}

impl From<&PtySession> for PtySessionView {
    fn from(s: &PtySession) -> Self {
        let exit = s.exit_code.lock().ok().and_then(|g| *g);
        Self {
            id: s.id.clone(),
            project_slug: s.project_slug.clone(),
            command: s.command.clone(),
            started_at: s.started_at.clone(),
            exit_code: exit,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer_windows_cursor_position_query(session: &PtySession) {
        if cfg!(target_os = "windows") {
            session
                .write_stdin(b"\x1b[1;1R")
                .expect("answer ConPTY cursor-position query");
        }
    }

    fn identity_test_command() -> (String, Vec<String>) {
        let command = if cfg!(target_os = "windows") {
            "whoami.exe"
        } else {
            "whoami"
        };
        (command.to_string(), Vec::new())
    }

    #[tokio::test]
    async fn test_spawn_echo_completes_with_exit_zero() {
        let (command, args) = identity_test_command();
        let session = spawn_pty(&std::env::temp_dir(), &command, &args, 24, 80).expect("spawn pty");
        answer_windows_cursor_position_query(&session);

        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            session.exit_notify.notified(),
        )
        .await;
        let code = session.exit_code.lock().unwrap();
        assert_eq!(*code, Some(0));
    }

    #[tokio::test]
    async fn test_subscribe_returns_replay_after_output() {
        let (command, args) = identity_test_command();
        let expected = std::process::Command::new(&command)
            .output()
            .expect("run identity command directly");
        assert!(expected.status.success(), "identity command must succeed");
        let expected = String::from_utf8_lossy(&expected.stdout)
            .trim()
            .to_ascii_lowercase();
        assert!(!expected.is_empty(), "identity command must write output");

        let session = spawn_pty(&std::env::temp_dir(), &command, &args, 24, 80).expect("spawn pty");
        answer_windows_cursor_position_query(&session);

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let (_rx, snapshot) = session.subscribe();
            let s = String::from_utf8_lossy(&snapshot);
            if s.to_ascii_lowercase().contains(&expected) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "expected identity {expected:?}, got: {s:?}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }

    #[tokio::test]
    async fn test_session_registry_insert_get_remove() {
        let reg = SessionRegistry::new();
        let (command, args) = identity_test_command();
        let s = spawn_pty(&std::env::temp_dir(), &command, &args, 24, 80).expect("spawn");
        answer_windows_cursor_position_query(&s);
        let id = s.id.clone();
        reg.insert(Arc::clone(&s)).await;
        assert!(reg.get(&id).await.is_some());
        assert_eq!(reg.len().await, 1);
        let removed = reg.remove(&id).await;
        assert!(removed.is_some());
        assert!(reg.get(&id).await.is_none());
    }
}
