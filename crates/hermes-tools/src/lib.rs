//! hermes-tools: tool bawaan Hermes Agent.
//!
//! Meniru 60+ built-in tools pada Hermes asli: shell/exec (multi-backend),
//! filesystem, web, memori, skills, sesi, dan utilitas.

pub mod browser;
pub mod browser_vault;
pub mod feishu;
pub mod fs;
pub mod kanban;
pub mod media;
pub mod memory_tools;
pub mod misc;
pub mod patch;
pub mod processes;
pub mod productivity;
pub mod schema;
pub mod shell;
pub mod skill_tools;
pub mod social;
pub mod video;
pub mod vision;
pub mod web;

use hermes_core::ToolRegistry;

/// Daftarkan semua tool bawaan ke registry.
pub fn register_builtin(reg: &mut ToolRegistry) {
    // Shell / exec
    reg.register(shell::ExecTool);
    reg.register(misc::ExecuteCodeTool);
    // Filesystem
    reg.register(fs::ReadFileTool);
    reg.register(fs::WriteFileTool);
    reg.register(fs::EditFileTool);
    reg.register(fs::ListDirTool);
    reg.register(fs::GlobTool);
    reg.register(fs::GrepTool);
    // Web
    reg.register(web::WebFetchTool);
    reg.register(web::WebSearchTool);
    // Memori
    reg.register(memory_tools::MemoryRememberTool);
    reg.register(memory_tools::MemoryRecallTool);
    reg.register(memory_tools::MemoryForgetTool);
    reg.register(memory_tools::MemoryPatchTool);
    reg.register(memory_tools::SessionSearchTool);
    // Skills
    reg.register(skill_tools::SkillListTool);
    reg.register(skill_tools::SkillReadTool);
    reg.register(skill_tools::SkillCreateTool);
    reg.register(skill_tools::SkillImproveTool);
    reg.register(skill_tools::SkillDeleteTool);
    // Utilitas
    reg.register(misc::DatetimeNowTool);

    reg.register(media::ImageGenerateTool);
    reg.register(media::TextToSpeechTool);
    reg.register(media::TranscribeAudioTool);

    reg.register(productivity::TodoListTool);
    reg.register(productivity::ClarifyTool);
    reg.register(productivity::DelegateTaskTool);
    reg.register(processes::ProcessManageTool);
    reg.register(patch::PatchTool);
    reg.register(social::XSearchTool);
    reg.register(social::ReactToMessageTool);
    reg.register(video::VideoGenerateTool);
    reg.register(video::VideoAnalyzeTool);
    reg.register(vision::VisionAnalyzeTool);

    reg.register(browser::BrowserNavigateTool);
    reg.register(browser::BrowserSnapshotTool);
    reg.register(browser::BrowserClickTool);
    reg.register(browser::BrowserTypeTool);
    reg.register(browser::BrowserPressTool);
    reg.register(browser::BrowserScrollTool);
    reg.register(browser::BrowserBackTool);
    reg.register(browser::BrowserGetImagesTool);
    reg.register(browser::BrowserVisionTool);
    reg.register(browser::BrowserConsoleTool);
    reg.register(browser::BrowserDialogTool);
    reg.register(browser::BrowserExecTool);
    reg.register(browser::BrowserCdpTool);
    reg.register(browser_vault::BrowserVaultTool);

    reg.register(feishu::FeishuDocReadTool);
    reg.register(feishu::FeishuDriveListCommentsTool);
    reg.register(feishu::FeishuDriveAddCommentTool);
    reg.register(feishu::FeishuDriveReplyCommentTool);
    reg.register(feishu::FeishuDriveListCommentRepliesTool);

    // Kanban (Worker A: board CRUD + create + show; Worker B: query/lifecycle; Worker C: review/comment/attach/link)
    reg.register(kanban::tools::KanbanBoardCreateTool);
    reg.register(kanban::tools::KanbanBoardListTool);
    reg.register(kanban::tools::KanbanBoardRemoveTool);
    reg.register(kanban::tools::KanbanCreateTool);
    reg.register(kanban::tools::KanbanShowTool);
    kanban::tools_queries::register_query_tools(&mut *reg);
    kanban::tools_meta::register_meta_tools(&mut *reg);
}

/// Hasil eksekusi perintah.
pub(crate) struct CommandOutput {
    pub code: i32,
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Jalankan `tokio::process::Command` dengan timeout; aman terhadap hang
/// (proses dibunuh otomatis via kill_on_drop bila timeout).
pub(crate) async fn run_command(
    cmd: &mut tokio::process::Command,
    timeout_secs: u64,
    tool_name: &str,
) -> hermes_core::Result<CommandOutput> {
    use std::time::Duration;
    use tokio::io::AsyncReadExt;

    let mut child = cmd
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| hermes_core::HermesError::tool(tool_name, format!("gagal spawn: {e}")))?;
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();

    let work = async {
        let read_out = async {
            let mut buf = Vec::new();
            if let Some(s) = stdout.as_mut() {
                let _ = s.read_to_end(&mut buf).await;
            }
            buf
        };
        let read_err = async {
            let mut buf = Vec::new();
            if let Some(s) = stderr.as_mut() {
                let _ = s.read_to_end(&mut buf).await;
            }
            buf
        };
        let (out, err, status) = tokio::join!(read_out, read_err, child.wait());
        (out, err, status)
    };

    let (out, err, status) = tokio::time::timeout(Duration::from_secs(timeout_secs), work)
        .await
        .map_err(|_| {
            hermes_core::HermesError::tool(
                tool_name,
                format!("timeout setelah {timeout_secs} detik"),
            )
        })?;
    let status = status
        .map_err(|e| hermes_core::HermesError::tool(tool_name, format!("wait gagal: {e}")))?;
    Ok(CommandOutput {
        code: status.code().unwrap_or(-1),
        success: status.success(),
        stdout: String::from_utf8_lossy(&out).into_owned(),
        stderr: String::from_utf8_lossy(&err).into_owned(),
    })
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Stub `ToolContext` untuk unit test tool (tanpa I/O nyata selain fs).
    use hermes_core::{AllowAllApproval, CancelFlag, MemoryStore, SkillStore, ToolContext};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;

    struct StubMemory;
    #[async_trait::async_trait]
    impl MemoryStore for StubMemory {
        async fn remember(&self, _e: hermes_core::MemoryEntry) -> hermes_core::Result<()> {
            Ok(())
        }
        async fn recall(
            &self,
            _q: &str,
            _l: usize,
        ) -> hermes_core::Result<Vec<hermes_core::MemoryEntry>> {
            Ok(vec![])
        }
        async fn forget(&self, _q: &str) -> hermes_core::Result<usize> {
            Ok(0)
        }
        async fn list_memories(
            &self,
            _l: usize,
        ) -> hermes_core::Result<Vec<hermes_core::MemoryEntry>> {
            Ok(vec![])
        }
        async fn save_session(&self, _s: &hermes_core::Session) -> hermes_core::Result<()> {
            Ok(())
        }
        async fn load_session(
            &self,
            _id: &str,
        ) -> hermes_core::Result<Option<hermes_core::Session>> {
            Ok(None)
        }
        async fn list_sessions(
            &self,
            _p: &str,
            _l: usize,
        ) -> hermes_core::Result<Vec<hermes_core::Session>> {
            Ok(vec![])
        }
        async fn delete_session(&self, _id: &str) -> hermes_core::Result<()> {
            Ok(())
        }
        async fn search_sessions(
            &self,
            _q: &str,
            _l: usize,
        ) -> hermes_core::Result<Vec<hermes_core::Session>> {
            Ok(vec![])
        }
    }

    struct StubSkills;
    #[async_trait::async_trait]
    impl SkillStore for StubSkills {
        async fn list(&self) -> hermes_core::Result<Vec<hermes_core::Skill>> {
            Ok(vec![])
        }
        async fn get(&self, _n: &str) -> hermes_core::Result<Option<hermes_core::Skill>> {
            Ok(None)
        }
        async fn create(
            &self,
            n: &str,
            d: &str,
            b: &str,
        ) -> hermes_core::Result<hermes_core::Skill> {
            Ok(hermes_core::Skill {
                name: n.into(),
                description: d.into(),
                body: b.into(),
                path: Default::default(),
                triggers: vec![],
            })
        }
        async fn update(&self, _n: &str, _b: &str) -> hermes_core::Result<()> {
            Ok(())
        }
        async fn delete(&self, _n: &str) -> hermes_core::Result<()> {
            Ok(())
        }
        async fn find_relevant(
            &self,
            _t: &str,
            _l: usize,
        ) -> hermes_core::Result<Vec<hermes_core::Skill>> {
            Ok(vec![])
        }
    }

    /// `ToolContext` uji dengan `working_dir` sesuai argumen.
    pub(crate) fn test_ctx(working_dir: PathBuf) -> ToolContext {
        ToolContext {
            session_cwd: Arc::new(std::sync::Mutex::new(working_dir.clone())),
            working_dir,
            approval: Arc::new(AllowAllApproval),
            memory: Arc::new(StubMemory),
            skills: Arc::new(StubSkills),
            cancel: CancelFlag::new(),
            env: HashMap::new(),
            allow_outside_workdir: true,
            provider: None,
            model: "test".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hermes_core::ToolEffect;

    fn effect_of(name: &str) -> ToolEffect {
        let mut reg = ToolRegistry::new();
        register_builtin(&mut reg);
        reg.effect_of(name)
    }

    /// Sanity klasifikasi effect (audit W1-B4): pure reader = Read, semua
    /// yang mengubah state (atau tak diketahui) = Write.
    #[test]
    fn klasifikasi_effect_tool_bawaan() {
        let readers = [
            "read_file",
            "list_dir",
            "glob",
            "grep",
            "web_fetch",
            "web_search",
            "memory_recall",
            "session_search",
            "skill_list",
            "skill_read",
            "datetime_now",
            "transcribe_audio",
            "video_analyze",
            "vision_analyze",
            "x_search",
            "feishu_doc_read",
            "feishu_drive_list_comments",
            "feishu_drive_list_comment_replies",
            "kanban_board_list",
            "kanban_show",
        ];
        for name in readers {
            assert_eq!(
                effect_of(name),
                ToolEffect::Read,
                "{name} harus diklasifikasikan Read"
            );
        }
        let writers = [
            "write_file",
            "edit_file",
            "patch",
            "exec",
            "execute_code",
            "memory_remember",
            "memory_forget",
            "skill_create",
            "skill_improve",
            "skill_delete",
            "todo_list",
            "clarify",
            "delegate_task",
            "process_manage",
            "image_generate",
            "text_to_speech",
            "video_generate",
            "react_to_message",
            "browser_navigate",
            "browser_snapshot",
            "browser_click",
            "browser_type",
            "browser_press",
            "browser_scroll",
            "browser_back",
            "browser_get_images",
            "browser_vision",
            "browser_console",
            "browser_dialog",
            "browser_exec",
            "browser_cdp",
            "browser_vault",
            "feishu_drive_add_comment",
            "feishu_drive_reply_comment",
            "kanban_board_create",
            "kanban_board_remove",
            "kanban_create",
        ];
        for name in writers {
            assert_eq!(
                effect_of(name),
                ToolEffect::Write,
                "{name} harus diklasifikasikan Write"
            );
        }
        // Tool tak dikenal → Write konservatif (barrier sekuensial).
        assert_eq!(effect_of("ghost_tool"), ToolEffect::Write);
    }
}
