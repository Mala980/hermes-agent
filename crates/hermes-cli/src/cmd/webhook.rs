//! Handler webhook.
//! Dipindah dari main.rs (refactor batas ukuran file).

use super::chat::AgentFactory;
use super::cli::WebhookCmd;
use super::cron::ChatAgentPool;
use crate::app::App;
use crate::approval::GatewayApproval;
use std::sync::Arc;

/// Webhook masuk: POST /hook {"text": "...", "session": "opsional"} → agen → {"response": "..."}.
pub(crate) async fn cmd_webhook(app: &App, cmd: WebhookCmd) -> anyhow::Result<()> {
    match cmd {
        WebhookCmd::Serve { port } => {
            let pool = ChatAgentPool::new(
                AgentFactory::from_app(app).with_approval(Arc::new(GatewayApproval::from_env())),
            );
            let secret = app.config.webhooks.secret.clone();
            println!("Webhook masuk di port {port} (POST /hook). Tekan Ctrl-C untuk berhenti.");
            hermes_gateway::serve_webhook(
                port,
                secret,
                move |wr: hermes_gateway::WebhookRequest| {
                    let pool = pool.clone();
                    async move {
                        let key = wr.session.clone().unwrap_or_else(|| "webhook".to_string());
                        pool.reply(&key, "wh", &wr.text).await
                    }
                },
            )
            .await?;
            Ok(())
        }
    }
}
