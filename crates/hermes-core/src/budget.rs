use crate::{HermesError, Result};

/// Batas iterasi agent loop agar tidak berjalan selamanya.
///
/// Paritas `agent/iteration_budget.py` di original (default parent 500).
///
/// Kontrak refund — pass yang MENICK budget (`tick`) tetapi ternyata tidak
/// melakukan kerja iterasi yang sesungguhnya HARUS mengembalikan tick-nya via
/// [`IterationBudget::refund`]:
///
/// - **compaction-only**: pass yang hanya menjalankan kompresi konteks
///   (pre-API compaction) lalu restart sebelum panggilan provider utama;
/// - **execute_code-only**: pass yang tool call-nya hanya `execute_code`
///   (programmatic tool calling, panggilan murah ala RPC —
///   `agent/turn_tool_round.py`);
/// - **preflight-only**: pass yang keluar SEBELUM panggilan provider utama
///   diterbitkan DAN tidak mengeksekusi tool apa pun (tidak ada LLM call
///   utama, tidak ada tool side effect). Contoh konkret di driver saat ini:
///   pass restart kompresi pra-provider. Setiap gate preflight di masa depan
///   yang menggugurkan pass antara `tick` dan panggilan provider wajib
///   me-refund.
///
/// Grace call — paritas `_budget_grace_call` (`agent/turn_iteration_prep.py`,
/// `agent/turn_truncation.py`): saat budget habis di tengah turn, model
/// mendapat SATU panggilan pamungkas tanpa tool agar turn yang sedang
/// berjalan selesai dengan rapi; turn berikutnya tetap diblokir.
#[derive(Debug, Clone)]
pub struct IterationBudget {
    max: u32,
    used: u32,
    /// Grace satu-kali sudah dikonsumsi? Tidak pernah di-reset: satu budget
    /// hanya memberi satu grace call, berapapun banyaknya turn.
    grace_used: bool,
}

impl IterationBudget {
    pub fn new(max: u32) -> Self {
        Self {
            max,
            used: 0,
            grace_used: false,
        }
    }

    /// Konsumsi satu iterasi; error bila habis. Tidak pernah memberi grace
    /// otomatis — pemanggil yang butuh semantik pamungkas memakai
    /// [`IterationBudget::tick_or_grace`].
    pub fn tick(&mut self) -> Result<u32> {
        if self.used >= self.max {
            return Err(HermesError::BudgetExhausted(self.max));
        }
        self.used += 1;
        Ok(self.used)
    }

    /// Konsumsi satu iterasi, atau — bila budget sudah habis — berikan SATU
    /// grace call pamungkas. Mengembalikan `(used, is_grace)`:
    ///
    /// - budget tersedia → seperti `tick()`: `used` + 1, `is_grace = false`;
    /// - budget habis dan grace belum dipakai → `Ok((used, true))` TANPA
    ///   menambah `used` (grace tidak memperpanjang hard budget; flag grace
    ///   dikonsumsi sehingga tick berikutnya gagal lagi);
    /// - budget habis dan grace sudah dipakai → `Err(BudgetExhausted)`.
    ///
    /// Driver memakai ini di tiap awal pass: pass grace WAJIB tanpa tool
    /// (toolless, seperti original) dan WAJIB menjadi pass terakhir turn.
    pub fn tick_or_grace(&mut self) -> Result<(u32, bool)> {
        if self.used < self.max {
            self.used += 1;
            return Ok((self.used, false));
        }
        if !self.grace_used {
            self.grace_used = true;
            return Ok((self.used, true));
        }
        Err(HermesError::BudgetExhausted(self.max))
    }

    /// Kembalikan satu iterasi ke budget. Paritas original
    /// (`agent/iteration_budget.py::IterationBudget.refund`): iterasi yang tidak
    /// seharusnya memakan budget — misalnya turn yang HANYA memanggil
    /// `execute_code` (programmatic tool calling, panggilan murah ala RPC),
    /// pass yang HANYA menjalankan kompresi pra-provider, atau pass
    /// preflight-only (tak ada LLM call utama, tak ada tool side effect) —
    /// di-refund. Tidak boleh underflow: refund saat used == 0 adalah no-op.
    pub fn refund(&mut self) {
        if self.used > 0 {
            self.used -= 1;
        }
    }

    pub fn used(&self) -> u32 {
        self.used
    }

    pub fn remaining(&self) -> u32 {
        self.max.saturating_sub(self.used)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_exhausts() {
        let mut b = IterationBudget::new(2);
        assert!(b.tick().is_ok());
        assert!(b.tick().is_ok());
        assert!(matches!(b.tick(), Err(HermesError::BudgetExhausted(2))));
    }

    #[test]
    fn refund_gives_back_one_iteration() {
        // Paritas original: tick -> refund -> tick lagi harus berhasil,
        // karena refund mengembalikan iterasi yang baru dikonsumsi.
        let mut b = IterationBudget::new(2);
        assert_eq!(b.tick().unwrap(), 1);
        b.refund();
        assert_eq!(b.used(), 0);
        assert_eq!(b.remaining(), 2);
        assert_eq!(b.tick().unwrap(), 1);
        assert_eq!(b.used(), 1);
    }

    #[test]
    fn refund_at_zero_is_noop() {
        // Seperti original: refund tidak boleh underflow.
        let mut b = IterationBudget::new(3);
        b.refund();
        assert_eq!(b.used(), 0);
        assert_eq!(b.remaining(), 3);
        assert_eq!(b.tick().unwrap(), 1);
    }

    #[test]
    fn refund_after_exhaust_allows_tick_again() {
        let mut b = IterationBudget::new(1);
        assert!(b.tick().is_ok());
        assert!(matches!(b.tick(), Err(HermesError::BudgetExhausted(1))));
        b.refund();
        assert!(b.tick().is_ok());
    }

    #[test]
    fn normal_turn_decrements_by_one() {
        // (e) Pass normal: satu tick = satu iterasi terpakai.
        let mut b = IterationBudget::new(500);
        let (n, grace) = b.tick_or_grace().unwrap();
        assert_eq!((n, grace), (1, false));
        assert_eq!(b.used(), 1);
        assert_eq!(b.remaining(), 499);
    }

    #[test]
    fn compaction_only_pass_does_not_decrement() {
        // (b) Pass yang HANYA menjalankan kompresi pra-provider di-refund:
        // tick -> refund -> used kembali 0.
        let mut b = IterationBudget::new(500);
        b.tick().unwrap();
        b.refund(); // driver me-refund pass compaction-only
        assert_eq!(b.used(), 0);
        assert_eq!(b.remaining(), 500);
    }

    #[test]
    fn execute_code_only_pass_does_not_decrement() {
        // (c) Pass yang tool call-nya hanya execute_code di-refund
        // (paritas agent/turn_tool_round.py).
        let mut b = IterationBudget::new(500);
        b.tick().unwrap();
        b.refund(); // driver me-refund pass execute_code-only
        assert_eq!(b.used(), 0);
        assert_eq!(b.remaining(), 500);
    }

    #[test]
    fn preflight_only_pass_does_not_decrement() {
        // (d) Pass preflight-only (tak ada LLM call utama, tak ada tool side
        // effect — mis. pass restart kompresi pra-provider) di-refund.
        let mut b = IterationBudget::new(2);
        b.tick().unwrap();
        b.tick().unwrap();
        b.refund(); // preflight-only: kembalikan tick terakhir
        assert_eq!(b.used(), 1);
        assert_eq!(b.remaining(), 1);
    }

    #[test]
    fn tick_stays_strict_without_grace() {
        // tick() biasa tidak pernah memberi grace otomatis.
        let mut b = IterationBudget::new(1);
        assert!(b.tick().is_ok());
        assert!(matches!(b.tick(), Err(HermesError::BudgetExhausted(1))));
        assert!(matches!(b.tick(), Err(HermesError::BudgetExhausted(1))));
    }

    #[test]
    fn grace_lets_inflight_turn_finish_then_blocks_next() {
        // (f) Budget habis di tengah turn: pass yang sedang berjalan dapat
        // SATU grace call pamungkas (tanpa menambah used), lalu pass
        // berikutnya diblokir.
        let mut b = IterationBudget::new(1);
        let (n, grace) = b.tick_or_grace().unwrap();
        assert_eq!((n, grace), (1, false));
        // Budget kini 0 di tengah turn — grace: turn boleh selesai.
        let (n, grace) = b.tick_or_grace().unwrap();
        assert_eq!((n, grace), (1, true));
        assert_eq!(b.used(), 1, "grace tidak menambah used");
        // Turn/pass berikutnya diblokir: grace hanya satu kali.
        assert!(matches!(
            b.tick_or_grace(),
            Err(HermesError::BudgetExhausted(1))
        ));
        assert!(matches!(b.tick(), Err(HermesError::BudgetExhausted(1))));
    }

    #[test]
    fn grace_not_rearmed_by_refund() {
        // Refund mengembalikan used, tetapi TIDAK menghidupkan kembali grace
        // yang sudah dikonsumsi: satu budget = satu grace call. Setelah
        // refund, tick berikutnya adalah tick normal; saat budget habis lagi,
        // tidak ada grace kedua.
        let mut b = IterationBudget::new(1);
        b.tick_or_grace().unwrap(); // (1, false)
        b.tick_or_grace().unwrap(); // (1, true): grace dikonsumsi
        b.refund(); // used kembali 0
        let (n, grace) = b.tick_or_grace().unwrap();
        assert_eq!(
            (n, grace),
            (1, false),
            "harus tick normal, bukan grace kedua"
        );
        assert!(matches!(
            b.tick_or_grace(),
            Err(HermesError::BudgetExhausted(1))
        ));
    }
}
