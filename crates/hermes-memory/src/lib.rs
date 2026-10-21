//! hermes-memory: memori persisten Hermes berbasis SQLite + FTS5.
//!
//! Meniru `hermes_state*.py` / memory system aslinya:
//! - memori lintas sesi dengan full-text search (FTS5)
//! - penyimpanan sesi percakapan + pencarian lintas sesi

pub mod store;
pub mod threat_scan;

pub use store::SqliteMemoryStore;
pub use threat_scan::{scan_threat, ThreatKind};
