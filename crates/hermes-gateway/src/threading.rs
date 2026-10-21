//! Mode reply-threading per platform.
//!
//! Paritas dengan `_should_thread_reply()` / `reply_to_mode` di adapter
//! aslinya (Telegram, Discord): mode `"off"` menonaktifkan anchor balasan,
//! `"first"` (default) menempelkan anchor hanya pada chunk pertama, `"all"`
//! menempelkannya pada setiap chunk. Nilai dibaca dari env
//! `<PLATFORM>_REPLY_TO_MODE` (port ini memakai env sebagai pengganti
//! `config.reply_to_mode` aslinya).

/// Mode penempelan anchor balasan pada pesan multi-chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyThreadMode {
    /// Tanpa anchor balasan pada chunk mana pun.
    Off,
    /// Anchor hanya pada chunk pertama (default aslinya).
    First,
    /// Anchor pada setiap chunk.
    All,
}

impl ReplyThreadMode {
    /// Baca mode dari env `var`: "off" / "first" / "all" (case-insensitive);
    /// nilai lain atau kosong → [`ReplyThreadMode::First`].
    pub fn from_env(var: &str) -> Self {
        match std::env::var(var)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "off" => ReplyThreadMode::Off,
            "all" => ReplyThreadMode::All,
            _ => ReplyThreadMode::First,
        }
    }

    /// Apakah chunk ke-`chunk_index` (0 = pertama) harus membawa anchor
    /// balasan ke `reply_to`, mengikuti mode — cermin
    /// `_should_thread_reply()` aslinya.
    pub fn applies(self, has_reply_target: bool, chunk_index: usize) -> bool {
        if !has_reply_target {
            return false;
        }
        match self {
            ReplyThreadMode::Off => false,
            ReplyThreadMode::All => true,
            ReplyThreadMode::First => chunk_index == 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_first_tanpa_env() {
        let _g = crate::allowlist::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("HERMES_TEST_RTM_X");
        assert_eq!(
            ReplyThreadMode::from_env("HERMES_TEST_RTM_X"),
            ReplyThreadMode::First
        );
    }

    #[test]
    fn parse_mode_dari_env() {
        let _g = crate::allowlist::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for (raw, want) in [
            ("off", ReplyThreadMode::Off),
            ("OFF", ReplyThreadMode::Off),
            ("first", ReplyThreadMode::First),
            ("all", ReplyThreadMode::All),
            ("ALL", ReplyThreadMode::All),
            ("ngawur", ReplyThreadMode::First),
            ("", ReplyThreadMode::First),
        ] {
            std::env::set_var("HERMES_TEST_RTM_X", raw);
            assert_eq!(
                ReplyThreadMode::from_env("HERMES_TEST_RTM_X"),
                want,
                "raw={raw:?}"
            );
        }
        std::env::remove_var("HERMES_TEST_RTM_X");
    }

    #[test]
    fn applies_sesuai_mode() {
        // first (default): hanya chunk 0.
        assert!(ReplyThreadMode::First.applies(true, 0));
        assert!(!ReplyThreadMode::First.applies(true, 1));
        assert!(!ReplyThreadMode::First.applies(true, 5));
        // all: semua chunk.
        assert!(ReplyThreadMode::All.applies(true, 0));
        assert!(ReplyThreadMode::All.applies(true, 3));
        // off: tidak ada.
        assert!(!ReplyThreadMode::Off.applies(true, 0));
        // tanpa target balasan: tidak ada anchor apa pun modenya.
        assert!(!ReplyThreadMode::First.applies(false, 0));
        assert!(!ReplyThreadMode::All.applies(false, 0));
    }
}
