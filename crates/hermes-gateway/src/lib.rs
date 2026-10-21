//! hermes-gateway: messaging gateway Hermes.
//!
//! Meniru gateway Hermes asli: satu agen dapat dijangkau dari banyak platform
//! (Telegram, Discord, Slack, WhatsApp, Signal, ...). Port Rust ini menyediakan
//! trait `Platform`, implementasi penuh Telegram (long-polling), dan kerangka
//! untuk platform lain.

pub mod allowlist;
pub mod discord;
pub mod email;
pub mod matrix;
pub mod rate_limit;
pub mod registry;
pub mod signal;
pub mod slack;
pub mod split;
pub mod teams;
pub mod telegram;
pub mod threading;
pub mod webhook;
pub mod whatsapp;

pub use discord::{serve_discord, DiscordMessage, DiscordPlatform};
pub use email::{serve_email, EmailMessage, EmailPlatform, EmailTlsMode};
pub use matrix::{
    build_text_content, content_bytes, markdown_to_matrix_html, sanitize_matrix_html, serve_matrix,
    MatrixMessage, MatrixPlatform, MATRIX_MAX_CONTENT_BYTES,
};
pub use registry::{Bot, PlatformRegistry};
pub use signal::{serve_signal, SignalMessage, SignalPlatform};
pub use slack::{format_slack_mrkdwn, serve_slack, SlackMessage, SlackPlatform};
pub use split::{
    char_len, matrix_message_limit, split_message, split_message_with_len, utf16_len, utf8_len,
    DISCORD_MESSAGE_LIMIT, MATRIX_MESSAGE_LIMIT, SIGNAL_MESSAGE_LIMIT, SLACK_MESSAGE_LIMIT,
    TEAMS_MESSAGE_LIMIT, TELEGRAM_MESSAGE_LIMIT, WHATSAPP_MESSAGE_LIMIT,
};
pub use teams::{serve_teams_webhook, teams_redirect_policy, TeamsMessage, TeamsPlatform};
pub use telegram::{
    escape_mdv2, format_telegram_mdv2, parse_updates, serve_telegram, strip_mdv2, TelegramPlatform,
    TelegramUpdate,
};
pub use threading::ReplyThreadMode;
pub use webhook::{notify_outgoing, serve_webhook, WebhookRequest};
pub use whatsapp::{
    format_whatsapp, run_whatsapp_gateway, send_voice_note, verify_subscription, WhatsAppMessage,
    WhatsAppPlatform,
};
