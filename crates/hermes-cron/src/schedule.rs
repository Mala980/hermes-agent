//! Parser jadwal terpadu untuk cron job.
//!
//! Kumpulan format yang diterima — tetap dan terdokumentasi; NLP penuh
//! (mis. "tiap senin jam 9 pagi") di luar cakupan:
//!
//! - Cron 6 kolom: `"0 30 9 * * *"` (detik menit jam tanggal bulan weekday).
//!   Disimpan byte-identik seperti yang diketik.
//! - Cron 5 kolom: `"30 9 * * *"` — detik diasumsikan `0`, dinormalisasi
//!   menjadi `"0 30 9 * * *"`. Dideteksi dari jumlah kolom (5 vs 6).
//! - Interval: `"every 15m"`, `"every 1h"`, `"every 30s"` (satuan: `s` detik,
//!   `m` menit, `h` jam, `d` hari, `w` minggu; tidak peka huruf besar/kecil).
//!   Berulang tiap durasi sejak tick terakhir.
//! - Sekali jalan: `"at 2026-10-22T09:00:00+08:00"` (ISO-8601/RFC3339).
//!   Tanpa offset (`"at 2026-10-22T09:00:00"`) dibaca sebagai wall-clock di
//!   zona waktu job. Waktu yang sudah lewat ditolak saat add. Job sekali-jalan
//!   **dihapus otomatis dari store** setelah berjalan tepat satu kali
//!   (lihat [`crate::scheduler`]); bila scheduler mati melewati waktunya, job
//!   tetap jalan sekali (terlambat) pada tick berikutnya, lalu dihapus.
//! - Kata kunci (tidak peka huruf besar/kecil):
//!   `hourly` → tiap jam tepat (`0 0 * * * *`),
//!   `daily`/`midnight` → tiap tengah malam (`0 0 0 * * *`),
//!   `weekly` → tiap Minggu tengah malam (`0 0 0 * * 1`; crate `cron` memakai
//!   1=Minggu … 7=Sabtu, bukan 0=Minggu ala cron UNIX),
//!   `noon` → tiap tengah hari (`0 0 12 * * *`).
//!
//! Semua jalur — [`CronJob::new`](crate::CronJob::new) (dipakai tool
//! `cron_add` maupun CLI `hermes cron add`) dan perhitungan ulang
//! `next_run` di scheduler — memakai [`parse_schedule`] yang sama.
//! Input yang tak bisa diparse menghasilkan error yang menyebut semua format
//! di atas.

use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};
use hermes_core::{HermesError, Result};
use std::str::FromStr;

/// Bantuan format jadwal; disisipkan ke setiap error parse.
const SCHEDULE_HELP: &str = "Format jadwal yang diterima:\n  \
    - cron 6 kolom: \"0 30 9 * * *\" (detik menit jam tanggal bulan weekday)\n  \
    - cron 5 kolom: \"30 9 * * *\" (detik diasumsikan 0)\n  \
    - interval: \"every 15m\" / \"every 1h\" / \"every 30s\" (satuan: s, m, h, d, w)\n  \
    - sekali jalan: \"at 2026-10-22T09:00:00+08:00\" (ISO-8601/RFC3339; tanpa offset = wall-clock zona waktu job)\n  \
    - kata kunci: hourly, daily, weekly, midnight, noon";

/// Jadwal yang sudah diparse dan dinormalisasi.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedSchedule {
    /// Cron berulang; `expr` sudah 6/7 kolom dan tervalidasi crate `cron`.
    Cron { expr: String },
    /// Interval berulang dalam detik (jangkar: tick terakhir).
    Interval { secs: u64 },
    /// Sekali jalan pada satu titik waktu.
    Once { at: OnceTime },
}

/// Titik waktu one-shot: instan absolut, atau wall-clock naif yang
/// diinterpretasikan di zona waktu job saat `next_run` dihitung.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnceTime {
    /// Instan absolut (RFC3339 ber-offset / `Z`).
    Instant(DateTime<Utc>),
    /// Wall-clock naif (`YYYY-MM-DDTHH:MM:SS` tanpa offset).
    WallClock(NaiveDateTime),
}

impl ParsedSchedule {
    /// Bentuk kanonis untuk disimpan di kolom `schedule` milik [`crate::CronJob`].
    /// `parse_schedule` atas bentuk kanonis selalu berhasil dan ekuivalen.
    pub fn canonical(&self) -> String {
        match self {
            ParsedSchedule::Cron { expr } => expr.clone(),
            ParsedSchedule::Interval { secs } => format!("every {secs}s"),
            ParsedSchedule::Once { at } => match at {
                OnceTime::Instant(dt) => format!("at {}", dt.to_rfc3339()),
                OnceTime::WallClock(naive) => {
                    format!("at {}", naive.format("%Y-%m-%dT%H:%M:%S"))
                }
            },
        }
    }

    /// True bila ini jadwal sekali-jalan.
    pub fn is_oneshot(&self) -> bool {
        matches!(self, ParsedSchedule::Once { .. })
    }

    /// Kemunculan berikutnya setelah `from` (eksklusif), atau `None` bila
    /// tidak ada (one-shot yang sudah lewat, atau one-shot yang sudah
    /// pernah berjalan — `last_run` terisi).
    ///
    /// Murni/deterministik terhadap `from` dan `last_run`: tidak membaca jam dinding.
    pub fn next_run_after(
        &self,
        tz: chrono_tz::Tz,
        from: DateTime<Utc>,
        last_run: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        match self {
            ParsedSchedule::Cron { expr } => {
                // `upcoming(tz)` pada kode lama = `after(now)`; memakai
                // `after(from)` membuat hasilnya deterministik tanpa
                // mengubah perilaku untuk from = now.
                let after = from.with_timezone(&tz);
                cron::Schedule::from_str(expr)
                    .ok()?
                    .after(&after)
                    .next()
                    .map(|dt| dt.with_timezone(&Utc))
            }
            ParsedSchedule::Interval { secs } => {
                let secs = i64::try_from(*secs).ok()?;
                Some(from + chrono::Duration::seconds(secs))
            }
            ParsedSchedule::Once { at } => {
                // Sekali jalan yang sudah fired tidak pernah dijadwalkan lagi.
                if last_run.is_some() {
                    return None;
                }
                let instant = match at {
                    OnceTime::Instant(dt) => *dt,
                    OnceTime::WallClock(naive) => tz
                        .from_local_datetime(naive)
                        .earliest()?
                        .with_timezone(&Utc),
                };
                (instant > from).then_some(instant)
            }
        }
    }
}

/// Parse string jadwal menjadi [`ParsedSchedule`]. Error selalu menyebut
/// format-format yang diterima.
pub fn parse_schedule(input: &str) -> Result<ParsedSchedule> {
    let s = input.trim();
    if s.is_empty() {
        return Err(cron_help(format!("jadwal kosong: `{input}`")));
    }
    let lower = s.to_ascii_lowercase();

    // 1. Kata kunci natural-language-lite (satu token).
    let keyword = match lower.as_str() {
        "hourly" => Some("0 0 * * * *"),
        "daily" | "midnight" => Some("0 0 0 * * *"),
        // Crate `cron`: 1=Minggu … 7=Sabtu (bukan 0=Minggu ala cron UNIX).
        "weekly" => Some("0 0 0 * * 1"),
        "noon" => Some("0 0 12 * * *"),
        _ => None,
    };
    if let Some(expr) = keyword {
        return Ok(ParsedSchedule::Cron {
            expr: expr.to_string(),
        });
    }

    // 2/3. Awalan kata tunggal: "every <durasi>" atau "at <datetime>".
    // Token pertama yang bukan kata kunci tak mungkin jadi kolom cron
    // yang valid, jadi tidak ada ambiguitas dengan jalur cron di bawah.
    let mut tokens = s.split_whitespace();
    let first = tokens.next().unwrap_or("").to_ascii_lowercase();
    if first == "every" {
        let rest: Vec<&str> = tokens.collect();
        let dur = rest
            .first()
            .filter(|_| rest.len() == 1)
            .copied()
            .unwrap_or("");
        let secs = parse_duration(dur).ok_or_else(|| {
            HermesError::Cron(format!(
                "interval tidak valid: `{input}`. Pakai `every <durasi>`, mis. \
                 `every 15m`, `every 1h`, `every 30s` (satuan: s detik, m menit, \
                 h jam, d hari, w minggu).\n{SCHEDULE_HELP}"
            ))
        })?;
        return Ok(ParsedSchedule::Interval { secs });
    }
    if first == "at" {
        let rest: Vec<&str> = tokens.collect();
        if rest.is_empty() {
            return Err(HermesError::Cron(format!(
                "waktu one-shot kosong: `{input}`. Pakai `at <datetime ISO-8601>`, \
                 mis. `at 2026-10-22T09:00:00+08:00`.\n{SCHEDULE_HELP}"
            )));
        }
        // Gabungkan lagi agar "at 2026-10-22 09:00:00" (pemisah spasi) bisa diparse.
        return parse_once(&rest.join(" "), input);
    }

    // 4. Ekspresi cron: 5 kolom → detik diasumsikan 0; selain itu
    // teruskan apa adanya (6/7 kolom) persis seperti perilaku lama.
    let expr = if s.split_whitespace().count() == 5 {
        format!("0 {s}")
    } else {
        s.to_string()
    };
    cron::Schedule::from_str(&expr)
        .map(|_| ParsedSchedule::Cron { expr })
        .map_err(|e| cron_help(format!("ekspresi cron tidak valid: {e}")))
}

/// Bungkus pesan error dengan daftar format yang diterima.
fn cron_help(msg: String) -> HermesError {
    HermesError::Cron(format!("{msg}\n{SCHEDULE_HELP}"))
}

/// Parse `<n><unit>`; unit: s/m/h/d/w (+ alias umum). Batas atas 10 tahun.
fn parse_duration(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    let cut = s.find(|c: char| !c.is_ascii_digit())?;
    let (num, unit) = s.split_at(cut);
    let n: u64 = num.parse().ok()?;
    if n == 0 {
        return None;
    }
    let mult: u64 = match unit {
        "s" | "sec" | "secs" | "second" | "seconds" | "detik" => 1,
        "m" | "min" | "mins" | "minute" | "minutes" | "menit" => 60,
        "h" | "hr" | "hrs" | "hour" | "hours" | "jam" => 3600,
        "d" | "day" | "days" | "hari" => 86_400,
        "w" | "week" | "weeks" | "minggu" => 604_800,
        _ => return None,
    };
    let secs = n.checked_mul(mult)?;
    // Batas kewarasan: 10 tahun.
    (secs <= 10 * 366 * 86_400).then_some(secs)
}

/// Parse badan `at <datetime>`: RFC3339 ber-offset dulu, lalu bentuk naif.
/// Waktu yang sudah lewat ditolak (cek best-effort memakai zona lokal
/// sistem untuk input naif — zona waktu job final baru diketahui belakangan).
fn parse_once(dt_str: &str, original: &str) -> Result<ParsedSchedule> {
    let now = Utc::now();
    if let Ok(dt) = DateTime::parse_from_rfc3339(dt_str) {
        let at = dt.with_timezone(&Utc);
        if at <= now {
            return Err(HermesError::Cron(format!(
                "waktu one-shot `{original}` sudah lewat; pakai waktu di masa depan.\n{SCHEDULE_HELP}"
            )));
        }
        return Ok(ParsedSchedule::Once {
            at: OnceTime::Instant(at),
        });
    }
    let naive = [
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
    ]
    .iter()
    .find_map(|fmt| NaiveDateTime::parse_from_str(dt_str, fmt).ok())
    .or_else(|| {
        NaiveDate::parse_from_str(dt_str, "%Y-%m-%d")
            .ok()
            .and_then(|d| d.and_hms_opt(0, 0, 0))
    });
    match naive {
        Some(naive) => {
            // Cek lewat best-effort: interpretasikan di zona lokal sistem.
            let probe = system_local_tz()
                .from_local_datetime(&naive)
                .earliest()
                .map(|dt| dt.with_timezone(&Utc));
            if probe.map(|at| at <= now).unwrap_or(false) {
                return Err(HermesError::Cron(format!(
                    "waktu one-shot `{original}` sudah lewat; pakai waktu di masa depan.\n{SCHEDULE_HELP}"
                )));
            }
            Ok(ParsedSchedule::Once {
                at: OnceTime::WallClock(naive),
            })
        }
        None => Err(HermesError::Cron(format!(
            "waktu one-shot tidak valid: `{original}`. Pakai `at <datetime ISO-8601>`, \
             mis. `at 2026-10-22T09:00:00+08:00` atau `at 2026-10-22T09:00:00` \
             (tanpa offset = wall-clock zona waktu job).\n{SCHEDULE_HELP}"
        ))),
    }
}

/// Zona waktu lokal sistem sebagai [`chrono_tz::Tz`]; UTC bila tak terdeteksi.
pub fn system_local_tz() -> chrono_tz::Tz {
    iana_time_zone::get_timezone()
        .ok()
        .and_then(|name| name.parse::<chrono_tz::Tz>().ok())
        .unwrap_or(chrono_tz::UTC)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, s).single().unwrap()
    }

    #[test]
    fn five_field_cron_assumes_seconds_zero() {
        let p = parse_schedule("30 9 * * *").unwrap();
        assert_eq!(
            p,
            ParsedSchedule::Cron {
                expr: "0 30 9 * * *".into()
            }
        );
        assert_eq!(p.canonical(), "0 30 9 * * *");
        // Next-run deterministik: dari 2026-10-21T00:00:00Z → 09:30 hari itu.
        let next = p
            .next_run_after(chrono_tz::UTC, utc(2026, 10, 21, 0, 0, 0), None)
            .unwrap();
        assert_eq!(next, utc(2026, 10, 21, 9, 30, 0));
    }

    #[test]
    fn six_field_cron_stored_verbatim() {
        let p = parse_schedule("0 30 9 * * *").unwrap();
        assert_eq!(p.canonical(), "0 30 9 * * *");
        // 7 kolom (dengan tahun) juga lewat apa adanya seperti perilaku lama.
        assert!(parse_schedule("0 0 0 1 1 * 2030").is_ok());
    }

    #[test]
    fn invalid_cron_error_names_expected_formats() {
        // 6 kolom invalid: pesan lama dipertahankan sebagai awalan…
        let err = parse_schedule("0 0 25 * * *").unwrap_err().to_string();
        assert!(err.contains("ekspresi cron tidak valid"), "pesan: {err}");
        // …tapi kini menyebut semua format yang diterima.
        for needle in [
            "cron 6 kolom",
            "cron 5 kolom",
            "every 15m",
            "at 2026-10-22T09:00:00+08:00",
            "hourly",
        ] {
            assert!(
                err.contains(needle),
                "error harus menyebut `{needle}`: {err}"
            );
        }
        // Bukan cron sama sekali → error juga menyebut format.
        let err2 = parse_schedule("kapan saja").unwrap_err().to_string();
        assert!(err2.contains("cron 5 kolom"), "pesan: {err2}");
        assert!(parse_schedule("").is_err());
        assert!(parse_schedule("   ").is_err());
    }

    #[test]
    fn interval_parses_units_case_insensitive() {
        let cases = [
            ("every 15m", 900),
            ("every 1h", 3600),
            ("every 30s", 30),
            ("EVERY 2H", 7200),
            ("every 1d", 86_400),
            ("every 1w", 604_800),
            ("every 90s", 90),
        ];
        for (input, secs) in cases {
            assert_eq!(
                parse_schedule(input).unwrap(),
                ParsedSchedule::Interval { secs },
                "{input}"
            );
        }
    }

    #[test]
    fn interval_next_run_is_from_plus_duration() {
        let p = parse_schedule("every 15m").unwrap();
        assert_eq!(p.canonical(), "every 900s");
        let from = utc(2026, 10, 21, 12, 0, 0);
        let next = p.next_run_after(chrono_tz::UTC, from, None).unwrap();
        assert_eq!(next, from + chrono::Duration::minutes(15));
        // Round-trip bentuk kanonis.
        assert_eq!(parse_schedule(&p.canonical()).unwrap(), p);
    }

    #[test]
    fn interval_rejects_garbage() {
        for bad in [
            "every",
            "every xyz",
            "every -5m",
            "every 0m",
            "every 1.5h",
            "every 15m 2h",
            "every 9999999999999w",
        ] {
            let err = parse_schedule(bad).unwrap_err().to_string();
            assert!(
                err.contains("interval tidak valid"),
                "`{bad}` harus error interval: {err}"
            );
        }
    }

    #[test]
    fn once_with_offset_resolves_to_instant() {
        // 09:00 +08:00 == 01:00Z. (Tanggal jauh di masa depan agar tes stabil.)
        let p = parse_schedule("at 2030-06-15T09:00:00+08:00").unwrap();
        let next = p
            .next_run_after(chrono_tz::UTC, utc(2030, 6, 14, 0, 0, 0), None)
            .unwrap();
        assert_eq!(next, utc(2030, 6, 15, 1, 0, 0));
        // Dari setelah waktunya → tidak ada kemunculan lagi.
        assert!(p
            .next_run_after(chrono_tz::UTC, utc(2030, 6, 16, 0, 0, 0), None)
            .is_none());
        assert_eq!(p.canonical(), "at 2030-06-15T01:00:00+00:00");
        assert_eq!(parse_schedule(&p.canonical()).unwrap(), p);
    }

    #[test]
    fn once_accepts_z_suffix_and_space_separator() {
        let p = parse_schedule("at 2030-06-15T01:00:00Z").unwrap();
        assert!(matches!(
            p,
            ParsedSchedule::Once {
                at: OnceTime::Instant(_)
            }
        ));
        let p2 = parse_schedule("at 2030-06-15 01:00:00").unwrap();
        assert!(matches!(
            p2,
            ParsedSchedule::Once {
                at: OnceTime::WallClock(_)
            }
        ));
    }

    #[test]
    fn once_naive_uses_job_timezone() {
        let p = parse_schedule("at 2030-06-15T09:00:00").unwrap();
        assert!(matches!(
            p,
            ParsedSchedule::Once {
                at: OnceTime::WallClock(_)
            }
        ));
        assert_eq!(p.canonical(), "at 2030-06-15T09:00:00");
        // Asia/Makassar = UTC+8 tanpa DST: 09:00 lokal == 01:00Z.
        let next = p
            .next_run_after(chrono_tz::Asia::Makassar, utc(2030, 6, 14, 0, 0, 0), None)
            .unwrap();
        assert_eq!(next, utc(2030, 6, 15, 1, 0, 0));
    }

    #[test]
    fn once_past_times_rejected_at_parse() {
        let err = parse_schedule("at 2000-01-01T00:00:00Z")
            .unwrap_err()
            .to_string();
        assert!(err.contains("sudah lewat"), "pesan: {err}");
        assert!(parse_schedule("at 2000-01-01 00:00:00").is_err());
    }

    #[test]
    fn once_rejects_garbage() {
        for bad in [
            "at",
            "at besok pagi",
            "at 2026-13-45T99:99:99",
            "at noonish",
        ] {
            assert!(parse_schedule(bad).is_err(), "`{bad}` harus error");
        }
    }

    #[test]
    fn keywords_map_to_cron_equivalents() {
        let cases = [
            ("hourly", "0 0 * * * *"),
            ("daily", "0 0 0 * * *"),
            ("midnight", "0 0 0 * * *"),
            ("weekly", "0 0 0 * * 1"),
            ("noon", "0 0 12 * * *"),
            ("DAILY", "0 0 0 * * *"),
            ("  Hourly  ", "0 0 * * * *"),
        ];
        for (kw, expr) in cases {
            assert_eq!(
                parse_schedule(kw).unwrap(),
                ParsedSchedule::Cron {
                    expr: expr.to_string()
                },
                "{kw}"
            );
        }
    }

    #[test]
    fn daily_next_run_from_fixed_instant() {
        let p = parse_schedule("daily").unwrap();
        // Dari 2026-10-21T00:00:01Z → tengah malam berikutnya.
        let next = p
            .next_run_after(chrono_tz::UTC, utc(2026, 10, 21, 0, 0, 1), None)
            .unwrap();
        assert_eq!(next, utc(2026, 10, 22, 0, 0, 0));
    }

    #[test]
    fn parse_is_case_insensitive_for_prefixes() {
        assert!(parse_schedule("Every 15m").is_ok());
        assert!(parse_schedule("AT 2030-06-15T01:00:00Z").is_ok());
    }
}
