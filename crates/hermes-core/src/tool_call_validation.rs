//! Validasi batch tool call SEBELUM dispatch — paritas
//! `agent/turn_tool_validation.py` dan `agent/turn_tool_round.py` (audit W1-B10).
//!
//! Urutan per batch: (1) id duplikat diunifikasi (kemunculan ke-2+ mendapat
//! akhiran `-2`, `-3`, …), (2) nama tool diperbaiki — cocok persis dulu, lalu
//! cocok ternormalisasi (lowercase, buang `_`/`-`/spasi) terhadap nama tool
//! yang dikenal dari [`ToolRegistry`], (3) nama tak dikenal dihitung sebagai
//! strike sesi — strike ke-3 menghentikan giliran dengan error yang menyebut
//! nama pelakunya, (4) pemanggilan identik (nama + argumen JSON sama) didedupe
//! dengan kemunculan pertama dipertahankan, (5) pemanggilan `delegate_task`
//! dibatasi per giliran.
//!
//! Strike dilacak per sesi lewat [`ToolCallValidator`] yang disimpan di `Agent`
//! (satu instance `Agent` = satu sesi). Nama tak dikenal pada strike 1–2 tetap
//! ada di batch hasil — pemanggil mengubahnya menjadi hasil error agar model
//! bisa koreksi diri (mixed_invalid_batch pada original).

use std::collections::{HashMap, HashSet};

use crate::{ToolCall, ToolRegistry};

/// Strike maksimum nama tool tak dikenal per sesi; strike ke-3 menghentikan giliran.
pub const MAX_INVALID_NAME_STRIKES: u32 = 3;
/// Batas pemanggilan `delegate_task` dalam satu giliran model.
pub const MAX_DELEGATE_TASK_PER_TURN: usize = 4;
/// Nama tool delegasi yang dibatasi per giliran.
pub const DELEGATE_TASK_TOOL: &str = "delegate_task";

/// Error validasi batch tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationError {
    /// Nama tool tak dikenal untuk ke-3 kalinya dalam sesi ini — giliran dihentikan.
    InvalidToolName { name: String },
    /// Terlalu banyak pemanggilan `delegate_task` dalam satu giliran.
    DelegateTaskCapExceeded { count: usize },
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ValidationError::InvalidToolName { name } => write!(
                f,
                "model generated invalid tool call '{name}' \
                 ({MAX_INVALID_NAME_STRIKES} invalid-name strikes this session); halting turn"
            ),
            ValidationError::DelegateTaskCapExceeded { count } => write!(
                f,
                "too many delegate_task calls in one turn: {count} \
                 (max {MAX_DELEGATE_TASK_PER_TURN})"
            ),
        }
    }
}

impl std::error::Error for ValidationError {}

/// Validator batch tool call; satu instance per sesi agent — counter strike nama
/// tak dikenal hidup di sini (paritas `agent._invalid_tool_retries`).
#[derive(Debug, Default)]
pub struct ToolCallValidator {
    invalid_name_strikes: u32,
}

impl ToolCallValidator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Jumlah strike nama tool tak dikenal pada sesi ini.
    pub fn invalid_name_strikes(&self) -> u32 {
        self.invalid_name_strikes
    }

    /// Validasi satu batch tool call dari model SEBELUM dispatch.
    ///
    /// Mengembalikan batch yang id-nya sudah diunifikasi, namanya diperbaiki,
    /// didedupe, dan dibatasi `delegate_task`-nya. Nama tak dikenal pada strike
    /// 1–2 tetap ada di batch — pemanggil WAJIB mengubahnya menjadi
    /// `ToolResult` error TANPA mengeksekusi, agar model bisa koreksi diri.
    pub fn validate_tool_calls(
        &mut self,
        calls: Vec<ToolCall>,
        registry: &ToolRegistry,
    ) -> Result<Vec<ToolCall>, ValidationError> {
        let mut calls = calls;
        uniquify_duplicate_ids(&mut calls);
        let invalid_names =
            repair_tool_names(&mut calls, registry, &mut self.invalid_name_strikes)?;
        deduplicate_calls(&mut calls);
        cap_delegate_task_calls(&calls)?;
        // Batch tanpa nama tak dikenal: reset strike (paritas original
        // me-reset counter `_invalid_tool_retries` pada validasi sukses).
        if invalid_names == 0 {
            self.invalid_name_strikes = 0;
        }
        Ok(calls)
    }
}

/// (1) Unifikasi id duplikat dalam satu batch: kemunculan ke-2 dan seterusnya
/// mendapat akhiran `-2`, `-3`, … Id kosong dibiarkan (ditangani hilir).
fn uniquify_duplicate_ids(calls: &mut [ToolCall]) {
    let mut seen: HashSet<String> = HashSet::new();
    for call in calls.iter_mut() {
        if call.id.is_empty() {
            continue;
        }
        if seen.insert(call.id.clone()) {
            continue;
        }
        let base = call.id.clone();
        let mut n = 2u32;
        loop {
            let candidate = format!("{base}-{n}");
            if seen.insert(candidate.clone()) {
                call.id = candidate;
                break;
            }
            n += 1;
        }
    }
}

/// Normalisasi untuk pencocokan longgar: lowercase, buang `_`, `-`, spasi.
fn normalize_tool_name(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .filter(|c| !matches!(c, '_' | '-' | ' '))
        .collect()
}

/// (2)+(3) Perbaiki nama tool: cocok persis dulu, lalu cocok ternormalisasi
/// terhadap nama tool yang dikenal dari registry. Nama yang tetap tak dikenal
/// dihitung sebagai strike sesi; strike ke-3 menghentikan giliran.
/// Mengembalikan jumlah nama tak dikenal pada batch ini.
fn repair_tool_names(
    calls: &mut [ToolCall],
    registry: &ToolRegistry,
    strikes: &mut u32,
) -> Result<usize, ValidationError> {
    let known = registry.names();
    let known_set: HashSet<&str> = known.iter().map(String::as_str).collect();
    // Peta nama ternormalisasi → nama asli. `names()` sudah terurut, jadi bila
    // dua nama ternormalisasi sama, pemenangnya deterministik.
    let mut normalized: HashMap<String, &str> = HashMap::new();
    for name in &known {
        normalized.entry(normalize_tool_name(name)).or_insert(name);
    }
    let mut invalid = 0usize;
    for call in calls.iter_mut() {
        if known_set.contains(call.name.as_str()) {
            continue;
        }
        if let Some(repaired) = normalized.get(&normalize_tool_name(&call.name)) {
            call.name = repaired.to_string();
            continue;
        }
        invalid += 1;
        *strikes += 1;
        if *strikes >= MAX_INVALID_NAME_STRIKES {
            // Jangan biarkan counter basi setelah halt.
            *strikes = 0;
            return Err(ValidationError::InvalidToolName {
                name: call.name.clone(),
            });
        }
    }
    Ok(invalid)
}

/// Argumen JSON kanonis: kunci objek diurutkan rekursif agar urutan kunci /
/// whitespace tak bisa menghindari dedupe (paritas `json.dumps(sort_keys=True)`
/// pada `_deduplicate_tool_calls`).
fn canonical_args(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys
                .into_iter()
                .map(|k| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(k).unwrap_or_default(),
                        canonical_args(&map[k])
                    )
                })
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        serde_json::Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(canonical_args).collect();
            format!("[{}]", parts.join(","))
        }
        other => other.to_string(),
    }
}

/// (4) Buang pemanggilan identik (nama + argumen JSON sama); yang pertama menang.
fn deduplicate_calls(calls: &mut Vec<ToolCall>) {
    let mut seen: HashSet<(String, String)> = HashSet::new();
    calls.retain(|call| {
        let key = (call.name.clone(), canonical_args(&call.arguments));
        seen.insert(key)
    });
}

/// (5) Batasi pemanggilan `delegate_task` per giliran; selebihnya → error jelas.
fn cap_delegate_task_calls(calls: &[ToolCall]) -> Result<(), ValidationError> {
    let count = calls
        .iter()
        .filter(|c| c.name == DELEGATE_TASK_TOOL)
        .count();
    if count > MAX_DELEGATE_TASK_PER_TURN {
        return Err(ValidationError::DelegateTaskCapExceeded { count });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use serde_json::json;

    struct StubTool {
        name: &'static str,
    }

    #[async_trait]
    impl crate::Tool for StubTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "stub untuk tes validasi tool call"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            json!({})
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &crate::ToolContext,
        ) -> crate::Result<crate::ToolResult> {
            Ok(crate::ToolResult {
                tool_call_id: String::new(),
                name: self.name.to_string(),
                content: "ok".to_string(),
                is_error: false,
            })
        }
    }

    fn test_registry() -> ToolRegistry {
        let mut reg = ToolRegistry::new();
        reg.register(StubTool { name: "read_file" });
        reg.register(StubTool {
            name: "delegate_task",
        });
        reg
    }

    fn call(id: &str, name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments: args,
        }
    }

    #[test]
    fn duplicate_ids_are_uniquified() {
        let mut v = ToolCallValidator::new();
        let reg = test_registry();
        let out = v
            .validate_tool_calls(
                vec![
                    call("call_1", "read_file", json!({})),
                    call("call_1", "read_file", json!({"a": 1})),
                    call("call_1", "read_file", json!({"a": 2})),
                ],
                &reg,
            )
            .unwrap();
        let ids: Vec<&str> = out.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["call_1", "call_1-2", "call_1-3"]);
    }

    #[test]
    fn exact_name_passes_through() {
        let mut v = ToolCallValidator::new();
        let reg = test_registry();
        let out = v
            .validate_tool_calls(vec![call("id1", "read_file", json!({}))], &reg)
            .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "read_file");
        assert_eq!(v.invalid_name_strikes(), 0);
    }

    #[test]
    fn normalized_name_is_repaired() {
        let mut v = ToolCallValidator::new();
        let reg = test_registry();
        let out = v
            .validate_tool_calls(
                vec![
                    call("id1", "read-file", json!({"path": "a"})),
                    call("id2", "Read File", json!({"path": "b"})),
                ],
                &reg,
            )
            .unwrap();
        assert_eq!(out[0].name, "read_file");
        assert_eq!(out[1].name, "read_file");
        // Nama yang berhasil diperbaiki bukan strike.
        assert_eq!(v.invalid_name_strikes(), 0);
    }

    #[test]
    fn unknown_name_strikes_halt_at_three() {
        let mut v = ToolCallValidator::new();
        let reg = test_registry();
        let one_bad = || vec![call("id1", "frobnicate", json!({}))];

        // Strike 1–2: batch lolos (nama tak dikenal jadi hasil error di hilir).
        assert!(v.validate_tool_calls(one_bad(), &reg).is_ok());
        assert_eq!(v.invalid_name_strikes(), 1);
        assert!(v.validate_tool_calls(one_bad(), &reg).is_ok());
        assert_eq!(v.invalid_name_strikes(), 2);

        // Batch valid penuh me-reset strike.
        assert!(v
            .validate_tool_calls(vec![call("id1", "read_file", json!({}))], &reg)
            .is_ok());
        assert_eq!(v.invalid_name_strikes(), 0);

        // Tiga nama tak dikenal lagi → strike ke-3 menghentikan giliran.
        assert!(v.validate_tool_calls(one_bad(), &reg).is_ok());
        assert!(v.validate_tool_calls(one_bad(), &reg).is_ok());
        let err = v.validate_tool_calls(one_bad(), &reg).unwrap_err();
        assert_eq!(
            err,
            ValidationError::InvalidToolName {
                name: "frobnicate".to_string()
            }
        );
        assert!(
            err.to_string().contains("frobnicate"),
            "error harus menyebut nama pelakunya: {err}"
        );
    }

    #[test]
    fn exact_duplicate_calls_are_deduped_keeping_first() {
        let mut v = ToolCallValidator::new();
        let reg = test_registry();
        let out = v
            .validate_tool_calls(
                vec![
                    call("id-a", "read_file", json!({"path": "x", "n": 1})),
                    // Duplikat persis (urutan kunci JSON beda — tetap terdeteksi).
                    call("id-b", "read_file", json!({"n": 1, "path": "x"})),
                    call("id-c", "read_file", json!({"path": "y"})),
                ],
                &reg,
            )
            .unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].id, "id-a");
        assert_eq!(out[1].id, "id-c");
    }

    #[test]
    fn delegate_task_cap_exceeded_errors() {
        let mut v = ToolCallValidator::new();
        let reg = test_registry();
        let five: Vec<ToolCall> = (0..5)
            .map(|i| call(&format!("d{i}"), "delegate_task", json!({"task": i})))
            .collect();
        let err = v.validate_tool_calls(five, &reg).unwrap_err();
        assert_eq!(err, ValidationError::DelegateTaskCapExceeded { count: 5 });
        assert!(
            err.to_string().contains("delegate_task"),
            "error harus jelas: {err}"
        );

        // Tepat di batas (4) lolos.
        let four: Vec<ToolCall> = (0..4)
            .map(|i| call(&format!("d{i}"), "delegate_task", json!({"task": i})))
            .collect();
        assert!(v.validate_tool_calls(four, &reg).is_ok());
    }

    #[test]
    fn validation_preserves_batch_order() {
        let mut v = ToolCallValidator::new();
        let reg = test_registry();
        let out = v
            .validate_tool_calls(
                vec![
                    call("id-a", "read_file", json!({"path": "a"})),
                    call("id-b", "delegate_task", json!({"task": 1})),
                    // Duplikat dari id-a → dibuang, sisanya tetap berurutan.
                    call("id-c", "read_file", json!({"path": "a"})),
                    call("id-d", "read_file", json!({"path": "b"})),
                ],
                &reg,
            )
            .unwrap();
        let ids: Vec<&str> = out.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["id-a", "id-b", "id-d"]);
    }
}
