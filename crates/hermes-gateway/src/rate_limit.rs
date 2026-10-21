//! Penanganan rate limit HTTP 429 bersama untuk semua platform gateway.
//!
//! Setiap jalur kirim HTTP (telegram, discord, slack, whatsapp, matrix,
//! teams): pada respons 429, tidur sesuai petunjuk retry (dibatasi 60 detik),
//! coba ulang SATU kali, lalu kembalikan error rate-limited yang jelas bila
//! masih gagal.
//!
//! Cakupan: hanya platform yang mengirim via HTTP. Signal (subproses
//! `signal-cli`) dan Email (SMTP via lettre) tidak punya respons HTTP 429
//! sehingga tidak tercakup di sini.
use hermes_core::{HermesError, Result};
use std::future::Future;
use std::time::Duration;

/// Batas atas penantian atas perintah server: jangan pernah tidur lebih dari
/// 60 detik karena satu respons 429 (melindungi dari DoS diri sendiri bila
/// server mengirim nilai absurd seperti `Retry-After: 3600`).
pub const RETRY_AFTER_CAP_SECS: u64 = 60;

/// Penantian default bila 429 tidak membawa petunjuk retry yang dapat di-parse.
const DEFAULT_RETRY_WAIT: Duration = Duration::from_secs(5);

/// Parse petunjuk retry dari respons 429 menjadi durasi tunggu.
///
/// Prioritas:
/// 1. Header `Retry-After` dalam detik (mis. `120`). Bentuk HTTP-date
///    SENGAJA tidak di-parse — diperlakukan sebagai "tak ada" agar kita
///    tidak menebak epoch dari jam yang mungkin miring (didokumentasikan
///    sebagai seconds-only).
/// 2. Field `retry_after` (detik, boleh float ala Discord) di body JSON
///    error — konvensi Bot API Telegram
///    (`{"ok":false,"error_code":429,"retry_after":30}`) yang juga dipakai
///    Discord (`{"retry_after":1.5,...}`).
///
/// Mengembalikan `None` bila tidak ada petunjuk yang dapat di-parse.
/// Hasil selalu dibatasi [`RETRY_AFTER_CAP_SECS`] detik.
pub fn parse_retry_after(
    headers: &reqwest::header::HeaderMap,
    body: Option<&serde_json::Value>,
) -> Option<Duration> {
    let secs = retry_after_header_secs(headers).or_else(|| body.and_then(retry_after_body_secs))?;
    Some(Duration::from_secs_f64(
        secs.min(RETRY_AFTER_CAP_SECS as f64),
    ))
}

/// Baca `Retry-After` dari header: hanya bentuk detik (integer/float).
/// HTTP-date atau nilai malformed → `None` (lihat dok [`parse_retry_after`]).
fn retry_after_header_secs(headers: &reqwest::header::HeaderMap) -> Option<f64> {
    let v = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    let secs: f64 = v.trim().parse().ok()?;
    if secs.is_finite() && secs >= 0.0 {
        Some(secs)
    } else {
        None
    }
}

/// Baca field `retry_after` dari body JSON error (konvensi Telegram/Discord).
fn retry_after_body_secs(body: &serde_json::Value) -> Option<f64> {
    let secs = body.get("retry_after")?.as_f64()?;
    if secs.is_finite() && secs >= 0.0 {
        Some(secs)
    } else {
        None
    }
}

/// Tangani respons HTTP 429: tidur sesuai petunjuk retry (cap 60 dtk),
/// kirim ulang SATU kali via `resend`, kembalikan respons kedua.
///
/// - Bukan 429 → `resp` dikembalikan apa adanya, tanpa tidur.
/// - 429 → parse `Retry-After`/body `retry_after` (cap 60 dtk; default 5 dtk
///   bila tak ada petunjuk), sleep, lalu `resend()` dipanggil SEKALI untuk
///   mengulang request yang identik.
/// - Masih 429 setelah retry → `Err` rate-limited yang jelas.
///
/// `resend` harus membangun ulang request yang sama dari awal (closure
/// `Fn`, bukan `FnOnce`) karena ia dipanggil setelah respons pertama
/// dikonsumsi untuk membaca petunjuk retry.
pub async fn handle_429<F, Fut>(
    platform: &str,
    resp: reqwest::Response,
    resend: F,
) -> Result<reqwest::Response>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<reqwest::Response>>,
{
    if resp.status() != reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Ok(resp);
    }
    let headers = resp.headers().clone();
    let body_text = resp.text().await.unwrap_or_default();
    let body_json: Option<serde_json::Value> = serde_json::from_str(&body_text).ok();
    let wait = parse_retry_after(&headers, body_json.as_ref()).unwrap_or(DEFAULT_RETRY_WAIT);
    tracing::warn!(
        platform,
        wait_secs = wait.as_secs(),
        "rate limited (HTTP 429); tidur lalu kirim ulang sekali"
    );
    tokio::time::sleep(wait).await;
    // `resend` sudah memetakan error transport ke HermesError sendiri.
    let retry = resend().await?;
    if retry.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
        // Beri tahu pemanggil berapa lama server meminta menunggu.
        let h = retry.headers().clone();
        let t = retry.text().await.unwrap_or_default();
        let j: Option<serde_json::Value> = serde_json::from_str(&t).ok();
        let hint = parse_retry_after(&h, j.as_ref())
            .map(|d| format!(" (server meminta tunggu {} dtk)", d.as_secs()))
            .unwrap_or_default();
        return Err(HermesError::Gateway(format!(
            "{platform}: masih rate-limited (HTTP 429) setelah satu percobaan ulang{hint}; \
             hentikan pengiriman dan coba lagi nanti"
        )));
    }
    Ok(retry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderMap;
    use serde_json::json;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                reqwest::header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    #[test]
    fn header_detik_diparse() {
        let h = headers(&[("retry-after", "30")]);
        assert_eq!(parse_retry_after(&h, None), Some(Duration::from_secs(30)));
        // Spasi di sekitar nilai ditoleransi.
        let h = headers(&[("retry-after", "  7 ")]);
        assert_eq!(parse_retry_after(&h, None), Some(Duration::from_secs(7)));
        // Float juga diterima (beberapa API mengirim "1.5").
        let h = headers(&[("retry-after", "1.5")]);
        assert_eq!(
            parse_retry_after(&h, None),
            Some(Duration::from_millis(1500))
        );
    }

    #[test]
    fn header_http_date_sengaja_diabaikan_seconds_only() {
        // Bentuk HTTP-date TIDAK di-parse (didokumentasikan seconds-only):
        // diperlakukan sebagai tak ada, bukan ditebak.
        let h = headers(&[("retry-after", "Wed, 21 Oct 2026 07:00:00 GMT")]);
        assert_eq!(parse_retry_after(&h, None), None);
    }

    #[test]
    fn header_malformed_dan_negatif_jadi_none() {
        for v in ["", "soon", "-5", "NaN", "inf"] {
            let h = headers(&[("retry-after", v)]);
            assert_eq!(parse_retry_after(&h, None), None, "nilai: {v:?}");
        }
    }

    #[test]
    fn body_telegram_retry_after_diparse() {
        // Konvensi Bot API Telegram: {"ok":false,"error_code":429,"retry_after":N}.
        let h = HeaderMap::new();
        let body = json!({"ok": false, "error_code": 429, "retry_after": 45});
        assert_eq!(
            parse_retry_after(&h, Some(&body)),
            Some(Duration::from_secs(45))
        );
    }

    #[test]
    fn body_discord_retry_after_float_diparse() {
        // Discord mengirim retry_after sebagai float.
        let h = HeaderMap::new();
        let body =
            json!({"message": "You are being rate limited.", "retry_after": 1.5, "global": false});
        assert_eq!(
            parse_retry_after(&h, Some(&body)),
            Some(Duration::from_millis(1500))
        );
    }

    #[test]
    fn header_menang_atas_body() {
        let h = headers(&[("retry-after", "10")]);
        let body = json!({"retry_after": 99});
        assert_eq!(
            parse_retry_after(&h, Some(&body)),
            Some(Duration::from_secs(10))
        );
    }

    #[test]
    fn hilang_jadi_none() {
        let h = HeaderMap::new();
        assert_eq!(parse_retry_after(&h, None), None);
        assert_eq!(parse_retry_after(&h, Some(&json!({"ok": false}))), None);
        assert_eq!(
            parse_retry_after(&h, Some(&json!({"retry_after": "soon"}))),
            None
        );
        assert_eq!(
            parse_retry_after(&h, Some(&json!({"retry_after": -3}))),
            None
        );
    }

    #[test]
    fn cap_60_detik_untuk_nilai_absurd() {
        // Header absurd → dibatasi 60 dtk (anti-DoS diri sendiri).
        let h = headers(&[("retry-after", "3600")]);
        assert_eq!(
            parse_retry_after(&h, None),
            Some(Duration::from_secs(RETRY_AFTER_CAP_SECS))
        );
        // Body absurd → sama.
        let h = HeaderMap::new();
        let body = json!({"retry_after": 99999});
        assert_eq!(
            parse_retry_after(&h, Some(&body)),
            Some(Duration::from_secs(RETRY_AFTER_CAP_SECS))
        );
        // Tepat di cap tidak berubah.
        let h = headers(&[("retry-after", "60")]);
        assert_eq!(parse_retry_after(&h, None), Some(Duration::from_secs(60)));
    }
}
