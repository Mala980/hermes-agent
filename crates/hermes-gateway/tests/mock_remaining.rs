//! Test integrasi tanpa token untuk gateway yang belum terverifikasi:
//! Signal (via mock skrip `signal-cli`).
//!
//! Env var diserialkan via ENV_LOCK karena std::env bersifat global per-proses.
#![allow(clippy::await_holding_lock)]

use hermes_core::{HermesError, Platform};
use hermes_gateway::{serve_signal, SignalMessage, SignalPlatform};
use std::sync::Mutex;

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn set_env(k: &str, v: &str) {
    std::env::set_var(k, v);
}
fn remove_env(k: &str) {
    std::env::remove_var(k);
}

fn rand_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    format!(
        "{:x}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    )
}

/// Buat direktori dengan skrip mock `signal-cli`.
/// Skrip mencatat semua argumen ke `log`, lalu:
///
/// - `receive`: cetak satu envelope JSON pada panggilan pertama, kosong setelahnya.
/// - `send`: keluar 0 (atau 1 bila MOCK_SEND_FAIL=1).
///
/// Kembalikan (dir, path_log).
fn mock_signal_cli() -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "hermes-mocksig-{}-{}",
        std::process::id(),
        rand_suffix()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("args.log");
    let count = dir.join("recv.count");
    let script = format!(
        r#"#!/bin/bash
echo "ARGS:$*" >> "{log}"
if [ "$3" = "receive" ]; then
  n=$(cat "{count}" 2>/dev/null || echo 0)
  n=$((n+1)); echo "$n" > "{count}"
  if [ "$n" = "1" ]; then
    echo '{{"envelope":{{"sourceNumber":"+6281111111111","timestamp":1700000000000,"dataMessage":{{"message":"ping signal"}}}}}}'
  fi
  exit 0
elif [ "$3" = "send" ]; then
  if [ "$MOCK_SEND_FAIL" = "1" ]; then
    echo "boom: tidak bisa kirim" >&2
    exit 1
  fi
  exit 0
fi
echo "subcommand tak dikenal: $3" >&2
exit 2
"#,
        log = log.display(),
        count = count.display(),
    );
    let path = dir.join("signal-cli");
    std::fs::write(&path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    (dir, log)
}

fn signal_env(cli: &std::path::Path) {
    set_env("SIGNAL_CLI", cli.to_str().unwrap());
    set_env("SIGNAL_SENDER", "+6282222222222");
}

#[tokio::test]
async fn signal_send_ok() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (dir, log) = mock_signal_cli();
    signal_env(&dir.join("signal-cli"));

    let p = SignalPlatform;
    let res = p.send_message("+6289999999999", "halo signal").await;

    remove_env("SIGNAL_CLI");
    remove_env("SIGNAL_SENDER");
    res.expect("signal send gagal");
    let logged = std::fs::read_to_string(&log).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(logged.contains("-u +6282222222222"), "log: {logged}");
    assert!(logged.contains("send"), "log: {logged}");
    assert!(logged.contains("-m halo signal"), "log: {logged}");
    assert!(logged.contains("+6289999999999"), "log: {logged}");
}

#[tokio::test]
async fn signal_send_fail_is_error() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (dir, _log) = mock_signal_cli();
    signal_env(&dir.join("signal-cli"));
    set_env("MOCK_SEND_FAIL", "1");

    let p = SignalPlatform;
    let res = p.send_message("+6289999999999", "halo").await;

    remove_env("SIGNAL_CLI");
    remove_env("SIGNAL_SENDER");
    remove_env("MOCK_SEND_FAIL");
    let _ = std::fs::remove_dir_all(&dir);
    let err = res.expect_err("seharusnya error saat signal-cli gagal");
    assert!(err.to_string().contains("boom"), "err: {err}");
}

#[tokio::test]
async fn signal_receive_parses_and_replies() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (dir, log) = mock_signal_cli();
    signal_env(&dir.join("signal-cli"));
    set_env("SIGNAL_POLL_SECS", "1");

    let task = tokio::spawn(async move {
        serve_signal(|m: SignalMessage| async move {
            assert_eq!(m.from, "+6281111111111");
            assert_eq!(m.text, "ping signal");
            Ok::<_, HermesError>("pong signal".to_string())
        })
        .await
    });

    // Tunggu hingga balasan terkirim via mock send.
    let start = std::time::Instant::now();
    let mut replied = false;
    while start.elapsed().as_secs() < 15 {
        let logged = std::fs::read_to_string(&log).unwrap_or_default();
        if logged.contains("send -m pong signal +6281111111111") {
            replied = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    task.abort();
    remove_env("SIGNAL_CLI");
    remove_env("SIGNAL_SENDER");
    remove_env("SIGNAL_POLL_SECS");
    let _ = std::fs::remove_dir_all(&dir);
    assert!(replied, "balasan signal tidak terkirim via mock signal-cli");
}

#[tokio::test]
async fn signal_missing_sender_is_error() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    remove_env("SIGNAL_SENDER");
    let p = SignalPlatform;
    let res = p.send_message("+6289999999999", "halo").await;
    let err = res.expect_err("seharusnya error tanpa SIGNAL_SENDER");
    assert!(err.to_string().contains("SIGNAL_SENDER"), "err: {err}");
}

// ---------------------------------------------------------------------------
// Email: mock server SMTP (STARTTLS) + IMAP (implicit TLS / STARTTLS).
//
// Fixture sertifikat dibuat mandiri di /tmp/hermes-mailtest (srv.p12,
// password "test123") via openssl — TANPA sudo: CA tes digabung dengan bundle
// CA sistem menjadi ca-bundle.crt, lalu `SSL_CERT_FILE` diarahkan ke sana
// agar TLS client (lettre/native-tls, yang memakai OpenSSL) memercayai mock
// server tanpa mengubah system trust store. `ensure_mail_fixtures()`
// membuatnya bila belum ada; bila openssl tak tersedia, test dilewati.
// ---------------------------------------------------------------------------

use hermes_gateway::{serve_email, EmailMessage, EmailPlatform};
use tokio::io::{AsyncRead, AsyncWrite, BufReader};

const MAIL_FIX_DIR: &str = "/tmp/hermes-mailtest";

/// Pastikan fixture TLS ada dan `SSL_CERT_FILE` menunjuk ke bundle gabungan
/// (CA sistem + CA tes). Kembalikan false bila tidak bisa disiapkan.
fn ensure_mail_fixtures() -> bool {
    let p12 = format!("{MAIL_FIX_DIR}/srv.p12");
    let bundle = format!("{MAIL_FIX_DIR}/ca-bundle.crt");
    if std::path::Path::new(&p12).exists() && std::path::Path::new(&bundle).exists() {
        set_env("SSL_CERT_FILE", &bundle);
        return true;
    }
    eprintln!("membuat fixture TLS email di {MAIL_FIX_DIR} ...");
    let sh = format!(
        r#"set -e
mkdir -p {d}
cd {d}
openssl req -x509 -newkey rsa:2048 -keyout ca.key -out ca.crt -days 2 -nodes -subj "/CN=hermes-test-ca" 2>/dev/null
printf 'subjectAltName = IP:127.0.0.1, DNS:localhost\n' > srv.ext
openssl req -newkey rsa:2048 -nodes -keyout srv.key -out srv.csr -subj "/CN=127.0.0.1" 2>/dev/null
openssl x509 -req -in srv.csr -CA ca.crt -CAkey ca.key -CAcreateserial -out srv.crt -days 2 -extfile srv.ext 2>/dev/null
openssl pkcs12 -export -out srv.p12 -inkey srv.key -in srv.crt -passout pass:test123 2>/dev/null
# Gabung CA sistem + CA tes agar trust sistem tetap berlaku selama tes.
cat /etc/ssl/certs/ca-certificates.crt 2>/dev/null > ca-bundle.crt || true
cat ca.crt >> ca-bundle.crt
"#,
        d = MAIL_FIX_DIR
    );
    let ok = std::process::Command::new("bash")
        .arg("-c")
        .arg(&sh)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let ready = ok && std::path::Path::new(&p12).exists() && std::path::Path::new(&bundle).exists();
    if ready {
        set_env("SSL_CERT_FILE", &bundle);
    }
    ready
}

fn mail_tls_acceptor() -> tokio_native_tls::TlsAcceptor {
    let p12 = std::fs::read(format!("{MAIL_FIX_DIR}/srv.p12")).expect("srv.p12 hilang");
    let identity =
        tokio_native_tls::native_tls::Identity::from_pkcs12(&p12, "test123").expect("p12 invalid");
    tokio_native_tls::TlsAcceptor::from(
        tokio_native_tls::native_tls::TlsAcceptor::new(identity).expect("acceptor"),
    )
}

async fn write_line(w: &mut (dyn AsyncWrite + Unpin + Send), s: &str) {
    use tokio::io::AsyncWriteExt;
    w.write_all(s.as_bytes()).await.unwrap();
    w.write_all(b"\r\n").await.unwrap();
}

async fn read_line(r: &mut BufReader<&mut (dyn AsyncRead + Unpin + Send)>, buf: &mut String) {
    use tokio::io::AsyncBufReadExt;
    buf.clear();
    let n = r.read_line(buf).await.unwrap();
    assert!(n > 0, "koneksi mock terputus");
}

/// Satu sesi SMTP dengan STARTTLS; pesan DATA yang diterima dicatat.
async fn smtp_session(
    socket: tokio::net::TcpStream,
    acceptor: tokio_native_tls::TlsAcceptor,
    received: std::sync::Arc<Mutex<Vec<String>>>,
) {
    let (rh, wh) = socket.into_split();
    // Fase 1: plaintext hingga STARTTLS.
    let mut rh = rh;
    let mut reader = BufReader::new(&mut rh as &mut (dyn AsyncRead + Unpin + Send));
    let mut wh = wh;
    let mut w: &mut (dyn AsyncWrite + Unpin + Send) = &mut wh;
    write_line(&mut w, "220 mock-smtp ESMTP").await;
    let mut line = String::new();
    loop {
        read_line(&mut reader, &mut line).await;
        let up = line.to_uppercase();
        if up.starts_with("EHLO") {
            write_line(&mut w, "250-mock hello").await;
            write_line(&mut w, "250-STARTTLS").await;
            write_line(&mut w, "250 AUTH PLAIN LOGIN").await;
        } else if up.starts_with("STARTTLS") {
            write_line(&mut w, "220 siap TLS").await;
            break;
        } else {
            write_line(&mut w, "502 perintah tak dikenal pra-TLS").await;
        }
    }
    drop(reader);
    // Satukan kembali lalu upgrade ke TLS.
    let socket = rh.reunite(wh).expect("reunite");
    let tls = acceptor.accept(socket).await.expect("tls accept");
    let (tr, tw) = tokio::io::split(tls);
    let mut tr = tr;
    let mut tw = tw;
    let mut reader = BufReader::new(&mut tr as &mut (dyn AsyncRead + Unpin + Send));
    let mut w = &mut tw as &mut (dyn AsyncWrite + Unpin + Send);
    smtp_dialog(&mut reader, &mut w, &received).await;
}

async fn smtp_dialog(
    reader: &mut BufReader<&mut (dyn AsyncRead + Unpin + Send)>,
    w: &mut (dyn AsyncWrite + Unpin + Send),
    received: &std::sync::Arc<Mutex<Vec<String>>>,
) {
    let mut line = String::new();
    let mut authed = false;
    loop {
        read_line(reader, &mut line).await;
        let up = line.to_uppercase();
        let cmd = up.split_whitespace().next().unwrap_or("");
        match cmd {
            "EHLO" => {
                write_line(w, "250-mock hello tls").await;
                write_line(w, "250 AUTH PLAIN LOGIN").await;
            }
            "AUTH" => {
                if up.contains("PLAIN") {
                    // AUTH PLAIN <b64> atau tanpa argumen.
                    if up.split_whitespace().count() < 3 {
                        write_line(w, "334 ").await;
                        read_line(reader, &mut line).await;
                    }
                    authed = true;
                    write_line(w, "235 2.7.0 auth ok").await;
                } else if up.contains("LOGIN") {
                    write_line(w, "334 VXNlcm5hbWU6").await; // "Username:"
                    read_line(reader, &mut line).await;
                    write_line(w, "334 UGFzc3dvcmQ6").await; // "Password:"
                    read_line(reader, &mut line).await;
                    authed = true;
                    write_line(w, "235 2.7.0 auth ok").await;
                } else {
                    write_line(w, "504 mekanisme tak dikenal").await;
                }
            }
            "MAIL" => {
                write_line(w, if authed { "250 ok" } else { "530 auth dulu" }).await;
            }
            "RCPT" => {
                write_line(w, "250 ok").await;
            }
            "DATA" => {
                write_line(w, "354 kirim data, akhiri dengan <CRLF>.<CRLF>").await;
                let mut msg = String::new();
                loop {
                    read_line(reader, &mut line).await;
                    if line.trim_end_matches(['\r', '\n']) == "." {
                        break;
                    }
                    msg.push_str(&line);
                }
                received.lock().unwrap().push(msg);
                write_line(w, "250 ok: antrian mock").await;
            }
            "QUIT" => {
                write_line(w, "221 bye").await;
                break;
            }
            "RSET" | "NOOP" => {
                write_line(w, "250 ok").await;
            }
            _ => {
                write_line(w, "502 perintah tak dikenal").await;
            }
        }
    }
}

async fn run_mock_smtp(received: std::sync::Arc<Mutex<Vec<String>>>) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let acceptor = mail_tls_acceptor();
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                break;
            };
            let acc = acceptor.clone();
            let rec = received.clone();
            tokio::spawn(async move {
                smtp_session(socket, acc, rec).await;
            });
        }
    });
    addr
}

/// Satu sesi IMAP (TLS langsung / implicit): greeting, lalu dialog IMAP.
async fn imap_session(socket: tokio::net::TcpStream, acceptor: tokio_native_tls::TlsAcceptor) {
    let tls = acceptor.accept(socket).await.expect("imap tls accept");
    let (tr, tw) = tokio::io::split(tls);
    let mut tr = tr;
    let mut tw = tw;
    let mut reader = BufReader::new(&mut tr as &mut (dyn AsyncRead + Unpin + Send));
    let mut w = &mut tw as &mut (dyn AsyncWrite + Unpin + Send);
    write_line(&mut w, "* OK mock-imap siap").await;
    imap_dialog(&mut reader, &mut w).await;
}

/// Satu sesi IMAP dengan STARTTLS: plaintext hingga STARTTLS, lalu upgrade
/// TLS (tanpa greeting kedua), lalu dialog IMAP yang sama.
async fn imap_session_starttls(
    socket: tokio::net::TcpStream,
    acceptor: tokio_native_tls::TlsAcceptor,
) {
    let (rh, wh) = socket.into_split();
    let mut rh = rh;
    let mut wh = wh;
    let mut reader = BufReader::new(&mut rh as &mut (dyn AsyncRead + Unpin + Send));
    let mut w = &mut wh as &mut (dyn AsyncWrite + Unpin + Send);
    write_line(&mut w, "* OK mock-imap starttls siap").await;
    let mut line = String::new();
    loop {
        read_line(&mut reader, &mut line).await;
        let mut parts = line.split_whitespace();
        let tag = parts.next().unwrap_or("a0").to_string();
        let cmd = parts.next().unwrap_or("").to_uppercase();
        if cmd == "STARTTLS" {
            write_line(&mut w, &format!("{tag} OK mulai TLS")).await;
            break;
        } else if cmd == "CAPABILITY" {
            write_line(&mut w, "* CAPABILITY IMAP4rev1 STARTTLS").await;
            write_line(&mut w, &format!("{tag} OK selesai")).await;
        } else {
            write_line(&mut w, &format!("{tag} BAD perintah tak dikenal pra-TLS")).await;
        }
    }
    drop(reader);
    let socket = rh.reunite(wh).expect("reunite");
    let tls = acceptor.accept(socket).await.expect("imap tls accept");
    let (tr, tw) = tokio::io::split(tls);
    let mut tr = tr;
    let mut tw = tw;
    let mut reader = BufReader::new(&mut tr as &mut (dyn AsyncRead + Unpin + Send));
    let mut w = &mut tw as &mut (dyn AsyncWrite + Unpin + Send);
    imap_dialog(&mut reader, &mut w).await;
}

/// Dialog IMAP: LOGIN, SELECT, SEARCH UNSEEN → 1 pesan,
/// FETCH (ENVELOPE + BODY[TEXT]), STORE, LOGOUT.
async fn imap_dialog(
    reader: &mut BufReader<&mut (dyn AsyncRead + Unpin + Send)>,
    w: &mut (dyn AsyncWrite + Unpin + Send),
) {
    let mut line = String::new();
    loop {
        read_line(reader, &mut line).await;
        let tag = line.split_whitespace().next().unwrap_or("a0").to_string();
        let up = line.to_uppercase();
        if up.contains("LOGIN") {
            write_line(w, &format!("{tag} OK login ok")).await;
        } else if up.contains("SELECT") {
            write_line(w, "* 1 EXISTS").await;
            write_line(w, "* 1 RECENT").await;
            write_line(w, &format!("{tag} OK select ok")).await;
        } else if up.contains("SEARCH") {
            write_line(w, "* SEARCH 1").await;
            write_line(w, &format!("{tag} OK search ok")).await;
        } else if up.contains("FETCH") {
            // Mock melayani dua varian FETCH: BODY[TEXT] polos (lama) dan
            // RFC822 penuh. Varian RFC822 berisi pesan multipart/alternative
            // agar jalur `extract_text_body` (plain diutamakan, HTML dikupas)
            // teruji end-to-end.
            // NOTE: ')' penutup daftar atribut FETCH datang SETELAH literal
            // (bukan setelah "{n}"), sesuai grammar IMAP.
            let envelope = "ENVELOPE (\"Mon, 19 Oct 2026 09:00:00 +0800\" \"Subjek tes\" \
                 ((\"Pengirim\" NIL \"pengirim\" \"example.com\")) NIL NIL NIL NIL NIL NIL \"<mock-mid-1@example.com>\")";
            if up.contains("RFC822") {
                let raw = "From: Pengirim <pengirim@example.com>\r\n\
                           Subject: Subjek tes\r\n\
                           Content-Type: multipart/alternative; boundary=\"mockbnd\"\r\n\
                           \r\n\
                           --mockbnd\r\n\
                           Content-Type: text/plain; charset=utf-8\r\n\
                           \r\n\
                           hello email\r\n\
                           --mockbnd\r\n\
                           Content-Type: text/html; charset=utf-8\r\n\
                           \r\n\
                           <p>hello <b>email</b></p>\r\n\
                           --mockbnd--\r\n";
                write_line(
                    w,
                    &format!("* 1 FETCH ({envelope} RFC822 {{{}}}", raw.len()),
                )
                .await;
                {
                    use tokio::io::AsyncWriteExt;
                    w.write_all(raw.as_bytes()).await.unwrap();
                    // Literal IMAP: tepat {len} oktet, lalu ')' penutup
                    // daftar FETCH — tanpa CRLF tambahan di antara keduanya.
                    w.write_all(b")\r\n").await.unwrap();
                }
            } else {
                let body = "hello email";
                write_line(
                    w,
                    &format!("* 1 FETCH ({envelope} BODY[TEXT] {{{}}}", body.len()),
                )
                .await;
                {
                    use tokio::io::AsyncWriteExt;
                    w.write_all(body.as_bytes()).await.unwrap();
                    // Literal IMAP: tepat {len} oktet, lalu ')' penutup
                    // daftar FETCH — tanpa CRLF tambahan di antara keduanya.
                    w.write_all(b")\r\n").await.unwrap();
                }
            }
            write_line(w, &format!("{tag} OK fetch ok")).await;
        } else if up.contains("STORE") {
            write_line(w, &format!("{tag} OK store ok")).await;
        } else if up.contains("LOGOUT") {
            write_line(w, "* BYE bye").await;
            write_line(w, &format!("{tag} OK logout ok")).await;
            break;
        } else if up.contains("CAPABILITY") || up.contains("NOOP") {
            write_line(w, &format!("{tag} OK ok")).await;
        } else {
            write_line(w, &format!("{tag} BAD perintah tak dikenal")).await;
        }
    }
}

async fn run_mock_imap() -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let acceptor = mail_tls_acceptor();
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                break;
            };
            let acc = acceptor.clone();
            tokio::spawn(async move {
                imap_session(socket, acc).await;
            });
        }
    });
    addr
}

async fn run_mock_imap_starttls() -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let acceptor = mail_tls_acceptor();
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                break;
            };
            let acc = acceptor.clone();
            tokio::spawn(async move {
                imap_session_starttls(socket, acc).await;
            });
        }
    });
    addr
}

fn mail_env(smtp_port: u16, imap_port: u16) {
    set_env("EMAIL_SMTP_HOST", "127.0.0.1");
    set_env("EMAIL_SMTP_PORT", &smtp_port.to_string());
    set_env("EMAIL_USERNAME", "bot@example.com");
    set_env("EMAIL_PASSWORD", "secret");
    set_env("EMAIL_FROM", "bot@example.com");
    set_env("EMAIL_IMAP_HOST", "127.0.0.1");
    set_env("EMAIL_IMAP_PORT", &imap_port.to_string());
    // Mock IMAP default = implicit TLS; varian STARTTLS menimpa setelahnya.
    set_env("EMAIL_IMAP_SECURITY", "tls");
    set_env("EMAIL_POLL_SECS", "1");
    // Allowlist email fail-closed: izinkan pengirim mock.
    set_env("EMAIL_ALLOWED_USERS", "pengirim@example.com");
}
fn mail_unenv() {
    for k in [
        "EMAIL_SMTP_HOST",
        "EMAIL_SMTP_PORT",
        "EMAIL_USERNAME",
        "EMAIL_PASSWORD",
        "EMAIL_FROM",
        "EMAIL_IMAP_HOST",
        "EMAIL_IMAP_PORT",
        "EMAIL_IMAP_SECURITY",
        "EMAIL_POLL_SECS",
        "EMAIL_ALLOWED_USERS",
    ] {
        remove_env(k);
    }
}

#[tokio::test]
async fn email_smtp_send_ok() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if !ensure_mail_fixtures() {
        eprintln!("SKIP email_smtp_send_ok: fixture TLS tidak tersedia");
        return;
    }
    let received: std::sync::Arc<Mutex<Vec<String>>> = std::sync::Arc::new(Mutex::new(Vec::new()));
    let smtp_addr = run_mock_smtp(received.clone()).await;
    mail_env(smtp_addr.port(), 9999);
    // IMAP tidak dipakai di test ini; port dummy.

    let p = EmailPlatform::new(
        "127.0.0.1",
        smtp_addr.port(),
        "bot@example.com",
        "secret",
        "bot@example.com",
    );
    let res = p
        .send_message("tujuan@example.com|Subjek kustom", "halo smtp")
        .await;
    mail_unenv();
    res.expect("kirim email gagal");
    let msgs = received.lock().unwrap();
    assert_eq!(msgs.len(), 1, "pesan tidak sampai ke mock SMTP");
    let m = &msgs[0];
    assert!(m.contains("tujuan@example.com"), "msg: {m}");
    assert!(m.contains("Subjek kustom"), "msg: {m}");
    assert!(m.contains("halo smtp"), "msg: {m}");
}

#[tokio::test]
async fn email_imap_poll_replies() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if !ensure_mail_fixtures() {
        eprintln!("SKIP email_imap_poll_replies: fixture TLS tidak tersedia");
        return;
    }
    let received: std::sync::Arc<Mutex<Vec<String>>> = std::sync::Arc::new(Mutex::new(Vec::new()));
    let smtp_addr = run_mock_smtp(received.clone()).await;
    let imap_addr = run_mock_imap().await;
    mail_env(smtp_addr.port(), imap_addr.port());

    let platform = EmailPlatform::new(
        "127.0.0.1",
        smtp_addr.port(),
        "bot@example.com",
        "secret",
        "bot@example.com",
    );
    let task = tokio::spawn(async move {
        serve_email(platform, |m: EmailMessage| async move {
            assert_eq!(m.from, "pengirim@example.com");
            assert_eq!(m.subject, "Subjek tes");
            assert_eq!(m.body, "hello email");
            Ok::<_, HermesError>("balasan email!".to_string())
        })
        .await
    });

    // Tunggu balasan terkirim via mock SMTP.
    let start = std::time::Instant::now();
    let mut replied = false;
    while start.elapsed().as_secs() < 20 {
        {
            let msgs = received.lock().unwrap();
            if msgs.iter().any(|m| m.contains("balasan email!")) {
                let m = msgs.iter().find(|m| m.contains("balasan email!")).unwrap();
                assert!(m.contains("pengirim@example.com"), "balasan: {m}");
                assert!(m.contains("Re: Subjek tes"), "balasan: {m}");
                // Threading seperti aslinya: In-Reply-To + References
                // menunjuk Message-ID email masuk.
                assert!(
                    m.contains("In-Reply-To: <mock-mid-1@example.com>"),
                    "balasan: {m}"
                );
                assert!(
                    m.contains("References: <mock-mid-1@example.com>"),
                    "balasan: {m}"
                );
                replied = true;
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    task.abort();
    mail_unenv();
    assert!(replied, "balasan email tidak terkirim via mock SMTP");
}

/// Regresi: `send()` lettre yang blocking tidak boleh membuat starvation pada
/// runtime current-thread (dulu `email_smtp_send_ok` menggantung selamanya
/// karena mock server tak pernah dijadwalkan). Timeout 25 dtk mengubah
/// potensi hang menjadi kegagalan tes yang jelas.
#[tokio::test(flavor = "current_thread")]
async fn email_smtp_send_does_not_block_runtime() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if !ensure_mail_fixtures() {
        eprintln!("SKIP email_smtp_send_does_not_block_runtime: fixture TLS tidak tersedia");
        return;
    }
    let received: std::sync::Arc<Mutex<Vec<String>>> = std::sync::Arc::new(Mutex::new(Vec::new()));
    let smtp_addr = run_mock_smtp(received.clone()).await;
    mail_env(smtp_addr.port(), 9999);

    let p = EmailPlatform::new(
        "127.0.0.1",
        smtp_addr.port(),
        "bot@example.com",
        "secret",
        "bot@example.com",
    );
    // Port acak ≠ 465 → mode STARTTLS via default port; mock berbicara STARTTLS.
    assert_eq!(p.tls_mode(), hermes_gateway::EmailTlsMode::StartTls);
    let res = tokio::time::timeout(
        std::time::Duration::from_secs(25),
        p.send_message("tujuan@example.com|Subjek", "isi"),
    )
    .await;
    mail_unenv();
    let res = res.expect("send() memblokir runtime async (hang)");
    res.expect("kirim email gagal");
    assert_eq!(received.lock().unwrap().len(), 1);
}

/// Varian IMAP STARTTLS: `EMAIL_IMAP_SECURITY=starttls` harus melewati
/// handshake STARTTLS manual lalu memproses + membalas email seperti biasa.
#[tokio::test]
async fn email_imap_poll_starttls() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if !ensure_mail_fixtures() {
        eprintln!("SKIP email_imap_poll_starttls: fixture TLS tidak tersedia");
        return;
    }
    let received: std::sync::Arc<Mutex<Vec<String>>> = std::sync::Arc::new(Mutex::new(Vec::new()));
    let smtp_addr = run_mock_smtp(received.clone()).await;
    let imap_addr = run_mock_imap_starttls().await;
    mail_env(smtp_addr.port(), imap_addr.port());
    set_env("EMAIL_IMAP_SECURITY", "starttls");

    let platform = EmailPlatform::new(
        "127.0.0.1",
        smtp_addr.port(),
        "bot@example.com",
        "secret",
        "bot@example.com",
    );
    let task = tokio::spawn(async move {
        serve_email(platform, |m: EmailMessage| async move {
            assert_eq!(m.from, "pengirim@example.com");
            assert_eq!(m.subject, "Subjek tes");
            assert_eq!(m.body, "hello email");
            Ok::<_, HermesError>("balasan starttls!".to_string())
        })
        .await
    });

    // Tunggu balasan terkirim via mock SMTP.
    let start = std::time::Instant::now();
    let mut replied = false;
    while start.elapsed().as_secs() < 20 {
        {
            let msgs = received.lock().unwrap();
            if msgs.iter().any(|m| m.contains("balasan starttls!")) {
                replied = true;
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    task.abort();
    mail_unenv();
    assert!(replied, "balasan email tidak terkirim via mock SMTP");
}
