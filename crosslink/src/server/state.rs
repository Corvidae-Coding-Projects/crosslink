use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{broadcast, Mutex, MutexGuard};

use crate::db::Database;
use crate::server::ws::{self, WsEvent};

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Mutex<Database>>,

    pub crosslink_dir: PathBuf,

    pub version: &'static str,

    pub ws_tx: broadcast::Sender<WsEvent>,

    pub auth_token: String,

    pub dashboard_db_path: Option<PathBuf>,

    pub pty_registry: crate::dashboard::pty::SessionRegistry,
}

impl AppState {
    pub fn new(db: Database, crosslink_dir: PathBuf) -> Self {
        let (ws_tx, _ws_rx) = ws::channel();
        let auth_token = generate_auth_token();
        Self {
            db: Arc::new(Mutex::new(db)),
            crosslink_dir,
            version: env!("CARGO_PKG_VERSION"),
            ws_tx,
            auth_token,
            dashboard_db_path: None,
            pty_registry: crate::dashboard::pty::SessionRegistry::new(),
        }
    }

    #[must_use]
    pub fn with_dashboard_db(mut self, path: PathBuf) -> Self {
        self.dashboard_db_path = Some(path);
        self
    }

    pub async fn db(&self) -> MutexGuard<'_, Database> {
        self.db.lock().await
    }
}

fn generate_auth_token() -> String {
    if let Some(path) = token_path() {
        if let Ok(saved) = std::fs::read_to_string(&path) {
            let trimmed = saved.trim();
            if is_valid_hex_token(trimmed) {
                return trimmed.to_string();
            }
        }
        let fresh = fresh_hex_token();
        let _ = std::fs::create_dir_all(path.parent().unwrap_or_else(|| std::path::Path::new("/")));
        if std::fs::write(&path, &fresh).is_ok() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            }
        }
        return fresh;
    }
    fresh_hex_token()
}

pub fn rotate_auth_token() -> String {
    if let Some(path) = token_path() {
        let _ = std::fs::remove_file(&path);
    }
    generate_auth_token()
}

fn token_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    Some(home.join(".crosslink").join(".dashboard-token"))
}

fn is_valid_hex_token(s: &str) -> bool {
    s.len() == 32
        && s.chars()
            .all(|c| c.is_ascii_hexdigit() && (c.is_ascii_digit() || c.is_ascii_lowercase()))
}

fn fresh_hex_token() -> String {
    let mut buf = [0u8; 16];
    #[cfg(unix)]
    {
        use std::io::Read;

        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            let _ = f.read_exact(&mut buf);
        }
    }
    if buf == [0u8; 16] {
        use std::time::SystemTime;
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let pid = u128::from(std::process::id());
        let mixed = nanos.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(pid);
        buf.copy_from_slice(&mixed.to_le_bytes());
    }
    let mut hex = String::with_capacity(32);
    for byte in buf {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}
