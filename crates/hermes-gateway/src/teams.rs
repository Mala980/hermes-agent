//! Platform Microsoft Teams via Bot Framework.
//!
//! Kirim: token dari login.microsoftonline.com lalu POST ke serviceUrl.
//! Terima: webhook `POST /api/messages` (daftarkan URL publik di Azure Bot).
//! Env: `TEAMS_APP_ID`, `TEAMS_APP_PASSWORD`.
use async_trait::async_trait;
use hermes_core::{HermesError, Platform, Result};
use serde_json::{json, Value};
use std::future::Future;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex as AsyncMutex;

use crate::split::{split_message, TEAMS_MESSAGE_LIMIT};

/// URL endpoint token; bisa di-override via TEAMS_TOKEN_URL untuk pengujian.
fn token_url() -> String {
    std::env::var("TEAMS_TOKEN_URL").unwrap_or_else(|_| {
        "https://login.microsoftonline.com/botframework.com/oauth2/v2.0/token".to_string()
    })
}

struct TeamsAuth {
    app_id: String,
    app_password: String,
    token: AsyncMutex<Option<(String, u64)>>, // (token, expires_at_epoch)
    client: reqwest::Client,
}

impl TeamsAuth {
    fn from_env() -> Result<Self> {
        let client = reqwest::Client::builder()
            // Redirect guard: bearer token tidak boleh dibawa ke host lain.
            .redirect(teams_redirect_policy())
            .build()
            .map_err(|e| HermesError::Gateway(format!("teams client gagal: {e}")))?;
        Ok(Self {
            app_id: std::env::var("TEAMS_APP_ID")
                .map_err(|_| HermesError::Gateway("set TEAMS_APP_ID".into()))?,
            app_password: std::env::var("TEAMS_APP_PASSWORD")
                .map_err(|_| HermesError::Gateway("set TEAMS_APP_PASSWORD".into()))?,
            token: AsyncMutex::new(None),
            client,
        })
    }

    async fn token(&self) -> Result<String> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        {
            let guard = self.token.lock().await;
            if let Some((tok, exp)) = guard.as_ref() {
                if *exp > now + 60 {
                    return Ok(tok.clone());
                }
            }
        }
        let resp = self
            .client
            .post(token_url())
            .form(&[
                ("grant_type", "client_credentials"),
                ("client_id", self.app_id.as_str()),
                ("client_secret", self.app_password.as_str()),
                ("scope", "https://api.botframework.com/.default"),
            ])
            .send()
            .await
            .map_err(|e| HermesError::Gateway(format!("teams token gagal: {e}")))?;
        let v: Value = resp
            .json()
            .await
            .map_err(|e| HermesError::Gateway(format!("teams token parse gagal: {e}")))?;
        let tok = v
            .get("access_token")
            .and_then(|t| t.as_str())
            .ok_or_else(|| HermesError::Gateway(format!("token ditolak: {v}")))?
            .to_string();
        let exp = now + v.get("expires_in").and_then(|e| e.as_u64()).unwrap_or(3600);
        *self.token.lock().await = Some((tok.clone(), exp));
        Ok(tok)
    }
}

pub struct TeamsPlatform {
    auth: Arc<TeamsAuth>,
}

/// Bandingkan dua host URL untuk keperluan redirect policy: harus persis sama
/// (case-insensitive). Sengaja LEBIH KETAT dari allowlist `validate_service_url`
/// yang mengizinkan subdomain — redirect ke subdomain tetap dianggap
/// lintas-host dan dihentikan, sehingga bearer token ter-pin ke host persis
/// yang sudah divalidasi. Perubahan skema/port di host yang sama tetap
/// diizinkan (token tidak berpindah host).
fn same_redirect_host(prev: &url::Url, next: &url::Url) -> bool {
    match (prev.host_str(), next.host_str()) {
        (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
        _ => false,
    }
}

/// Redirect policy untuk client HTTP Teams: hanya redirect ke host yang SAMA
/// yang diikuti; redirect lintas-host DIHENTIKAN agar bearer token tidak
/// pernah dibawa ke host lain. Jumlah hop dibatasi 10 seperti default reqwest.
///
/// Catatan: reqwest 0.12 secara default sudah menghapus header `Authorization`
/// saat redirect lintas-host (`remove_sensitive_headers` di redirect.rs:
/// lintas-host = host ATAU port berbeda). Policy eksplisit ini tetap dipasang
/// sebagai pertahanan berlapis dan untuk menyamai perilaku original: kami
/// tidak mengandalkan semata-mata pada stripping header oleh library.
pub fn teams_redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        // previous[0] adalah URL awal yang host-nya sudah divalidasi
        // validate_service_url; bandingkan target redirect terhadapnya agar
        // rantai redirect tidak bisa bergeser host sedikit demi sedikit.
        let same_host = attempt
            .previous()
            .first()
            .map(|prev| same_redirect_host(prev, attempt.url()))
            .unwrap_or(false);
        if attempt.previous().len() > 10 || !same_host {
            attempt.stop()
        } else {
            attempt.follow()
        }
    })
}

/// Host Teams service yang diizinkan (allowlist), disalin dari original.
/// Hanya host persis di daftar ini, atau subdomainnya, yang boleh menerima
/// bearer token.
const TEAMS_ALLOWED_HOSTS: [&str; 2] = [
    "smba.trafficmanager.net",
    "smba.infra.gov.teams.microsoft.us",
];

/// Validasi `serviceUrl` sebelum request apa pun yang membawa bearer token
/// dibangun/dikirim. Harus `https`, tanpa userinfo, dan host-nya (case-insensitive)
/// persis di allowlist atau subdomain dari salah satunya.
fn validate_service_url(service_url: &str) -> Result<()> {
    let parsed = url::Url::parse(service_url).map_err(|_| {
        HermesError::Gateway(format!(
            "blocked serviceUrl (tidak bisa diparse): {service_url}"
        ))
    })?;
    if parsed.scheme() != "https" {
        let host = parsed.host_str().unwrap_or("?");
        return Err(HermesError::Gateway(format!(
            "blocked serviceUrl host (bukan https): {host}"
        )));
    }
    // Tolak userinfo: https://evil@smba.trafficmanager.net/ tidak boleh lolos
    // walau host-nya terlihat valid.
    if !parsed.username().is_empty() || parsed.password().is_some() {
        let host = parsed.host_str().unwrap_or("?");
        return Err(HermesError::Gateway(format!(
            "blocked serviceUrl host (userinfo tidak diizinkan): {host}"
        )));
    }
    let host = parsed.host_str().ok_or_else(|| {
        HermesError::Gateway(format!("blocked serviceUrl (tanpa host): {service_url}"))
    })?;
    let host_lc = host.to_lowercase();
    let allowed = TEAMS_ALLOWED_HOSTS
        .iter()
        .any(|base| host_lc == *base || host_lc.ends_with(&format!(".{base}")));
    if !allowed {
        return Err(HermesError::Gateway(format!(
            "blocked serviceUrl host: {host}"
        )));
    }
    Ok(())
}

impl TeamsPlatform {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            auth: Arc::new(TeamsAuth::from_env()?),
        })
    }

    /// Kirim pesan ke conversation tertentu.
    /// target: "serviceUrl|conversationId".
    pub async fn send_to(
        &self,
        service_url: &str,
        conversation_id: &str,
        text: &str,
    ) -> Result<()> {
        self.send_to_reply(service_url, conversation_id, None, text)
            .await
    }

    /// Kirim pesan, opsional sebagai balasan ke activity `reply_to`.
    ///
    /// Cermin `TeamsAdapter.send()` aslinya: balasan dikirim via endpoint
    /// reply per chunk; bila gagal (mis. grup 400 pada threaded send),
    /// jatuh kembali ke kirim datar — SDK aslinya tak punya error HTTP
    /// bertipe sehingga fallback berlaku untuk error apa pun.
    pub async fn send_to_reply(
        &self,
        service_url: &str,
        conversation_id: &str,
        reply_to: Option<&str>,
        text: &str,
    ) -> Result<()> {
        // Validasi dulu: bearer token tidak boleh dikirim ke host di luar allowlist.
        validate_service_url(service_url)?;
        let token = self.auth.token().await?;
        let base = service_url.trim_end_matches('/');
        let flat_url = format!("{base}/v3/conversations/{conversation_id}/activities");
        // Endpoint balasan Bot Framework: POST .../activities/{activityId}.
        let reply_url = reply_to
            .filter(|r| !r.is_empty() && *r != "0")
            .map(|r| format!("{flat_url}/{r}"));
        // Batas Teams ~28 KB per pesan (28000 karakter); pecah sadar-pagar.
        for chunk in split_message(text, Some(TEAMS_MESSAGE_LIMIT)) {
            // `"textFormat": "markdown"` — seperti `_send_message` aslinya.
            let payload = json!({"type": "message", "text": chunk, "textFormat": "markdown"});
            // Bungkus sebagai closure agar request identik bisa dikirim ulang
            // bila API menjawab 429 (lihat crate::rate_limit). URL diambil
            // sebagai String milik sendiri agar future yang dikembalikan
            // tidak meminjam apa pun (bebas masalah lifetime).
            let post = {
                let payload = payload.clone();
                let token = token.clone();
                let client = self.auth.client.clone();
                move |url: String| {
                    let payload = payload.clone();
                    let token = token.clone();
                    let client = client.clone();
                    async move {
                        client
                            .post(&url)
                            .bearer_auth(&token)
                            .json(&payload)
                            .send()
                            .await
                            .map_err(|e| HermesError::Gateway(format!("teams send gagal: {e}")))
                    }
                }
            };
            // Coba endpoint balasan dulu; gagal → kirim datar.
            let mut resp = if let Some(url) = reply_url.as_deref() {
                match post(url.to_string()).await {
                    Ok(r) if r.status().is_success() => r,
                    Ok(r) => {
                        tracing::debug!(
                            "teams reply gagal ({}); fallback ke kirim datar",
                            r.status()
                        );
                        post(flat_url.clone()).await?
                    }
                    Err(e) => {
                        tracing::debug!("teams reply error ({e}); fallback ke kirim datar");
                        post(flat_url.clone()).await?
                    }
                }
            } else {
                post(flat_url.clone()).await?
            };
            // 429 → tidur sesuai Retry-After (cap 60 dtk), retry sekali.
            // Kirim ulang memakai URL datar agar idempoten.
            let flat = flat_url.clone();
            resp = crate::rate_limit::handle_429("teams", resp, || post(flat.clone())).await?;
            if !resp.status().is_success() {
                let b = resp.text().await.unwrap_or_default();
                return Err(HermesError::Gateway(format!("teams menolak pesan: {b}")));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct TeamsMessage {
    pub service_url: String,
    pub conversation_id: String,
    pub from: String,
    pub text: String,
    /// ID activity pemicu (untuk balasan threaded); kosong bila tak ada.
    pub activity_id: String,
}

/// Urai target teams: "serviceUrl|conversationId" atau
/// "serviceUrl|conversationId|replyToActivityId" (threaded reply).
fn parse_teams_target(target: &str) -> Result<(&str, &str, Option<&str>)> {
    let mut parts = target.splitn(3, '|');
    let (service_url, conversation_id) = match (parts.next(), parts.next()) {
        (Some(s), Some(c)) if !s.is_empty() && !c.is_empty() => (s, c),
        _ => {
            return Err(HermesError::Gateway(
                "target teams harus 'serviceUrl|conversationId[|replyToActivityId]'".into(),
            ))
        }
    };
    Ok((
        service_url,
        conversation_id,
        parts.next().filter(|r| !r.is_empty()),
    ))
}

#[async_trait]
impl Platform for TeamsPlatform {
    fn id(&self) -> &str {
        "teams"
    }

    async fn send_message(&self, target: &str, text: &str) -> Result<()> {
        let (service_url, conversation_id, reply_to) = parse_teams_target(target)?;
        self.send_to_reply(service_url, conversation_id, reply_to, text)
            .await
    }
}

/// Terima Activity dari Bot Framework via webhook `POST /api/messages`.
pub async fn serve_teams_webhook<F, Fut>(port: u16, handler: F) -> Result<()>
where
    F: FnMut(TeamsMessage) -> Fut + Send + Clone + 'static,
    Fut: Future<Output = Result<String>> + Send + 'static,
{
    let platform = Arc::new(TeamsPlatform::from_env()?);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .map_err(|e| HermesError::Gateway(format!("bind gagal: {e}")))?;
    tracing::info!("teams webhook di port {port} (POST /api/messages)");
    loop {
        let (mut socket, _) = listener
            .accept()
            .await
            .map_err(|e| HermesError::Gateway(format!("accept gagal: {e}")))?;
        let platform = platform.clone();
        let mut handler = handler.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(&mut socket, &platform, &mut handler).await {
                tracing::warn!("teams webhook error: {e}");
            }
        });
    }
}

async fn handle_conn<F, Fut>(
    socket: &mut tokio::net::TcpStream,
    platform: &Arc<TeamsPlatform>,
    handler: &mut F,
) -> Result<()>
where
    F: FnMut(TeamsMessage) -> Fut,
    Fut: Future<Output = Result<String>>,
{
    let mut buf = vec![0u8; 65536];
    let n = socket
        .read(&mut buf)
        .await
        .map_err(|e| HermesError::Gateway(format!("read gagal: {e}")))?;
    if n == 0 {
        return Ok(());
    }
    let req = String::from_utf8_lossy(&buf[..n]).into_owned();
    let mut lines = req.lines();
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("").to_string();
    let path_no_query = path.split('?').next().unwrap_or("").to_string();

    let mut content_length = 0usize;
    for line in lines.by_ref() {
        if line.is_empty() {
            break;
        }
        if let Some(v) = line
            .strip_prefix("Content-Length:")
            .or_else(|| line.strip_prefix("content-length:"))
        {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }
    let header_end = req.find("\r\n\r\n").map(|i| i + 4).unwrap_or(req.len());
    let mut body = buf[header_end..n].to_vec();
    while body.len() < content_length {
        let mut tmp = vec![0u8; 8192];
        let m = socket
            .read(&mut tmp)
            .await
            .map_err(|e| HermesError::Gateway(format!("read body gagal: {e}")))?;
        if m == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..m]);
    }

    let response = if method == "POST" && path_no_query == "/api/messages" {
        let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        // Hanya proses message biasa (bukan typing/event).
        if v.get("type").and_then(|t| t.as_str()) == Some("message") {
            let tm = TeamsMessage {
                service_url: v
                    .get("serviceUrl")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string(),
                conversation_id: v
                    .pointer("/conversation/id")
                    .and_then(|c| c.as_str())
                    .unwrap_or("")
                    .to_string(),
                from: v
                    .pointer("/from/id")
                    .and_then(|f| f.as_str())
                    .unwrap_or("?")
                    .to_string(),
                text: v
                    .get("text")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_string(),
                activity_id: v
                    .get("id")
                    .and_then(|i| i.as_str())
                    .unwrap_or("")
                    .to_string(),
            };
            tracing::info!(from = %tm.from, "pesan teams masuk");
            let reply = match handler(tm.clone()).await {
                Ok(r) => r,
                Err(e) => format!("Maaf, terjadi error: {e}"),
            };
            if !tm.service_url.is_empty() && !tm.conversation_id.is_empty() && !reply.is_empty() {
                // Balas sebagai threaded reply ke activity pemicu; gagal →
                // fallback kirim datar (di dalam send_to_reply).
                let reply_to = if tm.activity_id.is_empty() {
                    None
                } else {
                    Some(tm.activity_id.as_str())
                };
                if let Err(e) = platform
                    .send_to_reply(&tm.service_url, &tm.conversation_id, reply_to, &reply)
                    .await
                {
                    tracing::warn!("gagal membalas teams: {e}");
                }
            }
            json_response(200, &json!({}))
        } else {
            json_response(200, &json!({}))
        }
    } else {
        json_response(404, &json!({"error": "not found"}))
    };
    socket
        .write_all(&response)
        .await
        .map_err(|e| HermesError::Gateway(format!("write gagal: {e}")))?;
    Ok(())
}

fn json_response(status: u16, body: &Value) -> Vec<u8> {
    let b = body.to_string();
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        _ => "",
    };
    let mut r = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        b.len()
    )
    .into_bytes();
    r.extend_from_slice(b.as_bytes());
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_url_allow_exact_hosts() {
        assert!(validate_service_url(
            "https://smba.trafficmanager.net/v3/conversations/abc/activities"
        )
        .is_ok());
        assert!(validate_service_url(
            "https://smba.infra.gov.teams.microsoft.us/v3/conversations/x"
        )
        .is_ok());
    }

    #[test]
    fn service_url_allow_subdomains() {
        assert!(validate_service_url("https://xyz.smba.trafficmanager.net/whatever").is_ok());
        assert!(
            validate_service_url("https://deep.sub.smba.infra.gov.teams.microsoft.us/").is_ok()
        );
    }

    #[test]
    fn service_url_allow_case_insensitive_host() {
        assert!(validate_service_url("https://SMBA.TrafficManager.NET/v3/conversations/x").is_ok());
    }

    #[test]
    fn service_url_deny_non_https() {
        let err = validate_service_url("http://smba.trafficmanager.net/").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("blocked serviceUrl host") && msg.contains("smba.trafficmanager.net"),
            "unexpected: {msg}"
        );
    }

    #[test]
    fn service_url_deny_evil_host() {
        let err = validate_service_url("https://evil.com/").unwrap_err();
        assert_eq!(
            err.to_string(),
            "gateway error: blocked serviceUrl host: evil.com"
        );
    }

    #[test]
    fn service_url_deny_suffix_trick() {
        // smba.trafficmanager.net.evil.com BUKAN subdomain dari allowlist.
        let err = validate_service_url("https://smba.trafficmanager.net.evil.com/").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("blocked serviceUrl host")
                && msg.contains("smba.trafficmanager.net.evil.com"),
            "unexpected: {msg}"
        );
    }

    #[test]
    fn service_url_deny_userinfo_trick() {
        // Host aslinya smba.trafficmanager.net, tapi userinfo tetap ditolak.
        let err = validate_service_url("https://evil@smba.trafficmanager.net/").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("blocked serviceUrl host") && msg.contains("userinfo"),
            "unexpected: {msg}"
        );
    }

    #[test]
    fn service_url_deny_userinfo_trick_flipped() {
        // https://good@evil/ -> host aslinya "evil".
        let err = validate_service_url("https://good@evil/").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("blocked serviceUrl host"), "unexpected: {msg}");
    }

    #[test]
    fn service_url_deny_empty_and_garbage() {
        assert!(validate_service_url("").is_err());
        assert!(validate_service_url("not a url").is_err());
        assert!(validate_service_url("://bad").is_err());
        assert!(validate_service_url("ftp://smba.trafficmanager.net/").is_err());
    }

    fn u(s: &str) -> url::Url {
        url::Url::parse(s).unwrap()
    }

    #[test]
    fn redirect_same_host_exact_and_path_change() {
        assert!(same_redirect_host(
            &u("https://smba.trafficmanager.net/v3/conversations/a/activities"),
            &u("https://smba.trafficmanager.net/v3/conversations/b/activities"),
        ));
    }

    #[test]
    fn redirect_same_host_case_insensitive() {
        // url::Url sudah menormalkan host domain ke huruf kecil, tapi
        // perbandingan tetap case-insensitive untuk robustness (mis. IPv6).
        assert!(same_redirect_host(
            &u("https://smba.trafficmanager.net/a"),
            &u("https://SMBA.TRAFFICMANAGER.NET/b"),
        ));
    }

    #[test]
    fn redirect_http_to_https_same_host_allowed() {
        // Skema boleh berubah (upgrade/downgrade) selama host-nya sama:
        // bearer token tidak berpindah host.
        assert!(same_redirect_host(
            &u("http://smba.trafficmanager.net/a"),
            &u("https://smba.trafficmanager.net/b"),
        ));
        assert!(same_redirect_host(
            &u("https://smba.trafficmanager.net/a"),
            &u("http://smba.trafficmanager.net/b"),
        ));
    }

    #[test]
    fn redirect_subdomain_is_cross_host() {
        // Sengaja lebih ketat dari allowlist: redirect ke subdomain
        // dihentikan (token ter-pin ke host persis yang divalidasi).
        assert!(!same_redirect_host(
            &u("https://smba.trafficmanager.net/a"),
            &u("https://xyz.smba.trafficmanager.net/b"),
        ));
    }

    #[test]
    fn redirect_different_host_is_cross_host() {
        assert!(!same_redirect_host(
            &u("https://smba.trafficmanager.net/a"),
            &u("https://evil.com/b"),
        ));
    }

    #[test]
    fn redirect_missing_host_is_cross_host() {
        assert!(!same_redirect_host(
            &u("https://a.example/x"),
            &u("file:///y")
        ));
    }

    // --- Target reply threading (audit batch 8) ---

    #[test]
    fn parse_target_dua_dan_tiga_segmen() {
        let (s, c, r) = parse_teams_target("https://h/conv|abc").unwrap();
        assert_eq!((s, c, r), ("https://h/conv", "abc", None));
        let (s, c, r) = parse_teams_target("https://h/conv|abc|act123").unwrap();
        assert_eq!((s, c, r), ("https://h/conv", "abc", Some("act123")));
        // Segmen ketiga kosong = tanpa reply.
        let (_, _, r) = parse_teams_target("https://h/conv|abc|").unwrap();
        assert_eq!(r, None);
        // Format salah ditolak.
        assert!(parse_teams_target("tanpa-pemisah").is_err());
        assert!(parse_teams_target("|abc").is_err());
        assert!(parse_teams_target("https://h/|").is_err());
    }

    #[test]
    fn batas_teams_28000() {
        assert_eq!(crate::split::TEAMS_MESSAGE_LIMIT, 28000);
        let chunks = crate::split::split_message(
            &"t".repeat(30_000),
            Some(crate::split::TEAMS_MESSAGE_LIMIT),
        );
        assert_eq!(chunks.len(), 2);
        for c in &chunks {
            assert!(c.chars().count() <= 28000);
        }
    }
}
