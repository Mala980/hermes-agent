use hermes_core::{classify_error, HermesError};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Kumpulan API key dengan rotasi round-robin dan cooldown otomatis saat gagal.
/// Meniru `credential_pool.py` / key rotation pada Hermes asli.
#[derive(Debug)]
pub struct CredentialPool {
    keys: Vec<String>,
    cooldown_until: Vec<Option<Instant>>,
    cursor: usize,
    cooldown: Duration,
}

impl CredentialPool {
    pub fn new(keys: Vec<String>) -> Self {
        let n = keys.len();
        Self {
            keys,
            cooldown_until: vec![None; n],
            cursor: 0,
            cooldown: Duration::from_secs(60),
        }
    }

    pub fn from_env(var: Option<&str>, extra: Vec<String>) -> Self {
        let mut keys = extra;
        if let Some(v) = var {
            if let Ok(val) = std::env::var(v) {
                // Dukung beberapa key dipisah koma.
                keys.extend(
                    val.split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty()),
                );
            }
        }
        // Deduplikasi sambil menjaga urutan.
        let mut seen = std::collections::HashSet::new();
        keys.retain(|k| seen.insert(k.clone()));
        Self::new(keys)
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Ambil key berikutnya yang tidak sedang cooldown.
    pub fn next_key(&mut self) -> Option<String> {
        if self.keys.is_empty() {
            return None;
        }
        let now = Instant::now();
        for _ in 0..self.keys.len() {
            let i = self.cursor % self.keys.len();
            self.cursor = self.cursor.wrapping_add(1);
            let cooled = self.cooldown_until[i].map(|t| now < t).unwrap_or(false);
            if !cooled {
                return Some(self.keys[i].clone());
            }
        }
        // Semua cooldown: paksa pakai yang cooldown-nya paling cepat habis.
        let i = self
            .cooldown_until
            .iter()
            .enumerate()
            .min_by_key(|(_, t)| *t)
            .map(|(i, _)| i)
            .unwrap_or(0);
        Some(self.keys[i].clone())
    }

    /// Laporkan kegagalan untuk `key`. Key hanya masuk cooldown bila error-nya
    /// retryable (429/5xx/timeout) — error deterministik 4xx (400/401/403/404)
    /// tidak memicu cooldown karena ganti key tidak akan membantu.
    pub fn report_failure(&mut self, key: &str, err: &HermesError) {
        if !classify_error(err).is_retryable() {
            return;
        }
        if let Some(i) = self.keys.iter().position(|k| k == key) {
            self.cooldown_until[i] = Some(Instant::now() + self.cooldown);
        }
    }

    pub fn report_success(&mut self, key: &str) {
        if let Some(i) = self.keys.iter().position(|k| k == key) {
            self.cooldown_until[i] = None;
        }
    }
}

/// Wrapper thread-safe untuk dipakai provider async.
#[derive(Debug)]
pub struct SharedPool {
    inner: Mutex<CredentialPool>,
}

impl SharedPool {
    pub fn new(pool: CredentialPool) -> Self {
        Self {
            inner: Mutex::new(pool),
        }
    }

    pub fn next_key(&self) -> Option<String> {
        self.inner.lock().ok()?.next_key()
    }

    pub fn report_failure(&self, key: &str, err: &HermesError) {
        if let Ok(mut p) = self.inner.lock() {
            p.report_failure(key, err);
        }
    }

    pub fn report_success(&self, key: &str) {
        if let Ok(mut p) = self.inner.lock() {
            p.report_success(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_robin_and_cooldown() {
        let mut pool = CredentialPool::new(vec!["a".into(), "b".into()]);
        assert_eq!(pool.next_key().as_deref(), Some("a"));
        assert_eq!(pool.next_key().as_deref(), Some("b"));
        pool.report_failure("a", &HermesError::http(503, None, "down"));
        // "a" di-skip karena cooldown.
        assert_eq!(pool.next_key().as_deref(), Some("b"));
        pool.report_success("a");
        assert_eq!(pool.next_key().as_deref(), Some("a"));
    }

    #[test]
    fn no_cooldown_on_deterministic_4xx() {
        let mut pool = CredentialPool::new(vec!["a".into(), "b".into()]);
        for status in [400u16, 401, 403, 404] {
            pool.report_failure("a", &HermesError::http(status, None, "client error"));
        }
        // "a" TIDAK di-cooldown: round-robin tetap a, b, a, b, ...
        // (bila di-cooldown, "b" akan muncul dua kali berturut-turut).
        let seq: Vec<_> = (0..4).map(|_| pool.next_key()).collect();
        assert_eq!(
            seq,
            vec![
                Some("a".to_string()),
                Some("b".to_string()),
                Some("a".to_string()),
                Some("b".to_string()),
            ],
            "4xx deterministik tidak boleh memicu cooldown"
        );
    }

    #[test]
    fn cooldown_on_retryable_failures() {
        let mut pool = CredentialPool::new(vec!["a".into(), "b".into()]);
        pool.next_key(); // -> "a"
        pool.report_failure("a", &HermesError::http(429, None, "rate limited"));
        assert_eq!(pool.next_key().as_deref(), Some("b"));

        let mut pool = CredentialPool::new(vec!["a".into(), "b".into()]);
        pool.next_key(); // -> "a"
        pool.report_failure("a", &HermesError::transport(true, "timed out"));
        assert_eq!(pool.next_key().as_deref(), Some("b"));
    }

    #[test]
    fn empty_pool() {
        let mut pool = CredentialPool::new(vec![]);
        assert!(pool.next_key().is_none());
    }
}
