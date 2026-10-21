//! Pemisah pesan keluar yang sadar pagar kode (fence-aware).
//!
//! Paritas dengan `truncate_message()` di `gateway/platforms/base.py` aslinya
//! (dibantu `fence_state_after()` di `gateway/platforms/helpers.py`): pesan
//! panjang dipecah menjadi beberapa chunk tanpa pernah memutus di dalam blok
//! kode berpagar (<code>```</code> ... <code>```</code>). Bila sebuah pagar
//! terpotong oleh batas chunk, chunk yang terpotong ditutup dengan
//! <code>```</code> dan chunk berikutnya membuka ulang pagar dengan tag bahasa
//! yang sama — setiap chunk tetap valid sebagai markdown yang berdiri sendiri.
//!
//! Batas panjang pesan keluar per platform, selaras dengan
//! `MAX_MESSAGE_LENGTH` adapter aslinya:
//!
//! | Platform | Batas | Satuan |
//! |----------|-------|--------|
//! | Telegram | 4096  | unit UTF-16 (aslinya memakai `utf16_len`; batas API dihitung dalam code unit UTF-16) |
//! | Discord  | 2000  | karakter |
//! | Slack    | 39000 | karakter (API mengizinkan 40.000; sisakan margin) |
//! | Signal   | 8000  | karakter |
//! | WhatsApp | 4096  | karakter |
//! | Matrix   | 15000 | byte UTF-8 (default; bisa dioverride `MATRIX_MAX_MESSAGE_LENGTH` 500..15000) |
//! | Teams    | 28000 | karakter (~28 KB per pesan) |
//! | Email    | —     | satu email membawa seluruh body (seperti aslinya); tidak dipecah |
//!
//! Penyimpangan yang disengaja dari `truncate_message` aslinya (sesuai
//! spesifikasi tugas batch ini):
//!
//! - Tanpa indikator <code>(1/3)</code> di akhir chunk.
//! - Tanpa fallback "pisah pada spasi": bila tidak ada `\n` di luar pagar
//!   sebelum batas, langsung hard-split pada batas.
//! - Tanpa perlindungan inline-code-span (backtick tunggal yang tak
//!   berpasangan).
//! - Sisa teks setelah titik pisah hanya membuang `\r`/`\n` di depan, bukan
//!   `.lstrip()` penuh seperti aslinya, agar indentasi blok kode dan spasi
//!   antar-kata tidak hilang saat hard-split.

/// Batas pesan Telegram: 4096 unit UTF-16 (lihat [`utf16_len`]).
pub const TELEGRAM_MESSAGE_LIMIT: usize = 4096;
/// Batas pesan Discord: 2000 karakter.
pub const DISCORD_MESSAGE_LIMIT: usize = 2000;
/// Batas pesan Slack: 39000 karakter (API mengizinkan 40.000; sisakan margin).
pub const SLACK_MESSAGE_LIMIT: usize = 39000;
/// Batas pesan Signal: 8000 karakter.
pub const SIGNAL_MESSAGE_LIMIT: usize = 8000;
/// Batas pesan WhatsApp: 4096 karakter (seperti `MAX_MESSAGE_LENGTH` aslinya).
pub const WHATSAPP_MESSAGE_LIMIT: usize = 4096;
/// Batas pesan Matrix: 15000 byte UTF-8 (default aslinya; lihat
/// [`matrix_message_limit`] untuk override env).
pub const MATRIX_MESSAGE_LIMIT: usize = 15000;
/// Batas pesan Teams: 28000 karakter (~28 KB per pesan, seperti aslinya).
pub const TEAMS_MESSAGE_LIMIT: usize = 28000;

/// Panjang `s` dalam karakter Unicode (satuan default kebanyakan platform).
pub fn char_len(s: &str) -> usize {
    s.chars().count()
}

/// Panjang `s` dalam code unit UTF-16 — satuan yang dipakai Telegram untuk
/// menghitung batas 4096 (karakter astral, mis. emoji, dihitung 2).
///
/// Cermin `utf16_len` di `gateway/platforms/base.py` aslinya.
pub fn utf16_len(s: &str) -> usize {
    s.chars().map(|c| c.len_utf16()).sum()
}

/// Panjang `s` dalam byte UTF-8 — satuan batas Matrix (kapasitas event
/// homeserver dihitung dari JSON kanonis UTF-8).
pub fn utf8_len(s: &str) -> usize {
    s.len()
}

/// Batas pesan Matrix efektif: `MATRIX_MAX_MESSAGE_LENGTH` (env), dijepit
/// 500..15000 seperti `_resolve_max_message_length` aslinya; default 15000.
pub fn matrix_message_limit() -> usize {
    std::env::var("MATRIX_MAX_MESSAGE_LENGTH")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .map(|v| v.clamp(500, MATRIX_MESSAGE_LIMIT))
        .unwrap_or(MATRIX_MESSAGE_LIMIT)
}

/// Jumlah karakter (code point) terdepan dari `s` yang total panjangnya
/// menurut `len_fn` tidak melebihi `budget`. Karena iterasi berjalan per
/// karakter (bukan per code unit), titik potong tidak pernah membelah
/// pasangan surrogate / urutan multi-byte.
///
/// Cermin `_custom_unit_to_cp` di `gateway/platforms/base.py` aslinya.
fn units_to_chars(s: &str, budget: usize, len_fn: fn(&str) -> usize) -> usize {
    let mut acc = 0usize;
    let mut n = 0usize;
    let mut buf = [0u8; 4];
    for ch in s.chars() {
        let w = len_fn(ch.encode_utf8(&mut buf));
        if acc + w > budget {
            break;
        }
        acc += w;
        n += 1;
    }
    n
}

/// Penutup pagar yang ditambahkan di akhir chunk yang terpotong di dalam
/// pagar (selalu `"\n```"`, seperti `FENCE_CLOSE` aslinya).
const FENCE_CLOSE: &str = "\n```";

/// Status pagar kode setelah memindai `text` baris per baris, meniru
/// `fence_state_after` di `gateway/platforms/helpers.py` aslinya.
///
/// Mengembalikan `(di_dalam_pagar, tag_bahasa)`: `in_code` adalah status awal
/// sebelum `text` (untuk melanjutkan dari chunk sebelumnya), `lang` adalah
/// tag bahasa pagar yang sedang terbuka saat itu.
fn fence_state_after(text: &str, in_code: bool, lang: &str) -> (bool, String) {
    let mut in_code = in_code;
    let mut lang = lang.to_string();
    for line in text.split('\n') {
        if let Some(after) = line.trim().strip_prefix("```") {
            if in_code {
                in_code = false;
                lang.clear();
            } else {
                in_code = true;
                lang = after.split_whitespace().next().unwrap_or("").to_string();
            }
        }
    }
    (in_code, lang)
}

/// Pecah `text` menjadi chunk-chunk yang masing-masing maksimal `limit`
/// karakter Unicode, tanpa pernah memutus di dalam blok kode berpagar.
///
/// - `limit = None`: tidak ada batas — kembalikan `text` apa adanya dalam
///   satu chunk (untuk platform yang tidak memaksakan batas: Email).
/// - Titik pisah diutamakan pada `\n` terakhir sebelum batas yang berada di
///   luar pagar; bila tidak ada, hard-split tepat pada batas.
/// - Bila pagar terpotong, chunk ditutup dengan <code>```</code> dan chunk
///   berikutnya diawali <code>```bahasa</code> yang sama.
///
/// Selalu mengembalikan minimal satu chunk dan selalu berhenti (setiap
/// iterasi memajukan sisa teks minimal satu karakter).
pub fn split_message(text: &str, limit: Option<usize>) -> Vec<String> {
    split_message_with_len(text, limit, char_len)
}

/// Seperti [`split_message`], tetapi panjang diukur dengan `len_fn` (satuan
/// khusus platform): [`utf16_len`] untuk Telegram, [`utf8_len`] untuk Matrix.
///
/// Cermin parameter `len_fn` pada `truncate_message()` aslinya.
pub fn split_message_with_len(
    text: &str,
    limit: Option<usize>,
    len_fn: fn(&str) -> usize,
) -> Vec<String> {
    let Some(limit) = limit.map(|l| l.max(1)) else {
        return vec![text.to_string()];
    };
    if len_fn(text) <= limit {
        return vec![text.to_string()];
    }

    let mut chunks: Vec<String> = Vec::new();
    let mut rest: &str = text;
    // Tag bahasa pagar yang terbuka di akhir chunk sebelumnya (`None` bila
    // tidak ada) — chunk berikutnya harus membuka ulang pagar ini.
    let mut carry: Option<String> = None;
    // Panjang penutup pagar dalam satuan ukur (konstan untuk semua satuan
    // aditif: ASCII murni).
    let fence_close_len = len_fn(FENCE_CLOSE);

    while !rest.is_empty() {
        let prefix: String = match &carry {
            Some(lang) => format!("```{lang}\n"),
            None => String::new(),
        };
        let prefix_len = len_fn(&prefix);

        // Sisa teks muat dalam satu chunk terakhir: tutup pagar yang dibuka
        // ulang bila ternyata masih terbuka.
        if prefix_len + len_fn(rest) <= limit {
            let mut last = String::with_capacity(prefix.len() + rest.len() + 4);
            last.push_str(&prefix);
            last.push_str(rest);
            if let Some(lang) = carry.take() {
                let (in_code, _) = fence_state_after(rest, true, &lang);
                if in_code {
                    last.push_str(FENCE_CLOSE);
                }
            }
            chunks.push(last);
            break;
        }

        // Anggaran satuan-ukur untuk badan chunk setelah dikurangi prefix
        // pembuka-ulang dan penutup pagar; minimal 1 agar selalu maju.
        let budget = limit.saturating_sub(prefix_len + fence_close_len).max(1);
        // Jumlah karakter yang muat dalam anggaran, lalu batas byte-nya.
        let upto = rest
            .char_indices()
            .nth(units_to_chars(rest, budget, len_fn))
            .map(|(i, _)| i)
            .unwrap_or(rest.len());
        let region = &rest[..upto];

        // Satu sapuan: catat setiap '\n' beserta status pagar *setelah* baris
        // itu, lalu ambil yang terakhir yang berada di luar pagar.
        let mut in_code = carry.is_some();
        let mut lang = carry.clone().unwrap_or_default();
        let mut split_at: Option<usize> = None; // posisi byte '\n'
        let mut line_start = 0usize;
        for (i, ch) in region.char_indices() {
            if ch != '\n' {
                continue;
            }
            if let Some(after) = region[line_start..i].trim().strip_prefix("```") {
                if in_code {
                    in_code = false;
                    lang.clear();
                } else {
                    in_code = true;
                    lang = after.split_whitespace().next().unwrap_or("").to_string();
                }
            }
            if !in_code {
                split_at = Some(i);
            }
            line_start = i + 1;
        }

        // `Some(0)` (teks diawali '\n') akan menghasilkan chunk kosong —
        // jatuhkan ke hard-split seperti aslinya (`split_at < 1`).
        let (chunk_end, next_start) = match split_at {
            Some(p) if p > 0 => (p, p + 1),
            _ => (upto, upto),
        };
        let chunk_body = &rest[..chunk_end];

        let mut chunk = String::with_capacity(prefix.len() + chunk_body.len() + 4);
        chunk.push_str(&prefix);
        chunk.push_str(chunk_body);
        let (body_in_code, body_lang) =
            fence_state_after(chunk_body, carry.is_some(), carry.as_deref().unwrap_or(""));
        if body_in_code {
            chunk.push_str(FENCE_CLOSE);
            carry = Some(body_lang);
        } else {
            carry = None;
        }
        chunks.push(chunk);

        // Maju melewati pemisah; buang baris kosong di depan agar chunk tak
        // diawali baris kosong. Sengaja HANYA membuang `\r`/`\n` (bukan
        // `.trim_start()` penuh seperti `.lstrip()` aslinya) agar indentasi
        // kode dan spasi antar-kata tidak rusak saat hard-split.
        let next = rest[next_start..].trim_start_matches(['\r', '\n']);
        if next.is_empty() {
            break;
        }
        rest = next;
    }

    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pesan_pendek_tidak_dipecah() {
        let text = "halo dunia";
        assert_eq!(split_message(text, Some(4096)), vec![text.to_string()]);
        assert_eq!(split_message(text, None), vec![text.to_string()]);
    }

    #[test]
    fn limit_none_selalu_satu_chunk() {
        let text = "x".repeat(100_000);
        let chunks = split_message(&text, None);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], text);
    }

    #[test]
    fn pisah_pada_newline_luar_pagar() {
        let text = "baris satu\nbaris dua\nbaris tiga";
        let chunks = split_message(text, Some(15));
        assert!(chunks.len() >= 2, "harusnya terpecah: {chunks:?}");
        for c in &chunks {
            assert!(c.chars().count() <= 15, "chunk kepanjangan: {c:?}");
            assert!(!c.starts_with('\n'), "chunk diawali newline: {c:?}");
        }
        // Seluruh konten (tanpa whitespace pemisah) tetap ada.
        let joined: String = chunks.join("\n");
        assert!(joined.contains("baris satu"));
        assert!(joined.contains("baris tiga"));
    }

    #[test]
    fn hard_split_bila_tanpa_newline() {
        let text = "abcdefghij".repeat(10); // 100 karakter tanpa newline
        let chunks = split_message(&text, Some(30));
        assert!(chunks.len() >= 3);
        for c in &chunks {
            assert!(c.chars().count() <= 30, "chunk kepanjangan: {c:?}");
        }
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn pagar_tidak_terputus_antar_chunk() {
        let code = "fn main() {\n    println!(\"halo\");\n}\n".repeat(40);
        let text = format!("Pendahuluan\n\n```rust\n{code}```\n\nPenutup akhir.");
        let chunks = split_message(&text, Some(200));
        assert!(chunks.len() > 1, "harusnya terpecah");
        for c in &chunks {
            assert!(c.chars().count() <= 200, "chunk kepanjangan: {c:?}");
            // Setiap chunk harus seimbang pagarnya bila dipindai dari nol.
            let (in_code, _) = fence_state_after(c, false, "");
            assert!(
                !in_code,
                "chunk berakhir di dalam pagar tanpa penutup: {c:?}"
            );
        }
        // Chunk yang membuka ulang pagar memakai tag bahasa yang sama.
        assert!(
            chunks.iter().any(|c| c.starts_with("```rust\n")),
            "tidak ada chunk yang membuka ulang pagar rust: {chunks:?}"
        );
    }

    #[test]
    fn teks_hanya_pagar_panjang_tetap_aman() {
        // Seluruh teks adalah satu blok kode raksasa tanpa newline di luar
        // pagar: harus hard-split dengan aman (tutup/buka ulang per chunk).
        let text = format!("```python\n{}\n```", "x = 1\n".repeat(2000));
        let chunks = split_message(&text, Some(500));
        assert!(chunks.len() > 1);
        for c in &chunks {
            assert!(c.chars().count() <= 500, "chunk kepanjangan");
            let (in_code, _) = fence_state_after(c, false, "");
            assert!(!in_code, "chunk tak seimbang: {:?}", &c[..c.len().min(60)]);
        }
        // Chunk tengah (bukan pertama/terakhir) dibuka dan ditutup pagar.
        let mid = &chunks[chunks.len() / 2];
        assert!(mid.starts_with("```python\n"), "tak dibuka ulang: {mid:?}");
        assert!(mid.ends_with("\n```"), "tak ditutup: {mid:?}");
    }

    #[test]
    fn pagar_tanpa_tag_bahasa_tetap_seimbang() {
        let text = format!("awal\n\n```\n{}\n```\n\nakhir", "kode\n".repeat(300));
        let chunks = split_message(&text, Some(200));
        assert!(chunks.len() > 1);
        for c in &chunks {
            let (in_code, _) = fence_state_after(c, false, "");
            assert!(!in_code, "chunk tak seimbang: {c:?}");
        }
    }

    #[test]
    fn selalu_berhenti_untuk_input_aneh() {
        // Input patologis: newline di awal, pagar tak ditutup, limit kecil.
        for text in [
            "\n\n\n".to_string(),
            "```\n".to_string(),
            "\n".repeat(50),
            "a".repeat(10),
        ] {
            let chunks = split_message(&text, Some(3));
            assert!(!chunks.is_empty());
            for c in &chunks {
                assert!(c.chars().count() <= 3, "chunk kepanjangan: {c:?}");
            }
        }
    }

    #[test]
    fn batas_per_platform_dihormati() {
        let cases = [
            (DISCORD_MESSAGE_LIMIT, "discord"),
            (SLACK_MESSAGE_LIMIT, "slack"),
            (SIGNAL_MESSAGE_LIMIT, "signal"),
            (WHATSAPP_MESSAGE_LIMIT, "whatsapp"),
            (TEAMS_MESSAGE_LIMIT, "teams"),
        ];
        for (limit, name) in cases {
            // Campuran teks + blok kode agar jalur fence-aware teruji.
            let code = "let x = 1;\n".repeat(limit);
            let text = format!("intro\n\n```rust\n{code}```\n\noutro");
            let chunks = split_message(&text, Some(limit));
            assert!(chunks.len() > 1, "{name}: harusnya terpecah");
            for c in &chunks {
                assert!(
                    c.chars().count() <= limit,
                    "{name}: chunk {} > batas {limit}",
                    c.chars().count()
                );
            }
        }
    }

    #[test]
    fn konstanta_batas_sesuai_spesifikasi() {
        assert_eq!(TELEGRAM_MESSAGE_LIMIT, 4096);
        assert_eq!(DISCORD_MESSAGE_LIMIT, 2000);
        assert_eq!(SLACK_MESSAGE_LIMIT, 39000);
        assert_eq!(SIGNAL_MESSAGE_LIMIT, 8000);
        assert_eq!(WHATSAPP_MESSAGE_LIMIT, 4096);
        assert_eq!(MATRIX_MESSAGE_LIMIT, 15000);
        assert_eq!(TEAMS_MESSAGE_LIMIT, 28000);
    }

    #[test]
    fn utf16_len_menghitung_unit_utf16() {
        // ASCII = 1 unit; CJK = 1 unit; karakter astral (emoji) = 2 unit —
        // seperti `utf16_len` di gateway/platforms/base.py aslinya.
        assert_eq!(utf16_len("halo"), 4);
        assert_eq!(utf16_len("日本"), 2);
        assert_eq!(utf16_len("😀"), 2);
        assert_eq!(utf16_len("a😀b"), 4);
        assert_eq!(utf8_len("日本"), 6);
    }

    #[test]
    fn telegram_split_menghormati_batas_utf16() {
        // 3000 emoji = 6000 unit UTF-16 > 4096 → harus terpecah, dan tiap
        // chunk tidak melebihi 4096 unit UTF-16 (dengan satuan karakter
        // biasa, teks ini "hanya" 3000 karakter dan tidak akan terpecah).
        let text = "😀".repeat(3000);
        let chunks = split_message_with_len(text.as_str(), Some(TELEGRAM_MESSAGE_LIMIT), utf16_len);
        assert!(chunks.len() > 1, "harusnya terpecah: {}", chunks.len());
        for c in &chunks {
            assert!(
                utf16_len(c) <= TELEGRAM_MESSAGE_LIMIT,
                "chunk {} unit > 4096",
                utf16_len(c)
            );
        }
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn matrix_split_menghormati_batas_byte_utf8() {
        // 6000 karakter CJK = 18000 byte > 15000 → terpecah per byte.
        let text = "日".repeat(6000);
        let chunks = split_message_with_len(text.as_str(), Some(MATRIX_MESSAGE_LIMIT), utf8_len);
        assert!(chunks.len() > 1, "harusnya terpecah");
        for c in &chunks {
            assert!(
                c.len() <= MATRIX_MESSAGE_LIMIT,
                "chunk {} byte > 15000",
                c.len()
            );
        }
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn matrix_limit_env_dijepit_500_sampai_15000() {
        // Env bersifat global per-proses: serialkan seperti tes allowlist.
        let _g = crate::allowlist::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("MATRIX_MAX_MESSAGE_LENGTH").ok();
        std::env::set_var("MATRIX_MAX_MESSAGE_LENGTH", "2000");
        assert_eq!(matrix_message_limit(), 2000);
        std::env::set_var("MATRIX_MAX_MESSAGE_LENGTH", "10");
        assert_eq!(matrix_message_limit(), 500, "dijepit ke bawah");
        std::env::set_var("MATRIX_MAX_MESSAGE_LENGTH", "99999");
        assert_eq!(matrix_message_limit(), 15000, "dijepit ke atas");
        std::env::set_var("MATRIX_MAX_MESSAGE_LENGTH", "bukan-angka");
        assert_eq!(matrix_message_limit(), 15000, "tak valid → default");
        match prev {
            Some(v) => std::env::set_var("MATRIX_MAX_MESSAGE_LENGTH", v),
            None => std::env::remove_var("MATRIX_MAX_MESSAGE_LENGTH"),
        }
    }

    #[test]
    fn units_to_chars_tidak_membelah_surrogate() {
        // Budget 5 unit UTF-16 atas "a😀b": 'a'=1, '😀'=2 (total 3),
        // 'b' akan jadi 4 ≤ 5 → 3 karakter.
        assert_eq!(units_to_chars("a😀b", 5, utf16_len), 3);
        // Budget 2: 'a'=1, '😀' butuh 2 (total 3 > 2) → berhenti di 1.
        assert_eq!(units_to_chars("a😀b", 2, utf16_len), 1);
        // Budget 0 → 0 karakter (pemanggil menjamin budget ≥ 1).
        assert_eq!(units_to_chars("abc", 0, char_len), 0);
    }
}
