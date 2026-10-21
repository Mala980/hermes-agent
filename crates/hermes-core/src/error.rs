use thiserror::Error;

#[derive(Debug, Error)]
pub enum HermesError {
    #[error("provider error: {0}")]
    Provider(String),
    /// Error HTTP dari provider LLM, dengan info klasifikasi retry.
    #[error("provider error: HTTP {status}: {message}")]
    ProviderHttp {
        status: u16,
        retry_after: Option<std::time::Duration>,
        message: String,
    },
    /// Kegagalan transport (timeout / koneksi) sebelum respons HTTP tiba.
    #[error("provider transport error: {message}")]
    ProviderTransport { timeout: bool, message: String },
    #[error("tool error [{tool}]: {message}")]
    Tool { tool: String, message: String },
    #[error("config error: {0}")]
    Config(String),
    #[error("memory error: {0}")]
    Memory(String),
    #[error("skill error: {0}")]
    Skill(String),
    #[error("mcp error: {0}")]
    Mcp(String),
    #[error("gateway error: {0}")]
    Gateway(String),
    #[error("cron error: {0}")]
    Cron(String),
    #[error("approval denied for tool `{0}`")]
    ApprovalDenied(String),
    #[error("interrupted")]
    Interrupted,
    #[error("iteration budget exhausted ({0} iterations)")]
    BudgetExhausted(u32),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, HermesError>;

impl HermesError {
    pub fn tool<S: Into<String>, M: Into<String>>(tool: S, message: M) -> Self {
        HermesError::Tool {
            tool: tool.into(),
            message: message.into(),
        }
    }

    /// Error HTTP dari provider, lengkap dengan `Retry-After` bila ada.
    pub fn http<S: Into<String>>(
        status: u16,
        retry_after: Option<std::time::Duration>,
        message: S,
    ) -> Self {
        HermesError::ProviderHttp {
            status,
            retry_after,
            message: message.into(),
        }
    }

    /// Kegagalan transport (mis. timeout atau koneksi putus) sebelum respons.
    pub fn transport<S: Into<String>>(timeout: bool, message: S) -> Self {
        HermesError::ProviderTransport {
            timeout,
            message: message.into(),
        }
    }
}
