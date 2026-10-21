//! Helper allowlist fail-closed bersama untuk gateway platform.
//!
//! Pola mengikuti `telegram.rs`: setiap platform membaca
//! `<PLATFORM>_ALLOWED_USERS` digabung (union) dengan `GATEWAY_ALLOWED_USERS`
//! (dipisah koma; `"*"` = semua pengguna). Bila keduanya kosong, tidak ada
//! pengguna yang diizinkan (fail-closed).
use std::collections::HashSet;

/// Baca satu env allowlist CSV menjadi set ID (kosong bila env tak diset).
pub(crate) fn read_allow_csv(var: &str) -> HashSet<String> {
    std::env::var(var)
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Benar bila env bernilai true/1/yes (case-insensitive).
pub(crate) fn env_truthy(var: &str) -> bool {
    matches!(
        std::env::var(var)
            .unwrap_or_default()
            .trim()
            .to_lowercase()
            .as_str(),
        "true" | "1" | "yes"
    )
}

/// Kunci global untuk tes yang menyentuh env allowlist.
///
/// Tes-tes allowlist di semua modul platform menyentuh env yang sama
/// (`GATEWAY_ALLOWED_USERS`, `GATEWAY_ALLOW_ALL_USERS`, dsb.) dan Rust
/// menjalankan tes antar-modul secara paralel, jadi semuanya memakai satu
/// kunci ini agar tidak saling menginjak.
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Simpan nilai lama lalu set (atau hapus bila `None`) sebuah env var.
#[cfg(test)]
pub(crate) fn set_var(name: &str, v: Option<&str>) -> Option<String> {
    let old = std::env::var(name).ok();
    match v {
        Some(s) => std::env::set_var(name, s),
        None => std::env::remove_var(name),
    }
    old
}

/// Kembalikan sebuah env var ke nilai lamanya.
#[cfg(test)]
pub(crate) fn restore_var(name: &str, old: Option<String>) {
    match old {
        Some(s) => std::env::set_var(name, s),
        None => std::env::remove_var(name),
    }
}

/// Kosongkan semua var auth yang disebut; kembalikan nilai lama untuk restore.
#[cfg(test)]
pub(crate) fn clear_auth_env(vars: &[&'static str]) -> Vec<(&'static str, Option<String>)> {
    vars.iter().map(|&n| (n, set_var(n, None))).collect()
}

/// Kembalikan var auth ke nilai lama.
#[cfg(test)]
pub(crate) fn restore_auth_env(saved: Vec<(&'static str, Option<String>)>) {
    for (name, old) in saved {
        restore_var(name, old);
    }
}
