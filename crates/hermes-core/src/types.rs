use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Peran sebuah pesan dalam percakapan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// Satu blok konten di dalam pesan.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolCall {
        call: ToolCall,
    },
    ToolResult {
        result: ToolResult,
    },
    /// Gambar untuk model vision: data URL (data:image/...;base64,...) atau URL http(s).
    Image {
        data_url: String,
    },
}

/// Permintaan pemanggilan tool dari model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

/// Hasil eksekusi sebuah tool call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub tool_call_id: String,
    pub name: String,
    pub content: String,
    pub is_error: bool,
}

/// Satu pesan percakapan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl Message {
    pub fn system(text: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: vec![ContentBlock::Text { text: text.into() }],
            name: None,
        }
    }

    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::Text { text: text.into() }],
            name: None,
        }
    }

    pub fn assistant_text(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: text.into() }],
            name: None,
        }
    }

    /// Gabungkan semua blok teks menjadi satu string.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    /// Teks untuk estimasi token: mencakup blok teks, argumen tool call
    /// (JSON-serialized), dan isi tool result — meniru heuristik aslinya
    /// yang menjumlahkan estimasi per pesan atas SELURUH daftar pesan
    /// (`estimate_messages_tokens_rough`). Gambar dikecualikan agar payload
    /// base64 tidak membengkakkan estimasi secara artifisial.
    pub fn text_for_estimation(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.clone()),
                ContentBlock::ToolCall { call } => serde_json::to_string(&call.arguments).ok(),
                ContentBlock::ToolResult { result } => Some(result.content.clone()),
                ContentBlock::Image { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    /// Render pesan untuk prompt ringkasan (kompaksi konteks): menyertakan
    /// argumen tool call dan isi tool result dengan marker, agar fakta yang
    /// berasal dari output tool ikut terlihat oleh summarizer — meniru
    /// `_serialize_records_for_summary` aslinya.
    pub fn render_for_summary(&self) -> String {
        self.content
            .iter()
            .map(|b| match b {
                ContentBlock::Text { text } => text.clone(),
                ContentBlock::ToolCall { call } => format!(
                    "[tool {} args]: {}",
                    call.name,
                    serde_json::to_string(&call.arguments).unwrap_or_else(|_| "{}".to_string())
                ),
                ContentBlock::ToolResult { result } => format!(
                    "[tool {} result{}]: {}",
                    result.name,
                    if result.is_error { " (error)" } else { "" },
                    result.content
                ),
                ContentBlock::Image { .. } => "[image]".to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Estimasi kasar jumlah token (chars/4), seperti heuristik umum.
    pub fn estimated_tokens(&self) -> usize {
        self.text_for_estimation().len() / 4 + 4
    }
}

/// Definisi tool dalam format JSON Schema untuk provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Permintaan chat ke provider LLM.
#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    /// Upaya reasoning yang diminta untuk turn ini (`None` = default provider).
    /// Diisi agen dari `ProfileConfig::reasoning_effort` (yang bisa di-override
    /// flag CLI `--reasoning` untuk sekali jalan). Transport memetakannya ke
    /// parameter wire masing-masing (paritas kosakata `EFFORT_LADDER` di
    /// `agent/reasoning_effort.py` original).
    pub reasoning_effort: Option<String>,
}

/// Tangga effort reasoning kanonis, rendah → tinggi (paritas `EFFORT_LADDER`
/// di `agent/reasoning_effort.py` original; `none` = thinking dimatikan).
pub const REASONING_EFFORTS: &[&str] = &[
    "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
];

/// Anggaran token thinking untuk transport yang butuh budget numerik
/// (Anthropic `thinking.budget_tokens`, Gemini `thinkingConfig.thinkingBudget`).
/// `none` (dan level tak dikenal) → `None` = thinking tidak diaktifkan.
pub fn thinking_budget_for_effort(level: &str) -> Option<u64> {
    match level {
        "minimal" => Some(512),
        "low" => Some(1024),
        "medium" => Some(2048),
        "high" => Some(4096),
        "xhigh" => Some(8192),
        "max" => Some(16384),
        "ultra" => Some(32768),
        _ => None,
    }
}

/// Event streaming dari provider.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    TextDelta(String),
    ToolCallDelta {
        index: usize,
        id: Option<String>,
        name: Option<String>,
        arguments_delta: String,
    },
    Done(Usage),
    Error(String),
}

/// Pemakaian token dari satu completion.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Token input yang dibaca dari prompt cache (nama wire Anthropic
    /// `cache_read_input_tokens`; paritas `extract_cache_stats` di
    /// `agent/transports/anthropic.py` original).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
    /// Token input yang dipakai menulis entri prompt cache (nama wire
    /// Anthropic `cache_creation_input_tokens`; paritas original yang sama).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
}

impl Usage {
    pub fn total(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }
}

/// Satu sesi percakapan yang tersimpan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub title: String,
    pub profile: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub messages: Vec<Message>,
    /// Byte system prompt yang dibekukan saat sesi pertama dibangun.
    /// Sesi lanjutan memakai ulang nilai ini apa adanya (bukan membangun
    /// ulang) agar prefix cache provider tetap hangat — paritas dengan
    /// kolom `system_prompt` di session DB original.
    pub system_prompt: Option<String>,
}

impl Session {
    pub fn new(profile: impl Into<String>) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4().to_string(),
            title: "Untitled".to_string(),
            profile: profile.into(),
            created_at: now,
            updated_at: now,
            messages: Vec::new(),
            system_prompt: None,
        }
    }
}

/// Target penyimpanan memori: `memory` = catatan agen (MEMORY.md di
/// original), `user` = profil pengguna (USER.md di original). Paritas
/// `target` di `tools/memory_tool.py` aslinya: dua store terpisah dengan
/// anggaran dan blok injeksi system-prompt masing-masing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum MemoryTarget {
    /// Catatan agen (default, sama seperti default `target="memory"` aslinya).
    #[default]
    Memory,
    /// Profil pengguna.
    User,
}

impl std::fmt::Display for MemoryTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MemoryTarget::Memory => write!(f, "memory"),
            MemoryTarget::User => write!(f, "user"),
        }
    }
}

impl std::str::FromStr for MemoryTarget {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "memory" => Ok(MemoryTarget::Memory),
            "user" => Ok(MemoryTarget::User),
            other => Err(format!(
                "target memori tidak dikenal: '{other}' (gunakan 'memory' atau 'user')"
            )),
        }
    }
}

/// Satu entri memori persisten.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub id: String,
    pub kind: String,
    pub text: String,
    pub importance: f32,
    pub created_at: DateTime<Utc>,
    /// Store pemilik entri (`memory`/`user`). Default `memory` agar data lama
    /// (sebelum kolom ini ada) tetap terbaca sebagai catatan agen.
    #[serde(default)]
    pub target: MemoryTarget,
}

impl MemoryEntry {
    pub fn new(kind: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            kind: kind.into(),
            text: text.into(),
            importance: 0.5,
            created_at: Utc::now(),
            target: MemoryTarget::Memory,
        }
    }

    /// Varian builder untuk menulis ke target `user` (profil pengguna).
    pub fn with_target(mut self, target: MemoryTarget) -> Self {
        self.target = target;
        self
    }
}

/// Satu skill (procedural memory) ala agentskills.io / SKILL.md.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: std::path::PathBuf,
    pub body: String,
    #[serde(default)]
    pub triggers: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_call_message() -> Message {
        Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "Saya baca dulu.".to_string(),
                },
                ContentBlock::ToolCall {
                    call: ToolCall {
                        id: "call_1".to_string(),
                        name: "read_file".to_string(),
                        arguments: serde_json::json!({"path": "/tmp/data.txt"}),
                    },
                },
            ],
            name: None,
        }
    }

    fn tool_result_message(content: &str, is_error: bool) -> Message {
        Message {
            role: Role::Tool,
            content: vec![ContentBlock::ToolResult {
                result: ToolResult {
                    tool_call_id: "call_1".to_string(),
                    name: "read_file".to_string(),
                    content: content.to_string(),
                    is_error,
                },
            }],
            name: None,
        }
    }

    #[test]
    fn estimated_tokens_counts_tool_result_content() {
        let content = "kunci api: ABC-123-XYZ";
        let msg = tool_result_message(content, false);
        assert_eq!(msg.estimated_tokens(), content.len() / 4 + 4);
        // Tanpa isi tool result, estimasinya tinggal overhead per pesan.
        let empty = tool_result_message("", false);
        assert_eq!(empty.estimated_tokens(), 4);
        assert!(msg.estimated_tokens() > empty.estimated_tokens());
    }

    #[test]
    fn estimated_tokens_counts_tool_call_arguments() {
        let msg = tool_call_message();
        let args = serde_json::to_string(&serde_json::json!({"path": "/tmp/data.txt"})).unwrap();
        let expected = "Saya baca dulu.".len() + args.len();
        assert_eq!(msg.estimated_tokens(), expected / 4 + 4);
    }

    #[test]
    fn text_contract_stays_text_only() {
        // text() tetap hanya menggabungkan blok teks — pemanggil lain
        // (judul sesi, FTS, preview pencarian) tidak berubah perilakunya.
        let msg = tool_result_message("hasil tool", false);
        assert_eq!(msg.text(), "");
        let msg2 = tool_call_message();
        assert_eq!(msg2.text(), "Saya baca dulu.");
    }

    #[test]
    fn render_for_summary_marks_tool_blocks() {
        let call = tool_call_message().render_for_summary();
        assert!(
            call.contains("[tool read_file args]:"),
            "args marker hilang:\n{call}"
        );
        assert!(call.contains("/tmp/data.txt"), "isi args hilang:\n{call}");
        assert!(call.contains("Saya baca dulu."), "teks hilang:\n{call}");

        let ok = tool_result_message("isi file penting", false).render_for_summary();
        assert!(
            ok.contains("[tool read_file result]:"),
            "result marker hilang:\n{ok}"
        );
        assert!(ok.contains("isi file penting"), "isi result hilang:\n{ok}");

        let err = tool_result_message("gagal", true).render_for_summary();
        assert!(
            err.contains("[tool read_file result (error)]:"),
            "marker error hilang:\n{err}"
        );
    }
}
