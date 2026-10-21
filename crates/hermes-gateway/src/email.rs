//! Platform Email: kirim via SMTP (lettre), terima via IMAP polling.
//!
//! Env: `EMAIL_SMTP_HOST`, `EMAIL_SMTP_PORT` (default 587),
//! `EMAIL_SMTP_SECURITY` (`tls`|`starttls`|`plain`; default dari port:
//! 465 → `tls`, lainnya → `starttls`),
//! `EMAIL_USERNAME`, `EMAIL_PASSWORD`, `EMAIL_FROM`,
//! `EMAIL_IMAP_HOST`, `EMAIL_IMAP_PORT` (default 993),
//! `EMAIL_IMAP_SECURITY` (default `tls`),
//! `EMAIL_POLL_SECS` (default 60).
use async_trait::async_trait;
use hermes_core::{HermesError, Platform, Result};
use std::future::Future;

use crate::allowlist::{env_truthy, read_allow_csv};
use crate::split::split_message;
use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};

/// Mode TLS untuk koneksi SMTP/IMAP — selaras dengan `EMAIL_SMTP_SECURITY` /
/// `EMAIL_IMAP_SECURITY` di `plugins/platforms/email/adapter.py` aslinya.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmailTlsMode {
    /// TLS implisit sejak awal koneksi (SMTPS port 465 / IMAPS port 993).
    Implicit,
    /// Plaintext lalu upgrade via STARTTLS (default untuk port selain 465).
    StartTls,
    /// Plaintext tanpa TLS sama sekali.
    Plaintext,
}

impl EmailTlsMode {
    /// Default bila tidak dikonfigurasi: port 465 → implicit, lainnya → STARTTLS.
    pub fn default_for_port(port: u16) -> Self {
        if port == 465 {
            EmailTlsMode::Implicit
        } else {
            EmailTlsMode::StartTls
        }
    }

    /// Normalisasi nilai konfigurasi ala `_normalize_security` aslinya:
    /// `tls`/`ssl`/`implicit` → Implicit, `starttls` → StartTls,
    /// `plain`/`none` → Plaintext. Nilai tak dikenal → `default`
    /// (salah ketik tidak pernah menurunkan ke plaintext).
    pub fn parse(value: &str, default: EmailTlsMode) -> EmailTlsMode {
        let raw: String = value
            .trim()
            .to_lowercase()
            .chars()
            .filter(|c| *c != '-' && *c != '_')
            .collect();
        let mode = match raw.as_str() {
            "tls" | "ssl" | "implicit" => Some(EmailTlsMode::Implicit),
            "starttls" => Some(EmailTlsMode::StartTls),
            "plain" | "none" => Some(EmailTlsMode::Plaintext),
            _ => None,
        };
        match mode {
            Some(m) => m,
            None => {
                if !raw.is_empty() {
                    tracing::warn!("mode TLS email tak dikenal {value:?}; memakai {default:?}");
                }
                default
            }
        }
    }
}

pub struct EmailPlatform {
    smtp_host: String,
    smtp_port: u16,
    username: String,
    password: String,
    from: String,
    tls_mode: EmailTlsMode,
    /// Konteks thread per pengirim: alamat → (subjek terakhir, message-id
    /// terakhir) — cermin `_thread_context` aslinya, dipakai untuk header
    /// `In-Reply-To`/`References` pada balasan.
    thread_ctx: Arc<StdMutex<HashMap<String, (String, String)>>>,
}

impl EmailPlatform {
    pub fn new(
        smtp_host: impl Into<String>,
        smtp_port: u16,
        username: impl Into<String>,
        password: impl Into<String>,
        from: impl Into<String>,
    ) -> Self {
        Self {
            smtp_host: smtp_host.into(),
            smtp_port,
            username: username.into(),
            password: password.into(),
            from: from.into(),
            tls_mode: EmailTlsMode::default_for_port(smtp_port),
            thread_ctx: Arc::new(StdMutex::new(HashMap::new())),
        }
    }

    /// Override eksplisit mode TLS SMTP (mengalahkan default berbasis port).
    pub fn with_tls_mode(mut self, mode: EmailTlsMode) -> Self {
        self.tls_mode = mode;
        self
    }

    /// Mode TLS SMTP yang dipakai platform ini.
    pub fn tls_mode(&self) -> EmailTlsMode {
        self.tls_mode
    }

    pub fn from_env() -> Result<Self> {
        let smtp_port: u16 = std::env::var("EMAIL_SMTP_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(587);
        let tls_mode = EmailTlsMode::parse(
            &std::env::var("EMAIL_SMTP_SECURITY").unwrap_or_default(),
            EmailTlsMode::default_for_port(smtp_port),
        );
        Ok(Self::new(
            std::env::var("EMAIL_SMTP_HOST")
                .map_err(|_| HermesError::Gateway("set EMAIL_SMTP_HOST".into()))?,
            smtp_port,
            std::env::var("EMAIL_USERNAME")
                .map_err(|_| HermesError::Gateway("set EMAIL_USERNAME".into()))?,
            std::env::var("EMAIL_PASSWORD")
                .map_err(|_| HermesError::Gateway("set EMAIL_PASSWORD".into()))?,
            std::env::var("EMAIL_FROM").unwrap_or_else(|_| {
                std::env::var("EMAIL_USERNAME").unwrap_or_else(|_| "hermes@localhost".into())
            }),
        )
        .with_tls_mode(tls_mode))
    }

    /// Catat konteks thread dari email masuk: subjek + message-id terakhir
    /// per pengirim — cermin `_thread_context` aslinya.
    pub fn record_thread_context(&self, from: &str, subject: &str, message_id: &str) {
        if from.is_empty() {
            return;
        }
        if let Ok(mut ctx) = self.thread_ctx.lock() {
            ctx.insert(
                from.to_string(),
                (subject.to_string(), message_id.to_string()),
            );
        }
    }

    /// Message-id terakhir yang diketahui untuk `to` (penerima balasan).
    fn thread_message_id(&self, to: &str) -> Option<String> {
        self.thread_ctx
            .lock()
            .ok()?
            .get(to)
            .map(|(_, mid)| mid.clone())
            .filter(|mid| !mid.is_empty())
    }

    /// Domain untuk Message-ID generasian; `localhost` bila alamat from
    /// tak punya `@` — cermin `_message_id_domain` aslinya.
    fn message_id_domain(&self) -> String {
        self.from
            .rsplit('@')
            .next()
            .filter(|d| !d.is_empty() && *d != self.from.as_str())
            .map(|d| d.to_string())
            .unwrap_or_else(|| "localhost".to_string())
    }

    /// Bangun Message-ID unik `<hermes-<12hex>@domain>` seperti aslinya.
    fn new_message_id(&self) -> String {
        let hex: String = uuid::Uuid::new_v4()
            .as_simple()
            .to_string()
            .chars()
            .take(12)
            .collect();
        format!("<hermes-{hex}@{}>", self.message_id_domain())
    }

    /// Kirim email; `reply_to_msg_id` eksplisit (atau konteks thread terakhir
    /// untuk `to`) menjadi header `In-Reply-To`/`References` — cermin
    /// `_new_reply` aslinya.
    async fn send_raw(
        &self,
        to: &str,
        subject: &str,
        body: &str,
        reply_to_msg_id: Option<&str>,
    ) -> Result<()> {
        use lettre::message::header::{ContentType, InReplyTo, MessageId, References};
        use lettre::transport::smtp::authentication::Credentials;
        use lettre::{Message, SmtpTransport, Transport};

        // Email tidak memaksakan batas isi di port ini (dipertahankan satu
        // email per kirim); tetap lewat helper bersama agar konsisten.
        let chunks = split_message(body, None);
        let body = chunks.concat();
        // Threading: id eksplisit diutamakan, lalu konteks thread terakhir.
        let in_reply_to: Option<String> = reply_to_msg_id
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| self.thread_message_id(to));
        let mut builder = Message::builder()
            .from(
                self.from
                    .parse()
                    .map_err(|e| HermesError::Gateway(format!("alamat from tidak valid: {e}")))?,
            )
            .to(to
                .parse()
                .map_err(|e| HermesError::Gateway(format!("alamat to tidak valid: {e}")))?)
            .subject(subject)
            .header(ContentType::TEXT_PLAIN)
            .header(MessageId::from(self.new_message_id()));
        if let Some(mid) = in_reply_to {
            builder = builder
                .header(InReplyTo::from(mid.clone()))
                .header(References::from(mid));
        }
        let email = builder
            .body(body.to_string())
            .map_err(|e| HermesError::Gateway(format!("bangun email gagal: {e}")))?;
        let creds = Credentials::new(self.username.clone(), self.password.clone());
        let builder = match self.tls_mode {
            EmailTlsMode::Implicit => SmtpTransport::relay(&self.smtp_host)
                .map_err(|e| HermesError::Gateway(format!("smtp relay gagal: {e}")))?,
            EmailTlsMode::StartTls => SmtpTransport::starttls_relay(&self.smtp_host)
                .map_err(|e| HermesError::Gateway(format!("smtp starttls gagal: {e}")))?,
            EmailTlsMode::Plaintext => SmtpTransport::builder_dangerous(&self.smtp_host),
        };
        let mailer = builder.port(self.smtp_port).credentials(creds).build();
        // lettre bersifat blocking: jalankan di thread blocking agar tidak
        // membuat starvation pada runtime async (sebelumnya menggantung pada
        // runtime current-thread karena mock server tak pernah dijadwalkan).
        tokio::task::spawn_blocking(move || mailer.send(&email))
            .await
            .map_err(|e| HermesError::Gateway(format!("smtp task gagal: {e}")))?
            .map_err(|e| HermesError::Gateway(format!("kirim email gagal: {e}")))?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct EmailMessage {
    pub from: String,
    pub subject: String,
    pub body: String,
    /// Message-ID email masuk (untuk threading balasan); kosong bila tak ada.
    pub message_id: String,
}

#[async_trait]
impl Platform for EmailPlatform {
    fn id(&self) -> &str {
        "email"
    }

    async fn send_message(&self, target: &str, text: &str) -> Result<()> {
        // target: alamat email, atau "alamat|subjek".
        let (to, subject) = match target.split_once('|') {
            Some((t, s)) => (t, s),
            None => (target, "Balasan Hermes"),
        };
        self.send_raw(to, subject, text, None).await
    }
}

/// Daftar alamat email pengirim yang diizinkan: gabungan (union)
/// `EMAIL_ALLOWED_USERS` dan `GATEWAY_ALLOWED_USERS` (dipisah koma;
/// `"*"` = semua pengirim). Mengembalikan `None` bila keduanya kosong →
/// semua pengirim ditolak (fail-closed).
pub fn email_allowlist() -> Option<std::collections::HashSet<String>> {
    let mut ids = read_allow_csv("EMAIL_ALLOWED_USERS");
    ids.extend(read_allow_csv("GATEWAY_ALLOWED_USERS"));
    if ids.is_empty() {
        None
    } else {
        Some(ids)
    }
}

/// Saklar "izinkan semua": `EMAIL_ALLOW_ALL_USERS` (khusus dev) atau
/// `GATEWAY_ALLOW_ALL_USERS` bernilai true/1/yes.
pub fn email_allow_all() -> bool {
    env_truthy("EMAIL_ALLOW_ALL_USERS") || env_truthy("GATEWAY_ALLOW_ALL_USERS")
}

/// Keputusan izin murni (mudah diunit-tes): `from` adalah alamat email
/// pengirim. Urutan: allow-all dulu, lalu allowlist, default tolak.
pub fn is_email_user_allowed(
    from: &str,
    allowlist: &Option<std::collections::HashSet<String>>,
) -> bool {
    if email_allow_all() {
        return true;
    }
    match allowlist {
        None => false,
        Some(set) => set.contains("*") || set.contains(from),
    }
}

/// Polling IMAP INBOX untuk email belum dibaca → handler → balas via SMTP.
pub async fn serve_email<F, Fut>(platform: EmailPlatform, mut handler: F) -> Result<()>
where
    F: FnMut(EmailMessage) -> Fut,
    Fut: Future<Output = Result<String>>,
{
    let imap_host = std::env::var("EMAIL_IMAP_HOST")
        .map_err(|_| HermesError::Gateway("set EMAIL_IMAP_HOST".into()))?;
    let imap_port: u16 = std::env::var("EMAIL_IMAP_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(993);
    let poll_secs: u64 = std::env::var("EMAIL_POLL_SECS")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(60);
    // Kredensial IMAP = kredensial SMTP (umumnya sama).
    let username = platform.username.clone();
    let password = platform.password.clone();
    // Mode TLS IMAP ala `EMAIL_IMAP_SECURITY` aslinya (default: implicit TLS).
    let imap_mode = EmailTlsMode::parse(
        &std::env::var("EMAIL_IMAP_SECURITY").unwrap_or_default(),
        EmailTlsMode::Implicit,
    );
    tracing::info!("email: polling IMAP {imap_host}:{imap_port} tiap {poll_secs}s");
    let allowlist = email_allowlist();
    if email_allow_all() {
        tracing::warn!(
            "allowlist email: DINONAKTIFKAN via *_ALLOW_ALL_USERS (semua pengirim diizinkan)"
        );
    }
    match &allowlist {
        Some(set) if set.contains("*") => {
            tracing::info!("allowlist email: semua pengirim diizinkan (*)")
        }
        Some(set) => tracing::info!("allowlist email: {} pengirim diizinkan", set.len()),
        None => tracing::warn!(
            "EMAIL_ALLOWED_USERS tidak diset: semua email masuk akan ditolak (fail-closed)"
        ),
    }

    loop {
        if let Err(e) = poll_once(
            ImapTarget {
                host: &imap_host,
                port: imap_port,
                username: &username,
                password: &password,
                mode: imap_mode,
            },
            &platform,
            &mut handler,
            &allowlist,
        )
        .await
        {
            tracing::warn!("email poll error: {e}");
        }
        tokio::time::sleep(std::time::Duration::from_secs(poll_secs)).await;
    }
}

/// Target koneksi untuk satu putaran polling IMAP.
struct ImapTarget<'a> {
    host: &'a str,
    port: u16,
    username: &'a str,
    password: &'a str,
    mode: EmailTlsMode,
}

async fn poll_once<F, Fut>(
    target: ImapTarget<'_>,
    platform: &EmailPlatform,
    handler: &mut F,
    allowlist: &Option<std::collections::HashSet<String>>,
) -> Result<()>
where
    F: FnMut(EmailMessage) -> Fut,
    Fut: Future<Output = Result<String>>,
{
    use tokio_util::compat::TokioAsyncReadCompatExt;
    let tcp = tokio::net::TcpStream::connect((target.host, target.port))
        .await
        .map_err(|e| HermesError::Gateway(format!("imap connect gagal: {e}")))?;
    match target.mode {
        EmailTlsMode::Implicit => {
            // async-imap butuh TLS; pakai tokio-native-tls via manual upgrade.
            let tls_stream = tls_upgrade(tcp, target.host).await?;
            // async-imap memakai futures-io; adaptor dari tokio.
            let client = async_imap::Client::new(tls_stream.compat());
            run_imap_session(
                client,
                target.username,
                target.password,
                platform,
                handler,
                allowlist,
            )
            .await
        }
        EmailTlsMode::StartTls => {
            // STARTTLS manual pada level tokio: baca greeting, kirim STARTTLS,
            // pastikan OK, lalu upgrade ke TLS (tidak ada greeting kedua).
            use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
            let mut reader = tokio::io::BufReader::new(tcp);
            let mut line = String::new();
            reader
                .read_line(&mut line)
                .await
                .map_err(|e| HermesError::Gateway(format!("imap greeting gagal: {e}")))?;
            line.clear();
            reader
                .write_all(b"a1 STARTTLS\r\n")
                .await
                .map_err(|e| HermesError::Gateway(format!("imap starttls kirim gagal: {e}")))?;
            reader
                .flush()
                .await
                .map_err(|e| HermesError::Gateway(format!("imap starttls flush gagal: {e}")))?;
            reader
                .read_line(&mut line)
                .await
                .map_err(|e| HermesError::Gateway(format!("imap starttls baca gagal: {e}")))?;
            if !line.to_uppercase().starts_with("A1 OK") {
                return Err(HermesError::Gateway(format!(
                    "imap starttls ditolak: {}",
                    line.trim()
                )));
            }
            let tcp = reader.into_inner();
            let tls_stream = tls_upgrade(tcp, target.host).await?;
            let client = async_imap::Client::new(tls_stream.compat());
            run_imap_session(
                client,
                target.username,
                target.password,
                platform,
                handler,
                allowlist,
            )
            .await
        }
        EmailTlsMode::Plaintext => {
            let client = async_imap::Client::new(tcp.compat());
            run_imap_session(
                client,
                target.username,
                target.password,
                platform,
                handler,
                allowlist,
            )
            .await
        }
    }
}

/// Upgrade stream TCP polos menjadi TLS via tokio-native-tls.
async fn tls_upgrade<S>(stream: S, host: &str) -> Result<tokio_native_tls::TlsStream<S>>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let tls = tokio_native_tls::TlsConnector::from(
        tokio_native_tls::native_tls::TlsConnector::new()
            .map_err(|e| HermesError::Gateway(format!("tls gagal: {e}")))?,
    );
    tls.connect(host, stream)
        .await
        .map_err(|e| HermesError::Gateway(format!("tls handshake gagal: {e}")))
}

/// Sesi IMAP: login → SELECT → SEARCH UNSEEN → fetch → handler → balas via SMTP.
async fn run_imap_session<F, Fut, S>(
    client: async_imap::Client<S>,
    username: &str,
    password: &str,
    platform: &EmailPlatform,
    handler: &mut F,
    allowlist: &Option<std::collections::HashSet<String>>,
) -> Result<()>
where
    F: FnMut(EmailMessage) -> Fut,
    Fut: Future<Output = Result<String>>,
    S: futures::io::AsyncRead + futures::io::AsyncWrite + Unpin + std::fmt::Debug + Send,
{
    use futures::StreamExt;
    let mut session = client
        .login(username, password)
        .await
        .map_err(|(e, _)| HermesError::Gateway(format!("imap login gagal: {e:?}")))?;
    session
        .select("INBOX")
        .await
        .map_err(|e| HermesError::Gateway(format!("imap select gagal: {e}")))?;

    let unseen = session
        .search("UNSEEN")
        .await
        .map_err(|e| HermesError::Gateway(format!("imap search gagal: {e}")))?;
    for seq in unseen.iter() {
        // Kumpulkan dulu (stream meminjam session), lalu proses.
        let mut fetched: Vec<(String, String, String, String)> = Vec::new();
        {
            let mut msgs = session
                // RFC822 penuh agar part `text/plain` bisa dipilih dari
                // struktur MIME (lihat `extract_text_body`).
                .fetch(format!("{seq}"), "(RFC822 ENVELOPE)")
                .await
                .map_err(|e| HermesError::Gateway(format!("imap fetch gagal: {e}")))?;
            while let Some(msg) = msgs.next().await {
                let msg = msg.map_err(|e| HermesError::Gateway(format!("imap msg gagal: {e}")))?;
                let from: String = msg
                    .envelope()
                    .and_then(|e| e.from.as_ref())
                    .and_then(|v| v.first())
                    .map(|a| {
                        let mailbox = a
                            .mailbox
                            .as_ref()
                            .map(|m| String::from_utf8_lossy(m).into_owned())
                            .unwrap_or_default();
                        let host = a
                            .host
                            .as_ref()
                            .map(|h| String::from_utf8_lossy(h).into_owned())
                            .unwrap_or_default();
                        format!("{mailbox}@{host}")
                    })
                    .unwrap_or_default();
                let subject: String = msg
                    .envelope()
                    .and_then(|e| e.subject.as_ref())
                    .map(|s| String::from_utf8_lossy(s).into_owned())
                    .unwrap_or_default();
                let message_id: String = msg
                    .envelope()
                    .and_then(|e| e.message_id.as_ref())
                    .map(|s| String::from_utf8_lossy(s).into_owned())
                    .unwrap_or_default();
                let body: String = msg
                    .body()
                    .map(extract_text_body)
                    // Fallback: server yang hanya mengembalikan BODY[TEXT]
                    // (mis. mock) — pakai apa adanya.
                    .or_else(|| msg.text().map(|b| String::from_utf8_lossy(b).into_owned()))
                    .unwrap_or_default();
                fetched.push((from, subject, message_id, body));
            }
        }
        for (from, subject, message_id, body) in fetched {
            if from.is_empty() {
                continue;
            }
            // Jangan balas email dari diri sendiri.
            if from.eq_ignore_ascii_case(&platform.from) || from.eq_ignore_ascii_case(username) {
                continue;
            }
            if !is_email_user_allowed(&from, allowlist) {
                tracing::warn!(from = %from, "email dari pengirim tak diizinkan, dilewati");
                // Tetap tandai sudah dibaca agar tidak diproses ulang
                // terus-menerus pada polling berikutnya.
                let _ = session.store(format!("{seq}"), "+FLAGS (\\Seen)").await;
                continue;
            }
            // Catat konteks thread untuk header In-Reply-To/References.
            platform.record_thread_context(&from, &subject, &message_id);
            let em = EmailMessage {
                from: from.clone(),
                subject: subject.clone(),
                body: body.trim().to_string(),
                message_id: message_id.clone(),
            };
            tracing::info!(from = %em.from, subject = %em.subject, "email masuk");
            let reply = match handler(em).await {
                Ok(r) => r,
                Err(e) => format!("Maaf, terjadi error: {e}"),
            };
            let reply_subject = if subject.to_lowercase().starts_with("re:") {
                subject.clone()
            } else {
                format!("Re: {subject}")
            };
            let reply_to = if message_id.is_empty() {
                None
            } else {
                Some(message_id.as_str())
            };
            if let Err(e) = platform
                .send_raw(&from, &reply_subject, &reply, reply_to)
                .await
            {
                tracing::warn!("gagal membalas email: {e}");
            }
            // Tandai sudah dibaca.
            let _ = session.store(format!("{seq}"), "+FLAGS (\\Seen)").await;
        }
    }
    session
        .logout()
        .await
        .map_err(|e| HermesError::Gateway(format!("imap logout gagal: {e}")))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Ekstraksi body email (audit W4-B-E3).
//
// Aslinya (`plugins/platforms/email/adapter.py`): `_extract_text_body`
// memilih part `text/plain` pertama yang bukan lampiran; bila hanya ada
// `text/html`, tag HTML dikupas (`_strip_html`, via `_HTML_SUBS`) menjadi
// teks. Port ini meniru perilaku itu dengan parser MIME minimalis tulisan
// tangan (tanpa dependensi baru):
//
// - `extract_text_body` menerima pesan RFC822 mentah, mengurai struktur
//   MIME-nya, dan mengembalikan teks terbaik (plain diutamakan).
// - `strip_html` mengupas tag ala `_HTML_SUBS` aslinya: `<br>`/`<p>` menjadi
//   newline, tag lain dibuang, entitas umum (`&nbsp;`/`&amp;`/`&lt;`/`&gt;`)
//   di-decode, rangkaian newline berlebih diciutkan. BUKAN renderer HTML
//   penuh — itu di luar cakupan.
// - Transfer encoding yang didukung: `base64`, `quoted-printable`, serta
//   `7bit`/`8bit`/`binary` (apa adanya). Charset: UTF-8/US-ASCII dan
//   ISO-8859-1; charset lain di-decode sebagai UTF-8 lossy (deviasi
//   terdokumentasi — aslinya memakai codec penuh Python).

/// Satu part MIME: header (nama sudah lower-case, unfolding selesai) + body
/// mentah.
struct MimePart<'a> {
    headers: Vec<(String, String)>,
    body: &'a [u8],
}

impl<'a> MimePart<'a> {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    fn is_attachment(&self) -> bool {
        self.header("content-disposition")
            .map(|d| d.to_ascii_lowercase().contains("attachment"))
            .unwrap_or(false)
    }
}

/// Pisahkan pesan/part mentah menjadi (header, body) pada baris kosong
/// pertama. Bekerja pada byte agar offset body tetap tepat.
fn split_header_body(raw: &[u8]) -> (&[u8], &[u8]) {
    let mut line_start = 0usize;
    for (i, &b) in raw.iter().enumerate() {
        if b != b'\n' {
            continue;
        }
        let mut line_end = i;
        if line_end > line_start && raw[line_end - 1] == b'\r' {
            line_end -= 1;
        }
        if line_end == line_start {
            // Baris kosong: header selesai tepat sebelum baris ini.
            return (&raw[..line_start], &raw[i + 1..]);
        }
        line_start = i + 1;
    }
    // Tak ada baris kosong: seluruhnya header, body kosong.
    (raw, &[][..])
}

/// Urai satu part MIME (header unfolding RFC 5322 sederhana).
fn parse_mime_part(raw: &[u8]) -> MimePart<'_> {
    let (hbytes, body) = split_header_body(raw);
    let htext = String::from_utf8_lossy(hbytes);
    let mut headers: Vec<(String, String)> = Vec::new();
    let mut name: Option<String> = None;
    let mut value = String::new();
    for line in htext.lines() {
        if (line.starts_with(' ') || line.starts_with('\t')) && name.is_some() {
            // Baris lanjutan (folding): gabung dengan satu spasi.
            value.push(' ');
            value.push_str(line.trim());
        } else {
            if let Some(n) = name.take() {
                headers.push((n, std::mem::take(&mut value)));
            }
            if let Some((n, v)) = line.split_once(':') {
                name = Some(n.trim().to_ascii_lowercase());
                value = v.trim().to_string();
            }
            // Baris tanpa ':' diabaikan (header malformed).
        }
    }
    if let Some(n) = name.take() {
        headers.push((n, value));
    }
    MimePart { headers, body }
}

/// Urai nilai `Content-Type` menjadi (mime-type lower-case, parameter).
/// Tanpa header → `text/plain` (seperti `get_content_type()` aslinya).
fn parse_content_type(value: &str) -> (String, std::collections::HashMap<String, String>) {
    let mut pieces = value.split(';');
    let mime = pieces.next().unwrap_or("").trim().to_ascii_lowercase();
    let mime = if mime.is_empty() {
        "text/plain".to_string()
    } else {
        mime
    };
    let mut params = std::collections::HashMap::new();
    for p in pieces {
        if let Some((k, v)) = p.split_once('=') {
            params.insert(
                k.trim().to_ascii_lowercase(),
                v.trim().trim_matches('"').to_string(),
            );
        }
    }
    (mime, params)
}

/// Pecah body multipart menjadi part-part mentah berdasarkan `boundary`
/// (RFC 2046: delimiter di awal baris; preamble/epilogue diabaikan).
fn split_multipart<'a>(body: &'a [u8], boundary: &str) -> Vec<&'a [u8]> {
    let delim = format!("--{boundary}");
    let close = format!("--{boundary}--");
    let delim = delim.as_bytes();
    let close = close.as_bytes();
    let mut parts: Vec<&[u8]> = Vec::new();
    let mut part_start: Option<usize> = None;
    let mut line_start = 0usize;
    let mut i = 0;
    while i <= body.len() {
        let at_end = i == body.len();
        if !at_end && body[i] != b'\n' {
            i += 1;
            continue;
        }
        let mut line_end = i;
        if line_end > line_start && body[line_end - 1] == b'\r' {
            line_end -= 1;
        }
        // Abaikan transport-padding (spasi/tab di ujung delimiter).
        let mut t_end = line_end;
        while t_end > line_start && (body[t_end - 1] == b' ' || body[t_end - 1] == b'\t') {
            t_end -= 1;
        }
        let line = &body[line_start..t_end];
        let is_delim = line == delim;
        let is_close = line == close;
        if is_delim || is_close {
            if let Some(ps) = part_start.take() {
                // Konten part berakhir tepat sebelum baris delimiter ini
                // (buang CRLF milik baris sebelumnya).
                let mut content_end = line_start;
                if content_end > ps && body[content_end - 1] == b'\n' {
                    content_end -= 1;
                    if content_end > ps && body[content_end - 1] == b'\r' {
                        content_end -= 1;
                    }
                }
                parts.push(&body[ps..content_end]);
            }
            if is_close {
                break;
            }
            part_start = Some(i + 1);
        }
        if at_end {
            break;
        }
        line_start = i + 1;
        i += 1;
    }
    parts
}

/// Hasil pengumpulan teks dari satu (sub-)pohon MIME.
#[derive(Default)]
struct PartText {
    plain: Option<String>,
    html: Option<String>,
}

/// Kumpulkan kandidat `text/plain` / `text/html` pertama (bukan lampiran)
/// dari part ini, rekursif untuk multipart bersarang.
fn collect_text(part: &MimePart) -> PartText {
    let ctype = part.header("content-type").unwrap_or("text/plain");
    let (mime, params) = parse_content_type(ctype);
    if let Some(boundary) = params
        .get("boundary")
        .filter(|_| mime.starts_with("multipart/"))
    {
        let mut acc = PartText::default();
        for sub_raw in split_multipart(part.body, boundary) {
            let sub = parse_mime_part(sub_raw);
            if sub.is_attachment() {
                continue;
            }
            let t = collect_text(&sub);
            if acc.plain.is_none() {
                acc.plain = t.plain;
            }
            if acc.html.is_none() {
                acc.html = t.html;
            }
            if acc.plain.is_some() && acc.html.is_some() {
                break;
            }
        }
        return acc;
    }
    if mime == "message/rfc822" {
        // Pesan tersarang: perlakukan body sebagai pesan baru (best-effort).
        return collect_text(&parse_mime_part(part.body));
    }
    let mut out = PartText::default();
    if (mime == "text/plain" || mime == "text/html") && !part.is_attachment() {
        let decoded = decode_part_body(part);
        if mime == "text/plain" {
            out.plain = Some(decoded);
        } else {
            out.html = Some(decoded);
        }
    }
    out
}

/// Decode body satu part teks sesuai `Content-Transfer-Encoding` + charset.
fn decode_part_body(part: &MimePart) -> String {
    let cte = part
        .header("content-transfer-encoding")
        .unwrap_or("7bit")
        .trim()
        .to_ascii_lowercase();
    let raw: Vec<u8> = match cte.as_str() {
        "base64" => decode_base64(part.body).unwrap_or_else(|| part.body.to_vec()),
        "quoted-printable" => decode_quoted_printable(part.body),
        _ => part.body.to_vec(), // 7bit, 8bit, binary, x-* : apa adanya
    };
    let ctype = part.header("content-type").unwrap_or("");
    let (_, params) = parse_content_type(ctype);
    decode_charset(&raw, params.get("charset").map(|s| s.as_str()))
}

/// Decode charset secukupnya: UTF-8/US-ASCII dan ISO-8859-1; sisanya
/// UTF-8 lossy (deviasi terdokumentasi).
fn decode_charset(bytes: &[u8], charset: Option<&str>) -> String {
    let normalized = charset.unwrap_or("utf-8").trim().to_ascii_lowercase();
    match normalized.as_str() {
        "iso-8859-1" | "latin-1" | "latin1" => bytes.iter().map(|&b| b as char).collect(),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Decode base64 tulisan tangan (abaikan whitespace; berhenti di padding).
/// `None` bila ada karakter tak valid.
fn decode_base64(input: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4 + 4);
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    for &b in input {
        let v: u32 = match b {
            b'A'..=b'Z' => (b - b'A') as u32,
            b'a'..=b'z' => (b - b'a' + 26) as u32,
            b'0'..=b'9' => (b - b'0' + 52) as u32,
            b'+' => 62,
            b'/' => 63,
            b'=' => break, // padding: sisa input diabaikan
            _ if b.is_ascii_whitespace() => continue,
            _ => return None,
        };
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// Decode quoted-printable tulisan tangan (termasuk soft line-break).
/// Byte `=` yang malformed dibiarkan apa adanya (lenient).
fn decode_quoted_printable(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] != b'=' {
            out.push(input[i]);
            i += 1;
            continue;
        }
        // Soft break: "=\r\n" atau "=\n".
        if i + 2 < input.len() && input[i + 1] == b'\r' && input[i + 2] == b'\n' {
            i += 3;
            continue;
        }
        if i + 1 < input.len() && input[i + 1] == b'\n' {
            i += 2;
            continue;
        }
        if i + 2 < input.len() {
            if let (Some(h), Some(l)) = (hex_val(input[i + 1]), hex_val(input[i + 2])) {
                out.push(h << 4 | l);
                i += 3;
                continue;
            }
        }
        out.push(b'=');
        i += 1;
    }
    out
}

/// Kupas tag HTML menjadi teks polos, meniru `_strip_html` + `_HTML_SUBS`
/// aslinya tanpa regex: `<br>`/`<p>`/`</p>` menjadi newline, tag lain
/// dibuang, entitas umum di-decode, rangkaian 3+ newline diciutkan menjadi
/// 2, lalu di-trim. Bukan renderer HTML penuh — itu di luar cakupan.
pub(crate) fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let bytes = html.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'<' => {
                match bytes[i..].iter().position(|&b| b == b'>') {
                    Some(rel) => {
                        let tag = html[i + 1..i + rel].trim().to_ascii_lowercase();
                        let is_br = tag == "br"
                            || tag == "br/"
                            || tag.starts_with("br ")
                            || tag.starts_with("br/");
                        let is_p_open =
                            tag == "p" || tag.starts_with("p ") || tag.starts_with("p/");
                        if is_br || is_p_open || tag == "/p" {
                            out.push('\n');
                        }
                        // Tag lain: dibuang.
                        i += rel + 1;
                    }
                    None => {
                        // '<' tanpa penutup: biarkan apa adanya.
                        out.push('<');
                        i += 1;
                    }
                }
            }
            b'&' => {
                // Entitas `&...;` (dibatasi 10 karakter agar aman).
                let rel = bytes[i..].iter().position(|&b| b == b';');
                match rel {
                    Some(r) if r <= 10 => {
                        match &html[i + 1..i + r] {
                            "nbsp" => out.push(' '),
                            "amp" => out.push('&'),
                            "lt" => out.push('<'),
                            "gt" => out.push('>'),
                            _ => out.push_str(&html[i..i + r + 1]),
                        }
                        i += r + 1;
                    }
                    _ => {
                        out.push('&');
                        i += 1;
                    }
                }
            }
            _ => {
                let ch = html[i..].chars().next().expect("batas char valid");
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    // Ciutkan 3+ newline menjadi 2 (pola terakhir `_HTML_SUBS`), lalu trim.
    let mut collapsed = String::with_capacity(out.len());
    let mut nl_run = 0;
    for ch in out.chars() {
        if ch == '\n' {
            nl_run += 1;
            if nl_run <= 2 {
                collapsed.push('\n');
            }
        } else {
            nl_run = 0;
            collapsed.push(ch);
        }
    }
    collapsed.trim().to_string()
}

/// Ekstrak body teks terbaik dari pesan RFC822 mentah: part `text/plain`
/// pertama yang bukan lampiran; bila hanya ada `text/html`, kupas tag-nya.
/// Cerminan `_extract_text_body` aslinya.
pub(crate) fn extract_text_body(raw: &[u8]) -> String {
    let part = parse_mime_part(raw);
    let t = collect_text(&part);
    match t.plain {
        Some(p) if !p.is_empty() => p,
        _ => t.html.map(|h| strip_html(&h)).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::allowlist::{clear_auth_env, restore_auth_env, set_var, ENV_LOCK};

    const AUTH_VARS: [&str; 4] = [
        "EMAIL_ALLOWED_USERS",
        "EMAIL_ALLOW_ALL_USERS",
        "GATEWAY_ALLOWED_USERS",
        "GATEWAY_ALLOW_ALL_USERS",
    ];

    #[test]
    fn allowlist_kosong_fail_closed() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = clear_auth_env(&AUTH_VARS);
        assert!(email_allowlist().is_none());
        assert!(!email_allow_all());
        assert!(!is_email_user_allowed(
            "boss@example.com",
            &email_allowlist()
        ));
        restore_auth_env(saved);
    }

    #[test]
    fn allowlist_parse_koma_dan_spasi() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = clear_auth_env(&AUTH_VARS);
        set_var(
            "EMAIL_ALLOWED_USERS",
            Some("  boss@example.com , tim@example.com,,"),
        );
        let allow = Some(email_allowlist().expect("harus ter-parse"));
        assert_eq!(allow.as_ref().unwrap().len(), 2);
        assert!(is_email_user_allowed("boss@example.com", &allow));
        assert!(is_email_user_allowed("tim@example.com", &allow));
        assert!(!is_email_user_allowed("spam@example.com", &allow));
        restore_auth_env(saved);
    }

    #[test]
    fn allowlist_wildcard_buka_semua() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = clear_auth_env(&AUTH_VARS);
        set_var("EMAIL_ALLOWED_USERS", Some("*"));
        let allow = Some(email_allowlist().expect("harus ter-parse"));
        assert!(is_email_user_allowed("siapa@saja.com", &allow));
        restore_auth_env(saved);
    }

    #[test]
    fn allowlist_id_tak_dikenal_ditolak() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = clear_auth_env(&AUTH_VARS);
        set_var("EMAIL_ALLOWED_USERS", Some("boss@example.com"));
        let allow = Some(email_allowlist().expect("harus ter-parse"));
        assert!(!is_email_user_allowed("spam@example.com", &allow));
        assert!(!is_email_user_allowed("", &allow));
        restore_auth_env(saved);
    }

    #[test]
    fn allowlist_gateway_users_gabungan() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = clear_auth_env(&AUTH_VARS);
        set_var("EMAIL_ALLOWED_USERS", Some("boss@example.com"));
        set_var("GATEWAY_ALLOWED_USERS", Some("tim@example.com"));
        let allow = Some(email_allowlist().expect("harus ter-parse"));
        assert_eq!(allow.as_ref().unwrap().len(), 2);
        assert!(is_email_user_allowed("boss@example.com", &allow));
        assert!(is_email_user_allowed("tim@example.com", &allow));
        assert!(!is_email_user_allowed("spam@example.com", &allow));
        restore_auth_env(saved);
    }

    #[test]
    fn allow_all_membuka_semua() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = clear_auth_env(&AUTH_VARS);
        set_var("EMAIL_ALLOW_ALL_USERS", Some("true"));
        assert!(email_allow_all());
        assert!(is_email_user_allowed("siapa@saja.com", &None));
        restore_auth_env(saved);
    }

    // --- Mode TLS (audit W4-B-E2) ---

    const SMTP_VARS: [&str; 6] = [
        "EMAIL_SMTP_HOST",
        "EMAIL_SMTP_PORT",
        "EMAIL_SMTP_SECURITY",
        "EMAIL_USERNAME",
        "EMAIL_PASSWORD",
        "EMAIL_FROM",
    ];

    fn mk_platform(port: u16) -> EmailPlatform {
        EmailPlatform::new(
            "127.0.0.1",
            port,
            "bot@example.com",
            "secret",
            "bot@example.com",
        )
    }

    #[test]
    fn tls_default_dari_port() {
        // 465 → implicit; port lain → STARTTLS (selaras adapter.py aslinya).
        assert_eq!(mk_platform(465).tls_mode(), EmailTlsMode::Implicit);
        assert_eq!(mk_platform(587).tls_mode(), EmailTlsMode::StartTls);
        assert_eq!(mk_platform(25).tls_mode(), EmailTlsMode::StartTls);
        assert_eq!(mk_platform(2525).tls_mode(), EmailTlsMode::StartTls);
    }

    #[test]
    fn tls_override_eksplisit_menang() {
        assert_eq!(
            mk_platform(465)
                .with_tls_mode(EmailTlsMode::Plaintext)
                .tls_mode(),
            EmailTlsMode::Plaintext
        );
        assert_eq!(
            mk_platform(587)
                .with_tls_mode(EmailTlsMode::Implicit)
                .tls_mode(),
            EmailTlsMode::Implicit
        );
        assert_eq!(
            mk_platform(587)
                .with_tls_mode(EmailTlsMode::StartTls)
                .tls_mode(),
            EmailTlsMode::StartTls
        );
    }

    #[test]
    fn tls_parse_alias_dan_fallback() {
        use EmailTlsMode::*;
        // Alias ala `_normalize_security` aslinya.
        for v in ["tls", "TLS", "ssl", "implicit", "IMPLICIT"] {
            assert_eq!(EmailTlsMode::parse(v, StartTls), Implicit, "input {v}");
        }
        for v in ["starttls", "STARTTLS", "start_tls"] {
            assert_eq!(EmailTlsMode::parse(v, Implicit), StartTls, "input {v}");
        }
        for v in ["plain", "PLAIN", "none"] {
            assert_eq!(EmailTlsMode::parse(v, StartTls), Plaintext, "input {v}");
        }
        // Kosong / tak dikenal → default (salah ketik tak menurunkan ke plaintext).
        assert_eq!(EmailTlsMode::parse("", Implicit), Implicit);
        assert_eq!(EmailTlsMode::parse("  ", StartTls), StartTls);
        assert_eq!(EmailTlsMode::parse("bogus", Implicit), Implicit);
        assert_eq!(EmailTlsMode::parse("bogus", StartTls), StartTls);
        assert_eq!(EmailTlsMode::parse("bogus", Plaintext), Plaintext);
    }

    #[test]
    fn from_env_baca_smtp_security() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = clear_auth_env(&SMTP_VARS);
        set_var("EMAIL_SMTP_HOST", Some("mail.example.com"));
        set_var("EMAIL_USERNAME", Some("bot@example.com"));
        set_var("EMAIL_PASSWORD", Some("secret"));

        // Default dari port bila EMAIL_SMTP_SECURITY tak diset.
        set_var("EMAIL_SMTP_PORT", Some("587"));
        let p = EmailPlatform::from_env().expect("from_env");
        assert_eq!(p.tls_mode(), EmailTlsMode::StartTls);
        set_var("EMAIL_SMTP_PORT", Some("465"));
        let p = EmailPlatform::from_env().expect("from_env");
        assert_eq!(p.tls_mode(), EmailTlsMode::Implicit);

        // Override eksplisit menang atas default port.
        set_var("EMAIL_SMTP_PORT", Some("587"));
        set_var("EMAIL_SMTP_SECURITY", Some("tls"));
        let p = EmailPlatform::from_env().expect("from_env");
        assert_eq!(p.tls_mode(), EmailTlsMode::Implicit);
        set_var("EMAIL_SMTP_SECURITY", Some("plain"));
        let p = EmailPlatform::from_env().expect("from_env");
        assert_eq!(p.tls_mode(), EmailTlsMode::Plaintext);
        // Nilai tak dikenal → fallback ke default port, bukan plaintext.
        set_var("EMAIL_SMTP_SECURITY", Some("bogus"));
        let p = EmailPlatform::from_env().expect("from_env");
        assert_eq!(p.tls_mode(), EmailTlsMode::StartTls);

        restore_auth_env(saved);
    }

    // --- Ekstraksi body multipart/HTML (audit W4-B-E3) ---

    #[test]
    fn strip_html_tag_bersarang() {
        assert_eq!(
            strip_html("<div><p>Hello <b>world</b></p></div>"),
            "Hello world"
        );
        assert_eq!(
            strip_html(
                "<html><body><h1>Judul</h1><p>Isi <a href=\"x\">tautan</a>.</p></body></html>"
            ),
            "Judul\nIsi tautan."
        );
    }

    #[test]
    fn strip_html_br_dan_paragraf_jadi_newline() {
        assert_eq!(strip_html("a<br>b"), "a\nb");
        assert_eq!(strip_html("a<BR/>b"), "a\nb");
        assert_eq!(strip_html("a<br />b"), "a\nb");
        assert_eq!(strip_html("<p>satu</p><p>dua</p>"), "satu\n\ndua");
    }

    #[test]
    fn strip_html_entitas_umum() {
        assert_eq!(strip_html("a &amp; b"), "a & b");
        assert_eq!(strip_html("&lt;div&gt;"), "<div>");
        assert_eq!(strip_html("a&nbsp;&nbsp;b"), "a  b");
        // Entitas tak dikenal dibiarkan apa adanya.
        assert_eq!(strip_html("a &copy; b"), "a &copy; b");
        // Satu pass seperti aslinya: `&amp;lt;` → `&lt;` (tidak di-decode lagi).
        assert_eq!(strip_html("&amp;lt;"), "&lt;");
    }

    #[test]
    fn strip_html_newline_berlebih_diciutkan() {
        assert_eq!(strip_html("<p>a</p><br><br><p>b</p>"), "a\n\nb");
        assert_eq!(strip_html("  teks polos  "), "teks polos");
        assert_eq!(strip_html("tanpa tag"), "tanpa tag");
        // '<' tanpa penutup dibiarkan.
        assert_eq!(strip_html("a < b"), "a < b");
    }

    fn mime_msg(headers: &str, body: &str) -> Vec<u8> {
        format!("{headers}\r\n\r\n{body}").into_bytes()
    }

    #[test]
    fn extract_plain_diprioritaskan_dari_multipart() {
        let raw = mime_msg(
            "From: a@x.id\r\nContent-Type: multipart/alternative; boundary=\"B123\"",
            "--B123\r\n\
             Content-Type: text/plain; charset=utf-8\r\n\
             \r\n\
             Halo teks polos\r\n\
             --B123\r\n\
             Content-Type: text/html; charset=utf-8\r\n\
             \r\n\
             <p>Halo <b>HTML</b></p>\r\n\
             --B123--\r\n",
        );
        assert_eq!(extract_text_body(&raw), "Halo teks polos");
    }

    #[test]
    fn extract_html_saja_dikupas() {
        let raw = mime_msg(
            "Content-Type: multipart/alternative; boundary=\"B123\"",
            "--B123\r\n\
             Content-Type: text/html; charset=utf-8\r\n\
             \r\n\
             <div><p>Halo <b>dunia</b> &amp; semua</p></div>\r\n\
             --B123--\r\n",
        );
        assert_eq!(extract_text_body(&raw), "Halo dunia & semua");
    }

    #[test]
    fn extract_lampiran_teks_dilewati() {
        let raw = mime_msg(
            "Content-Type: multipart/mixed; boundary=\"M1\"",
            "--M1\r\n\
             Content-Type: text/plain; name=\"catatan.txt\"\r\n\
             Content-Disposition: attachment; filename=\"catatan.txt\"\r\n\
             \r\n\
             ISI LAMPIRAN\r\n\
             --M1\r\n\
             Content-Type: text/plain; charset=utf-8\r\n\
             \r\n\
             Isi pesan asli\r\n\
             --M1--\r\n",
        );
        assert_eq!(extract_text_body(&raw), "Isi pesan asli");
    }

    #[test]
    fn extract_base64_dan_quoted_printable() {
        // "Halo dunia" dalam base64.
        let raw = mime_msg(
            "Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: base64",
            "SGFsbyBkdW5pYQ==\r\n",
        );
        assert_eq!(extract_text_body(&raw), "Halo dunia");

        // Quoted-printable dengan soft break dan "=C3=A9" (é).
        let raw = mime_msg(
            "Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: quoted-printable",
            "caf=C3=A9 =\r\npagi\r\n",
        );
        assert_eq!(extract_text_body(&raw).trim(), "café pagi");
    }

    #[test]
    fn extract_non_multipart_html_langsung() {
        let raw = mime_msg(
            "Content-Type: text/html; charset=utf-8",
            "<p>Hai <i>semua</i></p>",
        );
        assert_eq!(extract_text_body(&raw), "Hai semua");
    }

    #[test]
    fn extract_non_multipart_plain_langsung() {
        let raw = mime_msg("Content-Type: text/plain", "teks biasa");
        assert_eq!(extract_text_body(&raw), "teks biasa");
        // Tanpa header Content-Type: default text/plain.
        let raw = mime_msg("From: a@x.id", "tanpa header tipe");
        assert_eq!(extract_text_body(&raw), "tanpa header tipe");
    }

    #[test]
    fn extract_multipart_bersarang() {
        let raw = mime_msg(
            "Content-Type: multipart/mixed; boundary=\"OUT\"",
            "--OUT\r\n\
             Content-Type: multipart/alternative; boundary=\"IN\"\r\n\
             \r\n\
             --IN\r\n\
             Content-Type: text/plain\r\n\
             \r\n\
             versi polos\r\n\
             --IN\r\n\
             Content-Type: text/html\r\n\
             \r\n\
             <p>versi html</p>\r\n\
             --IN--\r\n\
             --OUT--\r\n",
        );
        assert_eq!(extract_text_body(&raw), "versi polos");
    }

    #[test]
    fn extract_iso_8859_1() {
        // 0xE9 = é dalam latin-1.
        let mut raw = b"Content-Type: text/plain; charset=iso-8859-1\r\n\r\ncaf\xe9 pagi".to_vec();
        assert_eq!(extract_text_body(&raw), "café pagi");
        raw = b"Content-Type: text/plain\r\n\r\nkosong".to_vec();
        assert_eq!(extract_text_body(&raw), "kosong");
    }

    // --- Threading balasan (audit batch 8) ---

    fn test_platform() -> EmailPlatform {
        EmailPlatform::new(
            "smtp.contoh.id",
            587,
            "bot@contoh.id",
            "pw",
            "bot@contoh.id",
        )
    }

    #[test]
    fn thread_context_disimpan_dan_dibaca() {
        let p = test_platform();
        assert_eq!(p.thread_message_id("a@x.id"), None);
        p.record_thread_context("a@x.id", "Halo", "<mid-1@x.id>");
        assert_eq!(
            p.thread_message_id("a@x.id"),
            Some("<mid-1@x.id>".to_string())
        );
        // Pengirim lain tak terpengaruh; pengirim kosong diabaikan.
        assert_eq!(p.thread_message_id("b@x.id"), None);
        p.record_thread_context("", "Halo", "<mid-2@x.id>");
        assert_eq!(p.thread_message_id(""), None);
        // Pesan baru menimpa konteks lama.
        p.record_thread_context("a@x.id", "Re: Halo", "<mid-3@x.id>");
        assert_eq!(
            p.thread_message_id("a@x.id"),
            Some("<mid-3@x.id>".to_string())
        );
    }

    #[test]
    fn message_id_format_seperti_aslinya() {
        let p = test_platform();
        let mid = p.new_message_id();
        // <hermes-<12hex>@contoh.id>
        assert!(mid.starts_with("<hermes-"), "dapat: {mid}");
        assert!(mid.ends_with("@contoh.id>"), "dapat: {mid}");
        let hex = mid
            .trim_start_matches("<hermes-")
            .split('@')
            .next()
            .unwrap();
        assert_eq!(hex.len(), 12, "dapat: {mid}");
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()), "dapat: {mid}");
        // Unik per panggilan.
        assert_ne!(p.new_message_id(), p.new_message_id());
    }

    #[test]
    fn message_id_domain_fallback_localhost() {
        let p = test_platform();
        assert_eq!(p.message_id_domain(), "contoh.id");
        let p2 = EmailPlatform::new("h", 587, "u", "pw", "tanpa-domain");
        assert_eq!(p2.message_id_domain(), "localhost");
    }
}
