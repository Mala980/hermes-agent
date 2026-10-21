//! Handler cron (termasuk JobRunner).
//! Dipindah dari main.rs (refactor batas ukuran file).

use super::chat::AgentFactory;
use super::cli::CronCmd;
use crate::app::App;
use crate::approval::GatewayApproval;
use hermes_core::Agent;
use hermes_core::Session;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

pub(crate) async fn cmd_cron(app: &mut App, cmd: CronCmd) -> anyhow::Result<()> {
    match cmd {
        CronCmd::List => {
            let jobs = app.cron_store.list()?;
            if jobs.is_empty() {
                println!("Tidak ada cron job.");
            }
            for j in jobs {
                println!(
                    "- {} [{}] `{}` → {}:{} (berikutnya: {})",
                    j.name,
                    if j.enabled { "aktif" } else { "nonaktif" },
                    j.schedule,
                    j.platform,
                    j.target,
                    j.next_run
                        .map(|d| d.to_string())
                        .unwrap_or_else(|| "-".into())
                );
            }
        }
        CronCmd::Add {
            name,
            schedule,
            prompt,
            platform,
            target,
            timezone,
            overlap,
        } => {
            let mut job = hermes_cron::CronJob::new(
                name,
                schedule,
                prompt,
                platform,
                target,
                app.profile_name.clone(),
            )?;
            if let Some(tz) = timezone {
                // Nama IANA sudah divalidasi saat parsing argumen.
                job.timezone = Some(tz);
            }
            job.overlap = overlap.unwrap_or_default();
            // Hitung ulang next_run: timezone baru mengubah wall-clock jadwal.
            job.recompute_next();
            app.cron_store.add(&job)?;
            println!(
                "Job `{}` ditambahkan (zona: {}, overlap: {}).",
                job.name,
                job.timezone.as_deref().unwrap_or("lokal"),
                job.overlap
            );
        }
        CronCmd::Remove { name } => {
            if app.cron_store.remove(&name)? {
                println!("Job `{name}` dihapus.");
            } else {
                println!("Job `{name}` tidak ditemukan.");
            }
        }
        CronCmd::Toggle { name, off } => {
            if app.cron_store.set_enabled(&name, !off)? {
                println!(
                    "Job `{name}` {}.",
                    if off { "dinonaktifkan" } else { "diaktifkan" }
                );
            } else {
                println!("Job `{name}` tidak ditemukan.");
            }
        }
        CronCmd::Serve => {
            println!("Scheduler cron berjalan. Tekan Ctrl-C untuk berhenti.");
            let runner = Arc::new(CliJobRunner::from_app(app));
            hermes_cron::run_scheduler(
                app.cron_store.clone(),
                app.platforms.clone(),
                runner,
                30,
                app.cancel.clone(),
            )
            .await;
        }
    }
    Ok(())
}

/// JobRunner untuk CLI: setiap job dijalankan agen baru dengan GatewayApproval.
pub(crate) struct CliJobRunner {
    pub(crate) factory: AgentFactory,
    pub(crate) outgoing: Vec<String>,
}

impl CliJobRunner {
    pub(crate) fn from_app(app: &App) -> Self {
        let factory =
            AgentFactory::from_app(app).with_approval(Arc::new(GatewayApproval::from_env()));
        let outgoing = app.config.webhooks.outgoing.clone();
        Self { factory, outgoing }
    }
}

impl hermes_cron::JobRunner for CliJobRunner {
    fn run_job<'a>(
        &'a self,
        job: &'a hermes_cron::CronJob,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = hermes_core::Result<String>> + Send + 'a>>
    {
        Box::pin(async move {
            let session = Session::new(format!("cron:{}", job.profile));
            let mut agent = self.factory.build(session);
            let result = agent.run_turn(&job.prompt).await;
            if let Ok(ref response) = result {
                hermes_gateway::notify_outgoing(
                    &self.outgoing,
                    serde_json::json!({
                        "event": "cron.completed",
                        "job": job.name,
                        "response": response,
                    }),
                );
            }
            result
        })
    }
}

/// Pool agen per-chat untuk gateway: satu agen + sesi persisten per chat id.
#[derive(Clone)]
pub(crate) struct ChatAgentPool {
    pub(crate) agents: Arc<Mutex<HashMap<String, Agent>>>,
    pub(crate) factory: AgentFactory,
}

impl ChatAgentPool {
    pub(crate) fn new(factory: AgentFactory) -> Self {
        Self {
            agents: Arc::new(Mutex::new(HashMap::new())),
            factory,
        }
    }

    pub(crate) async fn reply(
        &self,
        key: &str,
        session_prefix: &str,
        text: &str,
    ) -> hermes_core::Result<String> {
        if text.trim() == "/new" {
            self.agents.lock().await.remove(key);
            return Ok("Sesi baru dimulai. 👋".to_string());
        }
        if !self.agents.lock().await.contains_key(key) {
            let session_id = format!("{session_prefix}-{key}");
            let session = self
                .factory
                .memory
                .load_session(&session_id)
                .await?
                .unwrap_or_else(|| {
                    let mut s = Session::new("gateway");
                    s.id = session_id;
                    s
                });
            let agent = self.factory.build_quiet(session);
            self.agents.lock().await.insert(key.to_string(), agent);
        }
        let mut guard = self.agents.lock().await;
        let agent = guard.get_mut(key).unwrap();
        agent.run_turn(text).await
    }

    /// Seperti `reply`, tapi kirim event ke webhook keluar bila dikonfigurasi.
    pub(crate) async fn reply_notify(
        &self,
        outgoing: &[String],
        platform: &str,
        key: &str,
        session_prefix: &str,
        text: &str,
    ) -> hermes_core::Result<String> {
        let reply = self.reply(key, session_prefix, text).await;
        if let Ok(ref response) = reply {
            hermes_gateway::notify_outgoing(
                outgoing,
                serde_json::json!({
                    "event": "gateway.reply",
                    "platform": platform,
                    "chat": key,
                    "response": response,
                }),
            );
        }
        reply
    }
}

#[cfg(test)]
mod cron_cli_parse_tests {
    use super::super::cli::{Cli, Commands};
    use super::*;
    use clap::Parser;

    pub(crate) fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("hermes").chain(args.iter().copied()))
    }

    fn add_cmd(cli: Cli) -> CronCmd {
        match cli.command {
            Some(Commands::Cron { cmd }) => cmd,
            _ => panic!("diharapkan subcommand cron"),
        }
    }

    #[test]
    fn cron_add_timezone_dan_overlap_diterima() {
        let cli = parse(&[
            "cron",
            "add",
            "pagi",
            "0 0 9 * * *",
            "sapa pengguna",
            "--target",
            "123",
            "--timezone",
            "Asia/Makassar",
            "--overlap",
            "allow",
        ])
        .expect("parse harus berhasil");
        match add_cmd(cli) {
            CronCmd::Add {
                timezone,
                overlap,
                name,
                ..
            } => {
                assert_eq!(name, "pagi");
                assert_eq!(timezone.as_deref(), Some("Asia/Makassar"));
                assert_eq!(overlap, Some(hermes_cron::OverlapPolicy::Allow));
            }
            _ => panic!("varian tak terduga"),
        }
    }

    #[test]
    fn cron_add_tanpa_flag_pakai_default() {
        let cli = parse(&[
            "cron",
            "add",
            "pagi",
            "0 0 9 * * *",
            "sapa pengguna",
            "--target",
            "123",
        ])
        .expect("parse harus berhasil");
        match add_cmd(cli) {
            CronCmd::Add {
                timezone, overlap, ..
            } => {
                assert_eq!(timezone, None);
                assert_eq!(overlap, None);
            }
            _ => panic!("varian tak terduga"),
        }
    }

    #[test]
    fn cron_add_overlap_bogus_ditolak_saat_parse() {
        let err = match parse(&[
            "cron",
            "add",
            "pagi",
            "0 0 9 * * *",
            "sapa pengguna",
            "--target",
            "123",
            "--overlap",
            "bogus",
        ]) {
            Ok(_) => panic!("--overlap bogus harus ditolak"),
            Err(e) => e,
        };
        let msg = err.to_string();
        assert!(
            msg.contains("bogus") && msg.contains("skip"),
            "pesan error harus jelas, dapat: {msg}"
        );
    }

    #[test]
    fn cron_add_timezone_invalid_ditolak_saat_parse() {
        let err = match parse(&[
            "cron",
            "add",
            "pagi",
            "0 0 9 * * *",
            "sapa pengguna",
            "--target",
            "123",
            "--timezone",
            "Mars/Olympus",
        ]) {
            Ok(_) => panic!("--timezone invalid harus ditolak"),
            Err(e) => e,
        };
        let msg = err.to_string();
        assert!(
            msg.contains("IANA"),
            "pesan error harus menyebut IANA, dapat: {msg}"
        );
    }
}
