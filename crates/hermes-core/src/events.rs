use crate::Usage;

/// Statistik diff per-tool call: jumlah baris yang ditambah (`+`) dan
/// dihapus (`-`), dihitung dari unified diff yang dibawa konten hasil tool
/// (mis. `edit_file`). `Copy` agar murah diteruskan lewat `AgentEvent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DiffStat {
    pub added: u64,
    pub removed: u64,
}

/// Hitung [`DiffStat`] dari teks hasil tool dengan memindai baris-baris
/// unified diff (`---`/`+++` + hunks `@@`).
///
/// Hanya baris di dalam hunk (`@@` …) yang dihitung: baris berawalan `+`
/// dihitung sebagai penambahan, berawalan `-` sebagai penghapusan.
/// Mengembalikan `None` bila tidak ada baris diff yang ditemukan (hasil
/// tool tidak memuat diff — mis. `patch`, `write_file`, `exec`).
pub fn diff_stat_from_text(content: &str) -> Option<DiffStat> {
    let mut added = 0u64;
    let mut removed = 0u64;
    let mut in_hunk = false;
    for line in content.lines() {
        if line.starts_with("@@") {
            in_hunk = true;
            continue;
        }
        if !in_hunk {
            continue;
        }
        match line.as_bytes().first() {
            Some(b'+') => added += 1,
            Some(b'-') => removed += 1,
            _ => {}
        }
    }
    if added == 0 && removed == 0 {
        None
    } else {
        Some(DiffStat { added, removed })
    }
}

/// Event yang dipancarkan agent loop ke UI (CLI, TUI, gateway, ...).
#[derive(Debug, Clone)]
pub enum AgentEvent {
    SessionStart {
        session_id: String,
    },
    /// Potongan teks balasan model (streaming).
    TextDelta(String),
    IterationStart {
        n: u32,
    },
    ToolCallStart {
        name: String,
        arguments: serde_json::Value,
    },
    ToolCallResult {
        name: String,
        ok: bool,
        preview: String,
        /// Statistik diff opsional: dihitung dari unified diff yang dibawa
        /// hasil tool (mis. edit_file). None bila hasil tidak memuat diff
        /// (mis. patch, write_file) — TUI tidak menampilkan apa-apa.
        diff_stat: Option<DiffStat>,
    },
    /// Konteks dikompresi karena melewati ambang.
    Compaction {
        before_tokens: usize,
        after_tokens: usize,
    },
    /// Skill baru dibuat oleh learning loop.
    SkillCreated {
        name: String,
    },
    /// Memori baru disimpan.
    MemorySaved {
        kind: String,
    },
    Warning(String),
    Done {
        reply: String,
        usage: Usage,
    },
    Error(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_diff() -> String {
        [
            "diganti di /tmp/a.txt (kecocokan persis)",
            "--- a.txt",
            "+++ b.txt",
            "@@ -1,3 +1,4 @@",
            " baris1",
            "-hello world",
            "-hapus ini",
            "+halo dunia",
            "+baris baru",
            " baris3",
            "",
        ]
        .join("\n")
    }

    #[test]
    fn diff_stat_counts_added_and_removed_in_hunks() {
        assert_eq!(
            diff_stat_from_text(&sample_diff()),
            Some(DiffStat {
                added: 2,
                removed: 2
            })
        );
    }

    #[test]
    fn diff_stat_ignores_plus_minus_outside_hunks() {
        // Baris +/- di luar hunk @@ bukan bagian diff.
        let text = "+bukan diff\n-beberapa teks\n--- a.txt\n+++ b.txt\n";
        assert_eq!(diff_stat_from_text(text), None);
    }

    #[test]
    fn diff_stat_none_when_no_diff() {
        assert_eq!(
            diff_stat_from_text("patch: diganti di /tmp/a.txt (kecocokan persis)"),
            None
        );
        assert_eq!(diff_stat_from_text(""), None);
        assert_eq!(diff_stat_from_text("ditulis /tmp/a.txt (42 byte)"), None);
    }

    #[test]
    fn diff_stat_multiple_hunks_accumulate() {
        let text = "@@ -1 +1 @@\n-a\n+b\n@@ -9 +9 @@\n-c\n+d\n+e\n";
        assert_eq!(
            diff_stat_from_text(text),
            Some(DiffStat {
                added: 3,
                removed: 2
            })
        );
    }
}
