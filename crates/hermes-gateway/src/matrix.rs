//! Platform Matrix via Client-Server API (murni HTTP, tanpa SDK).
//!
//! Butuh: `MATRIX_HOMESERVER` (mis. https://matrix.org), `MATRIX_ACCESS_TOKEN`,
//! dan `MATRIX_USER_ID` (mis. @bot:matrix.org). Kirim via PUT
//! `/rooms/{roomId}/send/m.room.message`; terima via `/sync` long-poll.
use async_trait::async_trait;
use hermes_core::{HermesConfig, HermesError, Platform, Result};
use serde_json::{json, Value};
use std::future::Future;
use std::path::Path;

use crate::allowlist::{env_truthy, read_allow_csv};
use crate::split::matrix_message_limit;

pub struct MatrixPlatform {
    homeserver: String,
    access_token: String,
    client: reqwest::Client,
}

impl MatrixPlatform {
    pub fn new(homeserver: impl Into<String>, access_token: impl Into<String>) -> Self {
        Self {
            homeserver: homeserver.into().trim_end_matches('/').to_string(),
            access_token: access_token.into(),
            client: reqwest::Client::new(),
        }
    }

    fn auth(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        rb.bearer_auth(&self.access_token)
    }
}

#[derive(Debug, Clone)]
pub struct MatrixMessage {
    pub room_id: String,
    pub sender: String,
    pub body: String,
    pub event_id: String,
}

#[async_trait]
impl Platform for MatrixPlatform {
    fn id(&self) -> &str {
        "matrix"
    }

    async fn send_message(&self, target: &str, text: &str) -> Result<()> {
        // target: room id (mis. !abc:matrix.org), atau "roomId|eventId"
        // untuk membalas event tertentu (relasi m.in_reply_to).
        let (room_id, reply_to) = match target.split_once('|') {
            Some((r, e)) if !e.is_empty() => (r, Some(e)),
            _ => (target, None),
        };
        // Batas 15000 byte UTF-8 (default; override via
        // MATRIX_MAX_MESSAGE_LENGTH) — satuan yang dipakai homeserver
        // menghitung kapasitas event. Pecah sadar-pagar.
        let limit = matrix_message_limit();
        for chunk in crate::split::split_message_with_len(text, Some(limit), crate::split::utf8_len)
        {
            let mut content = build_text_content(&chunk);
            // Threading balasan: setiap chunk membawa relasi — cermin
            // `_apply_relation_metadata` aslinya (dipanggil per chunk).
            if let Some(eid) = reply_to {
                content["m.relates_to"] = json!({ "m.in_reply_to": { "event_id": eid } });
            }
            // Anggaran event 45_000 byte: bila HTML membuat konten
            // kebesaran, kirim sebagai teks polos saja (cermin aslinya).
            if content_bytes(&content) > MATRIX_MAX_CONTENT_BYTES {
                if let Some(obj) = content.as_object_mut() {
                    obj.remove("format");
                    obj.remove("formatted_body");
                }
            }
            // txn unik per chunk agar server tidak menganggapnya duplikat.
            let txn = format!(
                "hermes-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or(0)
            );
            let url = format!(
                "{}/_matrix/client/v3/rooms/{}/send/m.room.message/{txn}",
                self.homeserver,
                url_encode(room_id),
            );
            // Bungkus sebagai closure agar request identik bisa dikirim ulang
            // bila API menjawab 429 (lihat crate::rate_limit).
            let send = || async {
                self.auth(self.client.put(&url))
                    .json(&content)
                    .send()
                    .await
                    .map_err(|e| HermesError::Gateway(format!("matrix send gagal: {e}")))
            };
            let resp = send().await?;
            // 429 → tidur sesuai Retry-After (cap 60 dtk), retry sekali.
            let resp = crate::rate_limit::handle_429("matrix", resp, send).await?;
            if !resp.status().is_success() {
                let b = resp.text().await.unwrap_or_default();
                return Err(HermesError::Gateway(format!("matrix menolak pesan: {b}")));
            }
        }
        Ok(())
    }
}

fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"!_.~".contains(&b) || b == b':' {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Anggaran byte konten event Matrix: homeserver menolak event yang JSON-nya
/// melebihi 65.536 byte; konten dibatasi 45.000 agar envelope + inflasi
/// base64 E2EE tetap muat — cermin `_MAX_CONTENT_BYTES` aslinya.
pub const MATRIX_MAX_CONTENT_BYTES: usize = 45_000;

/// Ukuran konten event seperti yang dihitung homeserver: JSON kanonis
/// UTF-8 tanpa escape `\u` — cermin `_content_bytes` aslinya.
pub fn content_bytes(content: &Value) -> usize {
    serde_json::to_string(content)
        .map(|s| s.len())
        .unwrap_or(usize::MAX)
}

/// Tag HTML yang diizinkan di `formatted_body` — cermin `_ALLOWED_TAGS`
/// pada `_MatrixHtmlSanitizer` aslinya.
const MATRIX_ALLOWED_TAGS: &[&str] = &[
    "a",
    "b",
    "blockquote",
    "br",
    "code",
    "del",
    "em",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "hr",
    "i",
    "li",
    "ol",
    "p",
    "pre",
    "s",
    "strike",
    "strong",
    "table",
    "tbody",
    "td",
    "th",
    "thead",
    "tr",
    "ul",
];

/// Escape teks untuk HTML.
fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

/// Apakah URL aman untuk atribut `href` (skema http/https/matrix/mailto,
/// atau relatif) — cermin `_safe_url` aslinya.
fn matrix_safe_url(value: &str) -> Option<String> {
    let stripped: String = value
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .to_string();
    if let Some(colon) = stripped.find(':') {
        let scheme = stripped[..colon].to_ascii_lowercase();
        let valid_scheme = !scheme.is_empty()
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c));
        if valid_scheme && !["http", "https", "matrix", "mailto"].contains(&scheme.as_str()) {
            return None;
        }
    }
    Some(stripped)
}

/// Atribut yang dipertahankan: `href` aman pada `<a>`, `class="language-*"`
/// pada `<code>` — cermin `_safe_attrs` aslinya.
fn matrix_safe_attrs(tag: &str, attrs: &[(String, String)]) -> String {
    let mut out = String::new();
    for (key, value) in attrs {
        let attr = key.to_ascii_lowercase();
        if tag == "a" && attr == "href" {
            if let Some(href) = matrix_safe_url(value) {
                if !href.is_empty() {
                    out.push_str(&format!(" href=\"{}\"", escape_html(&href)));
                }
            }
        } else if tag == "code" && attr == "class" {
            let ok = value.len() <= "language-".len() + 64
                && value.starts_with("language-")
                && value["language-".len()..]
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "_+.-".contains(c));
            if ok {
                out.push_str(&format!(" class=\"{}\"", escape_html(value)));
            }
        }
    }
    out
}

/// Urai `<tag attr="v">` / `</tag>` menjadi (is_close, nama, atribut).
fn parse_html_tag(inner: &str) -> (bool, String, Vec<(String, String)>) {
    let inner = inner.trim();
    let (is_close, rest) = match inner.strip_prefix('/') {
        Some(r) => (true, r),
        None => (false, inner),
    };
    let rest = rest.trim_end_matches('/');
    let name_end = rest
        .find(|c: char| c.is_whitespace() || c == '/')
        .unwrap_or(rest.len());
    let name = rest[..name_end].to_string();
    let mut attrs = Vec::new();
    let mut r = rest[name_end..].trim();
    while !r.is_empty() {
        let key_end = r
            .find(|c: char| c.is_whitespace() || c == '=' || c == '/')
            .unwrap_or(r.len());
        if key_end == 0 {
            break;
        }
        let key = r[..key_end].to_string();
        r = r[key_end..].trim_start();
        let mut value = String::new();
        if let Some(after_eq) = r.strip_prefix('=') {
            r = after_eq.trim_start();
            if let Some(q) = r.chars().next() {
                if q == '"' || q == '\'' {
                    if let Some(end) = r[1..].find(q) {
                        value = r[1..1 + end].to_string();
                        r = r[1 + end + 1..].trim_start();
                    } else {
                        r = "";
                    }
                } else {
                    let v_end = r
                        .find(|c: char| c.is_whitespace() || c == '>')
                        .unwrap_or(r.len());
                    value = r[..v_end].to_string();
                    r = r[v_end..].trim_start();
                }
            }
        }
        attrs.push((key, value));
        r = r.trim_start_matches('/');
        r = r.trim_start();
    }
    (is_close, name, attrs)
}

/// Sanitasi HTML ke subset tag yang diterima Matrix — cermin
/// `_sanitize_matrix_html` aslinya: tag di luar allowlist dibuang (isinya
/// dipertahankan), konten `<script>`/`<style>` dibuang seluruhnya.
pub fn sanitize_matrix_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let bytes = html.as_bytes();
    let mut i = 0;
    let mut skip_depth: u32 = 0;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            let start = i;
            while i < bytes.len() && bytes[i] != b'<' {
                i += 1;
            }
            // Teks dari pulldown-cmark sudah di-escape; teruskan apa adanya.
            if skip_depth == 0 {
                out.push_str(&html[start..i]);
            }
            continue;
        }
        // Komentar HTML dibuang.
        if html[i..].starts_with("<!--") {
            match html[i..].find("-->") {
                Some(end) => i += end + 3,
                None => i = bytes.len(),
            }
            continue;
        }
        // Cari '>' penutup dengan memperhatikan atribut ber-quote.
        let mut j = i + 1;
        let mut in_quote: Option<u8> = None;
        while j < bytes.len() {
            let b = bytes[j];
            if let Some(q) = in_quote {
                if b == q {
                    in_quote = None;
                }
            } else if b == b'"' || b == b'\'' {
                in_quote = Some(b);
            } else if b == b'>' {
                break;
            }
            j += 1;
        }
        if j >= bytes.len() {
            break; // tag tak tertutup — buang sisanya
        }
        let (is_close, name, attrs) = parse_html_tag(&html[i + 1..j]);
        i = j + 1;
        let lname = name.to_ascii_lowercase();
        if lname == "script" || lname == "style" {
            if is_close {
                skip_depth = skip_depth.saturating_sub(1);
            } else {
                skip_depth += 1;
            }
            continue;
        }
        if skip_depth > 0 {
            continue;
        }
        if !MATRIX_ALLOWED_TAGS.contains(&lname.as_str()) {
            continue; // buang tag, pertahankan isi
        }
        if is_close {
            if lname == "br" || lname == "hr" {
                continue;
            }
            out.push_str("</");
            out.push_str(&lname);
            out.push('>');
        } else {
            out.push('<');
            out.push_str(&lname);
            out.push_str(&matrix_safe_attrs(&lname, &attrs));
            out.push('>');
        }
    }
    out
}

/// Konversi markdown menjadi HTML untuk `formatted_body`; kembalikan `None`
/// bila HTML-nya sama dengan teks polos (tak perlu format) — cermin
/// `_build_text_message_content` aslinya (`if html and html != text`).
pub fn markdown_to_matrix_html(text: &str) -> Option<String> {
    use pulldown_cmark::{html, Options, Parser};
    if text.trim().is_empty() {
        return None;
    }
    let opts = Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS;
    let parser = Parser::new_ext(text, opts);
    let mut rendered = String::new();
    html::push_html(&mut rendered, parser);
    let rendered = rendered.trim().to_string();
    if rendered.is_empty() || rendered == text.trim() {
        return None;
    }
    Some(sanitize_matrix_html(&rendered))
}

/// Bangun konten event `m.room.message` untuk satu chunk: `body` polos +
/// `formatted_body` HTML bila markdown-nya me-render berbeda — cermin
/// `_build_text_message_content` aslinya.
pub fn build_text_content(chunk: &str) -> Value {
    let mut content = json!({"msgtype": "m.text", "body": chunk});
    if let Some(html) = markdown_to_matrix_html(chunk) {
        content["format"] = json!("org.matrix.custom.html");
        content["formatted_body"] = json!(html);
    }
    content
}

/// Daftar ID pengguna Matrix yang diizinkan: gabungan (union)
/// `MATRIX_ALLOWED_USERS` dan `GATEWAY_ALLOWED_USERS` (dipisah koma;
/// `"*"` = semua pengguna). Mengembalikan `None` bila keduanya kosong →
/// semua pengguna ditolak (fail-closed).
pub fn matrix_allowlist() -> Option<std::collections::HashSet<String>> {
    let mut ids = read_allow_csv("MATRIX_ALLOWED_USERS");
    ids.extend(read_allow_csv("GATEWAY_ALLOWED_USERS"));
    if ids.is_empty() {
        None
    } else {
        Some(ids)
    }
}

/// Saklar "izinkan semua": `MATRIX_ALLOW_ALL_USERS` (khusus dev) atau
/// `GATEWAY_ALLOW_ALL_USERS` bernilai true/1/yes.
pub fn matrix_allow_all() -> bool {
    env_truthy("MATRIX_ALLOW_ALL_USERS") || env_truthy("GATEWAY_ALLOW_ALL_USERS")
}

/// Keputusan izin murni (mudah diunit-tes): `user_id` adalah Matrix user ID
/// (mis. `@user:server`). Urutan: allow-all dulu, lalu allowlist, default tolak.
pub fn is_matrix_user_allowed(
    user_id: &str,
    allowlist: &Option<std::collections::HashSet<String>>,
) -> bool {
    if matrix_allow_all() {
        return true;
    }
    match allowlist {
        None => false,
        Some(set) => set.contains("*") || set.contains(user_id),
    }
}

/// Path file persistensi token sync Matrix (`next_batch`) di data dir Hermes
/// (`HERMES_HOME`, default `~/.hermes`) — konvensi yang sama dipakai
/// `memory.db`, `config.toml`, dsb.
pub fn matrix_since_path() -> std::path::PathBuf {
    HermesConfig::home_dir().join("matrix_since.json")
}

/// Muat token sync tersimpan; `None` bila file hilang atau rusak
/// (gateway mulai tanpa token → sync awal penuh seperti biasa).
fn load_since_token(path: &Path) -> Option<String> {
    let raw = std::fs::read(path).ok()?;
    let v: Value = serde_json::from_slice(&raw).ok()?;
    v.get("next_batch")?.as_str().map(|s| s.to_string())
}

/// Simpan token sync setelah setiap sync sukses. Kegagalan tulis hanya dicatat
/// (warn) — loop sync tidak boleh mati karenanya.
fn save_since_token(path: &Path, token: &str) {
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            tracing::warn!("matrix: gagal membuat dir data: {e}");
            return;
        }
    }
    let payload = json!({"next_batch": token});
    if let Err(e) = std::fs::write(path, payload.to_string()) {
        tracing::warn!("matrix: gagal menyimpan token sync: {e}");
    }
}

/// Loop /sync: terima pesan teks baru → handler → kirim balasan ke room.
pub async fn serve_matrix<F, Fut>(
    homeserver: String,
    access_token: String,
    user_id: String,
    mut handler: F,
) -> Result<()>
where
    F: FnMut(MatrixMessage) -> Fut,
    Fut: Future<Output = Result<String>>,
{
    let platform = MatrixPlatform::new(&homeserver, &access_token);
    let client = reqwest::Client::new();
    let token_path = matrix_since_path();
    let mut since: Option<String> = load_since_token(&token_path);
    if since.is_some() {
        tracing::info!("matrix: lanjutkan sync dari token tersimpan");
    }
    let allowlist = matrix_allowlist();
    if matrix_allow_all() {
        tracing::warn!(
            "allowlist matrix: DINONAKTIFKAN via *_ALLOW_ALL_USERS (semua pengguna diizinkan)"
        );
    }
    match &allowlist {
        Some(set) if set.contains("*") => {
            tracing::info!("allowlist matrix: semua pengguna diizinkan (*)")
        }
        Some(set) => tracing::info!("allowlist matrix: {} pengguna diizinkan", set.len()),
        None => tracing::warn!(
            "MATRIX_ALLOWED_USERS tidak diset: semua pesan masuk akan ditolak (fail-closed)"
        ),
    }

    // Sinkronisasi awal tanpa timeout untuk dapat batch token.
    tracing::info!("matrix: sinkronisasi awal...");
    loop {
        let mut url = format!("{homeserver}/_matrix/client/v3/sync?timeout=30000");
        if let Some(s) = &since {
            url.push_str(&format!("&since={}", url_encode(s)));
        }
        let resp = client
            .get(&url)
            .bearer_auth(&access_token)
            .send()
            .await
            .map_err(|e| HermesError::Gateway(format!("matrix sync gagal: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let b = resp.text().await.unwrap_or_default();
            // Token since basi → reset.
            if b.contains("M_UNKNOWN_TOKEN") {
                return Err(HermesError::Gateway(
                    "matrix access token tidak valid".into(),
                ));
            }
            tracing::warn!("matrix sync HTTP {status}: {b}");
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            continue;
        }
        let v: Value = resp
            .json()
            .await
            .map_err(|e| HermesError::Gateway(format!("matrix parse gagal: {e}")))?;
        since = v
            .get("next_batch")
            .and_then(|n| n.as_str())
            .map(|s| s.to_string());
        // Persistensi per sync sukses (seperti `sync_store.put_next_batch`
        // aslinya): restart tidak mengulang riwayat / membalas ulang pesan lama.
        if let Some(s) = &since {
            save_since_token(&token_path, s);
        }

        let rooms = v
            .pointer("/rooms/join")
            .and_then(|r| r.as_object())
            .cloned()
            .unwrap_or_default();
        for (room_id, room) in rooms {
            let events = room
                .pointer("/timeline/events")
                .and_then(|e| e.as_array())
                .cloned()
                .unwrap_or_default();
            for ev in events {
                if ev.get("type").and_then(|t| t.as_str()) != Some("m.room.message") {
                    continue;
                }
                let sender = ev.get("sender").and_then(|s| s.as_str()).unwrap_or("");
                if sender == user_id {
                    continue; // abaikan pesan sendiri
                }
                let content = ev.get("content").unwrap_or(&Value::Null);
                if content.get("msgtype").and_then(|m| m.as_str()) != Some("m.text") {
                    continue;
                }
                let body = content
                    .get("body")
                    .and_then(|b| b.as_str())
                    .unwrap_or("")
                    .to_string();
                if body.trim().is_empty() {
                    continue;
                }
                let mm = MatrixMessage {
                    room_id: room_id.clone(),
                    sender: sender.to_string(),
                    body,
                    event_id: ev
                        .get("event_id")
                        .and_then(|e| e.as_str())
                        .unwrap_or("")
                        .to_string(),
                };
                // Token `since`/`next_batch` sudah dimajukan per iterasi sync,
                // jadi pesan yang dilewati di sini tetap ter-ack.
                if !is_matrix_user_allowed(&mm.sender, &allowlist) {
                    tracing::warn!(sender = %mm.sender, "pesan matrix dari pengirim tak diizinkan, dilewati");
                    continue;
                }
                tracing::info!(room = %mm.room_id, sender = %mm.sender, "pesan matrix masuk");
                // Balas dengan relasi m.in_reply_to ke event pemicu
                // (cermin `_apply_relation_metadata` aslinya).
                let target = if mm.event_id.is_empty() {
                    mm.room_id.clone()
                } else {
                    format!("{}|{}", mm.room_id, mm.event_id)
                };
                match handler(mm.clone()).await {
                    Ok(reply) => {
                        if let Err(e) = platform.send_message(&target, &reply).await {
                            tracing::warn!("gagal membalas matrix: {e}");
                        }
                    }
                    Err(e) => {
                        let _ = platform
                            .send_message(&target, &format!("Maaf, terjadi error: {e}"))
                            .await;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::allowlist::{clear_auth_env, restore_auth_env, set_var, ENV_LOCK};

    const AUTH_VARS: [&str; 4] = [
        "MATRIX_ALLOWED_USERS",
        "MATRIX_ALLOW_ALL_USERS",
        "GATEWAY_ALLOWED_USERS",
        "GATEWAY_ALLOW_ALL_USERS",
    ];

    #[test]
    fn allowlist_kosong_fail_closed() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = clear_auth_env(&AUTH_VARS);
        assert!(matrix_allowlist().is_none());
        assert!(!matrix_allow_all());
        assert!(!is_matrix_user_allowed("@user:server", &matrix_allowlist()));
        restore_auth_env(saved);
    }

    #[test]
    fn allowlist_parse_koma_dan_spasi() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = clear_auth_env(&AUTH_VARS);
        set_var("MATRIX_ALLOWED_USERS", Some("  @a:server , @b:server,,"));
        let allow = Some(matrix_allowlist().expect("harus ter-parse"));
        assert_eq!(allow.as_ref().unwrap().len(), 2);
        assert!(is_matrix_user_allowed("@a:server", &allow));
        assert!(is_matrix_user_allowed("@b:server", &allow));
        assert!(!is_matrix_user_allowed("@c:server", &allow));
        restore_auth_env(saved);
    }

    #[test]
    fn allowlist_wildcard_buka_semua() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = clear_auth_env(&AUTH_VARS);
        set_var("MATRIX_ALLOWED_USERS", Some("*"));
        let allow = Some(matrix_allowlist().expect("harus ter-parse"));
        assert!(is_matrix_user_allowed("@siapa:saja", &allow));
        restore_auth_env(saved);
    }

    #[test]
    fn allowlist_id_tak_dikenal_ditolak() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = clear_auth_env(&AUTH_VARS);
        set_var("MATRIX_ALLOWED_USERS", Some("@a:server"));
        let allow = Some(matrix_allowlist().expect("harus ter-parse"));
        assert!(!is_matrix_user_allowed("@x:server", &allow));
        assert!(!is_matrix_user_allowed("", &allow));
        restore_auth_env(saved);
    }

    #[test]
    fn allowlist_gateway_users_gabungan() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = clear_auth_env(&AUTH_VARS);
        set_var("MATRIX_ALLOWED_USERS", Some("@a:server"));
        set_var("GATEWAY_ALLOWED_USERS", Some("@b:server"));
        let allow = Some(matrix_allowlist().expect("harus ter-parse"));
        assert_eq!(allow.as_ref().unwrap().len(), 2);
        assert!(is_matrix_user_allowed("@a:server", &allow));
        assert!(is_matrix_user_allowed("@b:server", &allow));
        assert!(!is_matrix_user_allowed("@c:server", &allow));
        restore_auth_env(saved);
    }

    #[test]
    fn allow_all_membuka_semua() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = clear_auth_env(&AUTH_VARS);
        set_var("MATRIX_ALLOW_ALL_USERS", Some("yes"));
        assert!(matrix_allow_all());
        assert!(is_matrix_user_allowed("siapa-saja", &None));
        restore_auth_env(saved);
    }

    fn temp_since_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "hermes-matrix-since-test-{}-{tag}",
            std::process::id()
        ))
    }

    #[test]
    fn since_token_roundtrip() {
        let dir = temp_since_path("roundtrip");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("matrix_since.json");
        save_since_token(&path, "s123_456");
        assert_eq!(load_since_token(&path).as_deref(), Some("s123_456"));
        // Timpa: nilai baru menang.
        save_since_token(&path, "s999_000");
        assert_eq!(load_since_token(&path).as_deref(), Some("s999_000"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn since_token_file_hilang_mulai_tanpa_token() {
        let path = temp_since_path("hilang").join("matrix_since.json");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        assert_eq!(load_since_token(&path), None);
    }

    #[test]
    fn since_token_file_rusak_mulai_tanpa_token() {
        let dir = temp_since_path("rusak");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("matrix_since.json");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, "bukan-json{{{").unwrap();
        assert_eq!(load_since_token(&path), None);
        // JSON valid tapi tanpa next_batch → tetap None.
        std::fs::write(&path, r#"{"foo": 1}"#).unwrap();
        assert_eq!(load_since_token(&path), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- formatted_body (audit batch 8) ---

    #[test]
    fn markdown_tebal_jadi_formatted_body() {
        let html = markdown_to_matrix_html("**tebal** dan biasa").expect("harus ada html");
        assert!(html.contains("<strong>tebal</strong>"), "dapat: {html}");
        let content = build_text_content("**tebal** dan biasa");
        assert_eq!(content["msgtype"], json!("m.text"));
        assert_eq!(content["body"], json!("**tebal** dan biasa"));
        assert_eq!(content["format"], json!("org.matrix.custom.html"));
        assert_eq!(content["formatted_body"], json!(html));
    }

    #[test]
    fn teks_polos_tanpa_format_tetap_dapat_formatted_body() {
        // Cermin aslinya (`if html and html != text`): "<p>halo</p>" !=
        // "halo" → formatted_body dikirim.
        let content = build_text_content("halo");
        assert_eq!(content["format"], json!("org.matrix.custom.html"));
        let fb = content["formatted_body"].as_str().unwrap();
        assert!(fb.contains("halo"), "dapat: {fb}");
    }

    #[test]
    fn sanitize_buang_script_dan_tag_tak_dikenal() {
        let html = sanitize_matrix_html(
            "<p>ok</p><script>alert(1)</script><blink>x</blink><a href=\"javascript:alert(1)\">y</a>",
        );
        assert!(html.contains("<p>ok</p>"), "dapat: {html}");
        assert!(
            !html.contains("alert(1)"),
            "script + js-url dibuang: {html}"
        );
        assert!(!html.contains("<blink>"), "tag tak dikenal dibuang: {html}");
        assert!(
            html.contains(">x<"),
            "isi tag tak dikenal dipertahankan: {html}"
        );
        assert!(html.contains(">y<"), "teks link dipertahankan: {html}");
    }

    #[test]
    fn sanitize_href_aman_dipertahankan() {
        let html = sanitize_matrix_html("<a href=\"https://contoh.id/x\">t</a>");
        assert_eq!(
            html, "<a href=\"https://contoh.id/x\">t</a>",
            "dapat: {html}"
        );
        let html2 = sanitize_matrix_html("<code class=\"language-rust\">c</code>");
        assert!(html2.contains("class=\"language-rust\""), "dapat: {html2}");
        // class sembarang dibuang.
        let html3 = sanitize_matrix_html("<code class=\"nakal\" onclick=\"x\">c</code>");
        assert!(!html3.contains("nakal"), "dapat: {html3}");
        assert!(!html3.contains("onclick"), "dapat: {html3}");
    }

    #[test]
    fn content_bytes_hitung_json_utf8() {
        // serde_json::to_string tak meng-escape non-ASCII (seperti
        // ensure_ascii=False aslinya).
        let c = json!({"body": "日本"});
        assert_eq!(content_bytes(&c), r#"{"body":"日本"}"#.len());
    }

    #[test]
    fn anggaran_konten_45k_bisa_dicek() {
        let big = "x".repeat(50_000);
        let content = build_text_content(&big);
        assert!(
            content_bytes(&content) > MATRIX_MAX_CONTENT_BYTES,
            "konten raksasa melebihi anggaran"
        );
    }
}
