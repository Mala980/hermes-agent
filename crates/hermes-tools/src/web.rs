//! Tool web: web_fetch (ambil + ekstrak teks halaman) dan web_search.
use crate::schema::*;
use async_trait::async_trait;
use hermes_core::{HermesError, Result, Tool, ToolContext, ToolEffect, ToolResult};
use regex::Regex;
use std::net::{IpAddr, Ipv4Addr};

// ---------------------------------------------------------------------------
// SSRF guard untuk web_fetch (audit W2-B24).
//
// Kebijakan: tolak URL yang membawa kredensial (`user:pass@`) dan tolak host
// yang menunjuk ke alamat privat/internal. Klasifikasi string murni
// (`classify_url`) tidak menyentuh jaringan sehingga bisa diuji hermetis;
// langkah DNS (`ensure_url_allowed`) hanya berjalan untuk nama host yang lolos
// pemeriksaan literal.
//
// CATATAN KEAMANAN — risiko DNS rebinding (TOCTOU): pengecekan DNS dan koneksi
// HTTP aktual terjadi pada waktu yang berbeda. Nama host yang jahat dapat
// mengubah jawaban DNS di antara pemeriksaan dan koneksi (rebinding), sehingga
// guard ini mengurangi — bukan menghilangkan — risiko SSRF. Mitigasi penuh
// memerlukan pinning hasil DNS ke koneksi (mis. resolver kustom di reqwest),
// yang belum diimplementasikan di sini.
// ---------------------------------------------------------------------------

/// Hasil klasifikasi host dari sebuah URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UrlTarget {
    /// Host adalah literal IP — tidak perlu DNS.
    Ip(IpAddr),
    /// Host adalah nama DNS — perlu pemeriksaan DNS sebelum fetch.
    Name(String),
}

/// Klasifikasikan URL secara murni (tanpa jaringan).
///
/// Mengembalikan `Err` dengan alasan penolakan bila URL dilarang:
/// skema bukan http(s), URL membawa kredensial, host adalah `localhost` /
/// `*.localhost`, atau host adalah literal IP privat/internal.
pub fn classify_url(url: &str) -> std::result::Result<UrlTarget, String> {
    // --- Skema ---
    let rest = url.split_once("://").and_then(|(scheme, rest)| {
        if scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") {
            Some(rest)
        } else {
            None
        }
    });
    let rest = match rest {
        Some(r) => r,
        None => return Err(format!("blocked: hanya URL http(s) yang diizinkan: {url}")),
    };

    // --- Otoritas: [userinfo@]host[:port], berhenti di /, ?, atau # ---
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if authority.is_empty() {
        return Err(format!("blocked: URL tidak memiliki host: {url}"));
    }

    // --- Kredensial: tolak userinfo apa pun (user maupun user:pass) ---
    // Ambil '@' terakhir agar `http://user@evil@example.com/` tidak lolos.
    let hostport = match authority.rfind('@') {
        Some(at) => {
            let userinfo = &authority[..at];
            if !userinfo.is_empty() {
                return Err("blocked: URL mengandung kredensial (userinfo@)".to_string());
            }
            &authority[at + 1..]
        }
        None => authority,
    };

    // --- Ekstrak host (buang port; tangani IPv6 dalam kurung siku) ---
    let host = if let Some(stripped) = hostport.strip_prefix('[') {
        match stripped.find(']') {
            Some(end) => &stripped[..end],
            None => {
                return Err(format!(
                    "blocked: literal IPv6 tidak valid (kurung tidak seimbang): {url}"
                ))
            }
        }
    } else if let Some(colon) = hostport.rfind(':') {
        // Hanya perlakukan sebagai port bila seluruh sufiks berupa digit.
        let port_part = &hostport[colon + 1..];
        if !port_part.is_empty() && port_part.bytes().all(|b| b.is_ascii_digit()) {
            &hostport[..colon]
        } else {
            hostport
        }
    } else {
        hostport
    };
    // Buang zone-id IPv6 mentah (`%eth0`) bila ada; sisanya tetap diperiksa.
    let host = host.split('%').next().unwrap_or(host);
    if host.is_empty() {
        return Err(format!("blocked: URL tidak memiliki host: {url}"));
    }

    // --- localhost dan *.localhost: tolak (diputuskan ditolak, bukan hanya
    // "localhost" persis, karena subdomainnya juga menunjuk loopback dan
    // rawan dipakai mengaburkan log/audit) ---
    let normalized = host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase();
    if normalized == "localhost" || normalized.ends_with(".localhost") {
        return Err(format!("blocked: host '{host}' adalah localhost"));
    }

    // --- Literal IP: periksa langsung terhadap rentang terlarang ---
    if let Ok(ip) = normalized.parse::<IpAddr>() {
        if is_blocked_ip(&ip) {
            return Err(format!(
                "blocked: host '{host}' adalah IP privat/internal ({ip})"
            ));
        }
        return Ok(UrlTarget::Ip(ip));
    }

    // Bukan literal IP yang dikenali std (mis. bentuk quirk seperti
    // `2130706433` yang oleh getaddrinfo tetap diurai sebagai 127.0.0.1):
    // serahkan ke pemeriksaan DNS di `ensure_url_allowed`.
    Ok(UrlTarget::Name(normalized))
}

/// Benar bila IP berada di rentang yang dilarang untuk web_fetch:
/// loopback, privat RFC1918, link-local (termasuk metadata cloud
/// 169.254.0.0/16), 0.0.0.0, dan padanan IPv6-nya.
fn is_blocked_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 127 // 127.0.0.0/8 loopback
                || o[0] == 10 // 10.0.0.0/8
                || (o[0] == 172 && (16..=31).contains(&o[1])) // 172.16.0.0/12
                || (o[0] == 192 && o[1] == 168) // 192.168.0.0/16
                || (o[0] == 169 && o[1] == 254) // 169.254.0.0/16 link-local / metadata cloud
                || *v4 == Ipv4Addr::UNSPECIFIED // 0.0.0.0
        }
        IpAddr::V6(v6) => {
            // IPv4-mapped (::ffff:127.0.0.1) diperiksa sebagai IPv4.
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return is_blocked_ip(&IpAddr::V4(mapped));
            }
            v6.is_loopback() // ::1
                || v6.is_unspecified() // ::
                || (v6.segments()[0] & 0xffc0) == 0xfe80 // fe80::/10 link-local
                || (v6.segments()[0] & 0xfe00) == 0xfc00 // fc00::/7 unique-local (padanan privat IPv6)
        }
    }
}

/// Pastikan URL boleh di-fetch: klasifikasi string + pemeriksaan DNS.
///
/// Untuk nama host, setiap alamat hasil resolusi DNS diperiksa terhadap
/// rentang terlarang; bila SATU SAJA terlarang, URL ditolak. Lihat catatan
/// DNS rebinding di bagian atas modul.
pub async fn ensure_url_allowed(url: &str) -> std::result::Result<(), String> {
    match classify_url(url)? {
        UrlTarget::Ip(_) => Ok(()), // sudah diperiksa di classify_url
        UrlTarget::Name(name) => {
            let addrs: Vec<IpAddr> = tokio::net::lookup_host((name.as_str(), 443))
                .await
                .map_err(|e| format!("blocked: gagal resolusi DNS untuk '{name}': {e}"))?
                .map(|sock| sock.ip())
                .collect();
            for ip in addrs {
                if is_blocked_ip(&ip) {
                    return Err(format!(
                        "blocked: host '{name}' me-resolve ke IP privat/internal ({ip})"
                    ));
                }
            }
            Ok(())
        }
    }
}

pub struct WebFetchTool;

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }
    fn description(&self) -> &str {
        "Mengambil URL dan mengembalikan konten teksnya (HTML diubah jadi teks)."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        object_schema(
            vec![("url", string_prop("URL http(s) yang diambil."))],
            vec!["url"],
        )
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::Read
    }

    async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext) -> Result<ToolResult> {
        let url = get_str(&args, "url")?;
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(HermesError::tool("web_fetch", "URL harus http(s)"));
        }
        // SSRF guard: tolak URL berkredensial / host privat-internal (lihat
        // `ensure_url_allowed` di atas untuk detail + catatan DNS rebinding).
        ensure_url_allowed(url)
            .await
            .map_err(|reason| HermesError::tool("web_fetch", reason))?;
        let client = reqwest::Client::builder()
            .user_agent("HermesAgent/0.1 (Rust port)")
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| HermesError::tool("web_fetch", e.to_string()))?;
        let resp = client
            .get(url)
            .send()
            .await
            .map_err(|e| HermesError::tool("web_fetch", format!("gagal fetch: {e}")))?;
        let status = resp.status();
        let html = resp
            .text()
            .await
            .map_err(|e| HermesError::tool("web_fetch", e.to_string()))?;
        let text = html_to_text(&html);
        Ok(ok(
            "",
            self.name(),
            format!("[HTTP {status}] {url}\n\n{}", truncate(&text, 30000)),
        ))
    }
}

/// Ubah HTML menjadi teks polos secara sederhana.
fn html_to_text(html: &str) -> String {
    // Buang script/style/noscript (tanpa backreference — regex crate tidak mendukungnya).
    let re_script = Regex::new(r"(?is)<script[^>]*>.*?</script>").unwrap();
    let no_script = re_script.replace_all(html, " ");
    let re_style = Regex::new(r"(?is)<(style|noscript)[^>]*>.*?</(style|noscript)>").unwrap();
    let no_script = re_style.replace_all(&no_script, " ");
    let re_tag = Regex::new(r"<[^>]+>").unwrap();
    let no_tag = re_tag.replace_all(&no_script, " ");
    let re_ws = Regex::new(r"[ \t\x0B\f\r]+").unwrap();
    let collapsed = re_ws.replace_all(&no_tag, " ");
    // Decode entitas umum.
    collapsed
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

pub struct WebSearchTool;

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }
    fn description(&self) -> &str {
        "Mencari di web dan mengembalikan judul, URL, dan cuplikan hasil."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        object_schema(
            vec![
                ("query", string_prop("Kueri pencarian.")),
                ("count", integer_prop("Jumlah hasil (default 8, maks 20).")),
            ],
            vec!["query"],
        )
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::Read
    }

    async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext) -> Result<ToolResult> {
        let query = get_str(&args, "query")?;
        let count = opt_u64(&args, "count").unwrap_or(8).clamp(1, 20) as usize;
        let results = duckduckgo_search(query, count).await?;
        if results.is_empty() {
            return Ok(ok("", self.name(), "tidak ada hasil.".to_string()));
        }
        let body = results
            .iter()
            .enumerate()
            .map(|(i, (t, u, s))| format!("{}. {t}\n   {u}\n   {s}", i + 1))
            .collect::<Vec<_>>()
            .join("\n\n");
        Ok(ok("", self.name(), body))
    }
}

async fn duckduckgo_search(query: &str, count: usize) -> Result<Vec<(String, String, String)>> {
    let client = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120 Safari/537.36")
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|e| HermesError::tool("web_search", e.to_string()))?;
    let url = format!("https://lite.duckduckgo.com/lite/?q={}", urlencoding(query));
    let html = client
        .get(&url)
        .send()
        .await
        .map_err(|e| HermesError::tool("web_search", format!("pencarian gagal: {e}")))?
        .text()
        .await
        .map_err(|e| HermesError::tool("web_search", e.to_string()))?;
    // Parse sederhana: <a rel="nofollow" href="URL">Judul</a> ... <td class='result-snippet'>Cuplikan</td>
    let re_link = Regex::new(r#"<a rel="nofollow" href="([^"]+)"[^>]*>(.*?)</a>"#).unwrap();
    let re_snip = Regex::new(r#"(?s)<td class='result-snippet'[^>]*>(.*?)</td>"#).unwrap();
    let links: Vec<(String, String)> = re_link
        .captures_iter(&html)
        .map(|c| {
            let url = html_unescape(&c[1]);
            let title = html_to_text(&c[2]).chars().take(200).collect::<String>();
            (title, url)
        })
        .collect();
    let snips: Vec<String> = re_snip
        .captures_iter(&html)
        .map(|c| html_to_text(&c[1]).chars().take(300).collect::<String>())
        .collect();
    let mut out = Vec::new();
    for (i, (t, u)) in links.into_iter().enumerate().take(count) {
        if u.contains("duckduckgo.com") {
            continue;
        }
        let s = snips.get(i).cloned().unwrap_or_default();
        out.push((t, u, s));
    }
    Ok(out)
}

fn html_unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
}

fn urlencoding(s: &str) -> String {
    // Percent-encode minimal tanpa crate tambahan.
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocked_msg(url: &str) -> String {
        classify_url(url)
            .expect_err("URL seharusnya ditolak")
            .clone()
    }

    #[test]
    fn ssrf_blocks_credential_urls() {
        for url in [
            "https://user:pass@example.com/",
            "https://user@example.com/",
            "http://admin:s3cret@93.184.216.34/path",
            // '@' terakhir menang: host jahat tidak bisa bersembunyi di userinfo.
            "http://user@evil.example@example.com/",
        ] {
            let msg = blocked_msg(url);
            assert!(
                msg.contains("kredensial"),
                "alasan untuk {url} harus menyebut kredensial, dapat: {msg}"
            );
        }
    }

    #[test]
    fn ssrf_blocks_localhost_names() {
        for url in [
            "http://localhost/",
            "http://localhost:8080/admin",
            "http://LOCALHOST/",
            "http://localhost./",
            // Diputuskan ditolak juga: *.localhost menunjuk loopback.
            "http://foo.localhost/",
            "http://a.b.localhost:3000/x?y=1",
        ] {
            let msg = blocked_msg(url);
            assert!(
                msg.contains("localhost"),
                "alasan untuk {url} harus menyebut localhost, dapat: {msg}"
            );
        }
    }

    #[test]
    fn ssrf_blocks_private_ipv4_literals() {
        for url in [
            "http://127.0.0.1/",
            "http://127.0.0.2:8080/",
            "http://10.1.2.3/",
            "http://10.255.255.255/admin",
            "http://172.16.0.1/",
            "http://172.31.255.254/",
            "http://192.168.1.1/",
            "http://169.254.169.254/latest/meta-data/",
            "http://0.0.0.0/",
        ] {
            let msg = blocked_msg(url);
            assert!(
                msg.contains("privat/internal"),
                "alasan untuk {url} harus menyebut IP privat/internal, dapat: {msg}"
            );
        }
    }

    #[test]
    fn ssrf_blocks_private_ipv6_literals() {
        for url in [
            "http://[::1]/",
            "http://[fe80::1]/",
            "http://[FE80::abcd]/",
            "http://[::]/",
            "http://[fc00::1]/",
            // IPv4-mapped: ::ffff:127.0.0.1 diperiksa sebagai 127.0.0.1.
            "http://[::ffff:127.0.0.1]/",
        ] {
            let msg = blocked_msg(url);
            assert!(
                msg.contains("privat/internal"),
                "alasan untuk {url} harus menyebut IP privat/internal, dapat: {msg}"
            );
        }
    }

    #[test]
    fn ssrf_blocks_non_http_schemes() {
        for url in [
            "ftp://example.com/file",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "data:text/plain,hello",
        ] {
            let msg = blocked_msg(url);
            assert!(
                msg.contains("http(s)"),
                "alasan untuk {url} harus menyebut http(s), dapat: {msg}"
            );
        }
    }

    #[test]
    fn ssrf_allows_public_urls() {
        // Nama host publik: lolos klasifikasi literal (tanpa DNS di tes).
        assert_eq!(
            classify_url("https://example.com/").unwrap(),
            UrlTarget::Name("example.com".to_string())
        );
        assert_eq!(
            classify_url("https://api.github.com/repos").unwrap(),
            UrlTarget::Name("api.github.com".to_string())
        );
        // Literal IP publik: lolos sebagai Ip.
        assert!(matches!(
            classify_url("http://93.184.216.34/").unwrap(),
            UrlTarget::Ip(_)
        ));
        assert!(matches!(
            classify_url("http://8.8.8.8/").unwrap(),
            UrlTarget::Ip(_)
        ));
        // Batas rentang 172.16.0.0/12: 172.15.x.x dan 172.32.x.x bersifat publik.
        assert!(matches!(
            classify_url("http://172.15.255.255/").unwrap(),
            UrlTarget::Ip(_)
        ));
        assert!(matches!(
            classify_url("http://172.32.0.1/").unwrap(),
            UrlTarget::Ip(_)
        ));
    }

    #[test]
    fn ssrf_rejects_malformed_urls() {
        assert!(classify_url("http:///").is_err());
        assert!(classify_url("https://").is_err());
        assert!(classify_url("http://[::1/").is_err()); // kurung tak seimbang
    }

    #[tokio::test]
    async fn ssrf_ensure_url_allowed_blocks_private_literal_without_dns() {
        // Literal IP tidak memicu DNS: hermetis.
        let err = ensure_url_allowed("http://169.254.169.254/latest/meta-data/")
            .await
            .expect_err("metadata cloud harus ditolak");
        assert!(err.contains("privat/internal"), "dapat: {err}");

        let err = ensure_url_allowed("http://[::1]/")
            .await
            .expect_err("loopback IPv6 harus ditolak");
        assert!(err.contains("privat/internal"), "dapat: {err}");

        ensure_url_allowed("http://93.184.216.34/")
            .await
            .expect("IP publik literal harus lolos tanpa DNS");
    }
}
