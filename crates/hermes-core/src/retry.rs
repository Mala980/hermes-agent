//! Klasifikasi error API + retry loop dengan jittered backoff.
//!
//! Meniru `classify_api_error` (`agent/error_classifier.py`),
//! `_run_api_retry_loop` (`agent/conversation_loop.py`), dan
//! `jittered_backoff` (`agent/retry_utils.py`) pada Hermes asli.
//!
//! Aturan klasifikasi:
//! - retryable: HTTP 429, 408, 5xx, atau kegagalan transport (timeout/koneksi)
//! - non-retryable: 400, 401, 403, 404, dan error 4xx/deterministik lainnya

use crate::{HermesError, Result};
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Jumlah maksimum percobaan total per panggilan LLM (1 awal + 2 retry),
/// meniru `max_retries` pada retry loop Hermes asli.
pub const MAX_ATTEMPTS: u32 = 3;
/// Delay dasar backoff antar retry.
pub const BASE_DELAY: Duration = Duration::from_secs(1);
/// Batas atas komponen eksponensial backoff (sebelum jitter).
pub const MAX_DELAY: Duration = Duration::from_secs(30);
/// Batas atas total tunggu untuk satu retry (termasuk jitter).
pub const MAX_TOTAL_DELAY: Duration = Duration::from_secs(45);
/// Batas atas `Retry-After` yang dihormati, mencegah tunggu berlebihan.
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(300);

/// Hasil klasifikasi sebuah error API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// Boleh dicoba lagi: HTTP 429/408/5xx, timeout, atau gangguan jaringan.
    Retryable,
    /// Deterministik — retry tidak akan membantu: 400/401/403/404, dsb.
    NonRetryable,
}

impl ErrorClass {
    pub fn is_retryable(self) -> bool {
        matches!(self, ErrorClass::Retryable)
    }
}

/// Klasifikasikan error menjadi retryable / non-retryable.
pub fn classify_error(err: &HermesError) -> ErrorClass {
    match err {
        HermesError::ProviderHttp { status, .. } => classify_status(*status),
        HermesError::ProviderTransport { .. } => ErrorClass::Retryable,
        HermesError::Provider(msg) => classify_message(msg),
        _ => ErrorClass::NonRetryable,
    }
}

fn classify_status(status: u16) -> ErrorClass {
    if status == 429 || status == 408 || (500..=599).contains(&status) {
        ErrorClass::Retryable
    } else {
        ErrorClass::NonRetryable
    }
}

/// Fallback heuristik untuk pesan error string lama, mis. "HTTP 503: ...",
/// "bedrock HTTP 429: ...", "request gagal: ... timed out",
/// "stream error: ... connection reset".
fn classify_message(msg: &str) -> ErrorClass {
    let lower = msg.to_lowercase();
    if let Some(status) = parse_http_status(&lower) {
        return classify_status(status);
    }
    // Timeout / gangguan jaringan tanpa kode status HTTP.
    const RETRYABLE_HINTS: &[&str] = &[
        "timed out",
        "timeout",
        "deadline exceeded",
        "connection reset",
        "connection refused",
        "connection aborted",
        "connection closed",
        "broken pipe",
        "dns",
        "name resolution",
        "network unreachable",
        "network is unreachable",
        "temporary failure",
        "econnreset",
        "etimedout",
        "econnrefused",
    ];
    if RETRYABLE_HINTS.iter().any(|h| lower.contains(h)) {
        ErrorClass::Retryable
    } else {
        ErrorClass::NonRetryable
    }
}

/// Ambil kode status dari fragmen "HTTP NNN" di dalam pesan (sudah lowercase).
fn parse_http_status(lower: &str) -> Option<u16> {
    let idx = lower.find("http")?;
    let rest = lower[idx + "http".len()..].trim_start();
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.len() == 3 {
        digits.parse().ok()
    } else {
        None
    }
}

/// Parse nilai header `Retry-After`: delta-detik ("120") atau HTTP-date
/// ("Wed, 21 Oct 2026 07:28:00 GMT"). Mengembalikan durasi tunggu, atau
/// `None` bila tidak bisa di-parse atau tanggalnya sudah lewat.
pub fn parse_retry_after(value: &str) -> Option<Duration> {
    let v = value.trim();
    if v.is_empty() {
        return None;
    }
    if let Ok(secs) = v.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    // HTTP-date (IMF-fixdate), mis. "Wed, 21 Oct 2026 07:28:00 GMT".
    // chrono tidak mengenal literal "GMT" sebagai offset, jadi parse sebagai
    // naive datetime lalu anggap UTC (nama hari tidak divalidasi).
    let rest = v.split_once(", ").map(|(_, r)| r).unwrap_or(v);
    let naive = chrono::NaiveDateTime::parse_from_str(rest, "%d %b %Y %H:%M:%S GMT").ok()?;
    let wait = naive.and_utc() - chrono::Utc::now();
    let secs = wait.num_seconds();
    if secs <= 0 {
        None
    } else {
        Some(Duration::from_secs(secs as u64))
    }
}

/// Ambil `Retry-After` dari error terstruktur; `None` bila tidak ada.
pub fn retry_after_of(err: &HermesError) -> Option<Duration> {
    match err {
        HermesError::ProviderHttp { retry_after, .. } => *retry_after,
        _ => None,
    }
}

/// Delay eksponensial deterministik untuk retry ke-`attempt` (1-based):
/// `min(1s * 2^(attempt-1), 30s)`.
pub fn backoff_base_delay(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(30);
    let secs = BASE_DELAY
        .as_secs()
        .saturating_mul(1u64 << shift)
        .min(MAX_DELAY.as_secs());
    Duration::from_secs(secs)
}

static JITTER_STATE: AtomicU64 = AtomicU64::new(0x9E3779B97F4A7C15);

/// Bilangan pseudo-acak dalam [0, 1): xorshift64* dengan seed dari
/// waktu + counter atomik. Tanpa dependensi `rand`, meniru pendekatan
/// `retry_utils.py` (seed dari `time.time_ns()` + counter).
fn unit_random() -> f64 {
    let tick = JITTER_STATE.fetch_add(0x9E3779B97F4A7C15, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x1234_5678);
    let mut x = nanos ^ tick;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    let r = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
    // 53 bit teratas -> presisi penuh f64.
    ((r >> 11) as f64) / ((1u64 << 53) as f64)
}

/// Delay backoff dengan jitter untuk retry ke-`attempt` (1-based):
/// `base + uniform[0, 0.5 * base]`, dibatasi 45 detik.
pub fn backoff_delay(attempt: u32) -> Duration {
    let base = backoff_base_delay(attempt);
    let jitter = Duration::from_secs_f64(unit_random() * 0.5 * base.as_secs_f64());
    base.saturating_add(jitter).min(MAX_TOTAL_DELAY)
}

/// Delay sebelum retry ke-`attempt`: hormati `Retry-After` bila ada
/// (dibatasi 300 detik), dan tidak kurang dari backoff biasa.
pub fn retry_delay(attempt: u32, err: &HermesError) -> Duration {
    let backoff = backoff_delay(attempt);
    match retry_after_of(err) {
        Some(ra) => backoff.max(ra.min(MAX_RETRY_AFTER)),
        None => backoff,
    }
}

/// Jalankan `op` dengan retry: hingga [`MAX_ATTEMPTS`] percobaan total, hanya
/// untuk error retryable, dengan jittered backoff (+ `Retry-After` bila ada)
/// di antara percobaan. Error non-retryable — atau bila percobaan habis —
/// langsung dikembalikan tanpa retry.
pub async fn with_retries<F, Fut, T>(mut op: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) if attempt < MAX_ATTEMPTS && classify_error(&e).is_retryable() => {
                let delay = retry_delay(attempt, &e);
                tracing::warn!(
                    "panggilan LLM gagal (percobaan {attempt}/{MAX_ATTEMPTS}): {e}; retry dalam {delay:?}"
                );
                tokio::time::sleep(delay).await;
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn http(status: u16) -> HermesError {
        HermesError::http(status, None, "boom")
    }

    #[test]
    fn classify_retryable_statuses() {
        for s in [408u16, 429, 500, 502, 503, 504, 599] {
            assert_eq!(
                classify_error(&http(s)),
                ErrorClass::Retryable,
                "status {s} harus retryable"
            );
        }
    }

    #[test]
    fn classify_non_retryable_statuses() {
        for s in [400u16, 401, 403, 404, 405, 409, 422] {
            assert_eq!(
                classify_error(&http(s)),
                ErrorClass::NonRetryable,
                "status {s} tidak boleh retryable"
            );
        }
    }

    #[test]
    fn classify_transport_errors_retryable() {
        assert_eq!(
            classify_error(&HermesError::transport(true, "request gagal: timed out")),
            ErrorClass::Retryable
        );
        assert_eq!(
            classify_error(&HermesError::transport(
                false,
                "request gagal: connection reset"
            )),
            ErrorClass::Retryable
        );
    }

    #[test]
    fn classify_string_fallback() {
        // Format string lama "HTTP NNN: ..." tetap terklasifikasi.
        assert_eq!(
            classify_error(&HermesError::Provider(
                "HTTP 503: Service Unavailable".into()
            )),
            ErrorClass::Retryable
        );
        assert_eq!(
            classify_error(&HermesError::Provider("bedrock HTTP 429: throttled".into())),
            ErrorClass::Retryable
        );
        assert_eq!(
            classify_error(&HermesError::Provider("HTTP 400: bad request".into())),
            ErrorClass::NonRetryable
        );
        // Timeout/koneksi tanpa status.
        assert_eq!(
            classify_error(&HermesError::Provider(
                "stream error: connection reset by peer".into()
            )),
            ErrorClass::Retryable
        );
        assert_eq!(
            classify_error(&HermesError::Provider(
                "request gagal: operation timed out".into()
            )),
            ErrorClass::Retryable
        );
        // Pesan tak dikenal: konservatif, tidak retry.
        assert_eq!(
            classify_error(&HermesError::Provider("sesuatu yang aneh".into())),
            ErrorClass::NonRetryable
        );
        // Error non-provider tidak pernah retry.
        assert_eq!(
            classify_error(&HermesError::Config("buruk".into())),
            ErrorClass::NonRetryable
        );
    }

    #[test]
    fn parse_retry_after_seconds() {
        assert_eq!(parse_retry_after("120"), Some(Duration::from_secs(120)));
        assert_eq!(parse_retry_after("  7 "), Some(Duration::from_secs(7)));
        assert_eq!(parse_retry_after("0"), Some(Duration::from_secs(0)));
        assert_eq!(parse_retry_after(""), None);
        assert_eq!(parse_retry_after("nonsense"), None);
    }

    #[test]
    fn parse_retry_after_http_date() {
        // Tanggal di masa depan -> durasi tunggu mendekati 3600s.
        let future = (chrono::Utc::now() + chrono::Duration::seconds(3600))
            .format("%a, %d %b %Y %H:%M:%S GMT")
            .to_string();
        let d = parse_retry_after(&future).expect("harus ter-parse");
        assert!(d >= Duration::from_secs(3590) && d <= Duration::from_secs(3600));
        // Tanggal yang sudah lewat -> None.
        let past = (chrono::Utc::now() - chrono::Duration::seconds(60))
            .format("%a, %d %b %Y %H:%M:%S GMT")
            .to_string();
        assert_eq!(parse_retry_after(&past), None);
    }

    #[test]
    fn backoff_base_shape() {
        assert_eq!(backoff_base_delay(1), Duration::from_secs(1));
        assert_eq!(backoff_base_delay(2), Duration::from_secs(2));
        assert_eq!(backoff_base_delay(3), Duration::from_secs(4));
        assert_eq!(backoff_base_delay(4), Duration::from_secs(8));
        assert_eq!(backoff_base_delay(5), Duration::from_secs(16));
        // Dibatasi 30s.
        assert_eq!(backoff_base_delay(6), Duration::from_secs(30));
        assert_eq!(backoff_base_delay(100), Duration::from_secs(30));
        // Monotonik naik.
        let mut prev = Duration::ZERO;
        for a in 1..=10 {
            let d = backoff_base_delay(a);
            assert!(d >= prev, "attempt {a}");
            prev = d;
        }
    }

    #[test]
    fn backoff_jitter_bounded() {
        for attempt in 1..=8 {
            for _ in 0..50 {
                let base = backoff_base_delay(attempt);
                let d = backoff_delay(attempt);
                assert!(d >= base, "attempt {attempt}: {d:?} < base {base:?}");
                assert!(
                    d <= base.mul_f64(1.5),
                    "attempt {attempt}: {d:?} melebihi 1.5x base"
                );
                assert!(
                    d <= MAX_TOTAL_DELAY,
                    "attempt {attempt}: {d:?} melebihi batas"
                );
            }
        }
    }

    #[test]
    fn retry_delay_honors_retry_after() {
        let err = HermesError::http(429, Some(Duration::from_secs(10)), "slow down");
        let d = retry_delay(1, &err);
        assert!(
            d >= Duration::from_secs(10),
            "harus menghormati Retry-After: {d:?}"
        );
        // Retry-After raksasa dibatasi 300s.
        let err = HermesError::http(503, Some(Duration::from_secs(10_000)), "down");
        let d = retry_delay(1, &err);
        assert!(d <= MAX_RETRY_AFTER, "Retry-After harus dibatasi: {d:?}");
        // Tanpa Retry-After -> backoff biasa.
        let d = retry_delay(1, &http(500));
        assert!(d >= Duration::from_secs(1) && d <= Duration::from_secs(2));
    }

    #[tokio::test]
    async fn with_retries_succeeds_after_transient_failures() {
        use std::sync::Mutex;
        let calls = Mutex::new(0u32);
        let res = with_retries(|| async {
            let mut n = calls.lock().unwrap();
            *n += 1;
            let attempt = *n;
            drop(n);
            if attempt < 3 {
                Err::<u32, HermesError>(http(503))
            } else {
                Ok(attempt)
            }
        })
        .await;
        assert_eq!(res.unwrap(), 3);
        assert_eq!(*calls.lock().unwrap(), 3);
    }

    #[tokio::test]
    async fn with_retries_stops_on_non_retryable() {
        use std::sync::Mutex;
        let calls = Mutex::new(0u32);
        let res = with_retries(|| async {
            *calls.lock().unwrap() += 1;
            Err::<u32, HermesError>(http(400))
        })
        .await;
        assert!(res.is_err());
        assert_eq!(*calls.lock().unwrap(), 1, "400 tidak boleh di-retry");
    }

    #[tokio::test]
    async fn with_retries_gives_up_after_max_attempts() {
        use std::sync::Mutex;
        let calls = Mutex::new(0u32);
        let res = with_retries(|| async {
            *calls.lock().unwrap() += 1;
            Err::<u32, HermesError>(http(500))
        })
        .await;
        assert!(res.is_err());
        assert_eq!(*calls.lock().unwrap(), MAX_ATTEMPTS);
    }
}
