//! hermes-core: fondasi port Rust dari Hermes Agent.
//!
//! Berisi tipe pesan, trait port (provider, tool, memory, skill, platform),
//! konfigurasi, agent loop, kompresi konteks, budget iterasi, dan event.

pub mod agent;
pub mod batch_segments;
pub mod budget;
pub mod cancel;
pub mod config;
pub mod context;
pub mod error;
pub mod events;
pub mod memory_ops;
pub mod ports;
pub mod retry;
pub mod tool_call_validation;
pub mod tools;
pub mod types;

pub use agent::{Agent, AgentDeps};
pub use budget::IterationBudget;
pub use cancel::CancelFlag;
pub use config::*;
pub use error::{HermesError, Result};
pub use events::{diff_stat_from_text, AgentEvent, DiffStat};
pub use memory_ops::{
    apply_memory_char_budget, locate_entry_index, splice_patch_span, truncated_dropped_note,
    MemoryBudgetReport, TRUNCATED_MARKER,
};
pub use ports::{LlmProvider, MemoryStore, Platform, SkillStore};
pub use retry::{
    backoff_base_delay, backoff_delay, classify_error, parse_retry_after, retry_after_of,
    retry_delay, with_retries, ErrorClass, BASE_DELAY, MAX_ATTEMPTS, MAX_DELAY, MAX_RETRY_AFTER,
    MAX_TOTAL_DELAY,
};
pub use tool_call_validation::{
    ToolCallValidator, ValidationError, DELEGATE_TASK_TOOL, MAX_DELEGATE_TASK_PER_TURN,
    MAX_INVALID_NAME_STRIKES,
};
pub use tools::{
    AllowAllApproval, ApprovalDecision, ApprovalHandler, DenyAllApproval, RiskLevel, Tool,
    ToolContext, ToolEffect, ToolRegistry,
};
pub use types::*;
