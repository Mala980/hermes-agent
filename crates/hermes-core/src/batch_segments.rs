//! Perencana segmen batch tool call (B4, W1-B4): port dari
//! `_plan_tool_batch_segments` (`agent/tool_dispatch_helpers.py`) di original.
//!
//! Batch dipecah menjadi segmen berurutan `(kind, calls)` sesuai urutan emisi:
//! reader berturut-turut dikumpulkan menjadi grup paralel (`join_all`);
//! sebuah writer menutup grup reader yang tertunda lalu dieksekusi sendirian;
//! read atas path yang baru ditulis di batch yang sama menunggu lewat barrier
//! path-overlap. Urutan hasil per-call dan batas side-effect sama persis
//! dengan eksekusi sekuensial penuh (original: "result order and side-effect
//! boundaries match fully-sequential execution").
//!
//! Klasifikasi read/write berasal dari anotasi [`ToolEffect`] di trait
//! [`Tool`](crate::Tool) — bukan tabel nama — sehingga tool baru/plug-in
//! ikut aturan ini otomatis. Tool tak dikenal/nonaktif diperlakukan sebagai
//! writer (barrier konservatif, seperti original).

use crate::{ToolCall, ToolEffect, ToolRegistry};
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

/// Jenis segmen dalam rencana batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentKind {
    /// Call dalam segmen boleh dieksekusi konkuren (hasil tetap sesuai urutan emisi).
    Parallel,
    /// Call dalam segmen dieksekusi satu per satu, berurutan.
    Sequential,
}

/// Tool filesystem dengan cakupan path (argumen `path`; grep/glob default ke
/// working dir): reader↔reader boleh berbagi subtree, tumpang-tindih apa pun
/// yang melibatkan writer menutup run sehingga call yang berkonflik menunggu
/// di segmen berikutnya. (original: `_PATH_SCOPED_READERS` /
/// `_PATH_SCOPED_WRITERS`; `edit_file` adalah writer khas port)
const PATH_SCOPED_TOOLS: &[&str] = &[
    "read_file",
    "glob",
    "grep",
    "write_file",
    "edit_file",
    "patch",
];

/// Pecah batch tool call menjadi segmen berurutan `(kind, calls)` sesuai urutan
/// emisi — tiruan `_plan_tool_batch_segments`.
///
/// Aturan (sama seperti original, klasifikasi via [`ToolEffect`]):
/// - Writer non-path-scoped, argumen non-objek, dan tool path-scoped dengan
///   cakupan tak diketahui adalah barrier sekuensial.
/// - Tool path-scoped bergabung ke run paralel hanya bila tidak konflik dengan
///   reservasi run: reader↔reader boleh tumpang-tindih; tumpang-tindih apa pun
///   yang melibatkan writer menutup run sehingga call memulai run BARU setelah
///   call yang berkonflik selesai.
/// - Run lebih pendek dari dua call diturunkan ke sekuensial; segmen sekuensial
///   yang bersebelahan digabung.
pub fn plan_tool_batch_segments(
    tool_calls: &[ToolCall],
    cwd: &Path,
    registry: &ToolRegistry,
) -> Vec<(SegmentKind, Vec<ToolCall>)> {
    let mut segments: Vec<(SegmentKind, Vec<ToolCall>)> = Vec::new();
    let mut current: Vec<ToolCall> = Vec::new();
    let mut reserved: Vec<(PathBuf, bool)> = Vec::new(); // (path kanonis, is_writer)

    for call in tool_calls {
        match batch_admission(call, cwd, registry) {
            None => {
                close_run(&mut segments, &mut current);
                reserved.clear();
                push_sequential(&mut segments, [call.clone()]);
            }
            Some((scoped, is_writer)) => {
                let conflict = scoped.iter().any(|path| {
                    reserved.iter().any(|(existing, existing_is_writer)| {
                        (is_writer || *existing_is_writer) && paths_overlap(path, existing)
                    })
                });
                if conflict {
                    close_run(&mut segments, &mut current);
                    reserved.clear();
                }
                reserved.extend(scoped.into_iter().map(|p| (p, is_writer)));
                current.push(call.clone());
            }
        }
    }
    close_run(&mut segments, &mut current);
    segments
}

/// Tutup run paralel yang sedang berjalan: run ≥ 2 call menjadi segmen
/// paralel, run 1 call diturunkan ke sekuensial (original: `_close_parallel`).
fn close_run(segments: &mut Vec<(SegmentKind, Vec<ToolCall>)>, current: &mut Vec<ToolCall>) {
    if current.len() >= 2 {
        segments.push((SegmentKind::Parallel, std::mem::take(current)));
    } else if !current.is_empty() {
        push_sequential(segments, std::mem::take(current));
    }
}

/// Tambah call ke segmen sekuensial; gabung dengan segmen sekuensial yang
/// bersebelahan bila ada (original: `_extend_sequential`).
fn push_sequential(
    segments: &mut Vec<(SegmentKind, Vec<ToolCall>)>,
    calls: impl IntoIterator<Item = ToolCall>,
) {
    let mut calls = calls.into_iter().peekable();
    if calls.peek().is_none() {
        return;
    }
    if let Some((SegmentKind::Sequential, tail)) = segments.last_mut() {
        tail.extend(calls);
    } else {
        segments.push((SegmentKind::Sequential, calls.collect()));
    }
}

/// Klasifikasikan satu call untuk perencana: `None` = barrier sekuensial,
/// selain itu `(scoped_paths, is_writer)` (paths kosong = reader aman-paralel
/// tanpa cakupan). Klasifikasi read/write berasal dari
/// [`ToolRegistry::effect_of`] — anotasi [`ToolEffect`] di trait tool.
/// (original: `_batch_admission`)
fn batch_admission(
    call: &ToolCall,
    cwd: &Path,
    registry: &ToolRegistry,
) -> Option<(Vec<PathBuf>, bool)> {
    let name = call.name.as_str();
    let effect = registry.effect_of(name);
    let args = call.arguments.as_object()?; // argumen non-objek → barrier
    if PATH_SCOPED_TOOLS.contains(&name) {
        let scoped = extract_scope_paths(name, args, cwd);
        if scoped.is_empty() {
            return None; // cakupan tak diketahui → barrier
        }
        // is_writer dari anotasi effect() tool, bukan tabel nama.
        return Some((scoped, effect == ToolEffect::Write));
    }
    match effect {
        // Reader murni: gabung run paralel tanpa reservasi path.
        ToolEffect::Read => Some((Vec::new(), false)),
        // Writer non-path-scoped (exec, browser_*, memory_remember, ...):
        // cakupan mutasinya tak bisa dilacak → barrier sekuensial.
        ToolEffect::Write => None,
    }
}

/// Setiap path kanonis yang dicadangkan call ini untuk cek tumpang-tindih.
/// (original: `_extract_parallel_scope_paths`)
fn extract_scope_paths(
    name: &str,
    args: &serde_json::Map<String, serde_json::Value>,
    cwd: &Path,
) -> Vec<PathBuf> {
    let raw: Vec<&str> = match args.get("path").and_then(|v| v.as_str()) {
        Some(p) if !p.trim().is_empty() => vec![p],
        // grep/glob default ke working dir — cadangkan cwd seperti
        // `search_files` di original; tool path-scoped lain tanpa path
        // berarti cakupan tak diketahui.
        _ if name == "glob" || name == "grep" => vec!["."],
        _ => Vec::new(),
    };
    // Dedup dengan urutan kemunculan pertama dipertahankan (original: dict.fromkeys).
    let mut seen = HashSet::new();
    raw.into_iter()
        .map(|p| normalize_path(p, cwd))
        .filter(|p| seen.insert(p.clone()))
        .collect()
}

/// Kanonikalisasi leksikal untuk deteksi tumpang-tindih: ekspansi `~`, jadikan
/// absolut terhadap `cwd`, bersihkan komponen `.`/`..`.
///
/// Original memakai realpath+normcase (butuh I/O dan gagal untuk target writer
/// yang belum ada); versi leksikal ini deterministik dan cukup untuk semantik
/// barrier.
fn normalize_path(raw: &str, cwd: &Path) -> PathBuf {
    let expanded = if let Some(rest) = raw.strip_prefix("~/") {
        match dirs::home_dir() {
            Some(home) => home.join(rest),
            None => PathBuf::from(raw),
        }
    } else {
        PathBuf::from(raw)
    };
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    };
    let mut out = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            _ => out.push(component.as_os_str()),
        }
    }
    out
}

/// True bila dua path kanonis merujuk subtree yang sama (salah satunya prefiks
/// komponen dari yang lain); path kosong tumpang-tindih dengan apa pun.
/// (original: `_paths_overlap`)
fn paths_overlap(left: &Path, right: &Path) -> bool {
    let left_parts: Vec<_> = left.components().collect();
    let right_parts: Vec<_> = right.components().collect();
    if left_parts.is_empty() || right_parts.is_empty() {
        return false;
    }
    let common = left_parts.len().min(right_parts.len());
    left_parts[..common] == right_parts[..common]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Result, Tool, ToolContext, ToolResult};
    use async_trait::async_trait;
    use serde_json::json;

    /// Tool mock dengan effect yang bisa diatur — uji perencana lewat anotasi
    /// [`ToolEffect`] seperti tool asli.
    struct FxTool {
        name: &'static str,
        effect: ToolEffect,
    }

    #[async_trait]
    impl Tool for FxTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "mock untuk uji perencana segmen"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn effect(&self) -> ToolEffect {
            self.effect
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolContext,
        ) -> Result<ToolResult> {
            unimplemented!()
        }
    }

    /// Registry uji: klasifikasi effect mencerminkan tool bawaan
    /// (`register_builtin`).
    fn registry() -> ToolRegistry {
        let mut reg = ToolRegistry::new();
        for (name, effect) in [
            ("web_search", ToolEffect::Read),
            ("web_fetch", ToolEffect::Read),
            ("read_file", ToolEffect::Read),
            ("glob", ToolEffect::Read),
            ("grep", ToolEffect::Read),
            ("write_file", ToolEffect::Write),
            ("edit_file", ToolEffect::Write),
            ("patch", ToolEffect::Write),
            ("exec", ToolEffect::Write),
            ("clarify", ToolEffect::Write),
        ] {
            reg.register(FxTool { name, effect });
        }
        reg
    }

    fn plan(calls: &[ToolCall]) -> Vec<(SegmentKind, Vec<ToolCall>)> {
        plan_tool_batch_segments(calls, &cwd(), &registry())
    }

    fn call(id: &str, name: &str, arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments,
        }
    }

    fn kinds(segments: &[(SegmentKind, Vec<ToolCall>)]) -> Vec<(SegmentKind, Vec<String>)> {
        segments
            .iter()
            .map(|(k, calls)| (*k, calls.iter().map(|c| c.id.clone()).collect()))
            .collect()
    }

    fn cwd() -> PathBuf {
        PathBuf::from("/kerja")
    }

    #[test]
    fn read_berbeda_path_jadi_satu_segmen_paralel() {
        let calls = vec![
            call("a", "read_file", json!({"path": "satu.txt"})),
            call("b", "read_file", json!({"path": "dua.txt"})),
        ];
        assert_eq!(
            kinds(&plan(&calls)),
            vec![(
                SegmentKind::Parallel,
                vec!["a".to_string(), "b".to_string()]
            )]
        );
    }

    #[test]
    fn reader_bersama_subtree_tetap_paralel() {
        let calls = vec![
            call("a", "read_file", json!({"path": "src"})),
            call("b", "grep", json!({"pattern": "x", "path": "src/lib.rs"})),
        ];
        assert_eq!(
            kinds(&plan(&calls)),
            vec![(
                SegmentKind::Parallel,
                vec!["a".to_string(), "b".to_string()]
            )]
        );
    }

    #[test]
    fn writer_menutup_run_sebelumnya() {
        // read(a) ∥ write(a): writer konflik dengan reader → barrier.
        // write(a) + read(b) tak tumpang-tindih → run paralel baru.
        let calls = vec![
            call("a", "read_file", json!({"path": "a.txt"})),
            call("b", "write_file", json!({"path": "a.txt", "content": "x"})),
            call("c", "read_file", json!({"path": "b.txt"})),
        ];
        assert_eq!(
            kinds(&plan(&calls)),
            vec![
                (SegmentKind::Sequential, vec!["a".to_string()]),
                (
                    SegmentKind::Parallel,
                    vec!["b".to_string(), "c".to_string()]
                ),
            ]
        );
    }

    #[test]
    fn writer_tanpa_overlap_tetap_paralel() {
        let calls = vec![
            call("a", "write_file", json!({"path": "a.txt", "content": "x"})),
            call("b", "read_file", json!({"path": "b.txt"})),
        ];
        assert_eq!(
            kinds(&plan(&calls)),
            vec![(
                SegmentKind::Parallel,
                vec!["a".to_string(), "b".to_string()]
            )]
        );
    }

    #[test]
    fn dua_writer_path_sama_jadi_barrier() {
        let calls = vec![
            call("a", "write_file", json!({"path": "a.txt", "content": "1"})),
            call("b", "write_file", json!({"path": "a.txt", "content": "2"})),
        ];
        assert_eq!(
            kinds(&plan(&calls)),
            vec![(
                SegmentKind::Sequential,
                vec!["a".to_string(), "b".to_string()]
            )]
        );
    }

    #[test]
    fn read_setelah_write_path_sama_diserialkan() {
        // Aturan path-overlap: read atas path yang baru ditulis di batch yang
        // sama harus menunggu — write menutup run, read menunggu di segmen
        // berikutnya (urutan eksekusi segmen menjamin write selesai dulu).
        let calls = vec![
            call("a", "write_file", json!({"path": "f.txt", "content": "1"})),
            call("b", "read_file", json!({"path": "f.txt"})),
        ];
        assert_eq!(
            kinds(&plan(&calls)),
            vec![(
                SegmentKind::Sequential,
                vec!["a".to_string(), "b".to_string()]
            )]
        );
    }

    #[test]
    fn read_path_lain_tidak_terpengaruh_write() {
        // read atas path yang tak ditulis tetap boleh paralel dengan writer.
        let calls = vec![
            call("a", "write_file", json!({"path": "a.txt", "content": "1"})),
            call("b", "read_file", json!({"path": "b.txt"})),
        ];
        assert_eq!(
            kinds(&plan(&calls)),
            vec![(
                SegmentKind::Parallel,
                vec!["a".to_string(), "b".to_string()]
            )]
        );
    }

    #[test]
    fn tool_tak_dikenal_adalah_barrier_write() {
        // Tool tak dikenal/nonaktif → effect Write konservatif (barrier),
        // paritas original.
        let calls = vec![
            call("a", "web_search", json!({"query": "x"})),
            call("b", "ghost_tool", json!({})),
            call("c", "web_search", json!({"query": "y"})),
        ];
        assert_eq!(
            kinds(&plan(&calls)),
            vec![(
                SegmentKind::Sequential,
                vec!["a".to_string(), "b".to_string(), "c".to_string()]
            )]
        );
    }

    #[test]
    fn clarify_adalah_barrier_dan_segmen_sekuensial_bergabung() {
        let calls = vec![
            call("a", "read_file", json!({"path": "a.txt"})),
            call("b", "clarify", json!({"question": "lanjut?"})),
            call("c", "read_file", json!({"path": "b.txt"})),
        ];
        assert_eq!(
            kinds(&plan(&calls)),
            vec![(
                SegmentKind::Sequential,
                vec!["a".to_string(), "b".to_string(), "c".to_string()]
            )]
        );
    }

    #[test]
    fn writer_non_path_scoped_adalah_barrier() {
        // exec (writer, cakupan tak terlacak) → barrier; segmen sekuensial
        // bersebelahan digabung (aturan original).
        let calls = vec![
            call("a", "web_search", json!({"query": "x"})),
            call("b", "exec", json!({"command": "ls"})),
            call("c", "web_search", json!({"query": "y"})),
        ];
        assert_eq!(
            kinds(&plan(&calls)),
            vec![(
                SegmentKind::Sequential,
                vec!["a".to_string(), "b".to_string(), "c".to_string()]
            )]
        );
    }

    #[test]
    fn argumen_non_objek_adalah_barrier() {
        // argumen non-objek → barrier; segmen sekuensial bersebelahan
        // digabung (aturan original).
        let calls = vec![
            call("a", "web_search", json!({"query": "x"})),
            call("b", "web_search", json!("bukan-objek")),
            call("c", "web_search", json!({"query": "y"})),
        ];
        assert_eq!(
            kinds(&plan(&calls)),
            vec![(
                SegmentKind::Sequential,
                vec!["a".to_string(), "b".to_string(), "c".to_string()]
            )]
        );
    }

    #[test]
    fn run_tunggal_diturunkan_ke_sekuensial() {
        // web_search aman-paralel tapi sendirian → sekuensial (original:
        // "Runs shorter than two calls demote to sequential").
        let calls = vec![call("a", "web_search", json!({"query": "x"}))];
        assert_eq!(
            kinds(&plan(&calls)),
            vec![(SegmentKind::Sequential, vec!["a".to_string()])]
        );
    }

    #[test]
    fn urutan_emisi_selalu_dipertahankan() {
        let calls = vec![
            call("a", "web_search", json!({"query": "1"})),
            call("b", "write_file", json!({"path": "f.txt", "content": "x"})),
            call("c", "read_file", json!({"path": "f.txt"})),
            call("d", "web_search", json!({"query": "2"})),
        ];
        let segments = plan(&calls);
        let order: Vec<&str> = segments
            .iter()
            .flat_map(|(_, cs)| cs.iter().map(|c| c.id.as_str()))
            .collect();
        assert_eq!(order, vec!["a", "b", "c", "d"]);
        assert_eq!(
            kinds(&segments),
            vec![
                (
                    SegmentKind::Parallel,
                    vec!["a".to_string(), "b".to_string()]
                ),
                (
                    SegmentKind::Parallel,
                    vec!["c".to_string(), "d".to_string()]
                ),
            ]
        );
    }

    #[test]
    fn normalisasi_path_menangkap_bentuk_ekuivalen() {
        // ./f.txt dan sub/../f.txt adalah path yang sama → konflik writer.
        let calls = vec![
            call(
                "a",
                "write_file",
                json!({"path": "./f.txt", "content": "1"}),
            ),
            call("b", "read_file", json!({"path": "sub/../f.txt"})),
        ];
        assert_eq!(
            kinds(&plan(&calls)),
            vec![(
                SegmentKind::Sequential,
                vec!["a".to_string(), "b".to_string()]
            )]
        );
    }
}
