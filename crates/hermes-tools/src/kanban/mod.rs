//! Tool kanban: papan tugas persisten ala Hermes asli.
//!
//! Cerminan Rust dari toolset `kanban` di aslinya
//! (`tools/kanban_tools.py`, `tools/kanban_tools_schemas.py`,
//! `hermes_cli/kanban_db*.py`, `hermes_cli/kanban_workflow.py`).
//!
//! # Peta tool asli → port Rust
//!
//! Aslinya mendaftarkan 15 tool (urutan registrasi dipertahankan):
//!
//! | # | Nama asli | Status port | Keterangan |
//! |---|-----------|-------------|------------|
//! | 1 | `kanban_board_create` | ✅ Worker A | *tidak ada di aslinya sebagai tool model* —
//! |   | | | cerminan `kanban_db_boards.create_board` (CLI `hermes kanban boards create`) |
//! | 2 | `kanban_board_list` | ✅ Worker A | cerminan `list_boards` |
//! | 3 | `kanban_board_remove` | ✅ Worker A | cerminan `remove_board` |
//! | 4 | `kanban_create` | ✅ Worker A | port langsung |
//! | 5 | `kanban_show` | ✅ Worker A | port langsung |
//! | 6 | `kanban_list` | ⬜ Worker B/C | port langsung |
//! | 7 | `kanban_complete` | ⬜ Worker B/C | port langsung |
//! | 8 | `kanban_block` | ⬜ Worker B/C | port langsung |
//! | 9 | `kanban_schedule` | ⬜ Worker B/C | port langsung |
//! | 10 | `kanban_request_review` | ⬜ Worker B/C | port langsung |
//! | 11 | `kanban_request_changes` | ⬜ Worker B/C | port langsung |
//! | 12 | `kanban_heartbeat` | ⬜ Worker B/C | port langsung |
//! | 13 | `kanban_comment` | ⬜ Worker B/C | port langsung |
//! | 14 | `kanban_attach` | ⬜ Worker B/C | port langsung |
//! | 15 | `kanban_attach_url` | ⬜ Worker B/C | port langsung |
//! | 16 | `kanban_attachments` | ⬜ Worker B/C | port langsung |
//! | 17 | `kanban_unblock` | ⬜ Worker B/C | port langsung |
//! | 18 | `kanban_link` | ⬜ Worker B/C | port langsung |
//!
//! # API `KanbanStore` / `BoardDb` (kontrak stabil untuk Worker B/C)
//!
//! Semua state tinggal di SQLite per board (`boards/<slug>/kanban.db` di
//! bawah kanban home). Resolusi home: `HERMES_KANBAN_HOME` →
//! `<HERMES_HOME>/kanban` → `<working_dir>/.hermes/kanban`
//! (lihat [`store::kanban_home`]).
//!
//! ```rust,no_run
//! use hermes_tools::kanban::store::{BoardOptions, KanbanStore, NewTask, TaskFilter};
//! use std::path::PathBuf;
//!
//! let store = KanbanStore::open(PathBuf::from("/tmp/kb-home")).unwrap();
//! store.create_board("proyek", BoardOptions::default()).unwrap();
//! let db = store.board_db("proyek").unwrap();
//!
//! let t = db.create_task(NewTask {
//!     title: "tulis laporan".into(),
//!     assignee: "writer".into(),
//!     ..Default::default()
//! }).unwrap();
//!
//! let ringkas = db.list_tasks(&TaskFilter::default()).unwrap();
//! db.move_task(&t.id, "blocked", Some("butuh data")).unwrap();
//! db.add_comment(&t.id, "writer", "menunggu CSV").unwrap();
//! db.heartbeat(&t.id, Some("50%")).unwrap();
//! ```
//!
//! ## Board (`KanbanStore`)
//!
//! - `KanbanStore::open(home)` — buka/buat kanban home.
//! - `create_board(slug, BoardOptions) -> BoardMeta` — semantik `mkdir -p`:
//!   board yang sudah ada dikembalikan (tidak error); pembuatan eksplisit
//!   menghapus penanda arsip.
//! - `list_boards(include_archived) -> Vec<BoardMeta>` — `default` selalu
//!   pertama; direktori sampah dilewati.
//! - `get_board(slug) -> Option<BoardMeta>` — `None` bila tidak ada.
//! - `remove_board(slug, archive) -> BoardMeta` — `archive=true` memindah ke
//!   `boards/_archived/<slug>-<ts>/`; `false` menghapus permanen; `default`
//!   tidak boleh dihapus.
//! - `board_db(slug) -> BoardDb` — buka koneksi board (buat board bila belum
//!   ada), lengkap dengan migrasi skema.
//! - `current_board() / set_current_board(slug) / clear_current_board()` —
//!   board aktif via file `current`, default `"default"`.
//!
//! ## Task (`BoardDb`)
//!
//! - `create_task(NewTask) -> KanbanTask` — validasi: `title` & `assignee`
//!   wajib (task tanpa assignee tidak pernah di-dispatch, seperti aslinya),
//!   `workspace_kind` ∈ {scratch, dir, worktree}, `dir` butuh
//!   `workspace_path` absolut, `initial_status` ∈ {running, blocked},
//!   `provider` membutuhkan `model`, semua `parents` harus ada.
//!   Status awal: `triage=true` → `triage`; parent belum `done` → `todo`;
//!   `initial_status="blocked"` → `blocked`; selain itu `running`.
//!   `idempotency_key` yang sudah dipakai task non-arsip mengembalikan task
//!   lama. ID berbentuk `t_<8 hex>` seperti aslinya. Mencatat event `created`.
//! - `get_task(id) -> Option<KanbanTask>`
//! - `list_tasks(&TaskFilter) -> Vec<TaskSummary>` — filter `assignee`,
//!   `status`, `tenant`, `include_archived`; `limit` default 50, maks 200;
//!   urut `priority` menurun lalu `created_at` menaik. Tiap baris memuat
//!   `parent_ids`, `child_ids`, `comment_count`, `attachment_count`.
//! - `update_task(id, TaskPatch) -> ()` — patch parsial; `Some(None)`
//!   mengosongkan kolom menjadi NULL.
//! - `move_task(id, to, reason) -> ()` — pindah status dengan validasi
//!   matriks [`store::can_transition`]; mencatat event `status_changed`;
//!   mengisi `started_at` saat pertama ke `running` dan `completed_at` saat
//!   ke `done`.
//! - `complete_task(id, summary, metadata_json, result) -> ()` — tulis
//!   handoff lalu pindah ke `done` (me-reset `block_recurrences` &
//!   `consecutive_failures` seperti aslinya).
//!
//! ## Komentar, event, heartbeat (`BoardDb`)
//!
//! - `add_comment(task_id, author, body) -> Comment` — `body` tak boleh kosong.
//! - `list_comments(task_id) -> Vec<Comment>` — kronologis menaik.
//! - `record_event(task_id, kind, payload) -> ()`
//! - `list_events(task_id, limit) -> Vec<TaskEvent>` — terbaru dulu.
//! - `heartbeat(task_id, note) -> ()` — update `last_heartbeat_at` + event
//!   `heartbeat` (cerminan `kanban_heartbeat`).
//!
//! ## Link dependensi (`BoardDb`)
//!
//! - `add_link(parent_id, child_id) -> ()` — menolak self-link & siklus
//!   (cerminan `kanban_link`); mencatat event `linked`.
//! - `remove_link(parent_id, child_id) -> ()`
//! - `parents_of(task_id) / children_of(task_id) -> Vec<String>`
//! - `promote_ready_children(parent_id) -> Vec<String>` — promosikan child
//!   `todo` yang semua parent-nya `done` menjadi `ready` (auto-promote
//!   aslinya); kembalikan id yang dipromosikan.
//!
//! ## Attachment (`BoardDb`)
//!
//! - `add_attachment_bytes(task_id, filename, content_type, bytes,
//!   uploaded_by) -> Attachment` — batas 25 MB (seperti aslinya); komponen
//!   direktori pada nama file dibuang; blob tersimpan di
//!   `attachments/<task_id>/`.
//! - `list_attachments(task_id) -> Vec<Attachment>`
//!
//! ## Status workflow (`store`)
//!
//! Konstanta `STATUS_TRIAGE/TODO/SCHEDULED/READY/RUNNING/BLOCKED/REVIEW/DONE/ARCHIVED`,
//! `ALL_STATUSES`, `valid_transitions(from)` (salinan tabel `manual`
//! `DEFAULT_WORKFLOW` aslinya) dan `can_transition(from, to)`.
//!
//! # Aturan tool
//!
//! - Setiap tool kanban menerima argumen opsional `board` (slug); bila
//!   kosong dipakai `HERMES_KANBAN_BOARD` → board aktif → `"default"`
//!   (cerminan `_DESC_BOARD` di skema aslinya).
//! - Tool yang mengubah state (`kanban_board_create`, `kanban_board_remove`,
//!   `kanban_create`, …) memakai `ToolEffect::Write`; pembaca murni
//!   (`kanban_board_list`, `kanban_show`, …) memakai `ToolEffect::Read`.

pub mod store;
pub mod tools;
pub mod tools_meta;
pub mod tools_queries;

pub use store::{
    Attachment, BoardDb, BoardMeta, BoardOptions, Comment, KanbanStore, KanbanTask, NewTask,
    TaskEvent, TaskFilter, TaskPatch, TaskSummary, ALL_STATUSES, MAX_ATTACHMENT_BYTES,
    STATUS_ARCHIVED, STATUS_BLOCKED, STATUS_DONE, STATUS_READY, STATUS_REVIEW, STATUS_RUNNING,
    STATUS_SCHEDULED, STATUS_TODO, STATUS_TRIAGE,
};
