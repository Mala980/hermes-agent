use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

/// Bendera pembatalan yang bisa dibagikan antar task (Ctrl-C / interrupt).
#[derive(Debug, Clone, Default)]
pub struct CancelFlag {
    inner: Arc<AtomicBool>,
}

impl CancelFlag {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.inner.store(true, Ordering::SeqCst);
    }

    /// Kembalikan ke keadaan belum-dibatalkan. Dipakai di awal tiap turn
    /// baru (paritas `cancel_event.clear()` di `acp_adapter/server.py`):
    /// pembatalan turn sebelumnya tidak boleh membekukan sesi selamanya.
    pub fn reset(&self) {
        self.inner.store(false, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.load(Ordering::SeqCst)
    }

    pub fn check(&self) -> crate::Result<()> {
        if self.is_cancelled() {
            Err(crate::HermesError::Interrupted)
        } else {
            Ok(())
        }
    }
}
