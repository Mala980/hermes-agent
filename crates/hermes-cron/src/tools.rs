//! Tool cron: cron_add, cron_list, cron_remove, cron_toggle.
//!
//! Memungkinkan agen mengelola jadwalnya sendiri — seperti Hermes asli.
use async_trait::async_trait;
use hermes_core::{Result, RiskLevel, Tool, ToolContext, ToolEffect, ToolResult};
use hermes_tools::schema::*;
use std::sync::Arc;

use crate::{CronJob, CronStore, OverlapPolicy};

pub struct CronAddTool {
    pub store: Arc<CronStore>,
}

#[async_trait]
impl Tool for CronAddTool {
    fn name(&self) -> &str {
        "cron_add"
    }
    fn description(&self) -> &str {
        "Menambahkan job terjadwal: jalankan prompt lewat agen lalu kirim hasilnya ke platform. \
         Format jadwal: cron 6 kolom \"0 30 9 * * *\" (detik menit jam tanggal bulan weekday); \
         cron 5 kolom \"30 9 * * *\" (detik=0); interval \"every 15m\"/\"every 1h\"/\"every 30s\"; \
         sekali jalan \"at 2026-10-22T09:00:00+08:00\" (ISO-8601; otomatis dihapus setelah berjalan); \
         kata kunci hourly, daily, weekly, midnight, noon."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        object_schema(
            vec![
                ("name", string_prop("Nama unik job.")),
                ("schedule", string_prop("Jadwal: cron 6/5 kolom, \"every 15m\" (interval), \
                    \"at <ISO-8601>\" (sekali jalan, auto-hapus), atau hourly/daily/weekly/midnight/noon.")),
                ("prompt", string_prop("Prompt yang dijalankan agen.")),
                ("platform", string_prop("Platform tujuan: telegram, ...")),
                (
                    "target",
                    string_prop("Target chat (chat id, channel, ...)."),
                ),
                (
                    "timezone",
                    string_prop(
                        "Opsional: zona waktu IANA untuk jadwal, mis. \"Asia/Makassar\". \
                         Default: zona waktu lokal sistem.",
                    ),
                ),
                (
                    "overlap",
                    string_prop(
                        "Opsional: \"skip\" (default, lewati tick bila run sebelumnya \
                         belum selesai) atau \"allow\" (izinkan tumpang tindih).",
                    ),
                ),
            ],
            vec!["name", "schedule", "prompt", "platform", "target"],
        )
    }
    fn risk(&self) -> RiskLevel {
        RiskLevel::Medium
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::Write
    }

    async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext) -> Result<ToolResult> {
        let mut job = CronJob::new(
            get_str(&args, "name")?,
            get_str(&args, "schedule")?,
            get_str(&args, "prompt")?,
            get_str(&args, "platform")?,
            get_str(&args, "target")?,
            "default",
        )?;
        if let Some(tz) = opt_str(&args, "timezone") {
            // Validasi nama zona IANA lebih awal.
            tz.parse::<chrono_tz::Tz>().map_err(|_| {
                hermes_core::HermesError::Cron(format!("timezone tidak dikenal: `{tz}`"))
            })?;
            job.timezone = Some(tz.to_string());
        }
        if let Some(o) = opt_str(&args, "overlap") {
            job.overlap = o.parse::<OverlapPolicy>()?;
        }
        job.recompute_next();
        let next = job.next_run.map(|d| d.to_string()).unwrap_or_default();
        self.store.add(&job)?;
        let kind_note = if job.is_oneshot() {
            "; sekali jalan — job otomatis dihapus setelah berjalan"
        } else {
            ""
        };
        Ok(ok(
            "",
            self.name(),
            format!(
                "job `{}` ditambahkan; jalan berikutnya: {next}{kind_note}",
                job.name
            ),
        ))
    }
}

pub struct CronListTool {
    pub store: Arc<CronStore>,
}

#[async_trait]
impl Tool for CronListTool {
    fn name(&self) -> &str {
        "cron_list"
    }
    fn description(&self) -> &str {
        "Menampilkan semua cron job beserta jadwal dan statusnya."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        object_schema(vec![], vec![])
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::Read
    }

    async fn execute(&self, _args: serde_json::Value, _ctx: &ToolContext) -> Result<ToolResult> {
        let jobs = self.store.list()?;
        if jobs.is_empty() {
            return Ok(ok("", self.name(), "tidak ada cron job.".to_string()));
        }
        let body = jobs
            .iter()
            .map(|j| {
                format!(
                    "- {} [{}] `{}`{} → {}:{} (berikutnya: {})",
                    j.name,
                    if j.enabled { "aktif" } else { "nonaktif" },
                    j.schedule,
                    j.timezone
                        .as_deref()
                        .map(|t| format!(" @{t}"))
                        .unwrap_or_default(),
                    j.platform,
                    j.target,
                    j.next_run
                        .map(|d| d.to_string())
                        .unwrap_or_else(|| "-".into())
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ok("", self.name(), body))
    }
}

pub struct CronRemoveTool {
    pub store: Arc<CronStore>,
}

#[async_trait]
impl Tool for CronRemoveTool {
    fn name(&self) -> &str {
        "cron_remove"
    }
    fn description(&self) -> &str {
        "Menghapus cron job berdasarkan nama atau id."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        object_schema(
            vec![("name", string_prop("Nama atau id job."))],
            vec!["name"],
        )
    }
    fn risk(&self) -> RiskLevel {
        RiskLevel::Medium
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::Write
    }

    async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext) -> Result<ToolResult> {
        let name = get_str(&args, "name")?;
        if self.store.remove(name)? {
            Ok(ok("", self.name(), format!("job `{name}` dihapus")))
        } else {
            Ok(ok("", self.name(), format!("job `{name}` tidak ditemukan")))
        }
    }
}

pub struct CronToggleTool {
    pub store: Arc<CronStore>,
}

#[async_trait]
impl Tool for CronToggleTool {
    fn name(&self) -> &str {
        "cron_toggle"
    }
    fn description(&self) -> &str {
        "Mengaktifkan/menonaktifkan cron job."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        object_schema(
            vec![
                ("name", string_prop("Nama atau id job.")),
                ("enabled", boolean_prop("true = aktif, false = nonaktif.")),
            ],
            vec!["name", "enabled"],
        )
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::Write
    }

    async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext) -> Result<ToolResult> {
        let name = get_str(&args, "name")?;
        let enabled = opt_bool(&args, "enabled").unwrap_or(true);
        if self.store.set_enabled(name, enabled)? {
            Ok(ok(
                "",
                self.name(),
                format!(
                    "job `{name}` {}",
                    if enabled {
                        "diaktifkan"
                    } else {
                        "dinonaktifkan"
                    }
                ),
            ))
        } else {
            Ok(ok("", self.name(), format!("job `{name}` tidak ditemukan")))
        }
    }
}

/// Daftarkan tool cron ke registry.
pub fn register_cron_tools(reg: &mut hermes_core::ToolRegistry, store: Arc<CronStore>) {
    reg.register(CronAddTool {
        store: store.clone(),
    });
    reg.register(CronListTool {
        store: store.clone(),
    });
    reg.register(CronRemoveTool {
        store: store.clone(),
    });
    reg.register(CronToggleTool { store });
}
