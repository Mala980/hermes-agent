use hermes_core::{HermesError, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{oneshot, Mutex};

/// Definisi tool dari server MCP.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpTool {
    pub name: String,
    pub description: Option<String>,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transport {
    Stdio,
    Http,
}

/// Klien MCP: JSON-RPC 2.0 lewat stdio atau HTTP.
pub struct McpClient {
    name: String,
    transport: Transport,
    // stdio state
    child: Option<Arc<Mutex<Child>>>,
    stdin: Option<Arc<Mutex<ChildStdin>>>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>,
    id_counter: AtomicU64,
    // http state
    http_url: Option<String>,
    http_headers: HashMap<String, String>,
    http_client: reqwest::Client,
}

impl McpClient {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Hubungkan via stdio: spawn `command` dan lakukan handshake initialize.
    pub async fn connect_stdio(
        name: impl Into<String>,
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
    ) -> Result<Self> {
        let name = name.into();
        let mut child = Command::new(command)
            .args(args)
            .envs(env)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| HermesError::Mcp(format!("gagal spawn `{command}`: {e}")))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| HermesError::Mcp("stdin tak tersedia".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| HermesError::Mcp("stdout tak tersedia".into()))?;

        let mut client = Self {
            name,
            transport: Transport::Stdio,
            child: None,
            stdin: Some(Arc::new(Mutex::new(stdin))),
            pending: Arc::new(Mutex::new(HashMap::new())),
            id_counter: AtomicU64::new(1),
            http_url: None,
            http_headers: HashMap::new(),
            http_client: reqwest::Client::new(),
        };
        let child = Arc::new(Mutex::new(child));
        client.child = Some(child.clone());
        client.spawn_reader(stdout);
        client.initialize().await?;
        Ok(client)
    }

    /// Hubungkan via HTTP (streamable HTTP / SSE POST).
    pub async fn connect_http(
        name: impl Into<String>,
        url: &str,
        headers: &HashMap<String, String>,
    ) -> Result<Self> {
        let mut client = Self {
            name: name.into(),
            transport: Transport::Http,
            child: None,
            stdin: None,
            pending: Arc::new(Mutex::new(HashMap::new())),
            id_counter: AtomicU64::new(1),
            http_url: Some(url.to_string()),
            http_headers: headers.clone(),
            http_client: reqwest::Client::new(),
        };
        client.initialize().await?;
        Ok(client)
    }

    fn spawn_reader(&self, stdout: ChildStdout) {
        let pending = self.pending.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let v: Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let id = v.get("id").and_then(|i| i.as_u64());
                if let Some(id) = id {
                    let tx = pending.lock().await.remove(&id);
                    if let Some(tx) = tx {
                        let res = if let Some(err) = v.get("error") {
                            Err(HermesError::Mcp(format!("rpc error: {err}")))
                        } else {
                            Ok(v.get("result").cloned().unwrap_or(Value::Null))
                        };
                        let _ = tx.send(res);
                    }
                }
            }
        });
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.id_counter.fetch_add(1, Ordering::SeqCst);
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        match self.transport {
            Transport::Stdio => {
                let (tx, rx) = oneshot::channel();
                self.pending.lock().await.insert(id, tx);
                let mut stdin = self.stdin.as_ref().unwrap().lock().await;
                let mut line = serde_json::to_string(&msg).unwrap();
                line.push('\n');
                stdin
                    .write_all(line.as_bytes())
                    .await
                    .map_err(|e| HermesError::Mcp(format!("tulis stdin gagal: {e}")))?;
                drop(stdin);
                tokio::time::timeout(std::time::Duration::from_secs(60), rx)
                    .await
                    .map_err(|_| HermesError::Mcp(format!("timeout menunggu respons `{method}`")))?
                    .map_err(|_| HermesError::Mcp("rpc channel terputus".into()))?
            }
            Transport::Http => {
                let url = self.http_url.as_ref().unwrap();
                let mut rb = self
                    .http_client
                    .post(url)
                    .header("Content-Type", "application/json")
                    .header("Accept", "application/json, text/event-stream");
                for (k, v) in &self.http_headers {
                    rb = rb.header(k.as_str(), v.as_str());
                }
                let resp = rb
                    .json(&msg)
                    .send()
                    .await
                    .map_err(|e| HermesError::Mcp(format!("http rpc gagal: {e}")))?;
                let text = resp
                    .text()
                    .await
                    .map_err(|e| HermesError::Mcp(e.to_string()))?;
                // Dukung respons JSON langsung maupun SSE (data: {...}).
                let payload = if text.trim_start().starts_with("data:") {
                    text.lines()
                        .filter_map(|l| l.strip_prefix("data:"))
                        .map(|l| l.trim())
                        .find(|l| !l.is_empty() && *l != "[DONE]")
                        .unwrap_or("{}")
                        .to_string()
                } else {
                    text
                };
                let v: Value = serde_json::from_str(&payload)
                    .map_err(|e| HermesError::Mcp(format!("rpc json invalid: {e}")))?;
                if let Some(err) = v.get("error") {
                    return Err(HermesError::Mcp(format!("rpc error: {err}")));
                }
                Ok(v.get("result").cloned().unwrap_or(Value::Null))
            }
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let msg = json!({"jsonrpc": "2.0", "method": method, "params": params});
        match self.transport {
            Transport::Stdio => {
                let mut stdin = self.stdin.as_ref().unwrap().lock().await;
                let mut line = serde_json::to_string(&msg).unwrap();
                line.push('\n');
                stdin
                    .write_all(line.as_bytes())
                    .await
                    .map_err(|e| HermesError::Mcp(format!("notify gagal: {e}")))?;
                Ok(())
            }
            Transport::Http => Ok(()), // notifikasi tak relevan untuk HTTP
        }
    }

    async fn initialize(&mut self) -> Result<()> {
        let result = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "roots": {}, "sampling": {} },
                    "clientInfo": { "name": "hermes-rs", "version": "0.1.0" },
                }),
            )
            .await?;
        let _ = result;
        self.notify("notifications/initialized", json!({})).await?;
        Ok(())
    }

    /// Daftar tool yang disediakan server.
    pub async fn list_tools(&self) -> Result<Vec<McpTool>> {
        let result = self.request("tools/list", json!({})).await?;
        let tools = result
            .get("tools")
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default();
        let mut out = Vec::new();
        for t in tools {
            let tool: McpTool = serde_json::from_value(t)
                .map_err(|e| HermesError::Mcp(format!("parse tool gagal: {e}")))?;
            out.push(tool);
        }
        Ok(out)
    }

    /// Panggil tool di server MCP.
    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<String> {
        let result = self
            .request("tools/call", json!({"name": name, "arguments": arguments}))
            .await?;
        // Ekstrak konten teks dari result.
        let mut texts = Vec::new();
        if let Some(content) = result.get("content").and_then(|c| c.as_array()) {
            for item in content {
                let t = item.get("type").and_then(|x| x.as_str()).unwrap_or("");
                match t {
                    "text" => {
                        if let Some(s) = item.get("text").and_then(|x| x.as_str()) {
                            texts.push(s.to_string());
                        }
                    }
                    "image" => {
                        texts.push("[gambar terlampir — tidak didukung di terminal]".to_string())
                    }
                    _ => texts.push(format!("[konten {t}]")),
                }
            }
        }
        let is_error = result
            .get("isError")
            .and_then(|b| b.as_bool())
            .unwrap_or(false);
        let body = if texts.is_empty() {
            serde_json::to_string_pretty(&result).unwrap_or_default()
        } else {
            texts.join("\n")
        };
        if is_error {
            return Err(HermesError::Mcp(format!("tool error: {body}")));
        }
        Ok(body)
    }

    /// Tutup koneksi (hentikan proses stdio).
    pub async fn close(&self) {
        if let Some(child) = &self.child {
            let mut c = child.lock().await;
            let _ = c.kill().await;
        }
        // Bangunkan semua pending dengan error agar tidak menggantung.
        let mut pending = self.pending.lock().await;
        for (_, tx) in pending.drain() {
            let _ = tx.send(Err(HermesError::Mcp("klien ditutup".into())));
        }
    }
}
