//! Lock tick lintas-proses untuk scheduler cron.
//!
//! Dua proses scheduler (mis. setelah crash + watchdog me-relaunch) bisa
//! menembakkan job yang sama dua kali. Sebelum memproses satu tick,
//! scheduler mencoba mengakuisisi lock eksklusif ini; bila gagal, tick
//! dilewati dengan warn log (bukan error).
//!
//! Mekanisme: file lock di direktori data cron ([`TICK_LOCK_FILE`]), dibuat
//! atomik via `create_new` (tepat satu proses yang berhasil). Isi file adalah
//! PID pemegang lock; bila file sudah ada, PID dibaca dan kelangsungan
//! hidupnya dicek via `kill(pid, 0)`. Lock dari proses yang sudah mati
//! dianggap basi (stale) lalu direklamasi.
//!
//! Keterbatasan yang didokumentasikan:
//! - Atomik antar-proses hanya dijamin pada filesystem lokal; di NFS atau
//!   filesystem jaringan lain, semantik `create_new` bisa tidak atomik.
//! - PID bisa dipakai ulang oleh OS: lock basi yang PID-nya kebetulan dipakai
//!   proses lain yang masih hidup akan dianggap valid sampai proses itu mati.
//!   Untuk skenario watchdog-relaunch (proses lama benar-benar mati) ini aman.
//! - Di platform non-Unix tidak ada `kill(2)`; lock yang ada selalu dianggap
//!   milik proses hidup (fail-closed) agar tidak terjadi double-fire.

use hermes_core::{HermesError, Result};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Nama file lock di direktori data cron.
pub const TICK_LOCK_FILE: &str = "cron.tick.lock";

/// Batas percobaan reklamasi lock basi sebelum menyerah (dianggap contended).
const MAX_RECLAIM_ATTEMPTS: u32 = 3;

/// Guard lock tick lintas-proses. File lock dihapus saat guard di-drop;
/// bila proses crash, file tertinggal dan direklamasi oleh akuisisi
/// berikutnya via cek PID.
pub struct TickLock {
    path: PathBuf,
}

impl TickLock {
    /// Coba akuisisi lock di `dir`.
    ///
    /// - `Ok(Some(lock))`: lock diperoleh; tahan guard selama tick berjalan.
    /// - `Ok(None)`: lock dipegang proses lain yang masih hidup → panggil
    ///   sebaiknya melewati tick dengan warn log (bukan error).
    /// - `Err`: kegagalan I/O di luar konflik (mis. permission) — perlakukan
    ///   seperti lock tak diperoleh: warn + lewati tick.
    pub fn acquire(dir: &Path) -> Result<Option<Self>> {
        std::fs::create_dir_all(dir)
            .map_err(|e| HermesError::Cron(format!("gagal membuat dir data cron: {e}")))?;
        let path = dir.join(TICK_LOCK_FILE);
        for _ in 0..MAX_RECLAIM_ATTEMPTS {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut f) => {
                    // Tulis PID agar proses lain bisa mendeteksi lock basi.
                    let _ = writeln!(f, "{}", std::process::id());
                    let _ = f.sync_all();
                    return Ok(Some(Self { path }));
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    match read_lock_pid(&path) {
                        Some(pid) if pid_alive(pid) => return Ok(None),
                        // Lock basi (PID mati / isi tak terbaca): hapus lalu
                        // coba lagi. Balapan dua proses yang sama-sama
                        // mereklamasi diselesaikan oleh create_new yang atomik:
                        // tepat satu pemenang; yang kalah melihat PID pemenang
                        // masih hidup lalu mengembalikan None.
                        _ => {
                            let _ = std::fs::remove_file(&path);
                        }
                    }
                }
                Err(e) => {
                    return Err(HermesError::Cron(format!(
                        "gagal membuat lockfile tick cron: {e}"
                    )))
                }
            }
        }
        // Tak bisa mereklamasi setelah beberapa percobaan: anggap contended.
        Ok(None)
    }
}

impl Drop for TickLock {
    fn drop(&mut self) {
        // Best-effort: bila proses crash, file tertinggal dan direklamasi
        // oleh akuisisi berikutnya via cek PID.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Baca PID dari isi lockfile; `None` bila tak terbaca / bukan angka valid.
fn read_lock_pid(path: &Path) -> Option<u32> {
    let content = std::fs::read_to_string(path).ok()?;
    let pid: u32 = content.trim().parse().ok()?;
    if pid == 0 {
        None
    } else {
        Some(pid)
    }
}

/// True bila proses `pid` masih hidup.
#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    // SAFETY: kill dengan sinyal 0 tidak mengirim apa-apa; hanya cek error.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        return true;
    }
    // EPERM = proses ada tapi tak boleh di-signal; ESRCH = tidak ada.
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Tanpa kill(2) (non-Unix): konservatif, anggap lock selalu valid
/// (fail-closed) agar tak ada double-fire.
#[cfg(not(unix))]
fn pid_alive(_pid: u32) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("hermes-cron-lock-test")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn second_acquire_skips_while_held() {
        let dir = test_dir("contended");
        let first = TickLock::acquire(&dir).unwrap();
        assert!(first.is_some(), "akuisisi pertama harus berhasil");
        assert_eq!(
            std::fs::read_to_string(dir.join(TICK_LOCK_FILE))
                .unwrap()
                .trim()
                .parse::<u32>()
                .unwrap(),
            std::process::id(),
            "lockfile harus berisi PID sendiri"
        );
        // Percobaan kedua selagi lock dipegang → None (lewati tick).
        let second = TickLock::acquire(&dir).unwrap();
        assert!(second.is_none(), "akuisisi kedua harus ditolak");
        drop(first);
        // Setelah dilepas, akuisisi berhasil lagi dan file dibersihkan.
        assert!(
            TickLock::acquire(&dir).unwrap().is_some(),
            "lock harus bisa diperoleh lagi setelah dilepas"
        );
    }

    #[test]
    fn stale_lock_reclaimed() {
        let dir = test_dir("stale");
        // Lock basi: PID dari proses yang sudah mati (child langsung di-wait).
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert!(
            !pid_alive(pid),
            "pre-kondisi: PID child yang sudah di-wait harus mati"
        );
        std::fs::write(dir.join(TICK_LOCK_FILE), format!("{pid}\n")).unwrap();
        let lock = TickLock::acquire(&dir).unwrap();
        assert!(lock.is_some(), "lock basi harus direklamasi");
    }

    #[test]
    fn unreadable_lockfile_treated_as_stale() {
        let dir = test_dir("unreadable");
        std::fs::write(dir.join(TICK_LOCK_FILE), "bukan-pid\n").unwrap();
        let lock = TickLock::acquire(&dir).unwrap();
        assert!(
            lock.is_some(),
            "lockfile tak terbaca harus diperlakukan sebagai basi"
        );
    }
}
