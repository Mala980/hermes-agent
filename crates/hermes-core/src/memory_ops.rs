//! Operasi murni atas entri memori: pencocokan toleran-tipografi,
//! penyambungan patch satu-rentang, dan anggaran karakter injeksi prompt.
//!
//! Dipakai oleh implementasi default trait `MemoryStore` (`ports.rs`) dan
//! implementasi SQLite (`hermes-memory`), sehingga perilaku `memory_patch`
//! dan budget karakter konsisten di semua store.

use crate::{HermesError, MemoryConfig, MemoryEntry, Result};

/// Penanda pemotongan: dipakai baik saat teks entri dipotong ke
/// `memory_max_chars_per_entry` maupun saat entri tertua dibuang karena
/// total melebihi `memory_max_total_chars`.
pub const TRUNCATED_MARKER: &str = "[truncated]";

/// Hasil penerapan anggaran karakter ke daftar entri yang akan disuntikkan
/// ke system prompt.
pub struct MemoryBudgetReport {
    /// Entri yang lolos anggaran, dalam urutan semula (relevansi), dengan
    /// teks yang sudah dipotong bila melebihi batas per-entri.
    pub entries: Vec<MemoryEntry>,
    /// Jumlah entri yang teksnya dipotong.
    pub truncated_entries: usize,
    /// Jumlah entri tertua yang dibuang karena total melebihi anggaran.
    pub dropped_entries: usize,
}

impl MemoryBudgetReport {
    /// Benar bila ada pemotongan/pembuangan (pemanggil menambahkan catatan).
    pub fn was_truncated(&self) -> bool {
        self.truncated_entries > 0 || self.dropped_entries > 0
    }
}

/// Terapkan anggaran karakter memori (W5-B25) ke entri yang akan disuntikkan
/// ke system prompt.
///
/// - Tiap entri dipotong ke `cfg.memory_max_chars_per_entry` karakter
///   (diberi penanda `[truncated]`).
/// - Bila total masih melebihi `cfg.memory_max_total_chars`, entri **tertua**
///   dibuang lebih dulu sampai muat.
/// - Nilai 0 pada salah satu batas berarti "tanpa batas" (aman terhadap
///   salah konfigurasi).
///
/// Penyimpangan terdokumentasi dari original: aslinya MENOLAK tulis yang
/// melebihi `memory_char_limit`/`user_char_limit` (dengan instruksi
/// konsolidasi) dan tidak pernah memotong memori pengguna saat injeksi.
/// Port ini memotong saat injeksi sesuai arahan tugas, karena store port
/// tidak menolak tulis over-budget.
pub fn apply_memory_char_budget(entries: &[MemoryEntry], cfg: &MemoryConfig) -> MemoryBudgetReport {
    let per_entry = cfg.memory_max_chars_per_entry;
    let total = cfg.memory_max_total_chars;

    // Tahap 1: potong per entri.
    let mut kept: Vec<MemoryEntry> = Vec::with_capacity(entries.len());
    let mut truncated_entries = 0usize;
    for e in entries {
        let mut e = e.clone();
        let len = e.text.chars().count();
        if per_entry > 0 && len > per_entry {
            let cut: String = e.text.chars().take(per_entry).collect();
            e.text = format!("{cut}… {TRUNCATED_MARKER}");
            truncated_entries += 1;
        }
        kept.push(e);
    }

    // Tahap 2: buang entri tertua dulu bila total melebihi anggaran.
    let mut dropped_entries = 0usize;
    if total > 0 {
        // Indeks diurutkan dari tertua ke terbaru; yang tertua dibuang dulu.
        let mut by_age: Vec<usize> = (0..kept.len()).collect();
        by_age.sort_by_key(|&i| kept[i].created_at);
        let mut drop = vec![false; kept.len()];
        let mut current: usize = kept.iter().map(|e| e.text.chars().count()).sum();
        for &i in &by_age {
            if current <= total {
                break;
            }
            // Jangan buang entri terakhir: lebih baik satu entri over-budget
            // daripada prompt memori yang kosong total.
            if kept.len() - dropped_entries <= 1 {
                break;
            }
            current -= kept[i].text.chars().count();
            drop[i] = true;
            dropped_entries += 1;
        }
        if dropped_entries > 0 {
            kept = kept
                .into_iter()
                .enumerate()
                .filter(|(i, _)| !drop[*i])
                .map(|(_, e)| e)
                .collect();
        }
    }

    MemoryBudgetReport {
        entries: kept,
        truncated_entries,
        dropped_entries,
    }
}

/// Catatan `[truncated]` untuk hint memori ketika ada entri yang dibuang
/// karena total melebihi anggaran.
pub fn truncated_dropped_note(dropped: usize, total_budget: usize) -> String {
    format!(
        "{TRUNCATED_MARKER} {dropped} entri tertua dibuang agar muat dalam anggaran {total_budget} karakter."
    )
}

// ---------------------------------------------------------------------------
// Pencocokan toleran-tipografi untuk `memory_patch`.
// Port dari `_normalize_for_match` / `_find_unique_match` di
// `tools/memory_tool_store.py` aslinya.
// ---------------------------------------------------------------------------

/// Bentuk lipat untuk pencocokan toleran: model sering mengetik ulang entri
/// dengan tanda kutip lurus untuk kutip kurawal, `-` untuk em dash, dan
/// baris yang ter-wrap ulang. Semua varian kutip/backtick -> `'`, semua
/// varian dash -> `-`, dan rangkaian whitespace -> satu spasi.
fn normalize_for_match(text: &str) -> String {
    let folded: String = text
        .replace("\\n", " ")
        .chars()
        .map(|c| match c {
            '"' | '`' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' | '\u{201C}'
            | '\u{201D}' | '\u{201E}' | '\u{201F}' => '\'',
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
            | '\u{2212}' => '-',
            _ => c,
        })
        .collect();
    folded.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Indeks entri yang dipilih `old_text`, atau error bila tidak ada / ambigu.
///
/// - Entri yang SAMA PERSIS dengan `old_text` (whole-entry exact) selalu
///   menang mutlak — substring hanya dipertimbangkan bila tidak ada entri
///   yang sama persis (aslinya: entri pendek tetap teralamatkan walau
///   teksnya termuat di entri lain yang lebih panjang).
/// - Bila beberapa entri BERBEDA cocok -> ambigu (error), seperti aslinya.
pub fn locate_entry_index(entries: &[MemoryEntry], old_text: &str) -> Result<usize> {
    if old_text.trim().is_empty() {
        return Err(HermesError::Memory(
            "old_text tidak boleh kosong.".to_string(),
        ));
    }
    // Prioritas 1: whole-entry exact.
    let exact: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.text == old_text)
        .map(|(i, _)| i)
        .collect();
    let matches: Vec<usize> = if !exact.is_empty() {
        exact
    } else {
        substring_matches(entries, old_text)
    };
    // Duplikat persis aman (yang pertama menang); entri berbeda -> ambigu.
    let distinct: std::collections::HashSet<&str> =
        matches.iter().map(|&i| entries[i].text.as_str()).collect();
    if distinct.len() > 1 {
        return Err(HermesError::Memory(format!(
            "old_text cocok dengan {} entri berbeda — buat lebih spesifik.",
            distinct.len()
        )));
    }
    matches.into_iter().next().ok_or_else(|| {
        HermesError::Memory(format!(
            "tidak ada entri memori yang cocok dengan '{old_text}'. \
             Salin old_text langsung dari entri yang dimaksud."
        ))
    })
}

fn substring_matches(entries: &[MemoryEntry], old_text: &str) -> Vec<usize> {
    let exact: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.text.contains(old_text))
        .map(|(i, _)| i)
        .collect();
    if !exact.is_empty() {
        return exact;
    }
    let needle = normalize_for_match(old_text);
    // Tanda baca/spasi saja akan memilih seluruh entri — tolak (aslinya).
    if !needle.chars().any(|c| c.is_alphanumeric()) {
        return vec![];
    }
    entries
        .iter()
        .enumerate()
        .filter(|(_, e)| normalize_for_match(&e.text).contains(needle.as_str()))
        .map(|(i, _)| i)
        .collect()
}

/// Ganti SATU rentang `old_text` di dalam teks entri dengan `new_text`
/// (paritas `_patch_entry` aslinya):
/// - rentang yang muncul >1 kali ditolak (ambigu) daripada diedit di semua tempat;
/// - hasil yang identik dengan semula ditolak (no-op).
pub fn splice_patch_span(entry_text: &str, old_text: &str, new_text: &str) -> Result<String> {
    let count = span_occurrences(entry_text, old_text);
    if count > 1 {
        return Err(HermesError::Memory(format!(
            "'{old_text}' muncul {count} kali dalam entri; patch hanya mengedit satu \
             rentang. Sertakan teks di sekitarnya agar old_text hanya muncul sekali."
        )));
    }
    let patched = entry_text.replacen(old_text, new_text, 1);
    if patched == entry_text {
        return Err(HermesError::Memory(format!(
            "patch tidak mengubah apa pun (no-op): '{old_text}' tidak ditemukan \
             byte-persis di entri."
        )));
    }
    Ok(patched)
}

/// Kemunculan `span` di `text`, termasuk yang tumpang tindih
/// (aslinya: '00' muncul dua kali di '1000').
fn span_occurrences(text: &str, span: &str) -> usize {
    if span.is_empty() || span.len() > text.len() {
        return 0;
    }
    (0..=(text.len() - span.len()))
        .filter(|&i| text[i..].starts_with(span))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn entry_at(text: &str, secs: i64) -> MemoryEntry {
        let mut e = MemoryEntry::new("fact", text);
        e.created_at = chrono::Utc.timestamp_opt(secs, 0).unwrap();
        e
    }

    fn cfg(per_entry: usize, total: usize) -> MemoryConfig {
        MemoryConfig {
            memory_max_chars_per_entry: per_entry,
            memory_max_total_chars: total,
        }
    }

    #[test]
    fn budget_truncates_long_entry_with_marker() {
        let entries = vec![entry_at(&"x".repeat(100), 1)];
        let r = apply_memory_char_budget(&entries, &cfg(10, 0));
        assert_eq!(r.entries.len(), 1);
        assert_eq!(r.truncated_entries, 1);
        assert!(r.entries[0].text.ends_with(TRUNCATED_MARKER));
        assert!(r.entries[0].text.chars().count() <= 10 + TRUNCATED_MARKER.len() + 3);
    }

    #[test]
    fn budget_drops_oldest_first_when_over_total() {
        let entries = vec![
            entry_at("tertua", 1),
            entry_at("tengah", 2),
            entry_at("terbaru", 3),
        ];
        // Total 6 + 5 + 7 = 18 > 12: buang tertua ("tertua", lalu "tengah").
        let r = apply_memory_char_budget(&entries, &cfg(0, 12));
        assert_eq!(r.dropped_entries, 2);
        assert_eq!(r.truncated_entries, 0);
        let texts: Vec<&str> = r.entries.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, vec!["terbaru"]);
    }

    #[test]
    fn budget_keeps_newest_order_and_never_empties() {
        let entries = vec![entry_at("satu", 1), entry_at("dua", 2)];
        // Anggaran 1: hanya satu entri yang bisa bertahan — yang terbaru.
        let r = apply_memory_char_budget(&entries, &cfg(0, 1));
        assert_eq!(r.entries.len(), 1);
        assert_eq!(r.entries[0].text, "dua");
    }

    #[test]
    fn budget_zero_means_unlimited() {
        let entries = vec![entry_at(&"y".repeat(5000), 1)];
        let r = apply_memory_char_budget(&entries, &cfg(0, 0));
        assert_eq!(r.entries.len(), 1);
        assert_eq!(r.truncated_entries, 0);
        assert_eq!(r.dropped_entries, 0);
        assert!(!r.was_truncated());
    }

    #[test]
    fn locate_prefers_whole_entry_exact_match() {
        let entries = vec![
            entry_at("test", 1),
            entry_at("semua tests lulus hari ini", 2),
        ];
        // "test" adalah entri utuh pertama -> menang walau substring ada di entri lain.
        assert_eq!(locate_entry_index(&entries, "test").unwrap(), 0);
    }

    #[test]
    fn locate_reports_ambiguous_on_distinct_matches() {
        let entries = vec![entry_at("apel merah", 1), entry_at("apel hijau", 2)];
        assert!(locate_entry_index(&entries, "apel").is_err());
    }

    #[test]
    fn locate_folds_typography() {
        let entries = vec![entry_at("dia bilang \u{201c}halo\u{201d} — pergi", 1)];
        // Kutip lurus + dash biasa tetap cocok dengan kutip kurawal + em dash.
        assert_eq!(
            locate_entry_index(&entries, "dia bilang \"halo\" - pergi").unwrap(),
            0
        );
    }

    #[test]
    fn splice_rejects_repeated_span() {
        let err = splice_patch_span("a X b X c", "X", "Y").unwrap_err();
        assert!(err.to_string().contains("2 kali"));
    }

    #[test]
    fn splice_rejects_noop() {
        assert!(splice_patch_span("abc", "zzz", "Y").is_err());
    }

    #[test]
    fn splice_replaces_single_span() {
        assert_eq!(
            splice_patch_span("teh manis", "teh", "kopi").unwrap(),
            "kopi manis"
        );
    }
}
