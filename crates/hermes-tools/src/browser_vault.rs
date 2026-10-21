//! Tool `browser_vault`: vault kredensial model-blind untuk autofill browser.
//!
//! Port dari `tools/browser_vault_tool.py` + `agent/vault_store.py` +
//! `agent/vault_backends/` (original). Satu tool model-facing dengan lima aksi:
//!
//! - `list` — daftar handle + metadata (identifier login BUKAN rahasia; password
//!   tidak pernah dikembalikan).
//! - `fill` — isi kolom password/kartu/alamat halaman SAAT INI dari sebuah handle.
//!   Rahasia di-resolve server-side, origin halaman harus COCOK PERSIS dengan origin
//!   terikat item (dicek sebelum dan ditegaskan ulang sinkron di dalam skrip fill),
//!   injeksi hanya lewat WebSocket CDP (tidak pernah argv), dan hasil tool hanya
//!   melaporkan `{filled_fields, kind, origin, success}`.
//! - `save_login` — butuh prompt bertopeng ke pengguna (tidak tersedia di port ini).
//! - `enter_code` — kode 2FA: dibuat server-side dari seed TOTP tersimpan bila ada,
//!   selebihnya butuh prompt pengguna (tidak tersedia di port ini).
//! - `unlock` — buka kunci manajer eksternal (butuh prompt master password;
//!   tidak tersedia di port ini → fail-closed).
//!
//! # Keychain → file: keputusan desain
//!
//! Original memakai OS keychain untuk manajer eksternal dan file terenkripsi
//! Fernet (0600) untuk vault lokal. Port ini TIDAK memakai OS keychain:
//!
//! - Vault lokal = file JSON terenkripsi di `<HERMES_HOME>/vault/`:
//!   direktori 0700, `vault.key` 0600 (64 byte acak: 32 byte kunci enkripsi +
//!   32 byte kunci MAC), `vault.json.enc` 0600 (envelope JSON: nonce heksa +
//!   payload heksa = ciphertext \|\| tag).
//! - Enkripsi = encrypt-then-MAC dengan konstruksi standar: stream cipher
//!   HMAC-SHA256-CTR + tag HMAC-SHA256 (encrypt-then-MAC, verifikasi
//!   constant-time). Bukan AES-GCM/Fernet (tidak ada crate AES yang tersedia
//!   offline); properti keamanannya setara untuk model ancaman file lokal.
//! - Berbeda dengan OS keychain: tidak ada penyimpanan berbasis hardware,
//!   tidak ada ACL per-aplikasi/biometrik, dan kunci mentah ada di filesystem
//!   (terlindungi hanya oleh permission Unix). Jangan menyalin direktori vault
//!   ke host bersama.
//! - TIDAK ada sinkronisasi cloud dan TIDAK ada fitur jaringan — sesuai batas
//!   tugas ("jangan mengarang sinkronisasi cloud atau fitur network").
//! - Manajer eksternal (1Password via CLI `op`, Bitwarden via CLI `bw`)
//!   dipertahankan sebagai backend baca-dengan-unlock seperti original, tanpa
//!   menyimpan token sesi di disk (hanya di memori proses).
//!
//! Batasan yang diakui vs original: prompt bertopeng pengguna (`save_login`,
//! `unlock`, `enter_code` tanpa seed) tidak ada di runtime Rust ini sehingga
//! aksi-aksi itu fail-closed dengan `error_type` yang diketik; classifier
//! kontrol login lengkap (`vault_login_classifier.py`) disederhanakan menjadi
//! pencocokan token autocomplete WHATWG; tidak ada registry redaksi global —
//! sebagai gantinya hasil tool tidak pernah memuat nilai rahasia dan pesan
//! error digosok via [`scrub_secret_from_text`].

use crate::schema::*;
use async_trait::async_trait;
use hermes_core::{
    HermesConfig, HermesError, Result, RiskLevel, Tool, ToolContext, ToolEffect, ToolResult,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

// ---------------------------------------------------------------------------
// Kriptografi internal (pure-Rust, tanpa crate tambahan)
// ---------------------------------------------------------------------------

/// Primitif kriptografi kecil khusus modul ini.
///
/// - SHA-1: implementasi langsung (hanya dipakai untuk TOTP, yang default
///   RFC 6238-nya SHA-1). Bukan untuk enkripsi.
/// - HMAC-SHA256: dari crate `hmac` + `sha2` (workspace).
/// - Enkripsi vault: HMAC-SHA256-CTR + tag HMAC-SHA256 (encrypt-then-MAC).
mod crypto {
    /// SHA-1 (FIPS 180-4). Hanya untuk TOTP; JANGAN dipakai untuk enkripsi/MAC.
    pub fn sha1(data: &[u8]) -> [u8; 20] {
        let mut h: [u32; 5] = [
            0x6745_2301,
            0xEFCD_AB89,
            0x98BA_DCFE,
            0x1032_5476,
            0xC3D2_E1F0,
        ];
        let bit_len = (data.len() as u64).wrapping_mul(8);
        let mut msg = data.to_vec();
        msg.push(0x80);
        while msg.len() % 64 != 56 {
            msg.push(0);
        }
        msg.extend_from_slice(&bit_len.to_be_bytes());
        for chunk in msg.as_chunks::<64>().0 {
            let mut w = [0u32; 80];
            for (i, slot) in w.iter_mut().enumerate().take(16) {
                let o = i * 4;
                *slot = u32::from_be_bytes([chunk[o], chunk[o + 1], chunk[o + 2], chunk[o + 3]]);
            }
            for i in 16..80 {
                w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
            }
            let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
            for (i, &wi) in w.iter().enumerate() {
                let (f, k) = match i {
                    0..=19 => ((b & c) | (!b & d), 0x5A82_7999),
                    20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                    40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                    _ => (b ^ c ^ d, 0xCA62_C1D6),
                };
                let tmp = a
                    .rotate_left(5)
                    .wrapping_add(f)
                    .wrapping_add(e)
                    .wrapping_add(k)
                    .wrapping_add(wi);
                e = d;
                d = c;
                c = b.rotate_left(30);
                b = a;
                a = tmp;
            }
            h[0] = h[0].wrapping_add(a);
            h[1] = h[1].wrapping_add(b);
            h[2] = h[2].wrapping_add(c);
            h[3] = h[3].wrapping_add(d);
            h[4] = h[4].wrapping_add(e);
        }
        let mut out = [0u8; 20];
        for (i, v) in h.iter().enumerate() {
            out[4 * i..4 * i + 4].copy_from_slice(&v.to_be_bytes());
        }
        out
    }

    /// HMAC-SHA1 manual (konstruksi standar RFC 2104).
    pub fn hmac_sha1(key: &[u8], msg: &[u8]) -> [u8; 20] {
        let mut kb = [0u8; 64];
        if key.len() > 64 {
            kb[..20].copy_from_slice(&sha1(key));
        } else {
            kb[..key.len()].copy_from_slice(key);
        }
        let mut ipad = [0x36u8; 64];
        let mut opad = [0x5cu8; 64];
        for i in 0..64 {
            ipad[i] ^= kb[i];
            opad[i] ^= kb[i];
        }
        let mut inner = ipad.to_vec();
        inner.extend_from_slice(msg);
        let inner_hash = sha1(&inner);
        let mut outer = opad.to_vec();
        outer.extend_from_slice(&inner_hash);
        sha1(&outer)
    }

    /// HMAC-SHA256 via crate workspace.
    pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        let mut m = Hmac::<Sha256>::new_from_slice(key)
            .expect("HMAC-SHA256 menerima kunci dengan panjang berapa pun");
        m.update(msg);
        m.finalize().into_bytes().into()
    }

    /// Byte acak dari sumber entropi OS.
    pub fn random_bytes(n: usize) -> std::io::Result<Vec<u8>> {
        use std::io::Read;
        let mut f = std::fs::File::open("/dev/urandom")?;
        let mut buf = vec![0u8; n];
        f.read_exact(&mut buf)?;
        Ok(buf)
    }

    /// Perbandingan constant-time untuk tag MAC.
    pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
        if a.len() != b.len() {
            return false;
        }
        let mut acc = 0u8;
        for (x, y) in a.iter().zip(b.iter()) {
            acc |= x ^ y;
        }
        acc == 0
    }

    /// Keystream HMAC-SHA256-CTR: blok_i = HMAC(k_enc, nonce || le64(i)).
    pub fn keystream(enc_key: &[u8], nonce: &[u8; 16], len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        let mut ctr: u64 = 0;
        while out.len() < len {
            let mut msg = Vec::with_capacity(24);
            msg.extend_from_slice(nonce);
            msg.extend_from_slice(&ctr.to_le_bytes());
            out.extend_from_slice(&hmac_sha256(enc_key, &msg));
            ctr += 1;
        }
        out.truncate(len);
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn sha1_vektor_rfc() {
            // "abc" -> a9993e364706816aba3e25717850c26c9cd0d89d
            let d = sha1(b"abc");
            assert_eq!(hex_encode(&d), "a9993e364706816aba3e25717850c26c9cd0d89d");
        }

        #[test]
        fn hmac_sha1_vektor_rfc2202() {
            // key = 0x0b * 20, data = "Hi There" -> b617318655057264e28bc0b6fb378c8ef146be00
            let mac = hmac_sha1(&[0x0bu8; 20], b"Hi There");
            assert_eq!(hex_encode(&mac), "b617318655057264e28bc0b6fb378c8ef146be00");
        }

        #[test]
        fn keystream_deterministik_dan_sepanjang_minta() {
            let a = keystream(b"kunci-32-byte-untuk-tes-12345678", &[7u8; 16], 100);
            let b = keystream(b"kunci-32-byte-untuk-tes-12345678", &[7u8; 16], 100);
            assert_eq!(a, b);
            assert_eq!(a.len(), 100);
            let c = keystream(b"kunci-32-byte-untuk-tes-12345678", &[8u8; 16], 100);
            assert_ne!(a, c);
        }

        fn hex_encode(b: &[u8]) -> String {
            b.iter().map(|x| format!("{x:02x}")).collect()
        }
    }
}

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

/// Error vault yang aman disurface (tidak pernah memuat nilai rahasia).
#[derive(Debug, Clone)]
pub struct VaultError(pub String);

impl std::fmt::Display for VaultError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for VaultError {}

/// Backend terkunci untuk sesi ini; surface harus meminta unlock.
#[derive(Debug, Clone)]
pub struct UnlockRequired {
    pub backend: String,
}

impl std::fmt::Display for UnlockRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} terkunci", self.backend)
    }
}

impl std::error::Error for UnlockRequired {}

fn verr(msg: impl Into<String>) -> VaultError {
    VaultError(msg.into())
}

// ---------------------------------------------------------------------------
// Helper encoding
// ---------------------------------------------------------------------------

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

fn hex_decode(s: &str) -> Result<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return Err(HermesError::tool(
            "browser_vault",
            "heksa vault panjangnya ganjil",
        ));
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let b = s.as_bytes();
    for i in (0..b.len()).step_by(2) {
        let hi = hex_val(b[i])?;
        let lo = hex_val(b[i + 1])?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn hex_val(c: u8) -> Result<u8> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(HermesError::tool(
            "browser_vault",
            "karakter heksa vault tidak valid",
        )),
    }
}

// ---------------------------------------------------------------------------
// Normalisasi origin / OTP (port dari vault_store.py)
// ---------------------------------------------------------------------------

/// Normalisasi URL/origin ke `skema://host[:port]`.
///
/// Port default (80 http, 443 https) dibuang sehingga
/// `https://example.com` dan `https://example.com:443` sama.
pub fn normalize_origin(url_or_origin: &str) -> std::result::Result<String, VaultError> {
    let value = url_or_origin.trim();
    if value.is_empty() {
        return Err(verr("origin wajib diisi"));
    }
    let (scheme, rest) = value
        .split_once("://")
        .ok_or_else(|| verr(format!("origin harus menyertakan skema (dapat {value:?})")))?;
    let scheme = scheme.to_lowercase();
    if scheme.is_empty() {
        return Err(verr(format!("tidak bisa mem-parse origin dari {value:?}")));
    }
    // host = sampai '/', '?', atau '#'; buang userinfo.
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .rsplit('@')
        .next()
        .unwrap_or("");
    if authority.is_empty() {
        return Err(verr(format!("tidak bisa mem-parse origin dari {value:?}")));
    }
    let (host_part, port_part) = if let Some(stripped) = authority.strip_prefix('[') {
        // IPv6 literal [::1]:port
        match stripped.split_once("]:") {
            Some((h, p)) => (format!("[{h}]"), Some(p)),
            None => (authority.to_string(), None),
        }
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) if !h.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => {
                (h.to_string(), Some(p))
            }
            _ => (authority.to_string(), None),
        }
    };
    let host = host_part.to_lowercase();
    if host.is_empty() {
        return Err(verr(format!("tidak bisa mem-parse origin dari {value:?}")));
    }
    let port: Option<u16> = match port_part {
        Some(p) => Some(
            p.parse()
                .map_err(|_| verr(format!("port tidak valid di origin {value:?}")))?,
        ),
        None => None,
    };
    let default_port = match scheme.as_str() {
        "http" => Some(80),
        "https" => Some(443),
        _ => None,
    };
    match port {
        Some(p) if Some(p) == default_port => Ok(format!("{scheme}://{host}")),
        Some(p) => Ok(format!("{scheme}://{host}:{p}")),
        None => Ok(format!("{scheme}://{host}")),
    }
}

/// Terima seed base32 mentah atau URI `otpauth://totp/...`; kembalikan bentuk
/// kanonis: seed base32 huruf besar, diikuti `|digit|periode|algo` HANYA bila
/// menyimpang dari default RFC 6238 (6/30/SHA1).
pub fn normalize_otp_secret(value: &str) -> std::result::Result<String, VaultError> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(String::new());
    }
    let mut digits = 6u32;
    let mut period = 30u64;
    let mut algo = "SHA1".to_string();
    let mut seed_src = value;
    if value.to_lowercase().starts_with("otpauth://") {
        let after_scheme = &value["otpauth://".len()..];
        // otpauth://totp/<label>?secret=... — netloc berhenti di '/' atau '?'.
        let host_end = after_scheme
            .find(&['/', '?'][..])
            .unwrap_or(after_scheme.len());
        let netloc = &after_scheme[..host_end];
        let query = after_scheme.split_once('?').map(|(_, q)| q).unwrap_or("");
        if netloc.to_lowercase() != "totp" {
            return Err(verr(
                "hanya tautan otpauth://totp yang didukung (HOTP berbasis counter tidak)",
            ));
        }
        let mut secret_q = "";
        for pair in query.split('&') {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            match k.to_lowercase().as_str() {
                "secret" => secret_q = v,
                "digits" => {
                    digits = v
                        .parse()
                        .map_err(|_| verr("otpauth:// digits/period harus integer"))?
                }
                "period" => {
                    period = v
                        .parse()
                        .map_err(|_| verr("otpauth:// digits/period harus integer"))?
                }
                "algorithm" => algo = v.to_uppercase().replace('-', ""),
                _ => {}
            }
        }
        seed_src = secret_q;
        if ![6, 7, 8].contains(&digits)
            || period == 0
            || !["SHA1", "SHA256", "SHA512"].contains(&algo.as_str())
        {
            return Err(verr(
                "parameter otpauth:// tidak didukung (digits 6-8, period > 0, SHA1/SHA256/SHA512)",
            ));
        }
    }
    let seed: String = seed_src
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect::<String>()
        .to_uppercase()
        .trim_end_matches('=')
        .to_string();
    if seed.is_empty() || seed.chars().any(|c| !matches!(c, 'A'..='Z' | '2'..='7')) {
        return Err(verr(
            "kunci authenticator harus berupa secret base32 atau URI otpauth://",
        ));
    }
    if (digits, period, algo.as_str()) == (6, 30, "SHA1") {
        Ok(seed)
    } else {
        Ok(format!("{seed}|{digits}|{period}|{algo}"))
    }
}

fn base32_decode(s: &str) -> std::result::Result<Vec<u8>, VaultError> {
    let mut bits: u32 = 0;
    let mut nbits = 0u8;
    let mut out = Vec::new();
    for c in s.chars() {
        let v = match c {
            'A'..='Z' => c as u8 - b'A',
            '2'..='7' => c as u8 - b'2' + 26,
            _ => return Err(verr("karakter base32 tidak valid")),
        } as u32;
        bits = (bits << 5) | v;
        nbits += 5;
        if nbits >= 8 {
            nbits -= 8;
            out.push((bits >> nbits) as u8);
            bits &= (1 << nbits) - 1;
        }
    }
    Ok(out)
}

/// TOTP RFC 6238 untuk seed tersimpan (lihat [`normalize_otp_secret`] untuk
/// bentuk `seed|digits|period|algo`). `at` = epoch detik (None = sekarang).
pub fn totp_now(seed: &str, at: Option<f64>) -> std::result::Result<String, VaultError> {
    let (seed_part, digits, period, algo) = if let Some((s, d, p, a)) = split4(seed) {
        let digits: u32 = d.parse().map_err(|_| verr("digits TOTP tidak valid"))?;
        let period: u64 = p.parse().map_err(|_| verr("period TOTP tidak valid"))?;
        (s, digits, period, a)
    } else {
        (seed, 6, 30, "SHA1")
    };
    let key = base32_decode(&seed_part.to_uppercase())?;
    let now = at.unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0)
    });
    let counter = (now / period as f64).floor() as u64;
    let msg = counter.to_be_bytes();
    let digest: Vec<u8> = match algo {
        "SHA256" => crypto::hmac_sha256(&key, &msg).to_vec(),
        "SHA512" => {
            use hmac::{Hmac, Mac};
            use sha2::Sha512;
            let mut m = Hmac::<Sha512>::new_from_slice(&key)
                .map_err(|_| verr("kunci HMAC-SHA512 tidak valid"))?;
            m.update(&msg);
            m.finalize().into_bytes().to_vec()
        }
        _ => crypto::hmac_sha1(&key, &msg).to_vec(),
    };
    let offset = (digest[digest.len() - 1] & 0x0F) as usize;
    let code = ((u32::from_be_bytes([
        digest[offset],
        digest[offset + 1],
        digest[offset + 2],
        digest[offset + 3],
    ]) & 0x7FFF_FFFF)
        % 10u32.pow(digits)) as u64;
    Ok(format!("{code:0>width$}", width = digits as usize))
}

fn split4(s: &str) -> Option<(&str, &str, &str, &str)> {
    let mut it = s.split('|');
    let a = it.next()?;
    let b = it.next()?;
    let c = it.next()?;
    let d = it.next()?;
    if it.next().is_some() {
        return None;
    }
    Some((a, b, c, d))
}

/// Gosok nilai rahasia dari sebuah string (mis. pesan exception) sebelum
/// disurface. Perbandingan substring eksak, case-sensitive.
pub fn scrub_secret_from_text(text: &str, secret: &HashMap<String, String>) -> String {
    let mut scrubbed = text.to_string();
    for value in secret.values() {
        if value.len() >= 3 && scrubbed.contains(value) {
            scrubbed = scrubbed.replace(value, "[REDACTED]");
        }
    }
    // Pola key=value yang umum ikut digosok.
    let re = regex::Regex::new("(?i)(password['\"]?\\s*[:=]\\s*)\\S+").unwrap();
    re.replace_all(&scrubbed, "${1}[REDACTED]").into_owned()
}

// ---------------------------------------------------------------------------
// VaultStore: vault lokal terenkripsi di <HERMES_HOME>/vault/
// ---------------------------------------------------------------------------

/// Jenis item vault (sama seperti original).
pub const VAULT_KINDS: &[&str] = &["login", "payment", "address"];
const LOGIN_IDENTIFIER_TYPES: &[&str] = &["email", "phone", "username"];

/// Nama field payload kanonis per jenis non-login. Masing-masing dipetakan ke
/// token autocomplete WHATWG yang menjadi target fill browser.
pub const PAYMENT_FIELDS: &[(&str, &str)] = &[
    ("card_number", "cc-number"),
    ("cardholder_name", "cc-name"),
    ("exp_month", "cc-exp-month"),
    ("exp_year", "cc-exp-year"),
    ("cvc", "cc-csc"),
    ("billing_postal_code", "postal-code"),
];
pub const ADDRESS_FIELDS: &[(&str, &str)] = &[
    ("address_line1", "address-line1"),
    ("address_line2", "address-line2"),
    ("city", "address-level2"),
    ("state", "address-level1"),
    ("postal_code", "postal-code"),
    ("country", "country-name"),
];
const PAYMENT_REQUIRED: &[&str] = &["card_number", "exp_month", "exp_year", "cvc"];
const ADDRESS_REQUIRED: &[&str] = &["address_line1", "city", "postal_code", "country"];

/// Tampilan metadata-only sebuah item vault. Tidak pernah memuat nilai rahasia.
///
/// Untuk `kind="login"`, identifier (email/username/telepon) adalah metadata,
/// bukan rahasia: agen boleh melihat dan mengetiknya sendiri. Hanya password
/// yang menjadi rahasia vault.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VaultItemMeta {
    pub id: String,
    pub kind: String,
    pub label: String,
    pub origin: Option<String>,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub identifier_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub identifier: Option<String>,
    #[serde(default)]
    pub has_otp: bool,
    /// Semua origin yang diikat ke item ini (backend manajer saja;
    /// `origin` adalah yang pertama/primer). Pencocokan fill tetap exact-origin
    /// terhadap daftar ini — tidak ada inferensi wildcard/subdomain.
    #[serde(default)]
    pub allowed_origins: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ItemRecord {
    id: String,
    kind: String,
    label: String,
    origin: Option<String>,
    created_at: String,
    identifier_type: Option<String>,
    identifier: Option<String>,
    secret: HashMap<String, String>,
}

/// Envelope file vault. Format terdokumentasi:
/// `{"format":"hermes-vault/1","nonce":"<32 heksa>","payload":"<heksa>"}`
/// dengan payload = ciphertext || tag(32 byte), ciphertext = plaintext XOR
/// keystream HMAC-SHA256-CTR, tag = HMAC-SHA256(k_mac, nonce || ciphertext).
#[derive(Debug, Serialize, Deserialize)]
struct VaultEnvelope {
    format: String,
    nonce: String,
    payload: String,
}

/// Kunci untuk serialisasi siklus read-modify-write di dalam proses.
///
/// Ditambah penulisan atomik (file temp + rename) sehingga pembaca tidak
/// pernah melihat tulisan setengah jalan. Lintas proses: last-writer-wins
/// (terdokumentasi; original memakai flock — primitif itu tidak tersedia
/// tanpa crate tambahan di port ini).
static STORE_LOCK: Mutex<()> = Mutex::new(());

/// Vault terenkripsi berbasis file, dicakup per-profil via HERMES_HOME.
pub struct VaultStore {
    base: PathBuf,
}

impl VaultStore {
    pub fn new(base_dir: PathBuf) -> Self {
        Self { base: base_dir }
    }

    /// Vault default: `<HERMES_HOME>/vault` (konvensi `HermesConfig::home_dir`).
    pub fn default_store() -> Self {
        let mut dir = std::env::var("HERMES_VAULT_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| HermesConfig::home_dir());
        if std::env::var("HERMES_VAULT_DIR").is_err() {
            dir = dir.join("vault");
        }
        Self::new(dir)
    }

    fn vault_path(&self) -> PathBuf {
        self.base.join("vault.json.enc")
    }

    fn key_path(&self) -> PathBuf {
        self.base.join("vault.key")
    }

    fn ensure_dir(&self) -> Result<()> {
        std::fs::create_dir_all(&self.base).map_err(|e| {
            HermesError::tool("browser_vault", format!("gagal membuat dir vault: {e}"))
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o700);
            let _ = std::fs::set_permissions(&self.base, perms);
        }
        Ok(())
    }

    /// Muat (atau buat) kunci 64 byte: 32 byte k_enc || 32 byte k_mac.
    fn load_key(&self) -> Result<([u8; 32], [u8; 32])> {
        self.ensure_dir()?;
        let path = self.key_path();
        let raw: Vec<u8> = if path.exists() {
            std::fs::read(&path).map_err(|e| {
                HermesError::tool("browser_vault", format!("gagal membaca kunci vault: {e}"))
            })?
        } else {
            let fresh = crypto::random_bytes(64).map_err(|e| {
                HermesError::tool("browser_vault", format!("gagal membuat kunci vault: {e}"))
            })?;
            // O_EXCL implisit: create_new gagal bila file sudah ada (race aman).
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .map_err(|e| {
                    HermesError::tool("browser_vault", format!("gagal menulis kunci vault: {e}"))
                })?;
            f.write_all(&fresh).map_err(|e| {
                HermesError::tool("browser_vault", format!("gagal menulis kunci vault: {e}"))
            })?;
            fresh
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        if raw.len() != 64 {
            return Err(HermesError::tool(
                "browser_vault",
                "file kunci vault rusak (panjang bukan 64 byte)",
            ));
        }
        let mut k_enc = [0u8; 32];
        let mut k_mac = [0u8; 32];
        k_enc.copy_from_slice(&raw[..32]);
        k_mac.copy_from_slice(&raw[32..]);
        Ok((k_enc, k_mac))
    }

    fn encrypt(&self, plaintext: &[u8]) -> Result<VaultEnvelope> {
        let (k_enc, k_mac) = self.load_key()?;
        let nonce_raw = crypto::random_bytes(16)
            .map_err(|e| HermesError::tool("browser_vault", format!("gagal membuat nonce: {e}")))?;
        let mut nonce = [0u8; 16];
        nonce.copy_from_slice(&nonce_raw);
        let ks = crypto::keystream(&k_enc, &nonce, plaintext.len());
        let ciphertext: Vec<u8> = plaintext
            .iter()
            .zip(ks.iter())
            .map(|(p, k)| p ^ k)
            .collect();
        let mut mac_msg = Vec::with_capacity(16 + ciphertext.len());
        mac_msg.extend_from_slice(&nonce);
        mac_msg.extend_from_slice(&ciphertext);
        let tag = crypto::hmac_sha256(&k_mac, &mac_msg);
        let mut payload = ciphertext;
        payload.extend_from_slice(&tag);
        Ok(VaultEnvelope {
            format: "hermes-vault/1".to_string(),
            nonce: hex_encode(&nonce),
            payload: hex_encode(&payload),
        })
    }

    fn decrypt(&self, env: &VaultEnvelope) -> Result<Vec<u8>> {
        if env.format != "hermes-vault/1" {
            return Err(HermesError::tool(
                "browser_vault",
                format!("format vault tidak dikenal: {}", env.format),
            ));
        }
        let (k_enc, k_mac) = self.load_key()?;
        let nonce_raw = hex_decode(&env.nonce)?;
        let payload = hex_decode(&env.payload)?;
        if nonce_raw.len() != 16 || payload.len() < 32 {
            return Err(HermesError::tool(
                "browser_vault",
                "file vault rusak (nonce/payload)",
            ));
        }
        let mut nonce = [0u8; 16];
        nonce.copy_from_slice(&nonce_raw);
        let (ciphertext, tag) = payload.split_at(payload.len() - 32);
        let mut mac_msg = Vec::with_capacity(16 + ciphertext.len());
        mac_msg.extend_from_slice(&nonce);
        mac_msg.extend_from_slice(ciphertext);
        let expect = crypto::hmac_sha256(&k_mac, &mac_msg);
        if !crypto::ct_eq(&expect, tag) {
            return Err(HermesError::tool(
                "browser_vault",
                "vault tidak bisa didekripsi (kunci tidak cocok atau file rusak)",
            ));
        }
        let ks = crypto::keystream(&k_enc, &nonce, ciphertext.len());
        Ok(ciphertext
            .iter()
            .zip(ks.iter())
            .map(|(c, k)| c ^ k)
            .collect())
    }

    fn read_all(&self) -> Result<Vec<ItemRecord>> {
        let path = self.vault_path();
        if !path.exists() {
            return Ok(Vec::new());
        }
        let raw = std::fs::read(&path)
            .map_err(|e| HermesError::tool("browser_vault", format!("gagal membaca vault: {e}")))?;
        if raw.is_empty() {
            return Ok(Vec::new());
        }
        let env: VaultEnvelope = serde_json::from_slice(&raw).map_err(|_| {
            HermesError::tool("browser_vault", "file vault rusak (JSON tidak valid)")
        })?;
        let plain = self.decrypt(&env)?;
        let v: Value = serde_json::from_slice(&plain).map_err(|_| {
            HermesError::tool("browser_vault", "file vault rusak (isi tidak valid)")
        })?;
        let items = v
            .get("items")
            .and_then(|i| i.as_array())
            .cloned()
            .unwrap_or_default();
        let mut out = Vec::with_capacity(items.len());
        for it in items {
            out.push(serde_json::from_value(it).map_err(|_| {
                HermesError::tool("browser_vault", "file vault rusak (bentuk item)")
            })?);
        }
        Ok(out)
    }

    fn write_all(&self, items: &[ItemRecord]) -> Result<()> {
        self.ensure_dir()?;
        let payload = serde_json::to_vec(&json!({"version": 1, "items": items})).map_err(|e| {
            HermesError::tool("browser_vault", format!("gagal serialisasi vault: {e}"))
        })?;
        let env = self.encrypt(&payload)?;
        let data = serde_json::to_vec(&env).map_err(|e| {
            HermesError::tool("browser_vault", format!("gagal serialisasi vault: {e}"))
        })?;
        // Tulis atomik: file temp + rename; 0600 sebelum rename.
        let tmp = self.base.join(format!(
            ".vault.{}.tmp",
            hex_encode(&crypto::random_bytes(8).map_err(|e| HermesError::tool(
                "browser_vault",
                format!("gagal membuat nonce: {e}")
            ))?)
        ));
        std::fs::write(&tmp, &data)
            .map_err(|e| HermesError::tool("browser_vault", format!("gagal menulis vault: {e}")))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        std::fs::rename(&tmp, self.vault_path())
            .map_err(|e| HermesError::tool("browser_vault", format!("gagal menulis vault: {e}")))?;
        Ok(())
    }

    fn meta_of(rec: &ItemRecord) -> VaultItemMeta {
        VaultItemMeta {
            id: rec.id.clone(),
            kind: rec.kind.clone(),
            label: rec.label.clone(),
            origin: rec.origin.clone(),
            created_at: rec.created_at.clone(),
            identifier_type: rec
                .identifier_type
                .clone()
                .filter(|_| rec.identifier.is_some()),
            identifier: rec.identifier.clone(),
            has_otp: rec
                .secret
                .get("otp_secret")
                .map(|s| !s.is_empty())
                .unwrap_or(false),
            allowed_origins: rec.origin.clone().map(|o| vec![o]).unwrap_or_default(),
        }
    }

    /// Tambah item. `secret` = payload sensitif (dienkripsi saat rest).
    ///
    /// Untuk `kind="login"`, `origin` wajib dan payload harus memuat
    /// `identifier_type`, `identifier`, `password`. Field identifier BUKAN
    /// rahasia — dipindah ke metadata; hanya `password` (+ seed OTP opsional)
    /// yang tinggal di payload terenkripsi.
    pub fn add_item(
        &self,
        kind: &str,
        label: &str,
        secret: &HashMap<String, String>,
        origin: Option<&str>,
    ) -> std::result::Result<VaultItemMeta, VaultError> {
        if !VAULT_KINDS.contains(&kind) {
            return Err(verr(format!(
                "jenis vault tidak dikenal {kind:?} (harus salah satu dari {VAULT_KINDS:?})"
            )));
        }
        let label = label.trim();
        if label.is_empty() {
            return Err(verr("label wajib diisi"));
        }
        let mut secret = secret.clone();
        let (norm_origin, identifier, identifier_type, stored_secret) = if kind == "login" {
            let origin = origin.ok_or_else(|| verr("origin wajib untuk item login"))?;
            let norm_origin = normalize_origin(origin)?;
            let id_type = secret.remove("identifier_type").unwrap_or_default();
            if !LOGIN_IDENTIFIER_TYPES.contains(&id_type.as_str()) {
                return Err(verr(format!(
                    "identifier_type harus salah satu dari {LOGIN_IDENTIFIER_TYPES:?}"
                )));
            }
            let identifier = secret
                .remove("identifier")
                .unwrap_or_default()
                .trim()
                .to_string();
            let password = secret.get("password").cloned().unwrap_or_default();
            if identifier.is_empty() || password.is_empty() {
                return Err(verr("item login butuh identifier dan password"));
            }
            let otp =
                normalize_otp_secret(secret.get("otp_secret").map(|s| s.as_str()).unwrap_or(""))?;
            let mut stored = HashMap::new();
            stored.insert("password".to_string(), password);
            if !otp.is_empty() {
                stored.insert("otp_secret".to_string(), otp);
            }
            (Some(norm_origin), Some(identifier), Some(id_type), stored)
        } else {
            let allowed: &[(&str, &str)] = if kind == "payment" {
                PAYMENT_FIELDS
            } else {
                ADDRESS_FIELDS
            };
            let names: HashSet<&str> = allowed.iter().map(|(n, _)| *n).collect();
            let mut stored = HashMap::new();
            for (k, v) in &secret {
                if names.contains(k.as_str()) && !v.trim().is_empty() {
                    stored.insert(k.clone(), v.clone());
                }
            }
            let required = if kind == "payment" {
                PAYMENT_REQUIRED
            } else {
                ADDRESS_REQUIRED
            };
            let missing: Vec<&&str> = required
                .iter()
                .filter(|f| !stored.contains_key(**f))
                .collect();
            if !missing.is_empty() {
                return Err(verr(format!(
                    "item {kind} membutuhkan field: {}",
                    missing
                        .iter()
                        .map(|s| s.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
            let norm_origin = match origin {
                Some(o) => Some(normalize_origin(o)?),
                None => None,
            };
            (norm_origin, None, None, stored)
        };

        let nonce = crypto::random_bytes(6).map_err(|e| verr(format!("gagal membuat id: {e}")))?;
        let id = format!("vault_{}", hex_encode(&nonce));
        let rec = ItemRecord {
            id,
            kind: kind.to_string(),
            label: label.to_string(),
            origin: norm_origin,
            created_at: chrono::Utc::now().to_rfc3339(),
            identifier_type,
            identifier,
            secret: stored_secret,
        };
        let _guard = STORE_LOCK.lock().unwrap();
        let mut items = self.read_all().map_err(|e| verr(e.to_string()))?;
        items.push(rec.clone());
        self.write_all(&items).map_err(|e| verr(e.to_string()))?;
        Ok(Self::meta_of(&rec))
    }

    /// Daftar metadata-only. Payload rahasia tidak pernah disertakan.
    pub fn list_items(&self) -> Result<Vec<VaultItemMeta>> {
        let _guard = STORE_LOCK.lock().unwrap();
        Ok(self.read_all()?.iter().map(Self::meta_of).collect())
    }

    pub fn has_items(&self) -> bool {
        self.list_items().map(|v| !v.is_empty()).unwrap_or(false)
    }

    pub fn remove_item(&self, item_id: &str) -> Result<bool> {
        let _guard = STORE_LOCK.lock().unwrap();
        let items = self.read_all()?;
        let before = items.len();
        let remaining: Vec<ItemRecord> = items.into_iter().filter(|r| r.id != item_id).collect();
        if remaining.len() == before {
            return Ok(false);
        }
        self.write_all(&remaining)?;
        Ok(true)
    }

    pub fn get_meta(&self, item_id: &str) -> Result<Option<VaultItemMeta>> {
        let _guard = STORE_LOCK.lock().unwrap();
        Ok(self
            .read_all()?
            .iter()
            .find(|r| r.id == item_id)
            .map(Self::meta_of))
    }

    /// Resolve payload rahasia terdekripsi — HANYA untuk pemakaian server-side.
    ///
    /// Pemanggil TIDAK BOLEH menaruh nilai kembalian ke hasil tool, log,
    /// exception, atau string apa pun yang mencapai session DB.
    pub fn resolve_secret(&self, item_id: &str) -> Result<HashMap<String, String>> {
        let _guard = STORE_LOCK.lock().unwrap();
        for rec in self.read_all()? {
            if rec.id == item_id {
                return Ok(rec.secret.clone());
            }
        }
        Err(HermesError::tool(
            "browser_vault",
            format!("tidak ada item vault dengan id {item_id:?}"),
        ))
    }
}

// ---------------------------------------------------------------------------
// Backend: kontrak + registry (port dari agent/vault_backends/)
// ---------------------------------------------------------------------------

/// Kontrak backend login: daftar metadata (tanpa rahasia) + resolve SATU
/// password saat fill. Manajer eksternal butuh unlock per-sesi;
/// [`VaultBackend::resolve_password`] mengembalikan error terkunci agar tool
/// bisa meminta unlock ke surface.
#[async_trait]
pub trait VaultBackend: Send + Sync {
    fn name(&self) -> &'static str;
    fn display_name(&self) -> &'static str;
    /// Prefix handle ("vault_", "op:", "bw:").
    fn prefix(&self) -> &'static str;
    fn needs_unlock(&self) -> bool {
        false
    }
    fn is_unlocked(&self) -> bool {
        true
    }
    fn owns(&self, handle: &str) -> bool {
        handle.starts_with(self.prefix())
    }
    /// Metadata saja. Backend eksternal yang terkunci mengembalikan vec kosong.
    async fn list_items(&self) -> Result<Vec<VaultItemMeta>>;
    async fn get_meta(&self, handle: &str) -> Result<Option<VaultItemMeta>> {
        Ok(self
            .list_items()
            .await?
            .into_iter()
            .find(|m| m.id == handle))
    }
    /// Server-side only; gagal bila terkunci.
    async fn resolve_password(&self, handle: &str) -> Result<String>;
    /// Kode one-time saat ini untuk login ber-seed TOTP, else None.
    /// Server-side only, seperti resolve_password.
    async fn resolve_otp(&self, handle: &str) -> Result<Option<String>> {
        let _ = handle;
        Ok(None)
    }
    /// Payload penuh item payment/address (server-side only). Manajer
    /// eksternal hanya me-list login, jadi default-nya bentuk password-only.
    async fn resolve_secret(&self, handle: &str) -> Result<HashMap<String, String>> {
        Ok(HashMap::from([(
            "password".to_string(),
            self.resolve_password(handle).await?,
        )]))
    }
}

/// Token sesi manajer eksternal: hanya di memori proses, tidak pernah di disk.
static SESSION_TOKENS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

fn session_tokens() -> &'static Mutex<HashMap<String, String>> {
    SESSION_TOKENS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn get_session_token(backend: &str) -> Option<String> {
    session_tokens().lock().unwrap().get(backend).cloned()
}

fn cli_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join(name);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(md) = std::fs::metadata(&cand) {
                if md.is_file() && md.permissions().mode() & 0o111 != 0 {
                    return Some(cand);
                }
            }
        }
        #[cfg(not(unix))]
        {
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

/// Jalankan CLI manajer dengan timeout; rahasia TIDAK PERNAH lewat argv.
/// `secret_stdin`: rahasia via stdin (pola `op signin`); `secret_env`:
/// rahasia via env var khusus-anak-proses (pola `bw --passwordenv`).
async fn run_manager_cli(
    bin: &PathBuf,
    args: &[&str],
    extra_env: &[(&str, &str)],
    secret_stdin: Option<&str>,
    secret_env: Option<(&str, &str)>,
    timeout_secs: u64,
    label: &str,
) -> Result<String> {
    use tokio::process::Command;
    let mut cmd = Command::new(bin);
    cmd.args(args);
    cmd.env("NO_COLOR", "1");
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let mut child_env_secret: Option<(String, String)> = None;
    if let Some((name, val)) = secret_env {
        child_env_secret = Some((name.to_string(), val.to_string()));
    }
    if let Some((name, val)) = child_env_secret {
        cmd.env(name, val);
    }
    if secret_stdin.is_some() {
        cmd.stdin(std::process::Stdio::piped());
    } else {
        cmd.stdin(std::process::Stdio::null());
    }
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| HermesError::tool(label, format!("gagal menjalankan {label}: {e}")))?;
    if let Some(secret) = secret_stdin {
        use tokio::io::AsyncWriteExt;
        if let Some(mut stdin) = child.stdin.take() {
            // Rahasia ditulis ke stdin anak, tidak pernah ke argv/env proses ini.
            let _ = stdin.write_all(format!("{secret}\n").as_bytes()).await;
        }
    }
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        child.wait_with_output(),
    )
    .await
    .map_err(|_| HermesError::tool(label, format!("{label} timeout setelah {timeout_secs}s")))?
    .map_err(|e| HermesError::tool(label, format!("gagal menjalankan {label}: {e}")))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(HermesError::tool(
            label,
            format!(
                "{label} gagal: {}",
                err.chars().take(200).collect::<String>()
            ),
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

// -- backend lokal ----------------------------------------------------------

/// Backend vault lokal (selalu aktif). Prefix handle `vault_`.
pub struct LocalBackend {
    store: VaultStore,
}

impl LocalBackend {
    pub fn with_store(store: VaultStore) -> Self {
        Self { store }
    }
}

#[async_trait]
impl VaultBackend for LocalBackend {
    fn name(&self) -> &'static str {
        "local"
    }
    fn display_name(&self) -> &'static str {
        "Local vault"
    }
    fn prefix(&self) -> &'static str {
        "vault_"
    }
    async fn list_items(&self) -> Result<Vec<VaultItemMeta>> {
        self.store.list_items()
    }
    async fn get_meta(&self, handle: &str) -> Result<Option<VaultItemMeta>> {
        self.store.get_meta(handle)
    }
    async fn resolve_password(&self, handle: &str) -> Result<String> {
        Ok(self
            .store
            .resolve_secret(handle)?
            .get("password")
            .cloned()
            .unwrap_or_default())
    }
    async fn resolve_otp(&self, handle: &str) -> Result<Option<String>> {
        let secret = self.store.resolve_secret(handle)?;
        match secret.get("otp_secret").map(|s| s.as_str()).unwrap_or("") {
            "" => Ok(None),
            seed => Ok(Some(totp_now(seed, None).map_err(|e| {
                HermesError::tool("browser_vault", format!("gagal membuat kode TOTP: {e}"))
            })?)),
        }
    }
    async fn resolve_secret(&self, handle: &str) -> Result<HashMap<String, String>> {
        self.store.resolve_secret(handle)
    }
}

// -- backend 1Password (CLI `op`) --------------------------------------------

/// Item Login 1Password via CLI `op`. Unlock: `op signin --raw` (master
/// password via stdin) atau token service-account dari env
/// `OP_SERVICE_ACCOUNT_TOKEN`.
pub struct OnePasswordBackend {
    bin: PathBuf,
    item_vaults: Mutex<HashMap<String, String>>,
}

impl OnePasswordBackend {
    pub fn detect() -> Option<Self> {
        cli_on_path("op").map(|bin| Self {
            bin,
            item_vaults: Mutex::new(HashMap::new()),
        })
    }

    async fn run(&self, args: &[&str]) -> Result<String> {
        let token = get_session_token("onepassword");
        let service = std::env::var("OP_SERVICE_ACCOUNT_TOKEN").unwrap_or_default();
        if service.is_empty() && token.is_none() {
            return Err(HermesError::tool(
                "browser_vault",
                UnlockRequired {
                    backend: "1Password".to_string(),
                }
                .to_string(),
            ));
        }
        let mut extra: Vec<(&str, &str)> = Vec::new();
        let token_owned;
        let service_owned;
        let use_service = !service.is_empty();
        if use_service {
            service_owned = service;
            extra.push(("OP_SERVICE_ACCOUNT_TOKEN", service_owned.as_str()));
        } else {
            token_owned = token.unwrap_or_default();
            // Nama var sesi mengikuti akun default seperti original.
            extra.push(("OP_SESSION", token_owned.as_str()));
        }
        let out = run_manager_cli(&self.bin, args, &extra, None, None, 30, "op").await;
        match out {
            Err(e) => {
                let msg = e.to_string().to_lowercase();
                if !use_service
                    && (msg.contains("session")
                        || msg.contains("sign in")
                        || msg.contains("not signed in"))
                {
                    session_tokens().lock().unwrap().remove("onepassword");
                    return Err(HermesError::tool(
                        "browser_vault",
                        UnlockRequired {
                            backend: "1Password".to_string(),
                        }
                        .to_string(),
                    ));
                }
                Err(e)
            }
            ok => ok,
        }
    }

    fn origins_of(urls: &[String]) -> Vec<String> {
        let mut out = Vec::new();
        for u in urls {
            if let Ok(o) = normalize_origin(u) {
                if !out.contains(&o) {
                    out.push(o);
                }
            }
        }
        out
    }
}

#[async_trait]
impl VaultBackend for OnePasswordBackend {
    fn name(&self) -> &'static str {
        "onepassword"
    }
    fn display_name(&self) -> &'static str {
        "1Password"
    }
    fn prefix(&self) -> &'static str {
        "op:"
    }
    fn needs_unlock(&self) -> bool {
        true
    }
    fn is_unlocked(&self) -> bool {
        !std::env::var("OP_SERVICE_ACCOUNT_TOKEN")
            .unwrap_or_default()
            .is_empty()
            || get_session_token("onepassword").is_some()
    }
    async fn list_items(&self) -> Result<Vec<VaultItemMeta>> {
        if !self.is_unlocked() {
            return Ok(Vec::new());
        }
        async {
            let raw = self
                .run(&["item", "list", "--categories", "Login", "--format", "json"])
                .await?;
            let items: Vec<Value> = serde_json::from_str(&raw).unwrap_or_default();
            let mut out = Vec::new();
            let mut vaults = self.item_vaults.lock().unwrap();
            vaults.clear();
            for item in items {
                let id = item.get("id").and_then(|v| v.as_str()).unwrap_or("");
                if id.is_empty() {
                    continue;
                }
                let vault_id = item
                    .get("vault")
                    .and_then(|v| v.get("id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                vaults.insert(id.to_string(), vault_id);
                let urls: Vec<String> = item
                    .get("urls")
                    .and_then(|u| u.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|u| u.get("href").and_then(|h| h.as_str()))
                            .map(|s| s.to_string())
                            .collect()
                    })
                    .unwrap_or_default();
                let origins = Self::origins_of(&urls);
                if origins.is_empty() {
                    continue;
                }
                let username = item
                    .get("additional_information")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                let web: Vec<String> = origins
                    .iter()
                    .filter(|o| o.starts_with("http://") || o.starts_with("https://"))
                    .cloned()
                    .collect();
                let allowed = if web.is_empty() {
                    vec![origins[0].clone()]
                } else {
                    web
                };
                out.push(VaultItemMeta {
                    id: format!("op:{id}"),
                    kind: "login".to_string(),
                    label: item
                        .get("title")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .unwrap_or(&origins[0])
                        .to_string(),
                    origin: Some(origins[0].clone()),
                    created_at: item
                        .get("created_at")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    identifier_type: if username.is_empty() {
                        None
                    } else {
                        Some("username".to_string())
                    },
                    identifier: if username.is_empty() {
                        None
                    } else {
                        Some(username)
                    },
                    has_otp: false,
                    allowed_origins: allowed,
                });
            }
            Ok(out)
        }
        .await
    }
    async fn resolve_password(&self, handle: &str) -> Result<String> {
        let item_id = handle.strip_prefix("op:").unwrap_or(handle);
        let vault_id = self
            .item_vaults
            .lock()
            .unwrap()
            .get(item_id)
            .cloned()
            .unwrap_or_default();
        async {
            let mut args: Vec<&str> = vec!["item", "get", item_id];
            let vault_owned;
            if !vault_id.is_empty() {
                vault_owned = vault_id;
                args.extend(["--vault", vault_owned.as_str()]);
            }
            args.extend(["--fields", "label=password", "--reveal"]);
            Ok(self
                .run(&args)
                .await?
                .trim_end_matches(&['\r', '\n'][..])
                .to_string())
        }
        .await
    }
    async fn resolve_otp(&self, handle: &str) -> Result<Option<String>> {
        let item_id = handle.strip_prefix("op:").unwrap_or(handle);
        let code = async { self.run(&["item", "get", item_id, "--otp"]).await }.await;
        match code {
            Ok(c) => {
                let c = c.trim().to_string();
                Ok(
                    if c.chars().all(|ch| ch.is_ascii_digit()) && !c.is_empty() {
                        Some(c)
                    } else {
                        None
                    },
                )
            }
            Err(_) => Ok(None),
        }
    }
}

// -- backend Bitwarden (CLI `bw`) --------------------------------------------

/// Vault password Bitwarden via CLI `bw`. Unlock: `bw unlock --raw
/// --passwordenv VAR` (kontrak non-interaktif CLI; variabel hanya ada di
/// environment anak-proses).
pub struct BitwardenBackend {
    bin: PathBuf,
}

impl BitwardenBackend {
    pub fn detect() -> Option<Self> {
        cli_on_path("bw").map(|bin| Self { bin })
    }

    async fn run(&self, args: &[&str]) -> Result<String> {
        let token = get_session_token("bitwarden");
        let Some(tok) = token else {
            return Err(HermesError::tool(
                "browser_vault",
                UnlockRequired {
                    backend: "Bitwarden".to_string(),
                }
                .to_string(),
            ));
        };
        let owned = tok;
        let full: Vec<&str> = args.iter().copied().chain(["--nointeraction"]).collect();
        let out = run_manager_cli(
            &self.bin,
            &full,
            &[("BW_SESSION", owned.as_str())],
            None,
            None,
            30,
            "bw",
        )
        .await;
        match out {
            Err(e) => {
                let msg = e.to_string().to_lowercase();
                if msg.contains("locked") || msg.contains("session") {
                    session_tokens().lock().unwrap().remove("bitwarden");
                    return Err(HermesError::tool(
                        "browser_vault",
                        UnlockRequired {
                            backend: "Bitwarden".to_string(),
                        }
                        .to_string(),
                    ));
                }
                Err(e)
            }
            ok => ok,
        }
    }
}

#[async_trait]
impl VaultBackend for BitwardenBackend {
    fn name(&self) -> &'static str {
        "bitwarden"
    }
    fn display_name(&self) -> &'static str {
        "Bitwarden"
    }
    fn prefix(&self) -> &'static str {
        "bw:"
    }
    fn needs_unlock(&self) -> bool {
        true
    }
    fn is_unlocked(&self) -> bool {
        get_session_token("bitwarden").is_some()
    }
    async fn list_items(&self) -> Result<Vec<VaultItemMeta>> {
        if !self.is_unlocked() {
            return Ok(Vec::new());
        }
        async {
            let raw = self.run(&["list", "items"]).await?;
            let items: Vec<Value> = serde_json::from_str(&raw).unwrap_or_default();
            let mut out = Vec::new();
            for item in items {
                if item.get("type").and_then(|v| v.as_u64()) != Some(1) {
                    continue;
                }
                let login = match item.get("login") {
                    Some(l) if l.is_object() => l,
                    _ => continue,
                };
                let mut origins = Vec::new();
                if let Some(uris) = login.get("uris").and_then(|u| u.as_array()) {
                    for uri in uris {
                        // URI match "Never" (5) bukan target fill.
                        if uri.get("match").and_then(|v| v.as_u64()) == Some(5) {
                            continue;
                        }
                        if let Some(s) = uri.get("uri").and_then(|v| v.as_str()) {
                            if let Ok(o) = normalize_origin(s) {
                                if !origins.contains(&o) {
                                    origins.push(o);
                                }
                            }
                        }
                    }
                }
                if origins.is_empty() {
                    continue;
                }
                let username = login
                    .get("username")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                let web: Vec<String> = origins
                    .iter()
                    .filter(|o| o.starts_with("http://") || o.starts_with("https://"))
                    .cloned()
                    .collect();
                let allowed = if web.is_empty() {
                    vec![origins[0].clone()]
                } else {
                    web
                };
                let id = item.get("id").and_then(|v| v.as_str()).unwrap_or("");
                out.push(VaultItemMeta {
                    id: format!("bw:{id}"),
                    kind: "login".to_string(),
                    label: item
                        .get("name")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .unwrap_or(&origins[0])
                        .to_string(),
                    origin: Some(origins[0].clone()),
                    created_at: item
                        .get("creationDate")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    identifier_type: if username.is_empty() {
                        None
                    } else {
                        Some("username".to_string())
                    },
                    identifier: if username.is_empty() {
                        None
                    } else {
                        Some(username)
                    },
                    has_otp: false,
                    allowed_origins: allowed,
                });
            }
            Ok(out)
        }
        .await
    }
    async fn resolve_password(&self, handle: &str) -> Result<String> {
        let item_id = handle.strip_prefix("bw:").unwrap_or(handle);
        async {
            Ok(self
                .run(&["get", "password", item_id])
                .await?
                .trim_end_matches(&['\r', '\n'][..])
                .to_string())
        }
        .await
    }
    async fn resolve_otp(&self, handle: &str) -> Result<Option<String>> {
        let item_id = handle.strip_prefix("bw:").unwrap_or(handle);
        let code = async { self.run(&["get", "totp", item_id]).await }.await;
        match code {
            Ok(c) => {
                let c = c.trim().to_string();
                Ok(
                    if !c.is_empty() && c.chars().all(|ch| ch.is_ascii_digit()) {
                        Some(c)
                    } else {
                        None
                    },
                )
            }
            Err(_) => Ok(None),
        }
    }
}

/// Backend lokal selalu aktif lebih dulu, lalu setiap manajer eksternal yang
/// terdeteksi di PATH (kecuali dimatikan — saat ini selalu auto-detect seperti
/// original yang zero-config).
pub fn enabled_backends() -> Vec<Box<dyn VaultBackend>> {
    let mut out: Vec<Box<dyn VaultBackend>> = vec![Box::new(LocalBackend {
        store: VaultStore::default_store(),
    })];
    if let Some(op) = OnePasswordBackend::detect() {
        out.push(Box::new(op));
    }
    if let Some(bw) = BitwardenBackend::detect() {
        out.push(Box::new(bw));
    }
    out
}

/// Backend pemilik sebuah handle (berdasar prefix).
pub fn backend_for_handle<'a>(
    backends: &'a [Box<dyn VaultBackend>],
    handle: &str,
) -> Option<&'a dyn VaultBackend> {
    backends.iter().find(|b| b.owns(handle)).map(|b| b.as_ref())
}

// ---------------------------------------------------------------------------
// Pembangun skrip fill (dievaluasi via CDP WebSocket, tidak pernah via argv)
// ---------------------------------------------------------------------------

/// JS untuk mengisi kolom password. Rahasia di-embed sebagai literal JSON
/// (aman dari quoting), dan origin halaman ditegaskan ulang SECARA SINKRON
/// di dalam skrip (pertahanan TOCTOU seperti original).
pub fn build_login_fill_js(password: &str, expected_origin: &str) -> String {
    let pw = serde_json::to_string(password).unwrap_or_else(|_| "\"\"".to_string());
    let eo = serde_json::to_string(expected_origin).unwrap_or_else(|_| "\"\"".to_string());
    format!(
        r#"(() => {{
  if (window.location.origin !== {eo}) return JSON.stringify({{refused:"origin_changed", found: window.location.origin}});
  const el = document.querySelector('input[type="password"]');
  if (!el) return JSON.stringify({{filled:0}});
  const proto = Object.getPrototypeOf(el);
  const desc = Object.getOwnPropertyDescriptor(proto, "value")
    || Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value");
  if (desc && desc.set) desc.set.call(el, {pw}); else el.value = {pw};
  el.dispatchEvent(new Event("input", {{bubbles:true}}));
  el.dispatchEvent(new Event("change", {{bubbles:true}}));
  return JSON.stringify({{filled:1}});
}})()"#
    )
}

/// JS untuk mengisi field kartu/alamat berdasar token autocomplete WHATWG.
/// `fields` = pasangan (token_autocomplete, nilai).
pub fn build_checkout_fill_js(fields: &[(&str, &str)], expected_origin: &str) -> String {
    let pairs: Vec<Value> = fields.iter().map(|(t, v)| json!([t, v])).collect();
    let pairs_js = serde_json::to_string(&pairs).unwrap_or_else(|_| "[]".to_string());
    let eo = serde_json::to_string(expected_origin).unwrap_or_else(|_| "\"\"".to_string());
    format!(
        r#"(() => {{
  if (window.location.origin !== {eo}) return JSON.stringify({{refused:"origin_changed", found: window.location.origin}});
  const fields = {pairs_js};
  let filled = 0;
  for (const [token, value] of fields) {{
    const el = document.querySelector('input[autocomplete="' + token + '"]')
      || document.querySelector('input[autocomplete="' + token.toLowerCase() + '"]');
    if (!el) continue;
    const proto = Object.getPrototypeOf(el);
    const desc = Object.getOwnPropertyDescriptor(proto, "value")
      || Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value");
    if (desc && desc.set) desc.set.call(el, value); else el.value = value;
    el.dispatchEvent(new Event("input", {{bubbles:true}}));
    el.dispatchEvent(new Event("change", {{bubbles:true}}));
    filled++;
  }}
  return JSON.stringify({{filled}});
}})()"#
    )
}

/// JS untuk mengisi kolom kode one-time (2FA).
pub fn build_otp_fill_js(code: &str) -> String {
    let c = serde_json::to_string(code).unwrap_or_else(|_| "\"\"".to_string());
    format!(
        r#"(() => {{
  const code = {c};
  const all = Array.from(document.querySelectorAll("input"));
  const el = all.find(i => /one-time|one_time|otp|totp|verification|authenticator|security.?code|2fa/i.test(
    [i.autocomplete, i.name, i.id, i.placeholder, i.getAttribute("aria-label")].filter(Boolean).join(" ")));
  if (!el) return JSON.stringify({{filled:0}});
  const proto = Object.getPrototypeOf(el);
  const desc = Object.getOwnPropertyDescriptor(proto, "value")
    || Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value");
  if (desc && desc.set) desc.set.call(el, code); else el.value = code;
  el.dispatchEvent(new Event("input", {{bubbles:true}}));
  el.dispatchEvent(new Event("change", {{bubbles:true}}));
  return JSON.stringify({{filled:1}});
}})()"#
    )
}

/// Pencocokan origin pengikat: exact-origin, tanpa inferensi wildcard/subdomain.
pub fn origin_allowed(page_origin: &str, allowed: &[String]) -> bool {
    allowed.iter().any(|o| o == page_origin)
}

// ---------------------------------------------------------------------------
// Tool browser_vault
// ---------------------------------------------------------------------------

fn tool_json(v: Value) -> Result<ToolResult> {
    Ok(ToolResult {
        tool_call_id: String::new(),
        name: "browser_vault".to_string(),
        content: v.to_string(),
        is_error: false,
    })
}

fn err_json(error_type: &str, error: impl Into<String>) -> Value {
    json!({"success": false, "error_type": error_type, "error": error.into()})
}

/// Kunci error "terkunci" dari backend eksternal → respons unlock_required.
fn is_locked_error(e: &HermesError) -> bool {
    e.to_string().contains("terkunci")
}

pub struct BrowserVaultTool;

impl BrowserVaultTool {
    async fn do_list() -> Result<ToolResult> {
        let backends = enabled_backends();
        let mut items = Vec::new();
        let mut locked = Vec::new();
        let mut errors = Vec::new();
        for b in &backends {
            if b.needs_unlock() && !b.is_unlocked() {
                // Backend terkunci tidak menyumbang item; dilaporkan agar agen
                // tahu mesti unlock — seperti original.
                locked.push(json!({
                    "backend": b.name(),
                    "display_name": b.display_name(),
                    "unlock": "browser_vault action=unlock",
                }));
                continue;
            }
            match b.list_items().await {
                Err(e) => errors.push(json!({
                    "backend": b.name(),
                    "error": e.to_string().chars().take(200).collect::<String>(),
                })),
                Ok(metas) => {
                    for meta in metas {
                        let mut entry = json!({
                            "handle": meta.id,
                            "backend": b.name(),
                            "label": meta.label,
                            "kind": meta.kind,
                            "origin": meta.origin,
                            // Login selalu bisa diisi (identifier diketik agen
                            // sendiri); payment/address butuh origin terikat.
                            "available": meta.kind == "login" || meta.origin.is_some(),
                        });
                        if meta.allowed_origins.len() > 1 {
                            entry["allowed_origins"] = json!(meta.allowed_origins);
                        }
                        if meta.has_otp || b.needs_unlock() {
                            entry["two_factor"] = json!(if meta.has_otp {
                                "automatic"
                            } else {
                                "automatic if the manager stores a TOTP seed, else the user is asked"
                            });
                        }
                        if let Some(id) = meta.identifier {
                            entry["identifier"] = json!(id);
                            entry["identifier_type"] = json!(meta.identifier_type);
                        }
                        items.push(entry);
                    }
                }
            }
        }
        let mut out = json!({"success": true, "items": items});
        if items.is_empty() {
            out["hint"] = json!(
                "Tidak ada login tersimpan. Jangan pernah mengetik password sendiri \
                 atau memintanya di chat, bahkan bila terlihat di halaman."
            );
        }
        if !locked.is_empty() {
            out["locked"] = json!(locked);
        }
        if !errors.is_empty() {
            out["errors"] = json!(errors);
        }
        tool_json(out)
    }

    /// Koneksi CDP yang diawasi. Fail-closed: tidak ada fallback argv —
    /// rahasia tidak pernah lewat command-line proses.
    async fn supervised_cdp() -> Result<std::sync::Arc<crate::browser::CdpClient>> {
        crate::browser::cdp().await.map_err(|e| {
            HermesError::tool(
                "browser_vault",
                format!(
                    "Vault fill butuh sesi browser CDP yang diawasi (WebSocket langsung). \
                     Jalankan browser lewat sesi yang dikelola Hermes dan coba lagi. Detail: {e}"
                ),
            )
        })
    }

    async fn current_page_origin(
        cdp: &std::sync::Arc<crate::browser::CdpClient>,
    ) -> Result<Option<String>> {
        let v = cdp.eval("window.location.href").await.map_err(|e| {
            HermesError::tool(
                "browser_vault",
                format!("gagal membaca origin halaman: {e}"),
            )
        })?;
        let href = v.as_str().unwrap_or("").trim();
        if href.is_empty() || href == "about:blank" {
            return Ok(None);
        }
        Ok(normalize_origin(href).ok())
    }

    async fn do_fill(handle: &str) -> Result<ToolResult> {
        if handle.is_empty() {
            return tool_json(err_json(
                "missing_handle",
                "fill butuh `handle` dari browser_vault action=list.",
            ));
        }
        let backends = enabled_backends();
        let backend = match backend_for_handle(&backends, handle) {
            Some(b) => b,
            None => {
                return tool_json(err_json(
                    "unknown_handle",
                    format!(
                        "Tidak ada item vault dengan handle {handle:?}. Gunakan browser_vault action=list."
                    ),
                ))
            }
        };
        if backend.needs_unlock() && !backend.is_unlocked() {
            return tool_json(err_json(
                "unlock_required",
                format!(
                    "{} terkunci; panggil browser_vault action=unlock.",
                    backend.display_name()
                ),
            ));
        }
        let meta = match backend.get_meta(handle).await {
            Ok(m) => m,
            Err(e) if is_locked_error(&e) => {
                return tool_json(err_json(
                    "unlock_required",
                    format!(
                        "{} terkunci; panggil browser_vault action=unlock.",
                        backend.display_name()
                    ),
                ))
            }
            Err(e) => return Err(e),
        };
        let Some(meta) = meta else {
            return tool_json(err_json(
                "unknown_handle",
                format!(
                    "Tidak ada item vault dengan handle {handle:?}. Gunakan browser_vault action=list."
                ),
            ));
        };
        if meta.kind != "login" && meta.origin.is_none() {
            return tool_json(err_json(
                "no_origin",
                format!(
                    "Item vault {handle:?} tidak punya origin terikat; item {} hanya diisi di situs tempatnya disimpan.",
                    meta.kind
                ),
            ));
        }
        // Catatan: original meminta konfirmasi manusia eksplisit sebelum kartu
        // ditulis ke halaman checkout. Port ini tidak punya surface elicitation;
        // gate persetujuan level tool-call (dynamic_risk=High untuk fill)
        // adalah titik konfirmasinya — didokumentasikan sebagai celah.

        // -- Pre-check pengikatan origin --
        let allowed: Vec<String> = if meta.allowed_origins.is_empty() {
            meta.origin.clone().into_iter().collect()
        } else {
            meta.allowed_origins.clone()
        };
        let cdp = match Self::supervised_cdp().await {
            Ok(c) => c,
            Err(e) => {
                return tool_json(err_json("supervisor_required", e.to_string()));
            }
        };
        let page_origin = match Self::current_page_origin(&cdp).await? {
            Some(o) => o,
            None => {
                return tool_json(err_json(
                    "no_page",
                    "Tidak bisa menentukan origin halaman. Navigasi ke halaman login dulu.",
                ))
            }
        };
        if !origin_allowed(&page_origin, &allowed) {
            return tool_json(err_json(
                "origin_mismatch",
                format!(
                    "Ditolak: origin halaman ({page_origin}) tidak cocok dengan origin terikat \
                     item ({}). Fill vault hanya jalan di origin persis tempat kredensial disimpan.",
                    allowed.join(", ")
                ),
            ));
        }

        // -- Resolve rahasia + fill (rahasia tidak pernah masuk string tercatat) --
        let fill_js: String;
        let mut scrub: HashMap<String, String> = HashMap::new();
        let mut field_tokens: Vec<String> = Vec::new();
        if meta.kind == "login" {
            let password = match backend.resolve_password(handle).await {
                Ok(p) => p,
                Err(e) if is_locked_error(&e) => {
                    return tool_json(err_json(
                        "unlock_required",
                        format!(
                            "{} terkunci; panggil browser_vault action=unlock.",
                            backend.display_name()
                        ),
                    ))
                }
                Err(e) => return Err(e),
            };
            scrub.insert("password".to_string(), password.clone());
            fill_js = build_login_fill_js(&password, &page_origin);
            drop(password);
        } else {
            let secret = match backend.resolve_secret(handle).await {
                Ok(s) => s,
                Err(e) if is_locked_error(&e) => {
                    return tool_json(err_json(
                        "unlock_required",
                        format!(
                            "{} terkunci; panggil browser_vault action=unlock.",
                            backend.display_name()
                        ),
                    ))
                }
                Err(e) => return Err(e),
            };
            scrub = secret.clone();
            let table = if meta.kind == "payment" {
                PAYMENT_FIELDS
            } else {
                ADDRESS_FIELDS
            };
            let pairs: Vec<(&str, &str)> = table
                .iter()
                .filter_map(|(name, token)| {
                    secret.get(*name).map(|v| {
                        field_tokens.push(token.to_string());
                        (*token, v.as_str())
                    })
                })
                .collect();
            if pairs.is_empty() {
                return tool_json(err_json(
                    "no_fillable_field",
                    format!(
                        "Tidak ada field {} yang cocok dengan item tersimpan di halaman ini.",
                        meta.kind
                    ),
                ));
            }
            fill_js = build_checkout_fill_js(&pairs, &page_origin);
            drop(secret);
        }

        let raw = match cdp.eval(&fill_js).await {
            Ok(v) => v,
            Err(e) => {
                return tool_json(err_json(
                    "eval_failed",
                    scrub_secret_from_text(&e.to_string(), &scrub),
                ))
            }
        };
        drop(scrub);
        let parsed: Value = raw
            .as_str()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or(Value::Null);
        if parsed.get("refused").and_then(|v| v.as_str()) == Some("origin_changed") {
            return tool_json(err_json(
                "origin_changed",
                format!(
                    "Ditolak: halaman berpindah dari origin terikat ({page_origin}) sebelum fill \
                     berjalan. Tidak ada yang ditulis."
                ),
            ));
        }
        let filled = parsed.get("filled").and_then(|v| v.as_u64()).unwrap_or(0);
        let mut out = json!({
            "success": filled > 0,
            "filled_fields": filled,
            "backend": backend.name(),
            "kind": meta.kind,
            "origin": page_origin,
        });
        if meta.kind == "login" {
            out["next"] = json!(
                "Submit. Bila situs meminta kode verifikasi, panggil browser_vault action=enter_code \
                 dengan handle ini."
            );
        } else {
            // Field yang ditargetkan — tidak pernah nilainya.
            field_tokens.sort();
            out["fields"] = json!(field_tokens);
        }
        tool_json(out)
    }

    async fn do_enter_code(handle: &str) -> Result<ToolResult> {
        // Jalur otomatis: seed TOTP tersimpan → kode dibuat server-side.
        // Tanpa seed, original meminta ke pengguna — tidak tersedia di port ini.
        let backends = enabled_backends();
        let code: Option<(String, String)> = if handle.is_empty() {
            None
        } else {
            match backend_for_handle(&backends, handle) {
                Some(b) => match b.resolve_otp(handle).await {
                    Ok(Some(c)) => Some((c, b.name().to_string())),
                    _ => None,
                },
                None => None,
            }
        };
        let Some((code, source)) = code else {
            return tool_json(err_json(
                "prompt_unavailable",
                "Situs meminta kode one-time dan port ini tidak bisa menanyakannya ke pengguna \
                 (tanpa prompt surface). Simpan kunci authenticator untuk login ini agar kode \
                 dibuat otomatis.",
            ));
        };
        let cdp = match Self::supervised_cdp().await {
            Ok(c) => c,
            Err(e) => return tool_json(err_json("supervisor_required", e.to_string())),
        };
        let page_origin = Self::current_page_origin(&cdp)
            .await?
            .unwrap_or_else(|| "unknown".to_string());
        let js = build_otp_fill_js(&code);
        // `code` di-drop segera setelah dipakai; tidak masuk hasil/log.
        let code_for_check = code.clone();
        drop(code);
        let raw = cdp
            .eval(&js)
            .await
            .map_err(|e| HermesError::tool("browser_vault", format!("gagal mengisi kode: {e}")))?;
        let parsed: Value = raw
            .as_str()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or(Value::Null);
        let filled = parsed.get("filled").and_then(|v| v.as_u64()).unwrap_or(0);
        let mut out = json!({
            "success": filled > 0,
            "filled_fields": filled,
            "origin": page_origin,
            "source": source,
            "next": "Submit formulir (banyak situs auto-submit saat digit terakhir masuk).",
        });
        // Pertahanan terakhir: pastikan kode tidak bocor ke output.
        if out.to_string().contains(&code_for_check) {
            out = err_json("internal_error", "redaction failure");
        }
        tool_json(out)
    }

    async fn do_unlock(backend_name: &str) -> Result<ToolResult> {
        let backends = enabled_backends();
        let backend = backends
            .iter()
            .find(|b| b.name() == backend_name && b.needs_unlock());
        let Some(backend) = backend else {
            return tool_json(err_json(
                "unknown_backend",
                format!("Tidak ada backend vault unlockable bernama {backend_name:?}."),
            ));
        };
        if backend.is_unlocked() {
            return tool_json(json!({
                "success": true,
                "backend": backend.name(),
                "already_unlocked": true,
            }));
        }
        // Original meminta master password via prompt bertopeng di surface.
        // Port ini tidak punya prompt surface → fail-closed dengan arahan.
        tool_json(err_json(
            "unlock_unavailable",
            format!(
                "{} terkunci dan sesi ini tidak bisa meminta master password (tanpa prompt \
                 surface). Untuk 1Password, set env OP_SERVICE_ACCOUNT_TOKEN; untuk Bitwarden, \
                 jalankan `bw unlock` lalu teruskan token sesi — atau unlock dari sesi Hermes \
                 interaktif bila tersedia.",
                backend.display_name()
            ),
        ))
    }

    async fn do_save_login() -> Result<ToolResult> {
        // Original meminta identifier+password via prompt bertopeng milik surface;
        // nilai tidak pernah masuk percakapan. Port ini tidak punya prompt
        // surface, dan model TIDAK BOLEH menerima/mengetik password → fail-closed.
        tool_json(err_json(
            "prompt_unavailable",
            "Sesi ini tidak bisa meminta login ke pengguna (tanpa prompt bertopeng). \
             Jangan pernah mengetik atau meminta password di chat. Tambahkan kredensial \
             lewat sarana di luar agen (mis. manajer password), lalu panggil \
             browser_vault action=list.",
        ))
    }
}

#[async_trait]
impl Tool for BrowserVaultTool {
    fn name(&self) -> &str {
        "browser_vault"
    }

    fn description(&self) -> &str {
        "Pakai login/password/kartu/alamat tersimpan di halaman browser SAAT INI tanpa rahasia \
         masuk percakapan. Aksi: list (panggil dulu: login/kartu/alamat sebagai handle; login \
         membawa identifier yang kamu ketik sendiri via tool input browser; manajer terkunci muncul \
         di `locked`); fill (handle: password untuk login, field kartu, field alamat; ditolak kecuali \
         origin halaman cocok dengan item); save_login (tidak tersedia di sesi ini); enter_code \
         (kode one-time 2FA: dibuat otomatis dari seed authenticator tersimpan); unlock (backend: \
         minta unlock 1Password/Bitwarden — butuh prompt pengguna). Jangan pernah mengetik, meminta, \
         atau mengulang password, nomor kartu, CVC, atau kode sendiri."
    }

    fn parameters_schema(&self) -> Value {
        object_schema(
            vec![
                (
                    "action",
                    json!({
                        "type": "string",
                        "enum": ["list", "fill", "save_login", "enter_code", "unlock"],
                        "description": "Aksi vault yang dijalankan.",
                    }),
                ),
                (
                    "handle",
                    string_prop("fill / enter_code: handle dari action=list."),
                ),
                (
                    "backend",
                    json!({
                        "type": "string",
                        "enum": ["onepassword", "bitwarden"],
                        "description": "unlock saja: backend manajer yang dibuka.",
                    }),
                ),
                (
                    "label",
                    string_prop("save_login saja: nama situs pendek (default: host)."),
                ),
            ],
            vec!["action"],
        )
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Medium
    }

    fn dynamic_risk(&self, args: &Value) -> RiskLevel {
        match args.get("action").and_then(|v| v.as_str()) {
            // list hanya membaca metadata.
            Some("list") => RiskLevel::Low,
            // fill/save_login/enter_code/unlock menggerakkan rahasia atau
            // mengubah state → titik konfirmasi persetujuan tool-call.
            _ => RiskLevel::High,
        }
    }

    /// Aksi bermutasi (fill/save_login/enter_code/unlock) → Write: barrier
    /// sekuensial di batch agar tidak jalan paralel dengan reader lain.
    fn effect(&self) -> ToolEffect {
        ToolEffect::Write
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolResult> {
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
        let handle = args.get("handle").and_then(|v| v.as_str()).unwrap_or("");
        let backend = args.get("backend").and_then(|v| v.as_str()).unwrap_or("");
        match action {
            "list" => Self::do_list().await,
            "fill" => Self::do_fill(handle).await,
            "save_login" => Self::do_save_login().await,
            "enter_code" => Self::do_enter_code(handle).await,
            "unlock" => Self::do_unlock(backend).await,
            _ => tool_json(err_json(
                "unknown_action",
                format!(
                    "Aksi tidak dikenal {action:?}; gunakan list, fill, save_login, enter_code, atau unlock."
                ),
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn tmp_base() -> PathBuf {
        let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("hvault-test-{}-{}", std::process::id(), n))
    }

    fn tmp_store() -> (VaultStore, PathBuf) {
        let base = tmp_base();
        let dir = base.clone();
        (VaultStore::new(base.join("vault")), dir)
    }

    fn secret(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn add_login(store: &VaultStore) -> VaultItemMeta {
        store
            .add_item(
                "login",
                "Example",
                &secret(&[
                    ("identifier_type", "email"),
                    ("identifier", "user@example.com"),
                    ("password", "s3cr3t-p@ss"),
                ]),
                Some("https://example.com/login"),
            )
            .expect("add login")
    }

    #[test]
    fn normalize_origin_cases() {
        assert_eq!(
            normalize_origin("https://example.com/login?x=1").unwrap(),
            "https://example.com"
        );
        assert_eq!(
            normalize_origin("https://example.com:443/a").unwrap(),
            "https://example.com"
        );
        assert_eq!(
            normalize_origin("http://example.com:80").unwrap(),
            "http://example.com"
        );
        assert_eq!(
            normalize_origin("https://example.com:8443").unwrap(),
            "https://example.com:8443"
        );
        assert_eq!(
            normalize_origin("HTTPS://EXAMPLE.COM/Path").unwrap(),
            "https://example.com"
        );
        assert!(normalize_origin("example.com").is_err());
        assert!(normalize_origin("").is_err());
        assert!(normalize_origin("://").is_err());
    }

    #[test]
    fn normalize_otp_secret_cases() {
        assert_eq!(
            normalize_otp_secret("JBSWY3DPEHPK3PXP").unwrap(),
            "JBSWY3DPEHPK3PXP"
        );
        assert_eq!(
            normalize_otp_secret("  jbswy3dpehpk3pxp ").unwrap(),
            "JBSWY3DPEHPK3PXP"
        );
        assert_eq!(
            normalize_otp_secret("otpauth://totp/Example?secret=JBSWY3DPEHPK3PXP").unwrap(),
            "JBSWY3DPEHPK3PXP"
        );
        assert_eq!(
            normalize_otp_secret(
                "otpauth://totp/Example?secret=JBSWY3DPEHPK3PXP&digits=8&period=60&algorithm=SHA256"
            )
            .unwrap(),
            "JBSWY3DPEHPK3PXP|8|60|SHA256"
        );
        assert!(normalize_otp_secret("otpauth://hotp/x?secret=JBSWY3DPEHPK3PXP").is_err());
        assert!(normalize_otp_secret("!!!not-base32!!!").is_err());
        assert_eq!(normalize_otp_secret("").unwrap(), "");
    }

    #[test]
    fn totp_vektor_rfc6238() {
        // Seed ASCII "12345678901234567890" → base32.
        let seed = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";
        for (at, expect) in [
            (59.0, "287082"),
            (1111111109.0, "081804"),
            (1111111111.0, "050471"),
            (1234567890.0, "005924"),
            (2000000000.0, "279037"),
            (20000000000.0, "353130"),
        ] {
            assert_eq!(totp_now(seed, Some(at)).unwrap(), expect, "t={at}");
        }
    }

    #[test]
    fn store_roundtrip_dan_metadata_tanpa_password() {
        let (store, _dir) = tmp_store();
        let meta = add_login(&store);
        assert!(meta.id.starts_with("vault_"));
        assert_eq!(meta.origin.as_deref(), Some("https://example.com"));
        assert_eq!(meta.identifier.as_deref(), Some("user@example.com"));
        assert_eq!(meta.identifier_type.as_deref(), Some("email"));
        assert!(!meta.has_otp);

        // Metadata yang diserialisasi TIDAK BOLEH memuat password.
        let meta_json = serde_json::to_string(&meta).unwrap();
        assert!(!meta_json.contains("s3cr3t-p@ss"));

        let items = store.list_items().unwrap();
        assert_eq!(items.len(), 1);
        let listed_json = serde_json::to_string(&items).unwrap();
        assert!(!listed_json.contains("s3cr3t-p@ss"));

        let resolved = store.resolve_secret(&meta.id).unwrap();
        assert_eq!(
            resolved.get("password").map(|s| s.as_str()),
            Some("s3cr3t-p@ss")
        );
        // Identifier TIDAK disimpan di payload rahasia (dia metadata).
        assert!(!resolved.contains_key("identifier"));

        assert!(store.has_items());
        assert!(store.remove_item(&meta.id).unwrap());
        assert!(!store.remove_item(&meta.id).unwrap());
        assert!(!store.has_items());
    }

    #[test]
    #[cfg(unix)]
    fn store_permission_dikeraskan() {
        use std::os::unix::fs::PermissionsExt;
        let (store, _dir) = tmp_store();
        add_login(&store);
        let dir_mode = std::fs::metadata(&store.base).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "dir vault harus 0700");
        let key_mode = std::fs::metadata(store.key_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(key_mode, 0o600, "vault.key harus 0600");
        let data_mode = std::fs::metadata(store.vault_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(data_mode, 0o600, "vault.json.enc harus 0600");
    }

    #[test]
    fn store_validasi_add_item() {
        let (store, _dir) = tmp_store();
        assert!(store.add_item("salah", "x", &secret(&[]), None).is_err());
        assert!(store
            .add_item("login", "  ", &secret(&[]), Some("https://a.test"))
            .is_err());
        // Login tanpa origin.
        assert!(store
            .add_item(
                "login",
                "x",
                &secret(&[
                    ("identifier_type", "email"),
                    ("identifier", "u@a.test"),
                    ("password", "p"),
                ]),
                None
            )
            .is_err());
        // Login tanpa password.
        assert!(store
            .add_item(
                "login",
                "x",
                &secret(&[("identifier_type", "email"), ("identifier", "u@a.test")]),
                Some("https://a.test")
            )
            .is_err());
        // Payment kurang field wajib.
        assert!(store
            .add_item(
                "payment",
                "kartu",
                &secret(&[("card_number", "4111111111111111")]),
                None
            )
            .is_err());
        // Payment valid; field tak dikenal dibuang.
        let m = store
            .add_item(
                "payment",
                "kartu",
                &secret(&[
                    ("card_number", "4111111111111111"),
                    ("exp_month", "12"),
                    ("exp_year", "2030"),
                    ("cvc", "123"),
                    ("nama_aneh", "dibuang"),
                ]),
                Some("https://shop.test"),
            )
            .unwrap();
        let resolved = store.resolve_secret(&m.id).unwrap();
        assert!(!resolved.contains_key("nama_aneh"));
        assert_eq!(resolved.get("cvc").map(|s| s.as_str()), Some("123"));
        // Address valid.
        assert!(store
            .add_item(
                "address",
                "rumah",
                &secret(&[
                    ("address_line1", "Jl. Mawar 1"),
                    ("city", "Denpasar"),
                    ("postal_code", "80111"),
                    ("country", "Indonesia"),
                ]),
                None
            )
            .is_ok());
    }

    #[test]
    fn store_seed_otp_tersimpan_di_payload() {
        let (store, _dir) = tmp_store();
        let meta = store
            .add_item(
                "login",
                "otp-site",
                &secret(&[
                    ("identifier_type", "username"),
                    ("identifier", "u"),
                    ("password", "p"),
                    ("otp_secret", "JBSWY3DPEHPK3PXP"),
                ]),
                Some("https://otp.test"),
            )
            .unwrap();
        assert!(meta.has_otp);
        let listed = store.list_items().unwrap();
        assert!(listed[0].has_otp);
        // Seed tidak bocor di metadata.
        assert!(!serde_json::to_string(&listed).unwrap().contains("JBSWY3DP"));
    }

    #[test]
    fn store_tamper_dan_kunci_salah_gagal() {
        let (store, _dir) = tmp_store();
        add_login(&store);
        // Rusak satu byte ciphertext → tag MAC tidak cocok.
        let path = store.vault_path();
        let mut raw = std::fs::read(&path).unwrap();
        let pos = raw.len() / 2;
        raw[pos] ^= 0x01;
        std::fs::write(&path, &raw).unwrap();
        assert!(store.list_items().is_err());

        // Kunci diganti → dekripsi gagal.
        let (store2, _d2) = tmp_store();
        add_login(&store2);
        std::fs::remove_file(store2.key_path()).unwrap();
        assert!(store2.list_items().is_err());
    }

    #[test]
    fn scrub_secret_dari_teks() {
        let mut s = HashMap::new();
        s.insert("password".to_string(), "s3cr3t-p@ss".to_string());
        s.insert("cvc".to_string(), "12".to_string()); // < 3 char → tidak digosok
        let out = scrub_secret_from_text("gagal: password=s3cr3t-p@ss untuk u", &s);
        assert!(!out.contains("s3cr3t-p@ss"));
        assert!(out.contains("[REDACTED]"));
    }

    #[test]
    fn build_login_fill_js_aman_dan_reassert_origin() {
        let js = build_login_fill_js("a\"b\\c", "https://example.com");
        // Password ter-escape sebagai literal JSON.
        assert!(js.contains(r#""a\"b\\c""#));
        // Origin ditegaskan ulang sinkron di dalam skrip.
        assert!(js.contains(r#"window.location.origin !== "https://example.com""#));
        assert!(js.contains(r#"input[type="password"]"#));
        assert!(js.contains(r#"JSON.stringify({filled:1})"#));
    }

    #[test]
    fn build_checkout_fill_js_token_tanpa_nilai_bocor_di_struktur() {
        let js = build_checkout_fill_js(
            &[("cc-number", "4111111111111111"), ("cc-csc", "123")],
            "https://shop.test",
        );
        assert!(js.contains(r#"window.location.origin !== "https://shop.test""#));
        assert!(js.contains("cc-number"));
    }

    #[test]
    fn origin_allowed_eksak() {
        let allowed = vec!["https://example.com".to_string()];
        assert!(origin_allowed("https://example.com", &allowed));
        assert!(!origin_allowed("https://sub.example.com", &allowed));
        assert!(!origin_allowed("https://example.com:8443", &allowed));
        assert!(!origin_allowed("http://example.com", &allowed));
    }

    #[test]
    fn local_backend_kontrak() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (store, _dir) = tmp_store();
            let meta = add_login(&store);
            let backend = LocalBackend::with_store(store);
            assert_eq!(backend.name(), "local");
            assert!(backend.owns(&meta.id));
            assert!(!backend.owns("op:xyz"));
            assert!(!backend.needs_unlock());
            let items = backend.list_items().await.unwrap();
            assert_eq!(items.len(), 1);
            assert_eq!(
                backend.get_meta(&meta.id).await.unwrap().unwrap().id,
                meta.id
            );
            assert_eq!(
                backend.resolve_password(&meta.id).await.unwrap(),
                "s3cr3t-p@ss"
            );
            assert_eq!(backend.resolve_otp(&meta.id).await.unwrap(), None);

            let backends: Vec<Box<dyn VaultBackend>> = vec![Box::new(LocalBackend::with_store(
                VaultStore::new(tmp_base()),
            ))];
            assert!(backend_for_handle(&backends, &meta.id).is_some());
            assert!(backend_for_handle(&backends, "op:abc").is_none());
        });
    }

    #[test]
    fn tool_effect_dan_risiko() {
        let tool = BrowserVaultTool;
        assert_eq!(tool.effect(), ToolEffect::Write);
        assert_eq!(
            tool.dynamic_risk(&json!({"action": "list"})),
            RiskLevel::Low
        );
        assert_eq!(
            tool.dynamic_risk(&json!({"action": "fill"})),
            RiskLevel::High
        );
        assert_eq!(tool.risk(), RiskLevel::Medium);
        let schema = tool.parameters_schema();
        let actions = schema["properties"]["action"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            actions,
            vec!["list", "fill", "save_login", "enter_code", "unlock"]
        );
        assert!(schema["required"]
            .as_array()
            .unwrap()
            .contains(&json!("action")));
    }

    #[tokio::test]
    async fn tool_aksi_tak_dikenal_dan_handle_kosong() {
        let ctx = crate::test_support::test_ctx(tmp_base());
        let tool = BrowserVaultTool;
        // Aksi tak dikenal → JSON error, bukan panic.
        let r = tool
            .execute(json!({"action": "ngawur"}), &ctx)
            .await
            .unwrap();
        assert!(r.content.contains("unknown_action"));
        // fill tanpa handle.
        let r = tool.execute(json!({"action": "fill"}), &ctx).await.unwrap();
        assert!(r.content.contains("missing_handle"));
        // unlock backend tak dikenal.
        let r = tool
            .execute(json!({"action": "unlock", "backend": "ngawur"}), &ctx)
            .await
            .unwrap();
        assert!(r.content.contains("unknown_backend"));
    }
}
