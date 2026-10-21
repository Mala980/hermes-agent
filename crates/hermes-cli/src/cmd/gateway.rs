//! Handler gateway.
//! Dipindah dari main.rs (refactor batas ukuran file).

use super::chat::AgentFactory;
use super::cron::ChatAgentPool;
use crate::app::App;
use crate::approval::unattended_approval;
use crate::approval::ApprovalKind;

pub(crate) async fn cmd_gateway(
    app: &App,
    platform: &str,
    webhook_port: u16,
    bot: Option<&str>,
    approval_kind: ApprovalKind,
) -> anyhow::Result<()> {
    // Mode multi-bot: --bot <nama> memilih entri [[bots]] di config.
    let (platform, token_override, profile_override): (&str, Option<String>, Option<String>) =
        match bot {
            Some(name) => {
                let b = app
                    .config
                    .bots
                    .iter()
                    .find(|b| b.name == name)
                    .ok_or_else(|| {
                        anyhow::anyhow!("bot `{name}` tidak ditemukan di config [[bots]]")
                    })?;
                let token = b
                    .token
                    .clone()
                    .or_else(|| b.token_env.as_ref().and_then(|e| std::env::var(e).ok()))
                    .ok_or_else(|| {
                        anyhow::anyhow!("bot `{name}` butuh `token` atau `token_env` yang diset")
                    })?;
                (b.platform.as_str(), Some(token), b.profile.clone())
            }
            None => (platform, None, None),
        };

    let mut factory = AgentFactory::from_app(app)
        .with_approval(unattended_approval(approval_kind, &app.profile.security));
    if let Some(profile_name) = profile_override {
        let profile = app
            .config
            .profiles
            .get(&profile_name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("profil `{profile_name}` tidak ditemukan untuk bot"))?;
        factory.profile = profile;
    }
    let pool = ChatAgentPool::new(factory);
    let outgoing = app.config.webhooks.outgoing.clone();
    match platform {
        "telegram" => {
            let token = token_override.clone().or_else(|| std::env::var("TELEGRAM_BOT_TOKEN").ok())
                .ok_or_else(|| anyhow::anyhow!("Telegram belum dikonfigurasi. Set TELEGRAM_BOT_TOKEN di env atau ~/.hermes/.env"))?;
            let tg = std::sync::Arc::new(hermes_gateway::TelegramPlatform::new(token.clone()));
            println!("Gateway Telegram berjalan. Tekan Ctrl-C untuk berhenti.");
            println!("Perintah: /new (sesi baru), /voice <teks> (kirim voice note).");
            hermes_gateway::serve_telegram(token, |chat_id, _from, text| {
                let pool = pool.clone();
                let tg = tg.clone();
                let outgoing = outgoing.clone();
                async move {
                    let reply = if let Some(say) = text.strip_prefix("/voice ") {
                        let say = say.trim();
                        if say.is_empty() {
                            Ok("Pakai: /voice <teks yang dibacakan>".to_string())
                        } else {
                            match hermes_tools::media::tts_bytes(say, "tts-1", "alloy", "opus")
                                .await
                            {
                                Ok(audio) => {
                                    match tg.send_voice(chat_id, &audio, "voice.ogg").await {
                                        Ok(()) => Ok("🎙️ Voice note terkirim.".to_string()),
                                        Err(e) => Ok(format!("Gagal kirim voice note: {e}")),
                                    }
                                }
                                Err(e) => Ok(format!("Gagal TTS (butuh MEDIA_API_KEY): {e}")),
                            }
                        }
                    } else {
                        pool.reply(&chat_id.to_string(), "tg", &text).await
                    };
                    if let Ok(ref response) = reply {
                        hermes_gateway::notify_outgoing(
                            &outgoing,
                            serde_json::json!({"event": "gateway.reply", "platform": "telegram",
                                "chat": chat_id.to_string(), "response": response}),
                        );
                    }
                    reply
                }
            })
            .await?;
            Ok(())
        }
        "discord" => {
            let token = token_override.clone().or_else(|| std::env::var("DISCORD_BOT_TOKEN").ok())
                .ok_or_else(|| anyhow::anyhow!("Discord belum dikonfigurasi. Set DISCORD_BOT_TOKEN (aktifkan intent MESSAGE_CONTENT)."))?;
            println!("Gateway Discord berjalan. Tekan Ctrl-C untuk berhenti.");
            hermes_gateway::serve_discord(token, move |dm: hermes_gateway::DiscordMessage| {
                let pool = pool.clone();
                let outgoing = outgoing.clone();
                async move {
                    pool.reply_notify(&outgoing, "discord", &dm.channel_id, "dc", &dm.content)
                        .await
                }
            })
            .await?;
            Ok(())
        }
        "slack" => {
            let bot_token = token_override.clone().or_else(|| std::env::var("SLACK_BOT_TOKEN").ok())
                .ok_or_else(|| anyhow::anyhow!("Slack belum dikonfigurasi. Set SLACK_BOT_TOKEN (xoxb) dan SLACK_APP_TOKEN (xapp)."))?;
            let app_token = std::env::var("SLACK_APP_TOKEN").map_err(|_| {
                anyhow::anyhow!("SLACK_APP_TOKEN tidak diset (diperlukan untuk Socket Mode).")
            })?;
            println!("Gateway Slack berjalan (socket mode). Tekan Ctrl-C untuk berhenti.");
            hermes_gateway::serve_slack(
                bot_token,
                app_token,
                move |sm: hermes_gateway::SlackMessage| {
                    let pool = pool.clone();
                    let outgoing = outgoing.clone();
                    async move {
                        pool.reply_notify(&outgoing, "slack", &sm.channel, "sl", &sm.text)
                            .await
                    }
                },
            )
            .await?;
            Ok(())
        }
        "whatsapp" => {
            let token = token_override.clone().or_else(|| std::env::var("WHATSAPP_TOKEN").ok())
                .ok_or_else(|| anyhow::anyhow!("WhatsApp belum dikonfigurasi. Set WHATSAPP_TOKEN, WHATSAPP_PHONE_NUMBER_ID."))?;
            let pnid = std::env::var("WHATSAPP_PHONE_NUMBER_ID")
                .map_err(|_| anyhow::anyhow!("WHATSAPP_PHONE_NUMBER_ID tidak diset."))?;
            let verify =
                std::env::var("WHATSAPP_VERIFY_TOKEN").unwrap_or_else(|_| "hermes".to_string());
            println!("Gateway WhatsApp berjalan (webhook port {webhook_port}). Tekan Ctrl-C untuk berhenti.");
            println!(
                "Daftarkan URL webhook https://<domain>:{webhook_port}/webhook di Meta dashboard."
            );
            println!(
                "Set WHATSAPP_APP_SECRET (app secret Meta) agar POST webhook diverifikasi HMAC; tanpanya webhook ditolak (503)."
            );
            hermes_gateway::run_whatsapp_gateway(
                pnid,
                token,
                verify,
                webhook_port,
                move |wm: hermes_gateway::WhatsAppMessage| {
                    let pool = pool.clone();
                    let outgoing = outgoing.clone();
                    async move {
                        pool.reply_notify(&outgoing, "whatsapp", &wm.from, "wa", &wm.text)
                            .await
                    }
                },
            )
            .await?;
            Ok(())
        }
        "matrix" => {
            let homeserver = std::env::var("MATRIX_HOMESERVER").ok()
                .ok_or_else(|| anyhow::anyhow!("Matrix belum dikonfigurasi. Set MATRIX_HOMESERVER, MATRIX_ACCESS_TOKEN, MATRIX_USER_ID."))?;
            let access_token = std::env::var("MATRIX_ACCESS_TOKEN")
                .map_err(|_| anyhow::anyhow!("MATRIX_ACCESS_TOKEN tidak diset."))?;
            let user_id = std::env::var("MATRIX_USER_ID")
                .map_err(|_| anyhow::anyhow!("MATRIX_USER_ID tidak diset."))?;
            println!("Gateway Matrix berjalan. Tekan Ctrl-C untuk berhenti.");
            hermes_gateway::serve_matrix(
                homeserver,
                access_token,
                user_id,
                move |mm: hermes_gateway::MatrixMessage| {
                    let pool = pool.clone();
                    let outgoing = outgoing.clone();
                    async move {
                        pool.reply_notify(&outgoing, "matrix", &mm.room_id, "mx", &mm.body)
                            .await
                    }
                },
            )
            .await?;
            Ok(())
        }
        "signal" => {
            println!("Gateway Signal berjalan (via signal-cli). Tekan Ctrl-C untuk berhenti.");
            hermes_gateway::serve_signal(move |sm: hermes_gateway::SignalMessage| {
                let pool = pool.clone();
                let outgoing = outgoing.clone();
                async move {
                    pool.reply_notify(&outgoing, "signal", &sm.from, "sg", &sm.text)
                        .await
                }
            })
            .await?;
            Ok(())
        }
        "teams" => {
            println!("Gateway Teams berjalan (webhook port {webhook_port}). Tekan Ctrl-C untuk berhenti.");
            hermes_gateway::serve_teams_webhook(
                webhook_port,
                move |tm: hermes_gateway::TeamsMessage| {
                    let pool = pool.clone();
                    let outgoing = outgoing.clone();
                    async move {
                        pool.reply_notify(&outgoing, "teams", &tm.conversation_id, "tm", &tm.text)
                            .await
                    }
                },
            )
            .await?;
            Ok(())
        }
        "email" => {
            let platform = hermes_gateway::EmailPlatform::from_env()
                .map_err(|e| anyhow::anyhow!("Email belum dikonfigurasi: {e}"))?;
            println!("Gateway Email berjalan (polling IMAP). Tekan Ctrl-C untuk berhenti.");
            hermes_gateway::serve_email(platform, move |em: hermes_gateway::EmailMessage| {
                let pool = pool.clone();
                let outgoing = outgoing.clone();
                async move {
                    let key = em.from.clone();
                    let text = if em.subject.is_empty() {
                        em.body.clone()
                    } else {
                        format!("[{}]\n{}", em.subject, em.body)
                    };
                    pool.reply_notify(&outgoing, "email", &key, "em", &text)
                        .await
                }
            })
            .await?;
            Ok(())
        }
        other => anyhow::bail!(
            "platform `{other}` belum didukung di port Rust ini. Platform terdaftar: {:?}",
            app.platforms.platform_ids()
        ),
    }
}
