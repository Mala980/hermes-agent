use crate::{
    locate_entry_index, splice_patch_span, ChatRequest, HermesError, MemoryEntry, MemoryTarget,
    Message, Result, Session, Skill, StreamEvent,
};
use async_trait::async_trait;
use tokio::sync::mpsc;

/// Port: provider LLM yang bisa streaming chat completion.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn name(&self) -> &str;
    fn default_model(&self) -> &str;

    /// Streaming completion; event dikirim lewat channel.
    async fn chat_stream(&self, req: ChatRequest) -> Result<mpsc::Receiver<StreamEvent>>;

    /// Versi non-streaming yang praktis untuk kompresi/ringkasan.
    async fn chat_simple(&self, req: ChatRequest) -> Result<(String, crate::Usage)> {
        let mut rx = self.chat_stream(req).await?;
        let mut text = String::new();
        let mut usage = crate::Usage::default();
        while let Some(ev) = rx.recv().await {
            match ev {
                StreamEvent::TextDelta(d) => text.push_str(&d),
                StreamEvent::Done(u) => usage = u,
                StreamEvent::Error(e) => return Err(HermesError::Provider(e)),
                StreamEvent::ToolCallDelta { .. } => {}
            }
        }
        Ok((text, usage))
    }
}

/// Port: penyimpanan memori persisten + sesi.
#[async_trait]
pub trait MemoryStore: Send + Sync {
    async fn remember(&self, entry: MemoryEntry) -> Result<()>;
    async fn recall(&self, query: &str, limit: usize) -> Result<Vec<MemoryEntry>>;
    async fn forget(&self, id_or_query: &str) -> Result<usize>;
    async fn list_memories(&self, limit: usize) -> Result<Vec<MemoryEntry>>;

    /// Ambil memori yang relevan, dibatasi ke satu target (`memory`/`user`).
    /// Implementasi default: saring hasil `recall` (cukup untuk stub/pengujian);
    /// store nyata sebaiknya meng-override dengan filter di query.
    async fn recall_for(
        &self,
        query: &str,
        limit: usize,
        target: MemoryTarget,
    ) -> Result<Vec<MemoryEntry>> {
        Ok(self
            .recall(query, limit)
            .await?
            .into_iter()
            .filter(|e| e.target == target)
            .collect())
    }

    /// Daftar memori, dibatasi ke satu target. Default: saring `list_memories`.
    async fn list_memories_for(
        &self,
        limit: usize,
        target: MemoryTarget,
    ) -> Result<Vec<MemoryEntry>> {
        Ok(self
            .list_memories(limit)
            .await?
            .into_iter()
            .filter(|e| e.target == target)
            .collect())
    }

    /// Hapus memori dalam satu target saja (paritas `target` di memory tool
    /// aslinya: tulis selalu terikat satu store). Default: cari kandidat
    /// dalam target lalu hapus per id.
    async fn forget_for(&self, id_or_query: &str, target: MemoryTarget) -> Result<usize> {
        let mut total = 0;
        // Batas kandidat longgar: store nyata meng-override dengan DELETE berfilter.
        for e in self.list_memories_for(10_000, target).await? {
            if e.id == id_or_query || e.text.contains(id_or_query) {
                total += self.forget(&e.id).await?;
            }
        }
        Ok(total)
    }

    /// Patch (edit sebagian) satu entri dalam target: cari entri yang memuat
    /// `old_text` (pencocokan toleran-tipografi ala original), ganti SATU
    /// rentang itu dengan `new_text`, sisa entri dipertahankan verbatim.
    /// Mengembalikan entri hasil patch. Default: read-modify-write via
    /// `list_memories_for` + `remember` (pemindaian ancaman ikut `remember`
    /// bila store-nya memindai); store nyata sebaiknya meng-override secara
    /// atomik.
    async fn patch_entry(
        &self,
        old_text: &str,
        new_text: &str,
        target: MemoryTarget,
    ) -> Result<MemoryEntry> {
        if new_text.trim().is_empty() {
            return Err(HermesError::Memory(
                "new_text tidak boleh kosong (gunakan forget untuk menghapus).".to_string(),
            ));
        }
        let entries = self.list_memories_for(10_000, target).await?;
        let idx = locate_entry_index(&entries, old_text)?;
        let patched_text = splice_patch_span(&entries[idx].text, old_text, new_text)?;
        let mut updated = entries[idx].clone();
        updated.text = patched_text;
        self.remember(updated.clone()).await?;
        Ok(updated)
    }

    async fn save_session(&self, session: &Session) -> Result<()>;
    /// Tambahkan satu pesan ke sesi lalu flush ke penyimpanan (append+flush).
    ///
    /// Paritas `agent/turn_tool_round.py` di original: tool-call turn dipersist
    /// SEBELUM side effect berjalan, dan tiap hasil tool di-flush satu per satu
    /// — crash di tengah turn tidak menghilangkan blok yang sudah tereksekusi.
    /// Implementasi default: pesan ditambahkan ke buffer `session` milik
    /// pemanggil, lalu seluruh sesi disimpan via `save_session`. Store boleh
    /// meng-override dengan append yang benar-benar atomik bila backend-nya
    /// mendukung.
    async fn append_session_message(&self, session: &mut Session, message: Message) -> Result<()> {
        session.messages.push(message);
        self.save_session(session).await
    }
    async fn load_session(&self, id: &str) -> Result<Option<Session>>;
    async fn list_sessions(&self, profile: &str, limit: usize) -> Result<Vec<Session>>;
    async fn delete_session(&self, id: &str) -> Result<()>;
    async fn search_sessions(&self, query: &str, limit: usize) -> Result<Vec<Session>>;
    /// Sesi terbaru lintas profil, terurut `updated_at` menurun (untuk bentuk
    /// *browse* di `session_search`). Default: tidak didukung — store nyata
    /// meng-override.
    async fn list_recent_sessions(&self, _limit: usize) -> Result<Vec<Session>> {
        Err(HermesError::Memory(
            "list_recent_sessions tidak didukung store ini".to_string(),
        ))
    }
}

/// Port: penyimpanan skill (procedural memory).
#[async_trait]
pub trait SkillStore: Send + Sync {
    async fn list(&self) -> Result<Vec<Skill>>;
    async fn get(&self, name: &str) -> Result<Option<Skill>>;
    async fn create(&self, name: &str, description: &str, body: &str) -> Result<Skill>;
    async fn update(&self, name: &str, body: &str) -> Result<()>;
    async fn delete(&self, name: &str) -> Result<()>;
    /// Cari skill yang relevan dengan teks (pencocokan trigger/deskripsi).
    async fn find_relevant(&self, text: &str, limit: usize) -> Result<Vec<Skill>>;
}

/// Port: platform messaging gateway.
#[async_trait]
pub trait Platform: Send + Sync {
    fn id(&self) -> &str;
    async fn send_message(&self, target: &str, text: &str) -> Result<()>;
    async fn send_message_html(&self, target: &str, html: &str) -> Result<()> {
        self.send_message(target, html).await
    }
}
