//! hermes-skills: procedural memory Hermes (SKILL.md).
//!
//! Meniru skills system aslinya: skill disimpan sebagai direktori berisi
//! SKILL.md dengan frontmatter (name, description), dapat dibuat/diubah oleh
//! learning loop maupun pengguna, dan kompatibel dengan format agentskills.io.

pub mod manager;

pub use manager::SkillManager;
