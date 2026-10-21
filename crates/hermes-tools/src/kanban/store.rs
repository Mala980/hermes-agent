//! Penyimpanan kanban berbasis SQLite (`KanbanStore` + `BoardDb`).
//!
//! Cerminan Rust dari lapisan penyimpanan kanban Hermes asli
//! (`hermes_cli/kanban_db*.py`): satu direktori kanban home berisi
//! `boards/<slug>/kanban.db` per board plus metadata `board.json`.
//! Lihat dokumentasi modul induk (`kanban/mod.rs`) untuk peta API publik
//! yang dipakai Worker B/C.

use hermes_core::{HermesError, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

// ---------------------------------------------------------------------------
// Status workflow (cerminan `hermes_cli/kanban_workflow.py::DEFAULT_WORKFLOW`)
// ---------------------------------------------------------------------------

/// Kolom workflow bawaan, sesuai urutan dashboard aslinya.
pub const STATUS_TRIAGE: &str = "triage";
pub const STATUS_TODO: &str = "todo";
pub const STATUS_SCHEDULED: &str = "scheduled";
pub const STATUS_READY: &str = "ready";
pub const STATUS_RUNNING: &str = "running";
pub const STATUS_BLOCKED: &str = "blocked";
pub const STATUS_REVIEW: &str = "review";
pub const STATUS_DONE: &str = "done";
/// Status arsip (bukan kolom dashboard; dipakai via filter `include_archived`).
pub const STATUS_ARCHIVED: &str = "archived";

/// Semua nilai yang sah untuk kolom `tasks.status`.
pub const ALL_STATUSES: &[&str] = &[
    STATUS_TRIAGE,
    STATUS_TODO,
    STATUS_SCHEDULED,
    STATUS_READY,
    STATUS_RUNNING,
    STATUS_BLOCKED,
    STATUS_REVIEW,
    STATUS_DONE,
    STATUS_ARCHIVED,
];

/// Batas ukuran attachment, meniru aslinya (25 MB).
pub const MAX_ATTACHMENT_BYTES: usize = 25 * 1024 * 1024;

/// Transisi manual yang diizinkan antar status — salinan tabel `manual` dari
/// `DEFAULT_WORKFLOW` di aslinya (termasuk quirk `review -> todo` yang
/// mendarat di `ready`; dipertahankan agar perilaku sama).
pub fn valid_transitions(from: &str) -> &'static [&'static str] {
    match from {
        "triage" => &["todo", "ready"],
        "todo" => &["triage", "scheduled", "ready"],
        "scheduled" => &["triage", "todo", "ready"],
        "ready" => &["triage", "todo", "scheduled", "blocked", "review", "done"],
        "running" => &[
            "triage",
            "todo",
            "scheduled",
            "ready",
            "blocked",
            "review",
            "done",
        ],
        "blocked" => &["triage", "todo", "scheduled", "ready", "done"],
        "review" => &["triage", "todo", "ready", "done"],
        "done" => &["triage", "todo", "ready"],
        "archived" => &["triage", "todo", "ready"],
        _ => &[],
    }
}

/// `true` bila `to` adalah target transisi yang sah dari status `from`.
/// Pengarsipan (`to == "archived"`) selalu diizinkan dari status apa pun —
/// aslinya arsip dilakukan lewat CLI, bukan lewat matriks transisi.
pub fn can_transition(from: &str, to: &str) -> bool {
    from == to || to == STATUS_ARCHIVED || valid_transitions(from).contains(&to)
}

// ---------------------------------------------------------------------------
// Tipe data publik
// ---------------------------------------------------------------------------

/// Metadata sebuah board (disimpan di `boards/<slug>/board.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoardMeta {
    pub slug: String,
    pub name: String,
    pub description: Option<String>,
    pub icon: Option<String>,
    pub color: Option<String>,
    pub project_id: Option<String>,
    pub archived: bool,
    pub created_at: i64,
}

/// Opsi pembuatan board — cerminan argumen `create_board` di aslinya.
#[derive(Debug, Default)]
pub struct BoardOptions {
    pub name: Option<String>,
    pub description: Option<String>,
    pub icon: Option<String>,
    pub color: Option<String>,
    pub project_id: Option<String>,
}

/// Satu baris task kanban (kolom inti dari tabel `tasks` aslinya).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KanbanTask {
    pub id: String,
    pub title: String,
    pub body: Option<String>,
    pub assignee: Option<String>,
    pub status: String,
    pub priority: i64,
    pub created_by: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub started_at: Option<i64>,
    pub completed_at: Option<i64>,
    pub workspace_kind: String,
    pub workspace_path: Option<String>,
    pub branch_name: Option<String>,
    pub project_id: Option<String>,
    pub tenant: Option<String>,
    pub result: Option<String>,
    pub summary: Option<String>,
    pub metadata_json: Option<String>,
    pub idempotency_key: Option<String>,
    pub consecutive_failures: i64,
    pub max_runtime_seconds: Option<i64>,
    pub last_heartbeat_at: Option<i64>,
    pub skills: Vec<String>,
    pub model_override: Option<String>,
    pub provider_override: Option<String>,
    pub reasoning_effort: Option<String>,
    pub max_retries: Option<i64>,
    pub goal_mode: bool,
    pub goal_max_turns: Option<i64>,
    pub session_id: Option<String>,
    pub block_kind: Option<String>,
    pub block_recurrences: i64,
    pub completion_contract: Option<String>,
}

/// Ringkasan task untuk `kanban_list` — cerminan "compact rows" aslinya.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSummary {
    pub id: String,
    pub title: String,
    pub status: String,
    pub assignee: Option<String>,
    pub priority: i64,
    pub tenant: Option<String>,
    pub parent_ids: Vec<String>,
    pub child_ids: Vec<String>,
    pub comment_count: i64,
    pub attachment_count: i64,
    pub created_at: i64,
}

/// Filter untuk `list_tasks` — cerminan filter CLI/list aslinya.
#[derive(Debug, Default, Clone)]
pub struct TaskFilter {
    pub assignee: Option<String>,
    pub status: Option<String>,
    pub tenant: Option<String>,
    pub include_archived: bool,
    /// Default 50, maks 200 (seperti aslinya).
    pub limit: Option<usize>,
}

/// Parameter pembuatan task — cerminan argumen `kanban_create` aslinya.
#[derive(Debug, Default)]
pub struct NewTask {
    pub title: String,
    pub assignee: String,
    pub body: Option<String>,
    pub parents: Vec<String>,
    pub tenant: Option<String>,
    pub priority: i64,
    pub workspace_kind: Option<String>,
    pub workspace_path: Option<String>,
    pub project_id: Option<String>,
    /// Bila true, task mendarat di `triage`, bukan jalur dispatch biasa.
    pub triage: bool,
    pub idempotency_key: Option<String>,
    pub max_runtime_seconds: Option<i64>,
    /// `"running"` (default) atau `"blocked"`.
    pub initial_status: Option<String>,
    pub skills: Vec<String>,
    pub goal_mode: bool,
    pub goal_max_turns: Option<i64>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub completion_contract: Option<String>,
    pub created_by: Option<String>,
    pub session_id: Option<String>,
}

/// Patch parsial untuk `update_task` — hanya field `Some` yang ditulis.
#[derive(Debug, Default)]
pub struct TaskPatch {
    pub body: Option<Option<String>>,
    pub assignee: Option<Option<String>>,
    pub priority: Option<i64>,
    pub tenant: Option<Option<String>>,
    pub result: Option<Option<String>>,
    pub summary: Option<Option<String>>,
    pub metadata_json: Option<Option<String>>,
    pub model_override: Option<Option<String>>,
    pub provider_override: Option<Option<String>>,
    pub block_kind: Option<Option<String>>,
}

/// Komentar pada thread sebuah task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Comment {
    pub id: i64,
    pub task_id: String,
    pub author: String,
    pub body: String,
    pub created_at: i64,
}

/// Satu entri event log task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskEvent {
    pub id: i64,
    pub task_id: String,
    pub run_id: Option<i64>,
    pub kind: String,
    pub payload: Option<String>,
    pub created_at: i64,
}

/// Metadata file yang dilampirkan ke task (blob-nya di disk).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    pub id: i64,
    pub task_id: String,
    pub filename: String,
    pub stored_path: String,
    pub content_type: Option<String>,
    pub size: i64,
    pub uploaded_by: Option<String>,
    pub created_at: i64,
}

// ---------------------------------------------------------------------------
// Helper internal
// ---------------------------------------------------------------------------

fn kb_err(msg: impl Into<String>) -> HermesError {
    HermesError::Other(format!("kanban: {}", msg.into()))
}

fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

static TASK_COUNTER: AtomicU64 = AtomicU64::new(0);

/// ID task ala aslinya: `t_` + 8 hex acak (`_new_task_id` memakai
/// `secrets.token_hex(4)`).
fn new_task_id() -> String {
    let n = TASK_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut h = Sha256::new();
    h.update(nanos.to_le_bytes());
    h.update(std::process::id().to_le_bytes());
    h.update(n.to_le_bytes());
    let d = h.finalize();
    format!("t_{:02x}{:02x}{:02x}{:02x}", d[0], d[1], d[2], d[3])
}

/// Normalisasi + validasi slug board (cerminan `_require_slug` aslinya).
pub fn normalize_board_slug(slug: &str) -> Result<String> {
    let s = slug.trim().to_lowercase();
    if s.is_empty() || s.len() > 64 {
        return Err(kb_err("board slug harus 1-64 karakter"));
    }
    if !s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(kb_err(
            "board slug hanya boleh berisi huruf, angka, '-' dan '_'",
        ));
    }
    Ok(s)
}

/// Resolusi kanban home: `HERMES_KANBAN_HOME` → `<HERMES_HOME>/kanban` →
/// `<working_dir>/.hermes/kanban`.
pub fn kanban_home(env: &HashMap<String, String>, working_dir: &Path) -> PathBuf {
    if let Some(h) = env.get("HERMES_KANBAN_HOME").filter(|s| !s.is_empty()) {
        return PathBuf::from(h);
    }
    if let Some(h) = env.get("HERMES_HOME").filter(|s| !s.is_empty()) {
        return PathBuf::from(h).join("kanban");
    }
    working_dir.join(".hermes").join("kanban")
}

fn parse_skills(s: Option<String>) -> Vec<String> {
    s.and_then(|v| serde_json::from_str::<Vec<String>>(&v).ok())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// KanbanStore — level home (manajemen board)
// ---------------------------------------------------------------------------

/// Titik masuk penyimpanan kanban: mengelola direktori home dan board-board.
///
/// Layout di disk (meniru aslinya):
/// ```text
/// <kanban_home>/
///   current                    # slug board aktif (opsional)
///   boards/
///     <slug>/
///       board.json             # metadata BoardMeta
///       kanban.db              # SQLite: tasks, task_links, task_comments,
///                              #   task_events, task_runs, task_attachments
///       attachments/<task_id>/  # blob attachment
/// ```
pub struct KanbanStore {
    home: PathBuf,
}

impl KanbanStore {
    /// Buka (atau buat) kanban home di `home`.
    pub fn open(home: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(home.join("boards")).map_err(|e| kb_err(e.to_string()))?;
        Ok(Self { home })
    }

    /// Kanban home yang dipakai store ini.
    pub fn home(&self) -> &Path {
        &self.home
    }

    fn board_dir(&self, slug: &str) -> PathBuf {
        self.home.join("boards").join(slug)
    }

    fn meta_path(&self, slug: &str) -> PathBuf {
        self.board_dir(slug).join("board.json")
    }

    /// Buat board; semantik `mkdir -p` seperti `create_board` aslinya:
    /// board yang sudah ada dikembalikan metadatanya (tidak error).
    pub fn create_board(&self, slug: &str, opts: BoardOptions) -> Result<BoardMeta> {
        let slug = normalize_board_slug(slug)?;
        let dir = self.board_dir(&slug);
        std::fs::create_dir_all(&dir).map_err(|e| kb_err(e.to_string()))?;
        let now = now_unix();
        let meta = BoardMeta {
            name: opts.name.unwrap_or_else(|| slug.clone()),
            description: opts.description,
            icon: opts.icon,
            color: opts.color,
            project_id: opts.project_id,
            archived: false,
            created_at: now,
            slug: slug.clone(),
        };
        // Pembuatan eksplisit menghapus penanda arsip (cerminan aslinya).
        let json = serde_json::to_string_pretty(&meta).map_err(|e| kb_err(e.to_string()))?;
        std::fs::write(self.meta_path(&slug), json).map_err(|e| kb_err(e.to_string()))?;
        // Sentuh DB agar langsung terlihat oleh list_boards.
        let _ = self.board_db(&slug)?;
        Ok(meta)
    }

    /// Metadata semua board: `default` selalu pertama bila ada.
    pub fn list_boards(&self, include_archived: bool) -> Result<Vec<BoardMeta>> {
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        // Board default selalu ada di urutan pertama (dibuat on-demand).
        if let Ok(meta) = self.create_board("default", BoardOptions::default()) {
            seen.insert(meta.slug.clone());
            if include_archived || !meta.archived {
                out.push(meta);
            }
        }
        let boards_root = self.home.join("boards");
        let mut slugs: Vec<String> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&boards_root) {
            for e in rd.flatten() {
                if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    slugs.push(e.file_name().to_string_lossy().into_owned());
                }
            }
        }
        slugs.sort();
        for slug in slugs {
            if seen.contains(&slug) {
                continue;
            }
            let normed = match normalize_board_slug(&slug) {
                Ok(s) => s,
                Err(_) => continue, // lewati direktori sampah
            };
            if normed != slug {
                continue;
            }
            // Direktori board valid: punya kanban.db atau board.json.
            let dir = self.board_dir(&normed);
            if !dir.join("kanban.db").exists() && !dir.join("board.json").exists() {
                continue;
            }
            if let Ok(meta) = self.read_board_meta(&normed) {
                if meta.archived && !include_archived {
                    continue;
                }
                seen.insert(normed.clone());
                out.push(meta);
            }
        }
        Ok(out)
    }

    /// Baca metadata board; `None` bila board tidak ada.
    pub fn get_board(&self, slug: &str) -> Result<Option<BoardMeta>> {
        let slug = normalize_board_slug(slug)?;
        if !self.board_dir(&slug).exists() {
            return Ok(None);
        }
        self.read_board_meta(&slug).map(Some)
    }

    fn read_board_meta(&self, slug: &str) -> Result<BoardMeta> {
        let path = self.meta_path(slug);
        if path.exists() {
            let raw = std::fs::read_to_string(&path).map_err(|e| kb_err(e.to_string()))?;
            return serde_json::from_str(&raw).map_err(|e| kb_err(e.to_string()));
        }
        // Self-heal: metadata hilang tapi DB ada → bangun default.
        Ok(BoardMeta {
            slug: slug.to_string(),
            name: slug.to_string(),
            description: None,
            icon: None,
            color: None,
            project_id: None,
            archived: false,
            created_at: now_unix(),
        })
    }

    /// Hapus board. `archive=true` memindahkannya ke `boards/_archived/<slug>-<ts>/`
    /// (seperti `remove_board` aslinya); `false` menghapus permanen.
    /// Board `default` tidak boleh dihapus.
    pub fn remove_board(&self, slug: &str, archive: bool) -> Result<BoardMeta> {
        let slug = normalize_board_slug(slug)?;
        if slug == "default" {
            return Err(kb_err("board 'default' tidak boleh dihapus"));
        }
        let dir = self.board_dir(&slug);
        if !dir.exists() {
            return Err(kb_err(format!("board '{slug}' tidak ada")));
        }
        let mut meta = self.read_board_meta(&slug)?;
        if archive {
            let dest = self
                .home
                .join("boards")
                .join("_archived")
                .join(format!("{slug}-{}", now_unix()));
            if let Some(p) = dest.parent() {
                std::fs::create_dir_all(p).map_err(|e| kb_err(e.to_string()))?;
            }
            std::fs::rename(&dir, &dest).map_err(|e| kb_err(e.to_string()))?;
            meta.archived = true;
        } else {
            std::fs::remove_dir_all(&dir).map_err(|e| kb_err(e.to_string()))?;
        }
        // Bila board yang dihapus adalah board aktif, kembali ke default.
        if self.current_board().unwrap_or_default() == slug {
            let _ = self.clear_current_board();
        }
        Ok(meta)
    }

    /// Buka koneksi database untuk sebuah board (membuat board bila belum ada).
    pub fn board_db(&self, slug: &str) -> Result<BoardDb> {
        let slug = normalize_board_slug(slug)?;
        let dir = self.board_dir(&slug);
        std::fs::create_dir_all(&dir).map_err(|e| kb_err(e.to_string()))?;
        let db_path = dir.join("kanban.db");
        let conn = Connection::open(&db_path).map_err(|e| kb_err(e.to_string()))?;
        let db = BoardDb {
            conn: Mutex::new(conn),
            slug: slug.clone(),
            attachments_root: dir.join("attachments"),
        };
        db.migrate()?;
        Ok(db)
    }

    /// Slug board aktif: file `current` → `"default"`.
    pub fn current_board(&self) -> Result<String> {
        let p = self.home.join("current");
        match std::fs::read_to_string(&p) {
            Ok(s) => normalize_board_slug(s.trim()).or(Ok("default".to_string())),
            Err(_) => Ok("default".to_string()),
        }
    }

    /// Tetapkan board aktif (harus sudah ada).
    pub fn set_current_board(&self, slug: &str) -> Result<()> {
        let slug = normalize_board_slug(slug)?;
        if self.get_board(&slug)?.is_none() {
            return Err(kb_err(format!("board '{slug}' tidak ada")));
        }
        std::fs::write(self.home.join("current"), &slug).map_err(|e| kb_err(e.to_string()))?;
        Ok(())
    }

    /// Hapus penanda board aktif (kembali ke `default`).
    pub fn clear_current_board(&self) -> Result<()> {
        let _ = std::fs::remove_file(self.home.join("current"));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// BoardDb — operasi per-board
// ---------------------------------------------------------------------------

/// Koneksi database satu board. Semua metode mengunci koneksi SQLite secara
/// singkat (pola yang sama dengan `SqliteMemoryStore`).
pub struct BoardDb {
    conn: Mutex<Connection>,
    slug: String,
    attachments_root: PathBuf,
}

impl BoardDb {
    /// Slug board yang dipegang koneksi ini.
    pub fn slug(&self) -> &str {
        &self.slug
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap()
    }

    fn migrate(&self) -> Result<()> {
        let conn = self.lock();
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS tasks (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                body TEXT,
                assignee TEXT,
                status TEXT NOT NULL,
                priority INTEGER NOT NULL DEFAULT 0,
                created_by TEXT,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                started_at INTEGER,
                completed_at INTEGER,
                workspace_kind TEXT NOT NULL DEFAULT 'scratch',
                workspace_path TEXT,
                branch_name TEXT,
                project_id TEXT,
                tenant TEXT,
                result TEXT,
                summary TEXT,
                metadata_json TEXT,
                idempotency_key TEXT,
                consecutive_failures INTEGER NOT NULL DEFAULT 0,
                max_runtime_seconds INTEGER,
                last_heartbeat_at INTEGER,
                current_run_id INTEGER,
                skills TEXT,
                model_override TEXT,
                provider_override TEXT,
                reasoning_effort TEXT,
                max_retries INTEGER,
                goal_mode INTEGER NOT NULL DEFAULT 0,
                goal_max_turns INTEGER,
                session_id TEXT,
                block_kind TEXT,
                block_recurrences INTEGER NOT NULL DEFAULT 0,
                completion_contract TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_tasks_status ON tasks(status);
            CREATE INDEX IF NOT EXISTS idx_tasks_assignee ON tasks(assignee);
            CREATE INDEX IF NOT EXISTS idx_tasks_tenant ON tasks(tenant);
            CREATE INDEX IF NOT EXISTS idx_tasks_idem ON tasks(idempotency_key);
            CREATE TABLE IF NOT EXISTS task_links (
                parent_id TEXT NOT NULL,
                child_id TEXT NOT NULL,
                PRIMARY KEY (parent_id, child_id)
            );
            CREATE INDEX IF NOT EXISTS idx_links_child ON task_links(child_id);
            CREATE INDEX IF NOT EXISTS idx_links_parent ON task_links(parent_id);
            CREATE TABLE IF NOT EXISTS task_comments (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                task_id TEXT NOT NULL,
                author TEXT NOT NULL,
                body TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_comments_task ON task_comments(task_id);
            CREATE TABLE IF NOT EXISTS task_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                task_id TEXT NOT NULL,
                run_id INTEGER,
                kind TEXT NOT NULL,
                payload TEXT,
                created_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_events_task ON task_events(task_id);
            CREATE TABLE IF NOT EXISTS task_runs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                task_id TEXT NOT NULL,
                profile TEXT,
                status TEXT NOT NULL,
                started_at INTEGER NOT NULL,
                ended_at INTEGER,
                outcome TEXT,
                summary TEXT,
                metadata TEXT,
                error TEXT,
                last_heartbeat_at INTEGER,
                max_runtime_seconds INTEGER
            );
            CREATE INDEX IF NOT EXISTS idx_runs_task ON task_runs(task_id);
            CREATE TABLE IF NOT EXISTS task_attachments (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                task_id TEXT NOT NULL,
                filename TEXT NOT NULL,
                stored_path TEXT NOT NULL,
                content_type TEXT,
                size INTEGER NOT NULL DEFAULT 0,
                uploaded_by TEXT,
                created_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_attachments_task ON task_attachments(task_id);
            "#,
        )
        .map_err(|e| kb_err(e.to_string()))?;
        Ok(())
    }

    // -- Tugas -----------------------------------------------------------

    /// Buat task baru; mengembalikan task yang tersimpan.
    ///
    /// Status awal meniru `create_task` aslinya: `triage=true` → `triage`;
    /// bila ada parent yang belum `done` → `todo`; `initial_status="blocked"`
    /// → `blocked`; selain itu `running` (default aslinya).
    /// `idempotency_key` yang sudah dipakai task non-arsip mengembalikan
    /// task lama, bukan duplikat.
    pub fn create_task(&self, t: NewTask) -> Result<KanbanTask> {
        let title = t.title.trim();
        if title.is_empty() {
            return Err(kb_err("title wajib diisi"));
        }
        let assignee = t.assignee.trim();
        if assignee.is_empty() {
            return Err(kb_err(
                "assignee wajib diisi — task tanpa assignee tidak pernah di-dispatch",
            ));
        }
        let workspace_kind = t.workspace_kind.as_deref().unwrap_or("scratch");
        if !["scratch", "dir", "worktree"].contains(&workspace_kind) {
            return Err(kb_err(format!(
                "workspace_kind harus salah satu dari scratch|dir|worktree, dapat '{workspace_kind}'"
            )));
        }
        if workspace_kind == "dir"
            && t.workspace_path
                .as_deref()
                .map(|p| p.trim().is_empty())
                .unwrap_or(true)
        {
            return Err(kb_err(
                "workspace_kind 'dir' membutuhkan workspace_path absolut",
            ));
        }
        if let Some(p) = &t.workspace_path {
            if workspace_kind == "dir" && !p.starts_with('/') {
                return Err(kb_err("workspace_path untuk 'dir' harus absolut"));
            }
        }
        let initial_status = t.initial_status.as_deref().unwrap_or("running");
        if ![STATUS_RUNNING, STATUS_BLOCKED].contains(&initial_status) {
            return Err(kb_err(format!(
                "initial_status harus 'running' atau 'blocked', dapat '{initial_status}'"
            )));
        }
        if t.provider.is_some() && t.model.is_none() {
            return Err(kb_err("provider membutuhkan model"));
        }

        let conn = self.lock();

        // Idempotency: kembalikan task non-arsip yang sudah memakai kunci ini.
        if let Some(key) = t.idempotency_key.as_deref().filter(|k| !k.is_empty()) {
            let existing: Option<String> = conn
                .query_row(
                    "SELECT id FROM tasks WHERE idempotency_key = ?1 AND status != 'archived' ORDER BY created_at DESC LIMIT 1",
                    params![key],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| kb_err(e.to_string()))?;
            if let Some(id) = existing {
                drop(conn);
                return self
                    .get_task(&id)?
                    .ok_or_else(|| kb_err("task idempoten hilang"));
            }
        }

        // Validasi parent: semuanya harus ada.
        let parents: Vec<String> = t.parents.into_iter().filter(|p| !p.is_empty()).collect();
        for p in &parents {
            let exists: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM tasks WHERE id = ?1)",
                    params![p],
                    |r| r.get(0),
                )
                .map_err(|e| kb_err(e.to_string()))?;
            if !exists {
                return Err(kb_err(format!("parent task '{p}' tidak ada")));
            }
        }

        // Status awal.
        let status = if t.triage {
            STATUS_TRIAGE.to_string()
        } else if initial_status == STATUS_BLOCKED {
            STATUS_BLOCKED.to_string()
        } else if parents.iter().any(|p| {
            conn.query_row("SELECT status FROM tasks WHERE id = ?1", params![p], |r| {
                let s: String = r.get(0)?;
                Ok(s)
            })
            .map(|s| s != STATUS_DONE)
            .unwrap_or(true)
        }) {
            STATUS_TODO.to_string()
        } else {
            STATUS_RUNNING.to_string()
        };

        let id = new_task_id();
        let now = now_unix();
        let skills_json = if t.skills.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&t.skills).map_err(|e| kb_err(e.to_string()))?)
        };
        conn.execute(
            "INSERT INTO tasks (id, title, body, assignee, status, priority, created_by,
                 created_at, updated_at, workspace_kind, workspace_path, project_id, tenant,
                 idempotency_key, max_runtime_seconds, skills, model_override, provider_override,
                 goal_mode, goal_max_turns, session_id, completion_contract)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21)",
            params![
                id,
                title,
                t.body,
                assignee,
                status,
                t.priority,
                t.created_by,
                now,
                workspace_kind,
                t.workspace_path,
                t.project_id,
                t.tenant,
                t.idempotency_key,
                t.max_runtime_seconds,
                skills_json,
                t.model,
                t.provider,
                t.goal_mode as i64,
                t.goal_max_turns,
                t.session_id,
                t.completion_contract,
            ],
        )
        .map_err(|e| kb_err(e.to_string()))?;
        for p in &parents {
            conn.execute(
                "INSERT OR IGNORE INTO task_links (parent_id, child_id) VALUES (?1, ?2)",
                params![p, id],
            )
            .map_err(|e| kb_err(e.to_string()))?;
        }
        conn.execute(
            "INSERT INTO task_events (task_id, kind, payload, created_at) VALUES (?1, 'created', ?2, ?3)",
            params![
                id,
                serde_json::json!({"status": status, "parents": parents}).to_string(),
                now
            ],
        )
        .map_err(|e| kb_err(e.to_string()))?;
        drop(conn);
        self.get_task(&id)?
            .ok_or_else(|| kb_err("task baru tidak terbaca kembali"))
    }

    /// Ambil satu task; `None` bila tidak ada.
    pub fn get_task(&self, id: &str) -> Result<Option<KanbanTask>> {
        let conn = self.lock();
        let row: Option<KanbanTask> = conn
            .query_row(
                "SELECT * FROM tasks WHERE id = ?1",
                params![id],
                row_to_task,
            )
            .optional()
            .map_err(|e| kb_err(e.to_string()))?;
        Ok(row)
    }

    /// Daftar ringkasan task dengan filter ala `kanban_list` aslinya.
    /// `limit` default 50, maks 200.
    pub fn list_tasks(&self, f: &TaskFilter) -> Result<Vec<TaskSummary>> {
        if let Some(s) = &f.status {
            if !ALL_STATUSES.contains(&s.as_str()) {
                return Err(kb_err(format!("status filter tidak dikenal: '{s}'")));
            }
        }
        let limit = f.limit.unwrap_or(50).clamp(1, 200) as i64;
        let conn = self.lock();
        let mut sql = String::from("SELECT * FROM tasks WHERE 1=1");
        if !f.include_archived {
            sql.push_str(" AND status != 'archived'");
        }
        if f.assignee.is_some() {
            sql.push_str(" AND assignee = ?");
        }
        if f.status.is_some() {
            sql.push_str(" AND status = ?");
        }
        if f.tenant.is_some() {
            sql.push_str(" AND tenant = ?");
        }
        sql.push_str(" ORDER BY priority DESC, created_at ASC LIMIT ?");
        let mut stmt = conn.prepare(&sql).map_err(|e| kb_err(e.to_string()))?;
        let mut param_values: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(a) = &f.assignee {
            param_values.push(Box::new(a.clone()));
        }
        if let Some(s) = &f.status {
            param_values.push(Box::new(s.clone()));
        }
        if let Some(tn) = &f.tenant {
            param_values.push(Box::new(tn.clone()));
        }
        param_values.push(Box::new(limit));
        let param_refs: Vec<&dyn rusqlite::ToSql> =
            param_values.iter().map(|b| b.as_ref()).collect();
        let tasks: Vec<KanbanTask> = stmt
            .query_map(param_refs.as_slice(), row_to_task)
            .map_err(|e| kb_err(e.to_string()))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| kb_err(e.to_string()))?;
        drop(stmt);

        let mut out = Vec::with_capacity(tasks.len());
        for t in tasks {
            let parent_ids: Vec<String> = conn
                .prepare("SELECT parent_id FROM task_links WHERE child_id = ?1")
                .map_err(|e| kb_err(e.to_string()))?
                .query_map(params![t.id], |r| r.get(0))
                .map_err(|e| kb_err(e.to_string()))?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|e| kb_err(e.to_string()))?;
            let child_ids: Vec<String> = conn
                .prepare("SELECT child_id FROM task_links WHERE parent_id = ?1")
                .map_err(|e| kb_err(e.to_string()))?
                .query_map(params![t.id], |r| r.get(0))
                .map_err(|e| kb_err(e.to_string()))?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|e| kb_err(e.to_string()))?;
            let comment_count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM task_comments WHERE task_id = ?1",
                    params![t.id],
                    |r| r.get(0),
                )
                .map_err(|e| kb_err(e.to_string()))?;
            let attachment_count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM task_attachments WHERE task_id = ?1",
                    params![t.id],
                    |r| r.get(0),
                )
                .map_err(|e| kb_err(e.to_string()))?;
            out.push(TaskSummary {
                id: t.id,
                title: t.title,
                status: t.status,
                assignee: t.assignee,
                priority: t.priority,
                tenant: t.tenant,
                parent_ids,
                child_ids,
                comment_count,
                attachment_count,
                created_at: t.created_at,
            });
        }
        Ok(out)
    }

    /// Terapkan patch parsial ke sebuah task.
    pub fn update_task(&self, id: &str, patch: TaskPatch) -> Result<()> {
        let conn = self.lock();
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM tasks WHERE id = ?1)",
                params![id],
                |r| r.get(0),
            )
            .map_err(|e| kb_err(e.to_string()))?;
        if !exists {
            return Err(kb_err(format!("task '{id}' tidak ada")));
        }
        let now = now_unix();
        let mut sets: Vec<String> = vec!["updated_at = ?".to_string()];
        let mut vals: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(now)];
        macro_rules! opt_col {
            ($field:expr, $col:literal) => {
                if let Some(v) = $field {
                    sets.push(format!("{col} = ?", col = $col));
                    vals.push(Box::new(v));
                }
            };
        }
        opt_col!(patch.body, "body");
        opt_col!(patch.assignee, "assignee");
        opt_col!(patch.tenant, "tenant");
        opt_col!(patch.result, "result");
        opt_col!(patch.summary, "summary");
        opt_col!(patch.metadata_json, "metadata_json");
        opt_col!(patch.model_override, "model_override");
        opt_col!(patch.provider_override, "provider_override");
        opt_col!(patch.block_kind, "block_kind");
        if let Some(p) = patch.priority {
            sets.push("priority = ?".to_string());
            vals.push(Box::new(p));
        }
        vals.push(Box::new(id.to_string()));
        let sql = format!("UPDATE tasks SET {} WHERE id = ?", sets.join(", "));
        let refs: Vec<&dyn rusqlite::ToSql> = vals.iter().map(|b| b.as_ref()).collect();
        conn.execute(&sql, refs.as_slice())
            .map_err(|e| kb_err(e.to_string()))?;
        Ok(())
    }

    /// Pindahkan task ke status lain, memvalidasi terhadap matriks transisi
    /// workflow bawaan. Mencatat event `status_changed`.
    pub fn move_task(&self, id: &str, to: &str, reason: Option<&str>) -> Result<()> {
        if !ALL_STATUSES.contains(&to) {
            return Err(kb_err(format!("status tujuan tidak dikenal: '{to}'")));
        }
        let conn = self.lock();
        let from: String = conn
            .query_row("SELECT status FROM tasks WHERE id = ?1", params![id], |r| {
                r.get(0)
            })
            .optional()
            .map_err(|e| kb_err(e.to_string()))?
            .ok_or_else(|| kb_err(format!("task '{id}' tidak ada")))?;
        if !can_transition(&from, to) {
            return Err(kb_err(format!(
                "transisi '{from}' → '{to}' tidak diizinkan workflow"
            )));
        }
        let now = now_unix();
        conn.execute(
            "UPDATE tasks SET status = ?1, updated_at = ?2,
                 started_at = COALESCE(started_at, CASE WHEN ?1 = 'running' THEN ?2 END),
                 completed_at = CASE WHEN ?1 = 'done' THEN ?2 ELSE completed_at END
             WHERE id = ?3",
            params![to, now, id],
        )
        .map_err(|e| kb_err(e.to_string()))?;
        let payload = serde_json::json!({"from": from, "to": to, "reason": reason}).to_string();
        conn.execute(
            "INSERT INTO task_events (task_id, kind, payload, created_at) VALUES (?1, 'status_changed', ?2, ?3)",
            params![id, payload, now],
        )
        .map_err(|e| kb_err(e.to_string()))?;
        Ok(())
    }

    /// Tandai task `done` dengan ringkasan handoff; dipakai `kanban_complete`.
    pub fn complete_task(
        &self,
        id: &str,
        summary: Option<&str>,
        metadata_json: Option<&str>,
        result: Option<&str>,
    ) -> Result<()> {
        let conn = self.lock();
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM tasks WHERE id = ?1)",
                params![id],
                |r| r.get(0),
            )
            .map_err(|e| kb_err(e.to_string()))?;
        if !exists {
            return Err(kb_err(format!("task '{id}' tidak ada")));
        }
        let now = now_unix();
        conn.execute(
            "UPDATE tasks SET summary = COALESCE(?1, summary),
                 metadata_json = COALESCE(?2, metadata_json),
                 result = COALESCE(?3, result),
                 completed_at = ?4, updated_at = ?4, block_recurrences = 0,
                 consecutive_failures = 0
             WHERE id = ?5",
            params![summary, metadata_json, result, now, id],
        )
        .map_err(|e| kb_err(e.to_string()))?;
        drop(conn);
        // move_task memvalidasi transisi; bila status saat ini tidak bisa ke
        // done (mis. triage), biarkan error apa adanya — pemanggil (Worker B)
        // yang memutuskan.
        self.move_task(id, STATUS_DONE, Some("completed"))
    }

    /// Tulis `block_kind` + `block_recurrences` secara atomik — dipakai
    /// `kanban_block` untuk routing block + loop-breaker (cerminan
    /// `_route_block` di aslinya). Tidak mengubah status dan tidak
    /// memvalidasi; pemanggil (tool) yang mengatur urutannya.
    /// (Tambahan Worker B — batch 7.)
    pub fn set_block_state(&self, id: &str, kind: Option<&str>, recurrences: i64) -> Result<()> {
        let conn = self.lock();
        let n = conn
            .execute(
                "UPDATE tasks SET block_kind = ?1, block_recurrences = ?2, updated_at = ?3
                 WHERE id = ?4",
                params![kind, recurrences, now_unix(), id],
            )
            .map_err(|e| kb_err(e.to_string()))?;
        if n == 0 {
            return Err(kb_err(format!("task '{id}' tidak ada")));
        }
        Ok(())
    }

    // -- Komentar ---------------------------------------------------------

    /// Tambahkan komentar ke thread task.
    pub fn add_comment(&self, task_id: &str, author: &str, body: &str) -> Result<Comment> {
        if body.trim().is_empty() {
            return Err(kb_err("body komentar wajib diisi"));
        }
        let conn = self.lock();
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM tasks WHERE id = ?1)",
                params![task_id],
                |r| r.get(0),
            )
            .map_err(|e| kb_err(e.to_string()))?;
        if !exists {
            return Err(kb_err(format!("task '{task_id}' tidak ada")));
        }
        let now = now_unix();
        conn.execute(
            "INSERT INTO task_comments (task_id, author, body, created_at) VALUES (?1,?2,?3,?4)",
            params![task_id, author, body, now],
        )
        .map_err(|e| kb_err(e.to_string()))?;
        let id = conn.last_insert_rowid();
        Ok(Comment {
            id,
            task_id: task_id.to_string(),
            author: author.to_string(),
            body: body.to_string(),
            created_at: now,
        })
    }

    /// Semua komentar sebuah task, urut kronologis.
    pub fn list_comments(&self, task_id: &str) -> Result<Vec<Comment>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT id, task_id, author, body, created_at FROM task_comments
                 WHERE task_id = ?1 ORDER BY id ASC",
            )
            .map_err(|e| kb_err(e.to_string()))?;
        let rows = stmt
            .query_map(params![task_id], |r| {
                Ok(Comment {
                    id: r.get(0)?,
                    task_id: r.get(1)?,
                    author: r.get(2)?,
                    body: r.get(3)?,
                    created_at: r.get(4)?,
                })
            })
            .map_err(|e| kb_err(e.to_string()))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| kb_err(e.to_string()))?;
        Ok(rows)
    }

    // -- Event ------------------------------------------------------------

    /// Catat event ke log task (heartbeat, komentar, transisi, dsb).
    pub fn record_event(&self, task_id: &str, kind: &str, payload: Option<&str>) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO task_events (task_id, kind, payload, created_at) VALUES (?1,?2,?3,?4)",
            params![task_id, kind, payload, now_unix()],
        )
        .map_err(|e| kb_err(e.to_string()))?;
        Ok(())
    }

    /// Event-event terakhir sebuah task (terbaru dulu), dibatasi `limit`.
    pub fn list_events(&self, task_id: &str, limit: usize) -> Result<Vec<TaskEvent>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT id, task_id, run_id, kind, payload, created_at FROM task_events
                 WHERE task_id = ?1 ORDER BY id DESC LIMIT ?2",
            )
            .map_err(|e| kb_err(e.to_string()))?;
        let rows = stmt
            .query_map(params![task_id, limit as i64], |r| {
                Ok(TaskEvent {
                    id: r.get(0)?,
                    task_id: r.get(1)?,
                    run_id: r.get(2)?,
                    kind: r.get(3)?,
                    payload: r.get(4)?,
                    created_at: r.get(5)?,
                })
            })
            .map_err(|e| kb_err(e.to_string()))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| kb_err(e.to_string()))?;
        Ok(rows)
    }

    /// Sinyal liveness (`kanban_heartbeat`): update `last_heartbeat_at` + event.
    pub fn heartbeat(&self, task_id: &str, note: Option<&str>) -> Result<()> {
        let conn = self.lock();
        let n = conn
            .execute(
                "UPDATE tasks SET last_heartbeat_at = ?1 WHERE id = ?2",
                params![now_unix(), task_id],
            )
            .map_err(|e| kb_err(e.to_string()))?;
        if n == 0 {
            return Err(kb_err(format!("task '{task_id}' tidak ada")));
        }
        let payload = note.map(|s| serde_json::json!({"note": s}).to_string());
        conn.execute(
            "INSERT INTO task_events (task_id, kind, payload, created_at) VALUES (?1, 'heartbeat', ?2, ?3)",
            params![task_id, payload, now_unix()],
        )
        .map_err(|e| kb_err(e.to_string()))?;
        Ok(())
    }

    // -- Link parent → child -----------------------------------------------

    /// Tambah edge dependensi parent→child. Menolak self-link dan siklus,
    /// seperti `kanban_link` aslinya.
    pub fn add_link(&self, parent_id: &str, child_id: &str) -> Result<()> {
        if parent_id == child_id {
            return Err(kb_err("self-link ditolak"));
        }
        let conn = self.lock();
        for id in [parent_id, child_id] {
            let exists: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM tasks WHERE id = ?1)",
                    params![id],
                    |r| r.get(0),
                )
                .map_err(|e| kb_err(e.to_string()))?;
            if !exists {
                return Err(kb_err(format!("task '{id}' tidak ada")));
            }
        }
        // Deteksi siklus: bila parent sudah bisa dicapai dari child lewat
        // edge parent→child, menambah edge ini menutup lingkaran.
        let mut stack = vec![child_id.to_string()];
        let mut seen = std::collections::HashSet::new();
        while let Some(cur) = stack.pop() {
            if cur == parent_id {
                return Err(kb_err("link ditolak: membentuk siklus dependensi"));
            }
            if !seen.insert(cur.clone()) {
                continue;
            }
            let children: Vec<String> = conn
                .prepare("SELECT child_id FROM task_links WHERE parent_id = ?1")
                .map_err(|e| kb_err(e.to_string()))?
                .query_map(params![cur], |r| r.get(0))
                .map_err(|e| kb_err(e.to_string()))?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|e| kb_err(e.to_string()))?;
            stack.extend(children);
        }
        conn.execute(
            "INSERT OR IGNORE INTO task_links (parent_id, child_id) VALUES (?1, ?2)",
            params![parent_id, child_id],
        )
        .map_err(|e| kb_err(e.to_string()))?;
        let now = now_unix();
        let payload = serde_json::json!({"parent_id": parent_id}).to_string();
        conn.execute(
            "INSERT INTO task_events (task_id, kind, payload, created_at) VALUES (?1, 'linked', ?2, ?3)",
            params![child_id, payload, now],
        )
        .map_err(|e| kb_err(e.to_string()))?;
        Ok(())
    }

    /// Hapus edge dependensi.
    pub fn remove_link(&self, parent_id: &str, child_id: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "DELETE FROM task_links WHERE parent_id = ?1 AND child_id = ?2",
            params![parent_id, child_id],
        )
        .map_err(|e| kb_err(e.to_string()))?;
        Ok(())
    }

    /// Id parent langsung sebuah task.
    pub fn parents_of(&self, task_id: &str) -> Result<Vec<String>> {
        let conn = self.lock();
        link_ids(
            &conn,
            "SELECT parent_id FROM task_links WHERE child_id = ?1",
            task_id,
        )
    }

    /// Id child langsung sebuah task.
    pub fn children_of(&self, task_id: &str) -> Result<Vec<String>> {
        let conn = self.lock();
        link_ids(
            &conn,
            "SELECT child_id FROM task_links WHERE parent_id = ?1",
            task_id,
        )
    }

    /// Promosikan child yang parent-nya semua `done` dari `todo` ke `ready`
    /// (dipakai setelah sebuah task selesai; cerminan auto-promote aslinya).
    /// Mengembalikan id task yang dipromosikan.
    pub fn promote_ready_children(&self, parent_id: &str) -> Result<Vec<String>> {
        let children = self.children_of(parent_id)?;
        let mut promoted = Vec::new();
        for child in children {
            let parents = self.parents_of(&child)?;
            let conn = self.lock();
            let mut all_done = true;
            let mut child_status = String::new();
            for p in &parents {
                let s: Option<String> = conn
                    .query_row("SELECT status FROM tasks WHERE id = ?1", params![p], |r| {
                        r.get(0)
                    })
                    .optional()
                    .map_err(|e| kb_err(e.to_string()))?;
                if s.as_deref() != Some(STATUS_DONE) {
                    all_done = false;
                    break;
                }
            }
            if all_done {
                child_status = conn
                    .query_row(
                        "SELECT status FROM tasks WHERE id = ?1",
                        params![child],
                        |r| r.get::<_, String>(0),
                    )
                    .optional()
                    .map_err(|e| kb_err(e.to_string()))?
                    .unwrap_or_default();
            }
            drop(conn);
            if all_done && child_status == STATUS_TODO {
                self.move_task(&child, STATUS_READY, Some("semua parent done"))?;
                promoted.push(child);
            }
        }
        Ok(promoted)
    }

    // -- Attachment ----------------------------------------------------------

    /// Simpan attachment dari bytes (maks 25 MB, seperti aslinya); nama file
    /// dibersihkan dari komponen direktori.
    pub fn add_attachment_bytes(
        &self,
        task_id: &str,
        filename: &str,
        content_type: Option<&str>,
        bytes: &[u8],
        uploaded_by: &str,
    ) -> Result<Attachment> {
        if bytes.len() > MAX_ATTACHMENT_BYTES {
            return Err(kb_err(format!(
                "attachment melebihi batas 25 MB ({} byte)",
                bytes.len()
            )));
        }
        let leaf = Path::new(filename)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| kb_err("nama file tidak valid"))?;
        let conn = self.lock();
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM tasks WHERE id = ?1)",
                params![task_id],
                |r| r.get(0),
            )
            .map_err(|e| kb_err(e.to_string()))?;
        if !exists {
            return Err(kb_err(format!("task '{task_id}' tidak ada")));
        }
        drop(conn);
        let dir = self.attachments_root.join(task_id);
        std::fs::create_dir_all(&dir).map_err(|e| kb_err(e.to_string()))?;
        // Hindari tabrakan nama: tambah counter bila perlu.
        let mut stored = dir.join(&leaf);
        let mut n = 1;
        while stored.exists() {
            let stem = Path::new(&leaf)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let ext = Path::new(&leaf)
                .extension()
                .map(|s| format!(".{}", s.to_string_lossy()))
                .unwrap_or_default();
            stored = dir.join(format!("{stem}-{n}{ext}"));
            n += 1;
        }
        std::fs::write(&stored, bytes).map_err(|e| kb_err(e.to_string()))?;
        let stored_name = stored
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or(leaf);
        let conn = self.lock();
        let now = now_unix();
        conn.execute(
            "INSERT INTO task_attachments (task_id, filename, stored_path, content_type, size, uploaded_by, created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![
                task_id,
                stored_name,
                stored.to_string_lossy(),
                content_type,
                bytes.len() as i64,
                uploaded_by,
                now
            ],
        )
        .map_err(|e| kb_err(e.to_string()))?;
        let id = conn.last_insert_rowid();
        Ok(Attachment {
            id,
            task_id: task_id.to_string(),
            filename: stored_name,
            stored_path: stored.to_string_lossy().into_owned(),
            content_type: content_type.map(|s| s.to_string()),
            size: bytes.len() as i64,
            uploaded_by: Some(uploaded_by.to_string()),
            created_at: now,
        })
    }

    /// Daftar attachment sebuah task.
    pub fn list_attachments(&self, task_id: &str) -> Result<Vec<Attachment>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT id, task_id, filename, stored_path, content_type, size, uploaded_by, created_at
                 FROM task_attachments WHERE task_id = ?1 ORDER BY id ASC",
            )
            .map_err(|e| kb_err(e.to_string()))?;
        let rows = stmt
            .query_map(params![task_id], |r| {
                Ok(Attachment {
                    id: r.get(0)?,
                    task_id: r.get(1)?,
                    filename: r.get(2)?,
                    stored_path: r.get(3)?,
                    content_type: r.get(4)?,
                    size: r.get(5)?,
                    uploaded_by: r.get(6)?,
                    created_at: r.get(7)?,
                })
            })
            .map_err(|e| kb_err(e.to_string()))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| kb_err(e.to_string()))?;
        Ok(rows)
    }
}

fn link_ids(conn: &Connection, sql: &str, task_id: &str) -> Result<Vec<String>> {
    let ids: Vec<String> = conn
        .prepare(sql)
        .map_err(|e| kb_err(e.to_string()))?
        .query_map(params![task_id], |r| r.get(0))
        .map_err(|e| kb_err(e.to_string()))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| kb_err(e.to_string()))?;
    Ok(ids)
}

fn row_to_task(r: &rusqlite::Row) -> rusqlite::Result<KanbanTask> {
    let skills: Option<String> = r.get(24)?;
    Ok(KanbanTask {
        id: r.get(0)?,
        title: r.get(1)?,
        body: r.get(2)?,
        assignee: r.get(3)?,
        status: r.get(4)?,
        priority: r.get(5)?,
        created_by: r.get(6)?,
        created_at: r.get(7)?,
        updated_at: r.get(8)?,
        started_at: r.get(9)?,
        completed_at: r.get(10)?,
        workspace_kind: r.get(11)?,
        workspace_path: r.get(12)?,
        branch_name: r.get(13)?,
        project_id: r.get(14)?,
        tenant: r.get(15)?,
        result: r.get(16)?,
        summary: r.get(17)?,
        metadata_json: r.get(18)?,
        idempotency_key: r.get(19)?,
        consecutive_failures: r.get(20)?,
        max_runtime_seconds: r.get(21)?,
        last_heartbeat_at: r.get(22)?,
        skills: parse_skills(skills),
        model_override: r.get(25)?,
        provider_override: r.get(26)?,
        reasoning_effort: r.get(27)?,
        max_retries: r.get(28).ok().flatten(),
        goal_mode: r.get::<_, i64>(29).map(|v| v != 0).unwrap_or(false),
        goal_max_turns: r.get(30).ok().flatten(),
        session_id: r.get(31).ok().flatten(),
        block_kind: r.get(32).ok().flatten(),
        block_recurrences: r.get(33)?,
        completion_contract: r.get(34).ok().flatten(),
    })
}

// NOTE: row_to_task memakai `SELECT *`; urutan kolom harus cocok dengan
// urutan CREATE TABLE tasks di migrate(). Indeks: 0 id, 1 title, 2 body,
// 3 assignee, 4 status, 5 priority, 6 created_by, 7 created_at, 8 updated_at,
// 9 started_at, 10 completed_at, 11 workspace_kind, 12 workspace_path,
// 13 branch_name, 14 project_id, 15 tenant, 16 result, 17 summary,
// 18 metadata_json, 19 idempotency_key, 20 consecutive_failures,
// 21 max_runtime_seconds, 22 last_heartbeat_at, 23 current_run_id,
// 24 skills, 25 model_override, 26 provider_override, 27 reasoning_effort,
// 28 max_retries, 29 goal_mode, 30 goal_max_turns, 31 session_id,
// 32 block_kind, 33 block_recurrences, 34 completion_contract.

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_home(tag: &str) -> PathBuf {
        let p =
            std::env::temp_dir().join(format!("hermes-kanban-test-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    fn open_store(tag: &str) -> KanbanStore {
        KanbanStore::open(tmp_home(tag)).unwrap()
    }

    #[test]
    fn board_crud_mkdir_p_semantics() {
        let store = open_store("board-crud");
        // Buat board baru.
        let meta = store
            .create_board(
                "proyek-a",
                BoardOptions {
                    name: Some("Proyek A".into()),
                    description: Some("board uji".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(meta.slug, "proyek-a");
        assert_eq!(meta.name, "Proyek A");
        assert!(!meta.archived);

        // mkdir -p: buat ulang slug yang sama → metadata, bukan error.
        let meta2 = store
            .create_board("proyek-a", BoardOptions::default())
            .unwrap();
        assert_eq!(meta2.slug, "proyek-a");

        // get_board + list_boards (default selalu pertama).
        let got = store.get_board("proyek-a").unwrap().unwrap();
        assert_eq!(got.slug, "proyek-a");
        let boards = store.list_boards(true).unwrap();
        assert_eq!(boards[0].slug, "default");
        assert!(boards.iter().any(|b| b.slug == "proyek-a"));
        assert!(store.get_board("tidak-ada").unwrap().is_none());

        // Board default tidak boleh dihapus.
        assert!(store.remove_board("default", true).is_err());
        assert!(store.remove_board("tidak-ada", true).is_err());

        // Arsipkan: direktori dipindah ke _archived.
        store.remove_board("proyek-a", true).unwrap();
        assert!(store.get_board("proyek-a").unwrap().is_none());
        let boards = store.list_boards(true).unwrap();
        assert!(!boards.iter().any(|b| b.slug == "proyek-a"));
    }

    #[test]
    fn board_slug_validation() {
        let store = open_store("board-slug");
        assert!(store.create_board("", BoardOptions::default()).is_err());
        assert!(store
            .create_board("ada spasi", BoardOptions::default())
            .is_err());
        assert!(
            store
                .create_board("UPPER_ok-123", BoardOptions::default())
                .unwrap()
                .slug
                == "upper_ok-123"
        );
    }

    #[test]
    fn current_board_roundtrip() {
        let store = open_store("board-current");
        assert_eq!(store.current_board().unwrap(), "default");
        store.create_board("b2", BoardOptions::default()).unwrap();
        store.set_current_board("b2").unwrap();
        assert_eq!(store.current_board().unwrap(), "b2");
        assert!(store.set_current_board("tidak-ada").is_err());
        store.clear_current_board().unwrap();
        assert_eq!(store.current_board().unwrap(), "default");
    }

    #[test]
    fn task_create_initial_status_rules() {
        let store = open_store("task-status");
        let db = store.board_db("default").unwrap();
        // Tanpa parent → running (default aslinya).
        let t1 = db
            .create_task(NewTask {
                title: "akar".into(),
                assignee: "w1".into(),
                ..Default::default()
            })
            .unwrap();
        assert!(t1.id.starts_with("t_"));
        assert_eq!(t1.status, STATUS_RUNNING);

        // Child dari parent yang belum done → todo.
        let t2 = db
            .create_task(NewTask {
                title: "anak".into(),
                assignee: "w2".into(),
                parents: vec![t1.id.clone()],
                ..Default::default()
            })
            .unwrap();
        assert_eq!(t2.status, STATUS_TODO);

        // triage=true memaksa triage.
        let t3 = db
            .create_task(NewTask {
                title: "butuh spek".into(),
                assignee: "w3".into(),
                triage: true,
                parents: vec![t1.id.clone()],
                ..Default::default()
            })
            .unwrap();
        assert_eq!(t3.status, STATUS_TRIAGE);

        // initial_status=blocked → blocked.
        let t4 = db
            .create_task(NewTask {
                title: "ops".into(),
                assignee: "w4".into(),
                initial_status: Some("blocked".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(t4.status, STATUS_BLOCKED);

        // Parent selesai → child baru langsung running.
        db.complete_task(&t1.id, Some("selesai"), None, None)
            .unwrap();
        let t5 = db
            .create_task(NewTask {
                title: "anak2".into(),
                assignee: "w2".into(),
                parents: vec![t1.id.clone()],
                ..Default::default()
            })
            .unwrap();
        assert_eq!(t5.status, STATUS_RUNNING);
    }

    #[test]
    fn task_create_validation() {
        let store = open_store("task-valid");
        let db = store.board_db("default").unwrap();
        // Title & assignee wajib.
        assert!(db
            .create_task(NewTask {
                title: "  ".into(),
                assignee: "w".into(),
                ..Default::default()
            })
            .is_err());
        assert!(db
            .create_task(NewTask {
                title: "x".into(),
                assignee: "".into(),
                ..Default::default()
            })
            .is_err());
        // workspace_kind tidak dikenal.
        assert!(db
            .create_task(NewTask {
                title: "x".into(),
                assignee: "w".into(),
                workspace_kind: Some("awan".into()),
                ..Default::default()
            })
            .is_err());
        // dir tanpa path absolut.
        assert!(db
            .create_task(NewTask {
                title: "x".into(),
                assignee: "w".into(),
                workspace_kind: Some("dir".into()),
                ..Default::default()
            })
            .is_err());
        assert!(db
            .create_task(NewTask {
                title: "x".into(),
                assignee: "w".into(),
                workspace_kind: Some("dir".into()),
                workspace_path: Some("relatif/path".into()),
                ..Default::default()
            })
            .is_err());
        // Parent fiktif.
        assert!(db
            .create_task(NewTask {
                title: "x".into(),
                assignee: "w".into(),
                parents: vec!["t_fiktif".into()],
                ..Default::default()
            })
            .is_err());
        // provider tanpa model.
        assert!(db
            .create_task(NewTask {
                title: "x".into(),
                assignee: "w".into(),
                provider: Some("anthropic".into()),
                ..Default::default()
            })
            .is_err());
    }

    #[test]
    fn idempotency_key_dedup() {
        let store = open_store("task-idem");
        let db = store.board_db("default").unwrap();
        let mk = || NewTask {
            title: "tugas idempoten".into(),
            assignee: "w".into(),
            idempotency_key: Some("kunci-1".into()),
            ..Default::default()
        };
        let a = db.create_task(mk()).unwrap();
        let b = db.create_task(mk()).unwrap();
        assert_eq!(a.id, b.id);
        let all = db.list_tasks(&TaskFilter::default()).unwrap();
        assert_eq!(all.len(), 1);
    }

    #[test]
    fn link_cycle_and_self_link_rejected() {
        let store = open_store("task-link");
        let db = store.board_db("default").unwrap();
        let a = db
            .create_task(NewTask {
                title: "a".into(),
                assignee: "w".into(),
                ..Default::default()
            })
            .unwrap();
        let b = db
            .create_task(NewTask {
                title: "b".into(),
                assignee: "w".into(),
                ..Default::default()
            })
            .unwrap();
        let c = db
            .create_task(NewTask {
                title: "c".into(),
                assignee: "w".into(),
                ..Default::default()
            })
            .unwrap();
        db.add_link(&a.id, &b.id).unwrap();
        db.add_link(&b.id, &c.id).unwrap();
        // c → a menutup siklus a→b→c→a.
        assert!(db.add_link(&c.id, &a.id).is_err());
        // Self-link.
        assert!(db.add_link(&a.id, &a.id).is_err());
        // Task fiktif.
        assert!(db.add_link(&a.id, "t_fiktif").is_err());
        // Link valid tetap tercatat.
        assert_eq!(db.parents_of(&b.id).unwrap(), vec![a.id.clone()]);
        assert_eq!(db.children_of(&a.id).unwrap(), vec![b.id.clone()]);
        db.remove_link(&a.id, &b.id).unwrap();
        assert!(db.parents_of(&b.id).unwrap().is_empty());
    }

    #[test]
    fn promote_ready_children_on_parent_done() {
        let store = open_store("task-promote");
        let db = store.board_db("default").unwrap();
        let p = db
            .create_task(NewTask {
                title: "parent".into(),
                assignee: "w".into(),
                ..Default::default()
            })
            .unwrap();
        let child = db
            .create_task(NewTask {
                title: "child".into(),
                assignee: "w".into(),
                parents: vec![p.id.clone()],
                ..Default::default()
            })
            .unwrap();
        assert_eq!(child.status, STATUS_TODO);
        db.complete_task(&p.id, Some("ok"), None, None).unwrap();
        let promoted = db.promote_ready_children(&p.id).unwrap();
        assert_eq!(promoted, vec![child.id.clone()]);
        let child = db.get_task(&child.id).unwrap().unwrap();
        assert_eq!(child.status, STATUS_READY);
    }

    #[test]
    fn move_task_validates_workflow() {
        let store = open_store("task-move");
        let db = store.board_db("default").unwrap();
        let t = db
            .create_task(NewTask {
                title: "gerak".into(),
                assignee: "w".into(),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(t.status, STATUS_RUNNING);
        // running → blocked sah.
        db.move_task(&t.id, STATUS_BLOCKED, Some("butuh input"))
            .unwrap();
        // blocked → running TIDAK ada di matriks aslinya.
        assert!(db.move_task(&t.id, STATUS_RUNNING, None).is_err());
        // blocked → ready sah.
        db.move_task(&t.id, STATUS_READY, None).unwrap();
        // Status fiktif ditolak.
        assert!(db.move_task(&t.id, "khayal", None).is_err());
        // Task fiktif ditolak.
        assert!(db.move_task("t_fiktif", STATUS_DONE, None).is_err());
        // Event status_changed tercatat.
        let events = db.list_events(&t.id, 10).unwrap();
        assert!(events.iter().any(|e| e.kind == "status_changed"));
    }

    #[test]
    fn comments_events_heartbeat_roundtrip() {
        let store = open_store("task-ceh");
        let db = store.board_db("default").unwrap();
        let t = db
            .create_task(NewTask {
                title: "catat".into(),
                assignee: "w".into(),
                ..Default::default()
            })
            .unwrap();
        let c = db.add_comment(&t.id, "w", "catatan **penting**").unwrap();
        assert_eq!(c.body, "catatan **penting**");
        assert!(db.add_comment(&t.id, "w", "   ").is_err());
        let comments = db.list_comments(&t.id).unwrap();
        assert_eq!(comments.len(), 1);

        db.record_event(&t.id, "custom", Some("{\"a\":1}")).unwrap();
        let events = db.list_events(&t.id, 5).unwrap();
        assert!(events.iter().any(|e| e.kind == "custom"));

        db.heartbeat(&t.id, Some("masih jalan")).unwrap();
        let t2 = db.get_task(&t.id).unwrap().unwrap();
        assert!(t2.last_heartbeat_at.is_some());
        assert!(db.heartbeat("t_fiktif", None).is_err());
    }

    #[test]
    fn attachments_bytes_cap_and_sanitize() {
        let store = open_store("task-attach");
        let db = store.board_db("default").unwrap();
        let t = db
            .create_task(NewTask {
                title: "lampiran".into(),
                assignee: "w".into(),
                ..Default::default()
            })
            .unwrap();
        let a = db
            .add_attachment_bytes(
                &t.id,
                "../../evil.pdf",
                Some("application/pdf"),
                b"isi",
                "w",
            )
            .unwrap();
        // Komponen direktori dibuang.
        assert_eq!(a.filename, "evil.pdf");
        assert!(a.stored_path.contains(&t.id));
        assert_eq!(a.size, 3);
        // File benar-benar tertulis di disk.
        assert_eq!(std::fs::read(&a.stored_path).unwrap(), b"isi");
        let list = db.list_attachments(&t.id).unwrap();
        assert_eq!(list.len(), 1);

        // Melebihi 25 MB ditolak.
        let big = vec![0u8; MAX_ATTACHMENT_BYTES + 1];
        assert!(db
            .add_attachment_bytes(&t.id, "big.bin", None, &big, "w")
            .is_err());
        // Task fiktif ditolak.
        assert!(db
            .add_attachment_bytes("t_fiktif", "x.txt", None, b"x", "w")
            .is_err());
    }

    #[test]
    fn list_tasks_filters_and_limit_cap() {
        let store = open_store("task-list");
        let db = store.board_db("default").unwrap();
        for i in 0..5 {
            db.create_task(NewTask {
                title: format!("tugas {i}"),
                assignee: if i % 2 == 0 { "w1".into() } else { "w2".into() },
                tenant: Some("tenant-a".into()),
                priority: i,
                ..Default::default()
            })
            .unwrap();
        }
        let all = db.list_tasks(&TaskFilter::default()).unwrap();
        assert_eq!(all.len(), 5);
        // Urut prioritas menurun.
        assert_eq!(all[0].priority, 4);

        let w1 = db
            .list_tasks(&TaskFilter {
                assignee: Some("w1".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(w1.len(), 3);

        let limited = db
            .list_tasks(&TaskFilter {
                limit: Some(2),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(limited.len(), 2);

        // Status filter tidak dikenal → error.
        assert!(db
            .list_tasks(&TaskFilter {
                status: Some("khayal".into()),
                ..Default::default()
            })
            .is_err());

        // Arsip disembunyikan kecuali include_archived.
        let id = all[0].id.clone();
        db.move_task(&id, STATUS_ARCHIVED, None).unwrap();
        let visible = db.list_tasks(&TaskFilter::default()).unwrap();
        assert_eq!(visible.len(), 4);
        let with_arch = db
            .list_tasks(&TaskFilter {
                include_archived: true,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(with_arch.len(), 5);
    }

    #[test]
    fn update_task_patch() {
        let store = open_store("task-patch");
        let db = store.board_db("default").unwrap();
        let t = db
            .create_task(NewTask {
                title: "patch me".into(),
                assignee: "w".into(),
                ..Default::default()
            })
            .unwrap();
        db.update_task(
            &t.id,
            TaskPatch {
                body: Some(Some("body baru".into())),
                priority: Some(9),
                block_kind: Some(Some("needs_input".into())),
                ..Default::default()
            },
        )
        .unwrap();
        let t2 = db.get_task(&t.id).unwrap().unwrap();
        assert_eq!(t2.body.as_deref(), Some("body baru"));
        assert_eq!(t2.priority, 9);
        assert_eq!(t2.block_kind.as_deref(), Some("needs_input"));
        // Bersihkan kembali ke NULL.
        db.update_task(
            &t.id,
            TaskPatch {
                body: Some(None),
                ..Default::default()
            },
        )
        .unwrap();
        let t3 = db.get_task(&t.id).unwrap().unwrap();
        assert_eq!(t3.body, None);
        assert!(db.update_task("t_fiktif", TaskPatch::default()).is_err());
    }

    #[test]
    fn boards_are_isolated_dbs() {
        let store = open_store("board-isolasi");
        store.create_board("a", BoardOptions::default()).unwrap();
        store.create_board("b", BoardOptions::default()).unwrap();
        let da = store.board_db("a").unwrap();
        let dbb = store.board_db("b").unwrap();
        da.create_task(NewTask {
            title: "hanya di a".into(),
            assignee: "w".into(),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(da.list_tasks(&TaskFilter::default()).unwrap().len(), 1);
        assert!(dbb.list_tasks(&TaskFilter::default()).unwrap().is_empty());
    }
}
