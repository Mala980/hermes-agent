//! Tools Feishu/Lark: baca dokumen & kelola komentar Drive.
//!
//! Env: `FEISHU_APP_ID`, `FEISHU_APP_SECRET` (atau `LARK_APP_ID`/`LARK_APP_SECRET`).
//! Tools: feishu_doc_read, feishu_drive_list_comments, feishu_drive_add_comment,
//! feishu_drive_reply_comment, feishu_drive_list_comment_replies.
use crate::schema::*;
use async_trait::async_trait;
use hermes_core::{HermesError, Result, RiskLevel, Tool, ToolContext, ToolEffect, ToolResult};
use serde_json::{json, Value};

fn creds() -> Result<(String, String)> {
    let id = std::env::var("FEISHU_APP_ID")
        .or_else(|_| std::env::var("LARK_APP_ID"))
        .map_err(|_| HermesError::tool("feishu", "set FEISHU_APP_ID / FEISHU_APP_SECRET"))?;
    let secret = std::env::var("FEISHU_APP_SECRET")
        .or_else(|_| std::env::var("LARK_APP_SECRET"))
        .map_err(|_| HermesError::tool("feishu", "set FEISHU_APP_ID / FEISHU_APP_SECRET"))?;
    Ok((id, secret))
}

/// Base URL Feishu Open API; bisa di-override via FEISHU_API_BASE untuk pengujian.
fn api_base() -> String {
    std::env::var("FEISHU_API_BASE").unwrap_or_else(|_| "https://open.feishu.cn".to_string())
}

async fn tenant_token(client: &reqwest::Client) -> Result<String> {
    let (app_id, app_secret) = creds()?;
    let v: Value = client
        .post(format!(
            "{}/open-apis/auth/v3/tenant_access_token/internal",
            api_base()
        ))
        .json(&json!({"app_id": app_id, "app_secret": app_secret}))
        .send()
        .await
        .map_err(|e| HermesError::tool("feishu", format!("auth gagal: {e}")))?
        .json()
        .await
        .map_err(|e| HermesError::tool("feishu", format!("auth parse gagal: {e}")))?;
    if v.get("code").and_then(|c| c.as_i64()) != Some(0) {
        return Err(HermesError::tool("feishu", format!("auth ditolak: {v}")));
    }
    v.get("tenant_access_token")
        .and_then(|t| t.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| HermesError::tool("feishu", "tanpa token"))
}

fn check(v: &Value, what: &str) -> Result<()> {
    if v.get("code").and_then(|c| c.as_i64()) != Some(0) {
        return Err(HermesError::tool("feishu", format!("{what} gagal: {v}")));
    }
    Ok(())
}

fn ok(name: &str, content: String) -> ToolResult {
    ToolResult {
        tool_call_id: String::new(),
        name: name.to_string(),
        content,
        is_error: false,
    }
}

macro_rules! feishu_tool {
    ($name:ident, $tool_name:expr, $effect:expr, $desc:expr, $params:expr, $required:expr, $slf:ident, $args:ident, $impl:block) => {
        pub struct $name;
        #[async_trait]
        impl Tool for $name {
            fn name(&self) -> &str {
                $tool_name
            }
            fn effect(&self) -> ToolEffect {
                $effect
            }
            fn description(&self) -> &str {
                $desc
            }
            fn parameters_schema(&self) -> Value {
                object_schema($params, $required)
            }
            fn risk(&self) -> RiskLevel {
                RiskLevel::Low
            }
            async fn execute(&self, $args: Value, _ctx: &ToolContext) -> Result<ToolResult> {
                let $slf = self;
                $impl
            }
        }
    };
}

feishu_tool!(
    FeishuDocReadTool,
    "feishu_doc_read",
    ToolEffect::Read,
    "Baca isi mentah dokumen Feishu/Lark Docs (docx).",
    vec![("document_id", string_prop("ID dokumen (dari URL docs)."))],
    vec!["document_id"],
    slf,
    args,
    {
        let id = get_str(&args, "document_id")?;
        let client = reqwest::Client::new();
        let token = tenant_token(&client).await?;
        let v: Value = client
            .get(format!(
                "{}/open-apis/docx/v1/documents/{id}/raw_content",
                api_base()
            ))
            .bearer_auth(&token)
            .send()
            .await
            .map_err(|e| HermesError::tool("feishu", format!("request gagal: {e}")))?
            .json()
            .await
            .map_err(|e| HermesError::tool("feishu", format!("parse gagal: {e}")))?;
        check(&v, "baca dokumen")?;
        let content = v
            .pointer("/data/content")
            .and_then(|c| c.as_str())
            .unwrap_or("");
        Ok(ok(slf.name(), format!("Isi dokumen {id}:\n{content}")))
    }
);

feishu_tool!(
    FeishuDriveListCommentsTool,
    "feishu_drive_list_comments",
    ToolEffect::Read,
    "Daftar komentar pada file Feishu Drive.",
    vec![("file_token", string_prop("Token file Drive."))],
    vec!["file_token"],
    slf,
    args,
    {
        let ft = get_str(&args, "file_token")?;
        let client = reqwest::Client::new();
        let token = tenant_token(&client).await?;
        let v: Value = client
            .get(format!(
                "{}/open-apis/drive/v1/files/{ft}/comments",
                api_base()
            ))
            .bearer_auth(&token)
            .send()
            .await
            .map_err(|e| HermesError::tool("feishu", format!("request gagal: {e}")))?
            .json()
            .await
            .map_err(|e| HermesError::tool("feishu", format!("parse gagal: {e}")))?;
        check(&v, "daftar komentar")?;
        let items = v
            .pointer("/data/items")
            .and_then(|i| i.as_array())
            .cloned()
            .unwrap_or_default();
        if items.is_empty() {
            return Ok(ok(slf.name(), "Tidak ada komentar.".to_string()));
        }
        let mut lines = Vec::new();
        for it in items {
            let cid = it.get("comment_id").and_then(|c| c.as_str()).unwrap_or("?");
            let text = comment_text(it.get("content"));
            lines.push(format!("- [{cid}] {text}"));
        }
        Ok(ok(slf.name(), lines.join("\n")))
    }
);

feishu_tool!(
    FeishuDriveAddCommentTool,
    "feishu_drive_add_comment",
    ToolEffect::Write,
    "Tambah komentar teks pada file Feishu Drive.",
    vec![
        ("file_token", string_prop("Token file Drive.")),
        ("text", string_prop("Isi komentar.")),
    ],
    vec!["file_token", "text"],
    slf,
    args,
    {
        let ft = get_str(&args, "file_token")?;
        let text = get_str(&args, "text")?;
        let client = reqwest::Client::new();
        let token = tenant_token(&client).await?;
        let v: Value = client
            .post(format!("{}/open-apis/drive/v1/files/{ft}/comments", api_base()))
            .bearer_auth(&token)
            .json(&json!({"content": {"elements": [{"type": "text_run", "text_run": {"content": text}}]}}))
            .send()
            .await
            .map_err(|e| HermesError::tool("feishu", format!("request gagal: {e}")))?
            .json()
            .await
            .map_err(|e| HermesError::tool("feishu", format!("parse gagal: {e}")))?;
        check(&v, "tambah komentar")?;
        let cid = v
            .pointer("/data/comment/comment_id")
            .and_then(|c| c.as_str())
            .unwrap_or("?");
        Ok(ok(slf.name(), format!("Komentar terkirim (id {cid}).")))
    }
);

feishu_tool!(
    FeishuDriveReplyCommentTool,
    "feishu_drive_reply_comment",
    ToolEffect::Write,
    "Balas komentar pada file Feishu Drive.",
    vec![
        ("file_token", string_prop("Token file Drive.")),
        ("comment_id", string_prop("ID komentar yang dibalas.")),
        ("text", string_prop("Isi balasan.")),
    ],
    vec!["file_token", "comment_id", "text"],
    slf,
    args,
    {
        let ft = get_str(&args, "file_token")?;
        let cid = get_str(&args, "comment_id")?;
        let text = get_str(&args, "text")?;
        let client = reqwest::Client::new();
        let token = tenant_token(&client).await?;
        let v: Value = client
            .post(format!("{}/open-apis/drive/v1/files/{ft}/comments/{cid}/reply", api_base()))
            .bearer_auth(&token)
            .json(&json!({"content": {"elements": [{"type": "text_run", "text_run": {"content": text}}]}}))
            .send()
            .await
            .map_err(|e| HermesError::tool("feishu", format!("request gagal: {e}")))?
            .json()
            .await
            .map_err(|e| HermesError::tool("feishu", format!("parse gagal: {e}")))?;
        check(&v, "balas komentar")?;
        Ok(ok(slf.name(), "Balasan terkirim.".to_string()))
    }
);

feishu_tool!(
    FeishuDriveListCommentRepliesTool,
    "feishu_drive_list_comment_replies",
    ToolEffect::Read,
    "Daftar balasan pada sebuah komentar file Feishu Drive.",
    vec![
        ("file_token", string_prop("Token file Drive.")),
        ("comment_id", string_prop("ID komentar.")),
    ],
    vec!["file_token", "comment_id"],
    slf,
    args,
    {
        let ft = get_str(&args, "file_token")?;
        let cid = get_str(&args, "comment_id")?;
        let client = reqwest::Client::new();
        let token = tenant_token(&client).await?;
        let v: Value = client
            .get(format!(
                "{}/open-apis/drive/v1/files/{ft}/comments/{cid}/replies",
                api_base()
            ))
            .bearer_auth(&token)
            .send()
            .await
            .map_err(|e| HermesError::tool("feishu", format!("request gagal: {e}")))?
            .json()
            .await
            .map_err(|e| HermesError::tool("feishu", format!("parse gagal: {e}")))?;
        check(&v, "daftar balasan")?;
        let items = v
            .pointer("/data/items")
            .and_then(|i| i.as_array())
            .cloned()
            .unwrap_or_default();
        if items.is_empty() {
            return Ok(ok(slf.name(), "Tidak ada balasan.".to_string()));
        }
        let mut lines = Vec::new();
        for it in items {
            let rid = it.get("reply_id").and_then(|r| r.as_str()).unwrap_or("?");
            lines.push(format!("- [{rid}] {}", comment_text(it.get("content"))));
        }
        Ok(ok(slf.name(), lines.join("\n")))
    }
);

/// Ekstrak teks dari konten komentar Feishu (rich text elements).
fn comment_text(content: Option<&Value>) -> String {
    content
        .and_then(|c| c.get("elements"))
        .and_then(|e| e.as_array())
        .map(|els| {
            els.iter()
                .filter_map(|el| el.pointer("/text_run/content").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}
