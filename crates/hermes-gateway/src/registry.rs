//! Registry platform + Bot Mode (named bots dengan model/memory/skills sendiri).
use crate::telegram::UnconfiguredPlatform;
use hermes_core::Platform;
use std::collections::HashMap;
use std::sync::Arc;

/// Bot bernama: identitas + target chat + profil config yang dipakai.
#[derive(Debug, Clone)]
pub struct Bot {
    pub name: String,
    pub platform: String,
    pub target: String,
    pub profile: String,
}

/// Registry platform gateway.
#[derive(Default)]
pub struct PlatformRegistry {
    platforms: HashMap<String, Arc<dyn Platform>>,
    bots: HashMap<String, Bot>,
}

impl PlatformRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, platform: Arc<dyn Platform>) {
        self.platforms.insert(platform.id().to_string(), platform);
    }

    /// Daftarkan stub untuk platform yang belum dikonfigurasi.
    pub fn register_stub(&mut self, id: &str, hint: &str) {
        self.platforms.insert(
            id.to_string(),
            Arc::new(UnconfiguredPlatform::new(id, hint)),
        );
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn Platform>> {
        self.platforms.get(id).cloned()
    }

    pub fn platform_ids(&self) -> Vec<String> {
        let mut ids: Vec<_> = self.platforms.keys().cloned().collect();
        ids.sort();
        ids
    }

    pub fn add_bot(&mut self, bot: Bot) {
        self.bots.insert(bot.name.clone(), bot);
    }

    pub fn bot(&self, name: &str) -> Option<&Bot> {
        self.bots.get(name)
    }

    pub fn bots(&self) -> Vec<&Bot> {
        self.bots.values().collect()
    }

    /// Kirim pesan lewat platform + target (dipakai cron & tool gateway_send).
    pub async fn send(
        &self,
        platform_id: &str,
        target: &str,
        text: &str,
    ) -> hermes_core::Result<()> {
        let p = self.get(platform_id).ok_or_else(|| {
            hermes_core::HermesError::Gateway(format!("platform `{platform_id}` tidak dikenal"))
        })?;
        p.send_message(target, text).await
    }
}
