//! Verifikasi end-to-end `teams_redirect_policy` terhadap server HTTP mock lokal.
//!
//! - Redirect ke host yang SAMA diikuti (302 -> 200).
//! - Redirect lintas-host DIHENTIKAN: client mengembalikan respons 302 tanpa
//!   pernah mengirim request (apalagi bearer token) ke host lain.
//! - Rantai redirect dibatasi 10 hop (tidak loop selamanya).
use hermes_gateway::teams::teams_redirect_policy;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

type Handler = dyn Fn(&str) -> (u16, Option<String>, String) + Send + Sync + 'static;

/// Server HTTP mentah: mencatat header Authorization tiap request, lalu
/// menjawab sesuai `handler(request_line) -> (status, location, body)`.
async fn spawn_on(
    bind: &str,
    handler: impl Fn(&str) -> (u16, Option<String>, String) + Send + Sync + 'static,
) -> (SocketAddr, Arc<Mutex<Vec<Option<String>>>>) {
    let handler: Arc<Handler> = Arc::new(handler);
    let listener = tokio::net::TcpListener::bind(format!("{bind}:0"))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let seen: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let seen2 = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let seen = seen2.clone();
            let handler = handler.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    return;
                }
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let request_line = req.lines().next().unwrap_or("").to_string();
                let mut auth: Option<String> = None;
                for line in req.lines().skip(1) {
                    if line.is_empty() {
                        break;
                    }
                    if let Some(rest) = line
                        .strip_prefix("Authorization:")
                        .or_else(|| line.strip_prefix("authorization:"))
                    {
                        auth = Some(rest.trim().to_string());
                    }
                }
                seen.lock().unwrap().push(auth);
                let (status, location, body) = handler(&request_line);
                let reason = match status {
                    200 => "OK",
                    302 => "Found",
                    _ => "?",
                };
                let loc = location
                    .map(|l| format!("Location: {l}\r\n"))
                    .unwrap_or_default();
                let resp = format!(
                    "HTTP/1.1 {status} {reason}\r\n{loc}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(resp.as_bytes()).await;
            });
        }
    });
    (addr, seen)
}

fn policy_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(teams_redirect_policy())
        .build()
        .unwrap()
}

#[tokio::test]
async fn same_host_redirect_is_followed() {
    let (addr, seen) = spawn_on("127.0.0.1", |rl| {
        if rl.contains(" /start ") {
            (302, Some("/final".to_string()), String::new())
        } else {
            (200, None, "final".to_string())
        }
    })
    .await;
    let resp = policy_client()
        .get(format!("http://{addr}/start"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "final");
    // Request awal + request lanjutan ke /final.
    assert_eq!(seen.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn cross_host_redirect_is_stopped_and_token_never_leaves() {
    // Host B di 127.0.0.2: benar-benar host berbeda dari A (127.0.0.1),
    // sekaligus mewakili kasus subdomain/evil host dalam semantik policy.
    let (addr_b, seen_b) = spawn_on("127.0.0.2", |_| (200, None, "stolen".to_string())).await;
    let (addr_a, seen_a) = spawn_on("127.0.0.1", move |rl| {
        if rl.contains(" /start ") {
            (302, Some(format!("http://{addr_b}/steal")), String::new())
        } else {
            (200, None, "unexpected".to_string())
        }
    })
    .await;
    let resp = policy_client()
        .get(format!("http://{addr_a}/start"))
        .bearer_auth("TOKEN-RAHASIA")
        .send()
        .await
        .unwrap();
    // Policy menghentikan redirect: respons 302 dikembalikan apa adanya.
    assert_eq!(resp.status(), 302);
    // Host B tidak pernah menerima request apa pun: token aman.
    assert!(
        seen_b.lock().unwrap().is_empty(),
        "host lain tidak boleh menerima request"
    );
    // Hanya host asal yang menerima request, lengkap dengan bearer token.
    let seen_a = seen_a.lock().unwrap();
    assert_eq!(seen_a.len(), 1);
    assert_eq!(seen_a[0].as_deref(), Some("Bearer TOKEN-RAHASIA"));
}

#[tokio::test]
async fn redirect_loop_stops_after_ten_hops() {
    let (addr, seen) = spawn_on("127.0.0.1", |_| {
        (302, Some("/loop".to_string()), String::new())
    })
    .await;
    let resp = policy_client()
        .get(format!("http://{addr}/loop"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 302);
    // Request awal + 10 redirect yang diikuti, lalu berhenti.
    assert_eq!(seen.lock().unwrap().len(), 11);
}
