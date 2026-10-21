//! Suite browser via Chrome DevTools Protocol (CDP).
//!
//! Butuh Chrome/Chromium berjalan dengan remote debugging, mis.
//! `chromium --headless --remote-debugging-port=9222`.
//! Env: `BROWSER_CDP_URL` (ws://.../devtools/page/...) atau otomatis
//! discovery via `BROWSER_CDP_HTTP` (default http://127.0.0.1:9222).
//!
//! Tools: browser_navigate, browser_snapshot, browser_click, browser_type,
//! browser_press, browser_scroll, browser_back, browser_get_images,
//! browser_vision, browser_console, browser_dialog, browser_exec, browser_cdp.
use crate::schema::*;
use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use hermes_core::{HermesError, Result, RiskLevel, Tool, ToolContext, ToolEffect, ToolResult};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message as WsMessage;

// ---------------------------------------------------------------------------
// Klien CDP
// ---------------------------------------------------------------------------

pub(crate) struct CdpClient {
    tx: mpsc::UnboundedSender<WsMessage>,
    pending: Arc<StdMutex<HashMap<u64, oneshot::Sender<Value>>>>,
    next_id: AtomicU64,
}

impl CdpClient {
    async fn connect() -> Result<Arc<Self>> {
        let ws_url = resolve_ws_url().await?;
        let (stream, _) = tokio_tungstenite::connect_async(&ws_url)
            .await
            .map_err(|e| HermesError::tool("browser", format!("CDP connect gagal: {e}")))?;
        let (mut sink, mut stream) = stream.split();
        let (tx, mut rx) = mpsc::unbounded_channel::<WsMessage>();
        let pending: Arc<StdMutex<HashMap<u64, oneshot::Sender<Value>>>> =
            Arc::new(StdMutex::new(HashMap::new()));
        let pending_r = pending.clone();
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if sink.send(msg).await.is_err() {
                    break;
                }
            }
        });
        tokio::spawn(async move {
            while let Some(msg) = stream.next().await {
                let Ok(WsMessage::Text(text)) = msg else {
                    continue;
                };
                let v: Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if let Some(id) = v.get("id").and_then(|i| i.as_u64()) {
                    if let Some(tx) = pending_r.lock().unwrap().remove(&id) {
                        let _ = tx.send(v);
                    }
                }
            }
        });
        let client = Arc::new(Self {
            tx,
            pending,
            next_id: AtomicU64::new(1),
        });
        // Aktifkan domain yang umum dipakai.
        for (method, params) in [
            ("Page.enable", json!({})),
            ("Runtime.enable", json!({})),
            ("Log.enable", json!({})),
        ] {
            let _ = client.call(method, params).await;
        }
        Ok(client)
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let msg = json!({"id": id, "method": method, "params": params}).to_string();
        self.tx
            .send(WsMessage::Text(msg.into()))
            .map_err(|e| HermesError::tool("browser", format!("CDP send gagal: {e}")))?;
        let resp = tokio::time::timeout(std::time::Duration::from_secs(60), rx)
            .await
            .map_err(|_| HermesError::tool("browser", "CDP timeout"))?
            .map_err(|_| HermesError::tool("browser", "CDP respons hilang"))?;
        if let Some(err) = resp.get("error") {
            return Err(HermesError::tool("browser", format!("CDP error: {err}")));
        }
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }

    pub(crate) async fn eval(&self, js: &str) -> Result<Value> {
        let r = self
            .call(
                "Runtime.evaluate",
                json!({"expression": js, "returnByValue": true, "awaitPromise": true}),
            )
            .await?;
        let rr = r.get("result").unwrap_or(&Value::Null);
        if let Some(exc) = rr.get("exceptionDetails") {
            return Err(HermesError::tool("browser", format!("JS error: {exc}")));
        }
        Ok(rr.get("value").cloned().unwrap_or(Value::Null))
    }
}

async fn resolve_ws_url() -> Result<String> {
    if let Ok(url) = std::env::var("BROWSER_CDP_URL") {
        if !url.is_empty() {
            return Ok(url);
        }
    }
    let http =
        std::env::var("BROWSER_CDP_HTTP").unwrap_or_else(|_| "http://127.0.0.1:9222".to_string());
    let targets: Value = reqwest::get(format!("{http}/json/list"))
        .await
        .map_err(|_| {
            HermesError::tool(
                "browser",
                "tidak ada Chrome CDP di 127.0.0.1:9222 — jalankan chromium dengan --remote-debugging-port=9222 atau set BROWSER_CDP_URL",
            )
        })?
        .json()
        .await
        .map_err(|e| HermesError::tool("browser", format!("CDP discovery gagal: {e}")))?;
    targets
        .as_array()
        .and_then(|a| {
            a.iter()
                .find(|t| t.get("type").and_then(|x| x.as_str()) == Some("page"))
        })
        .and_then(|t| t.get("webSocketDebuggerUrl").and_then(|u| u.as_str()))
        .map(|s| s.to_string())
        .ok_or_else(|| HermesError::tool("browser", "tidak ada tab page di target CDP"))
}

// Klien per-proses (dibuat ulang tiap panggilan tool; murah untuk localhost).
// pub(crate) agar tool browser_vault bisa memakai WebSocket CDP yang sama
// untuk fill rahasia (tidak pernah lewat argv).
pub(crate) async fn cdp() -> Result<Arc<CdpClient>> {
    CdpClient::connect().await
}

// ---------------------------------------------------------------------------
// Helper hasil
// ---------------------------------------------------------------------------

fn ok(name: &str, content: String) -> ToolResult {
    ToolResult {
        tool_call_id: String::new(),
        name: name.to_string(),
        content,
        is_error: false,
    }
}

const SNAPSHOT_JS: &str = r#"
(() => {
  const els = document.querySelectorAll('a, button, input, select, textarea, [role="button"]');
  const out = [];
  let i = 0;
  for (const el of els) {
    if (i >= 100) break;
    const r = el.getBoundingClientRect();
    if (r.width === 0 && r.height === 0) continue;
    const text = (el.innerText || el.value || el.getAttribute('aria-label') || el.getAttribute('placeholder') || '').trim().slice(0, 80);
    out.push({index: i, tag: el.tagName.toLowerCase(), type: el.getAttribute('type') || '', text, name: el.getAttribute('name') || '', id: el.id || ''});
    i++;
  }
  return {url: location.href, title: document.title, interactive: out,
          text: document.body ? document.body.innerText.slice(0, 4000) : ''};
})()"#;

// ---------------------------------------------------------------------------
// Tool-tool browser
// ---------------------------------------------------------------------------

macro_rules! browser_tool {
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
                RiskLevel::Medium
            }
            async fn execute(&self, $args: Value, _ctx: &ToolContext) -> Result<ToolResult> {
                let $slf = self;
                $impl
            }
        }
    };
}

async fn current_url(c: &CdpClient) -> String {
    c.eval("location.href")
        .await
        .ok()
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .unwrap_or_default()
}

browser_tool!(
    BrowserNavigateTool,
    "browser_navigate",
    ToolEffect::Write,
    "Navigasi tab browser ke URL.",
    vec![("url", string_prop("URL tujuan."))],
    vec!["url"],
    slf,
    args,
    {
        let url = get_str(&args, "url")?;
        let c = cdp().await?;
        c.call("Page.navigate", json!({"url": url})).await?;
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        Ok(ok(slf.name(), format!("Navigasi ke {url}.")))
    }
);

browser_tool!(
    BrowserSnapshotTool,
    "browser_snapshot",
    ToolEffect::Write,
    "Ambil snapshot halaman: URL, judul, elemen interaktif (dengan index), dan teks.",
    vec![],
    vec![],
    slf,
    _args,
    {
        let c = cdp().await?;
        let v = c.eval(SNAPSHOT_JS).await?;
        let mut out = format!(
            "URL: {}\nJudul: {}\n\n",
            v.get("url").and_then(|u| u.as_str()).unwrap_or("?"),
            v.get("title").and_then(|t| t.as_str()).unwrap_or("?")
        );
        if let Some(els) = v.get("interactive").and_then(|e| e.as_array()) {
            for el in els {
                out.push_str(&format!(
                    "[{}] {} {} \"{}\"\n",
                    el.get("index").and_then(|i| i.as_u64()).unwrap_or(0),
                    el.get("tag").and_then(|t| t.as_str()).unwrap_or("?"),
                    el.get("type").and_then(|t| t.as_str()).unwrap_or(""),
                    el.get("text").and_then(|t| t.as_str()).unwrap_or("")
                ));
            }
        }
        out.push_str(&format!(
            "\nTeks:\n{}",
            v.get("text").and_then(|t| t.as_str()).unwrap_or("")
        ));
        Ok(ok(slf.name(), out))
    }
);

/// Selector dari argumen: index numerik → nth interactive, atau CSS selector.
fn selector_js(args: &Value) -> Result<String> {
    if let Some(idx) = args.get("index").and_then(|i| i.as_u64()) {
        Ok(format!(
            r#"(() => {{ const els = [...document.querySelectorAll('a, button, input, select, textarea, [role="button"]')].filter(e => {{ const r = e.getBoundingClientRect(); return r.width > 0 && r.height > 0; }}); return els[{idx}] || null; }})()"#
        ))
    } else if let Some(sel) = args.get("selector").and_then(|s| s.as_str()) {
        let esc = sel.replace('\\', "\\\\").replace('\'', "\\'");
        Ok(format!("document.querySelector('{esc}')"))
    } else {
        Err(HermesError::tool(
            "browser",
            "butuh `index` atau `selector`",
        ))
    }
}

browser_tool!(
    BrowserClickTool,
    "browser_click",
    ToolEffect::Write,
    "Klik elemen (via index dari snapshot atau CSS selector).",
    vec![
        ("index", integer_prop("Index elemen dari browser_snapshot.")),
        ("selector", string_prop("CSS selector alternatif.")),
    ],
    vec![],
    slf,
    args,
    {
        let sel = selector_js(&args)?;
        let c = cdp().await?;
        let v = c.eval(&format!("(() => {{ const el = {sel}; if (!el) return 'NOT_FOUND'; el.scrollIntoView({{block:'center'}}); el.click(); return 'OK'; }})()")).await?;
        Ok(ok(
            slf.name(),
            format!("Klik: {}", v.as_str().unwrap_or("?")),
        ))
    }
);

browser_tool!(
    BrowserTypeTool,
    "browser_type",
    ToolEffect::Write,
    "Ketik teks ke input/textarea (via index atau selector).",
    vec![
        ("index", integer_prop("Index elemen dari browser_snapshot.")),
        ("selector", string_prop("CSS selector alternatif.")),
        ("text", string_prop("Teks yang diketik.")),
        (
            "submit",
            boolean_prop("Tekan Enter setelah mengetik (default false).")
        ),
    ],
    vec!["text"],
    slf,
    args,
    {
        let sel = selector_js(&args)?;
        let text = get_str(&args, "text")?
            .replace('\\', "\\\\")
            .replace('\'', "\\'")
            .replace('\n', "\\n");
        let submit = opt_bool(&args, "submit").unwrap_or(false);
        let c = cdp().await?;
        let js = format!(
            r#"(() => {{
            const el = {sel}; if (!el) return 'NOT_FOUND';
            el.focus();
            document.execCommand('selectAll', false, null);
            document.execCommand('insertText', false, '{text}');
            el.dispatchEvent(new Event('input', {{bubbles: true}}));
            el.dispatchEvent(new Event('change', {{bubbles: true}}));
            {submit_js}
            return 'OK';
        }})()"#,
            submit_js = if submit {
                "el.dispatchEvent(new KeyboardEvent('keydown', {key: 'Enter', keyCode: 13, bubbles: true}));"
            } else {
                ""
            }
        );
        let v = c.eval(&js).await?;
        Ok(ok(
            slf.name(),
            format!("Ketik: {}", v.as_str().unwrap_or("?")),
        ))
    }
);

browser_tool!(
    BrowserPressTool,
    "browser_press",
    ToolEffect::Write,
    "Tekan tombol keyboard (Enter, Tab, Escape, ArrowDown, ...).",
    vec![("key", string_prop("Nama tombol, mis. Enter."))],
    vec!["key"],
    slf,
    args,
    {
        let key = get_str(&args, "key")?;
        let c = cdp().await?;
        // Via Input domain bila tersedia, fallback ke JS.
        let r = c
            .call(
                "Input.dispatchKeyEvent",
                json!({"type": "keyDown", "key": key}),
            )
            .await;
        if r.is_ok() {
            let _ = c
                .call(
                    "Input.dispatchKeyEvent",
                    json!({"type": "keyUp", "key": key}),
                )
                .await;
            Ok(ok(slf.name(), format!("Tombol {key} ditekan.")))
        } else {
            let esc = key.replace('\'', "\\'");
            c.eval(&format!("document.dispatchEvent(new KeyboardEvent('keydown', {{key: '{esc}', bubbles: true}}))")).await?;
            Ok(ok(slf.name(), format!("Tombol {key} dikirim via JS.")))
        }
    }
);

browser_tool!(
    BrowserScrollTool,
    "browser_scroll",
    ToolEffect::Write,
    "Scroll halaman (up/down/top/bottom atau jumlah px).",
    vec![
        (
            "direction",
            string_prop("up | down | top | bottom (default down).")
        ),
        ("pixels", integer_prop("Jumlah px (default 600).")),
    ],
    vec![],
    slf,
    args,
    {
        let dir = opt_str(&args, "direction").unwrap_or("down");
        let px = opt_u64(&args, "pixels").unwrap_or(600);
        let c = cdp().await?;
        let js = match dir {
            "top" => "window.scrollTo(0, 0)".to_string(),
            "bottom" => "window.scrollTo(0, document.body.scrollHeight)".to_string(),
            "up" => format!("window.scrollBy(0, -{px})"),
            _ => format!("window.scrollBy(0, {px})"),
        };
        c.eval(&js).await?;
        Ok(ok(slf.name(), format!("Scroll {dir}.")))
    }
);

browser_tool!(
    BrowserBackTool,
    "browser_back",
    ToolEffect::Write,
    "Kembali ke halaman sebelumnya (history back).",
    vec![],
    vec![],
    slf,
    _args,
    {
        let c = cdp().await?;
        c.eval("history.back()").await?;
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        Ok(ok(
            slf.name(),
            format!("Kembali. URL kini: {}", current_url(&c).await),
        ))
    }
);

browser_tool!(
    BrowserGetImagesTool,
    "browser_get_images",
    ToolEffect::Write,
    "Daftar URL gambar di halaman.",
    vec![],
    vec![],
    slf,
    _args,
    {
        let c = cdp().await?;
        let v = c
            .eval("([...document.images].map(i => i.src).filter(s => s)).slice(0, 50)")
            .await?;
        let list: Vec<String> = v
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        Ok(ok(
            slf.name(),
            if list.is_empty() {
                "Tidak ada gambar.".to_string()
            } else {
                list.join("\n")
            },
        ))
    }
);

browser_tool!(
    BrowserVisionTool,
    "browser_vision",
    ToolEffect::Write,
    "Ambil screenshot halaman (PNG) dan simpan ke file; kembalikan path.",
    vec![(
        "output",
        string_prop("Path file PNG (default /tmp/hermes-shot.png).")
    )],
    vec![],
    slf,
    args,
    {
        let c = cdp().await?;
        let r = c
            .call("Page.captureScreenshot", json!({"format": "png"}))
            .await?;
        let b64 = r
            .get("data")
            .and_then(|d| d.as_str())
            .ok_or_else(|| HermesError::tool("browser", "screenshot gagal"))?;
        let bytes = base64_decode(b64).map_err(|e| HermesError::tool("browser", e))?;
        let path = opt_str(&args, "output")
            .unwrap_or("/tmp/hermes-shot.png")
            .to_string();
        std::fs::write(&path, &bytes)
            .map_err(|e| HermesError::tool("browser", format!("tulis gagal: {e}")))?;
        Ok(ok(slf.name(), format!("Screenshot tersimpan: {path} ({bytes_len} byte). Pakai vision_analyze untuk menganalisisnya.", bytes_len = bytes.len())))
    }
);

browser_tool!(
    BrowserConsoleTool,
    "browser_console",
    ToolEffect::Write,
    "Ambil log console terakhir dari halaman.",
    vec![("limit", integer_prop("Maks entri (default 50)."))],
    vec![],
    slf,
    args,
    {
        let limit = opt_u64(&args, "limit").unwrap_or(50);
        let c = cdp().await?;
        // Ambil via Log domain tidak menyimpan histori; pakai buffer JS bila ada,
        // fallback: evaluasi ulang tidak mungkin — kembalikan panduan.
        let v = c
            .eval(&format!(
                "((window.__hermesConsoleLog || []).slice(-{limit}).join('\\n')) || ''"
            ))
            .await?;
        let text = v.as_str().unwrap_or("");
        Ok(ok(
            slf.name(),
            if text.is_empty() {
                "Buffer console kosong. (Hermes meng-instrumentasi console pada sesi browser_tool asli; di port ini gunakan browser_exec untuk membaca state JS.)".to_string()
            } else {
                text.to_string()
            },
        ))
    }
);

browser_tool!(
    BrowserDialogTool,
    "browser_dialog",
    ToolEffect::Write,
    "Tangani dialog JS (alert/confirm/prompt): accept atau dismiss.",
    vec![
        ("action", string_prop("accept | dismiss.")),
        ("prompt_text", string_prop("Teks untuk prompt().")),
    ],
    vec!["action"],
    slf,
    args,
    {
        let action = get_str(&args, "action")?;
        let c = cdp().await?;
        let mut params = json!({"accept": action == "accept"});
        if let Some(t) = opt_str(&args, "prompt_text") {
            params["promptText"] = Value::String(t.to_string());
        }
        // handleJavaScriptDialog hanya valid saat dialog terbuka; error ditoleransi.
        match c.call("Page.handleJavaScriptDialog", params).await {
            Ok(_) => Ok(ok(slf.name(), format!("Dialog di-{action}."))),
            Err(e) => Ok(ok(slf.name(), format!("Tidak ada dialog terbuka ({e})."))),
        }
    }
);

browser_tool!(
    BrowserExecTool,
    "browser_exec",
    ToolEffect::Write,
    "Jalankan JavaScript di halaman dan kembalikan hasilnya (JSON).",
    vec![("script", string_prop("Kode JS; nilai return dikembalikan."))],
    vec!["script"],
    slf,
    args,
    {
        let script = get_str(&args, "script")?;
        let c = cdp().await?;
        let v = c.eval(script).await?;
        let s = serde_json::to_string_pretty(&v).unwrap_or_default();
        Ok(ok(
            slf.name(),
            format!("Hasil:\n{}", truncate_str(&s, 8000)),
        ))
    }
);

browser_tool!(
    BrowserCdpTool,
    "browser_cdp",
    ToolEffect::Write,
    "Panggil metode CDP mentah (escape hatch).",
    vec![
        (
            "method",
            string_prop("Nama metode CDP, mis. Page.navigate.")
        ),
        (
            "params",
            serde_json::json!({"type": "object", "description": "Parameter JSON."})
        ),
    ],
    vec!["method"],
    slf,
    args,
    {
        let method = get_str(&args, "method")?;
        let params = args.get("params").cloned().unwrap_or(json!({}));
        let c = cdp().await?;
        let r = c.call(method, params).await?;
        let s = serde_json::to_string_pretty(&r).unwrap_or_default();
        Ok(ok(slf.name(), truncate_str(&s, 8000)))
    }
);

fn truncate_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… [dipotong]", &s[..end])
}

fn base64_decode(s: &str) -> std::result::Result<Vec<u8>, String> {
    const TBL: &[u8; 256] = &{
        let mut t = [255u8; 256];
        let a = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut i = 0;
        while i < 64 {
            t[a[i] as usize] = i as u8;
            i += 1;
        }
        t
    };
    let clean: Vec<u8> = s
        .bytes()
        .filter(|&b| b != b'=' && !b.is_ascii_whitespace())
        .collect();
    let mut out = Vec::with_capacity(clean.len() * 3 / 4);
    for ch in clean.chunks(4) {
        let mut n: u32 = 0;
        let mut valid = 0;
        for &b in ch {
            let v = TBL[b as usize];
            if v == 255 {
                return Err("karakter base64 tidak valid".into());
            }
            n = (n << 6) | v as u32;
            valid += 1;
        }
        let pad = 4 - valid;
        n <<= 6 * pad;
        out.push((n >> 16) as u8);
        if valid > 2 {
            out.push((n >> 8) as u8);
        }
        if valid > 3 {
            out.push(n as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Mock server CDP: jawab method yang dipakai klien dengan respons kalengan.
    async fn mock_cdp_server() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            use futures::StreamExt;
            while let Some(msg) = ws.next().await {
                let Ok(WsMessage::Text(text)) = msg else {
                    continue;
                };
                let v: Value = serde_json::from_str(&text).unwrap();
                let id = v.get("id").cloned().unwrap_or(Value::Null);
                let method = v.get("method").and_then(|m| m.as_str()).unwrap_or("");
                let result = match method {
                    "Page.enable" | "Runtime.enable" | "Log.enable" => json!({}),
                    "Page.navigate" => json!({"frameId": "FRAME1"}),
                    "Runtime.evaluate" => {
                        let expr = v
                            .pointer("/params/expression")
                            .and_then(|e| e.as_str())
                            .unwrap_or("");
                        if expr.contains("location.href") {
                            json!({"result": {"type": "string", "value": "https://example.com/page"}})
                        } else {
                            json!({"result": {"type": "string", "value": "EVAL_OK"}})
                        }
                    }
                    "Input.dispatchKeyEvent" => json!({}),
                    "Page.captureScreenshot" => json!({"data": "aGk="}), // "hi"
                    _ => json!({}),
                };
                let resp = json!({"id": id, "result": result}).to_string();
                use futures::SinkExt;
                ws.send(WsMessage::Text(resp.into())).await.unwrap();
            }
        });
        format!("ws://{addr}")
    }

    #[tokio::test]
    async fn cdp_client_protocol_roundtrip() {
        let url = mock_cdp_server().await;
        std::env::set_var("BROWSER_CDP_URL", &url);
        let c = cdp().await.expect("connect");
        let r = c
            .call("Page.navigate", json!({"url": "https://example.com"}))
            .await
            .expect("navigate");
        assert_eq!(r.get("frameId").and_then(|f| f.as_str()), Some("FRAME1"));
        let v = c.eval("location.href").await.expect("eval");
        assert_eq!(v.as_str(), Some("https://example.com/page"));
        std::env::remove_var("BROWSER_CDP_URL");
    }

    #[tokio::test]
    async fn browser_tool_names_registered() {
        let mut reg = crate::ToolRegistry::new();
        crate::register_builtin(&mut reg);
        for name in [
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
        ] {
            assert!(reg.get(name).is_some(), "tool {name} tidak terdaftar");
        }
    }
}

#[cfg(test)]
mod real_browser_tests {
    use super::*;
    use hermes_core::ToolContext;

    struct StubMemory;
    #[async_trait::async_trait]
    impl hermes_core::MemoryStore for StubMemory {
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
    impl hermes_core::SkillStore for StubSkills {
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

    fn test_ctx() -> ToolContext {
        let working_dir = std::env::temp_dir();
        ToolContext {
            working_dir: working_dir.clone(),
            session_cwd: std::sync::Arc::new(std::sync::Mutex::new(working_dir)),
            approval: std::sync::Arc::new(hermes_core::AllowAllApproval),
            memory: std::sync::Arc::new(StubMemory),
            skills: std::sync::Arc::new(StubSkills),
            cancel: hermes_core::CancelFlag::new(),
            env: Default::default(),
            allow_outside_workdir: true,
            provider: None,
            model: "test".into(),
        }
    }

    /// Test terhadap Chrome nyata dengan halaman lokal (tanpa internet).
    /// Jalankan: HERMES_TEST_REAL_BROWSER=1 cargo test -p hermes-tools real_browser
    /// (Chrome headless harus jalan di :9222)
    #[tokio::test]
    async fn real_browser_navigate_snapshot_exec() {
        if std::env::var("HERMES_TEST_REAL_BROWSER").is_err() {
            return;
        }
        let page = concat!(
            "<!doctype html><html><head><title>Hermes Test Page</title></head><body>",
            "<h1>Halo Hermes</h1><p id=\"msg\">belum diklik</p>",
            "<button id=\"btn\" onclick=\"document.getElementById('msg').textContent='sudah diklik'\">Klik</button>",
            "<img src=\"data:image/png;base64,iVBORw0KGgo=\" alt=\"pixel\">",
            "</body></html>"
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let page = page.to_string();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let page = page.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = vec![0u8; 4096];
                    let _ = sock.read(&mut buf).await;
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        page.len()
                    );
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(page.as_bytes()).await;
                });
            }
        });

        let ctx = test_ctx();
        let url = format!("http://127.0.0.1:{port}/");
        let nav = BrowserNavigateTool;
        let r = nav
            .execute(serde_json::json!({"url": url}), &ctx)
            .await
            .expect("navigate");
        assert!(!r.is_error, "navigate error: {}", r.content);

        let snap = BrowserSnapshotTool;
        let r = snap
            .execute(serde_json::json!({}), &ctx)
            .await
            .expect("snapshot");
        assert!(
            r.content.contains("Hermes Test Page"),
            "snapshot: {}",
            &r.content[..300.min(r.content.len())]
        );
        assert!(
            r.content.contains("Halo Hermes"),
            "snapshot teks: {}",
            &r.content[..600.min(r.content.len())]
        );

        let exec = BrowserExecTool;
        let r = exec
            .execute(serde_json::json!({"script": "document.title"}), &ctx)
            .await
            .expect("exec");
        assert!(
            r.content.contains("Hermes Test Page"),
            "exec: {}",
            r.content
        );

        let click = BrowserClickTool;
        let r = click
            .execute(serde_json::json!({"selector": "#btn"}), &ctx)
            .await
            .expect("click");
        assert!(!r.is_error, "click error: {}", r.content);
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let r = exec
            .execute(
                serde_json::json!({"script": "document.getElementById('msg').textContent"}),
                &ctx,
            )
            .await
            .expect("exec2");
        assert!(
            r.content.contains("sudah diklik"),
            "klik tidak berefek: {}",
            r.content
        );

        let imgs = BrowserGetImagesTool;
        let r = imgs
            .execute(serde_json::json!({}), &ctx)
            .await
            .expect("images");
        assert!(!r.is_error, "images error: {}", r.content);
        assert!(
            r.content.contains("data:image/png"),
            "images: {}",
            r.content
        );

        let shot = BrowserVisionTool;
        let r = shot
            .execute(
                serde_json::json!({"output": "/tmp/hermes-test-shot.png"}),
                &ctx,
            )
            .await
            .expect("vision");
        assert!(!r.is_error, "vision error: {}", r.content);
        let meta = std::fs::metadata("/tmp/hermes-test-shot.png").expect("screenshot file");
        assert!(
            meta.len() > 1000,
            "screenshot terlalu kecil: {}",
            meta.len()
        );
    }
}
