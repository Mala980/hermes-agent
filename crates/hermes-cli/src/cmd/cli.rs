//! Definisi argumen CLI: struct Cli, enum Commands, dan subcommand.
//! Dipindah dari main.rs (refactor batas ukuran file).

use clap::Parser;
use clap::Subcommand;

#[derive(Parser)]
#[command(
    name = "hermes",
    version,
    about = "Hermes Agent — autonomous AI agent (Rust port)"
)]
pub(crate) struct Cli {
    /// Profil yang dipakai (default: profil aktif di config).
    #[arg(long, global = true)]
    pub(crate) profile: Option<String>,
    /// Working directory sesi (default: dari profil).
    #[arg(long, global = true)]
    pub(crate) workdir: Option<String>,
    /// Setujui semua eksekusi tool tanpa bertanya.
    #[arg(long, global = true)]
    pub(crate) yes: bool,
    /// Mode read-only: tolak semua tool berisiko.
    #[arg(long, global = true)]
    pub(crate) readonly: bool,
    /// Verbositas log (-v, -vv).
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub(crate) verbose: u8,
    /// Model untuk sekali jalan ini saja (tanpa menulis config; menang atas profil).
    #[arg(short = 'm', long, global = true)]
    pub(crate) model: Option<String>,
    /// Provider untuk sekali jalan ini saja (tanpa menulis config; menang atas profil).
    #[arg(long, global = true)]
    pub(crate) provider: Option<String>,
    /// Upaya reasoning untuk sekali jalan ini saja: none, minimal, low, medium,
    /// high, xhigh, max, ultra (tanpa menulis config; menang atas profil).
    #[arg(long, global = true)]
    pub(crate) reasoning: Option<String>,

    #[command(subcommand)]
    pub(crate) command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
pub(crate) enum Commands {
    /// Obrolan interaktif dengan agen.
    Chat {
        /// Lanjutkan sesi: id persis, `latest` (sesi terbaru), atau substring
        /// judul (harus cocok tepat satu).
        #[arg(long, short = 'r', alias = "session")]
        resume: Option<String>,
        /// Lanjutkan sesi terakhir (alias untuk `--resume latest`).
        #[arg(long = "continue", short = 'c')]
        continue_last: bool,
        /// Satu prompt, non-interaktif: jawab lalu keluar.
        #[arg(long = "query", short = 'q')]
        query: Option<String>,
        /// Baca prompt sekali-jalan dari file (`-` = stdin). Konflik dengan --query.
        #[arg(long = "query-file", conflicts_with = "query")]
        query_file: Option<String>,
        /// Jalankan tepat satu turn lalu keluar (implisit bila -q/--query-file dipakai).
        #[arg(long)]
        oneshot: bool,
        /// Format output mode sekali-jalan: text atau json.
        #[arg(long, default_value = "text")]
        format: String,
    },
    /// Jalankan satu prompt lalu keluar.
    Run {
        /// Prompt yang dijalankan.
        #[arg(short, long)]
        query: String,
        #[arg(long)]
        session: Option<String>,
        /// Cetak balasan akhir ke stdout (untuk dipakai subproses/delegasi).
        #[arg(long)]
        print: bool,
        /// Format output: text (default) atau json ({"response", "session"})
        /// untuk pemakaian skrip/delegasi.
        #[arg(long, default_value = "text")]
        format: String,
    },
    /// Kelola skills.
    Skills {
        #[command(subcommand)]
        cmd: SkillsCmd,
    },
    /// Kelola memori persisten.
    Memory {
        #[command(subcommand)]
        cmd: MemoryCmd,
    },
    /// Kelola konfigurasi.
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
    /// Kelola cron job.
    Cron {
        #[command(subcommand)]
        cmd: CronCmd,
    },
    /// Jalankan messaging gateway.
    Gateway {
        /// Platform: telegram, discord, slack, whatsapp, matrix, signal, teams, email.
        #[arg(long, default_value = "telegram")]
        platform: String,
        /// Port webhook (untuk whatsapp).
        #[arg(long, default_value_t = 8080)]
        webhook_port: u16,
        /// Nama bot dari config [[bots]] (mode multi-bot).
        #[arg(long)]
        bot: Option<String>,
    },
    /// Info MCP.
    Mcp {
        #[command(subcommand)]
        cmd: McpCmd,
    },
    /// Mode suara: text-to-speech & transkripsi.
    Voice {
        #[command(subcommand)]
        cmd: VoiceCmd,
    },
    /// Jalankan server ACP (Agent Client Protocol) via stdio untuk IDE.
    Acp,
    /// Batch: jalankan banyak prompt dari file JSONL.
    Batch {
        /// File input JSONL: tiap baris {"prompt": "...", "session": "opsional"}.
        input: String,
        /// File output JSONL (default: stdout).
        #[arg(long)]
        out: Option<String>,
    },
    /// Operasi sesi tersimpan.
    ///
    /// Batch 7 (tier 1): nama asli `sessions` tersedia sebagai alias
    /// (asli: `hermes sessions list|show|delete|...`).
    #[command(alias = "sessions")]
    Session {
        #[command(subcommand)]
        cmd: SessionCmd,
    },
    /// Webhook masuk: terima POST /hook dan jawab via agen.
    Webhook {
        #[command(subcommand)]
        cmd: WebhookCmd,
    },
    /// TUI interaktif (pengganti Ink TUI).
    Tui,
    /// Evaluasi kemampuan agen dengan suite kasus uji.
    Eval {
        #[command(subcommand)]
        cmd: EvalCmd,
    },
    /// Lihat/ganti model aktif.
    Model {
        /// Nama model baru (opsional).
        name: Option<String>,
    },
    /// Diagnosa instalasi & konfigurasi.
    Doctor,
    /// Lihat dan filter file log (batch 7 tier 1; mirip `hermes logs` asli).
    Logs {
        /// Nama log: agent (default), errors, gateway, gui, desktop, update,
        /// handoff, mcp, atau `list` untuk daftar file log yang ada.
        #[arg(default_value = "agent")]
        log: String,
        /// Jumlah baris terakhir yang ditampilkan (default 50).
        #[arg(short = 'n', long, default_value_t = 50)]
        lines: usize,
        /// Ikuti file secara real-time (seperti tail -f).
        #[arg(short = 'f', long)]
        follow: bool,
        /// Level minimum: DEBUG, INFO, WARNING, ERROR, CRITICAL.
        #[arg(long)]
        level: Option<String>,
        /// Filter substring session ID.
        #[arg(long)]
        session: Option<String>,
        /// Hanya baris dalam rentang relatif terakhir, mis. 30m, 1h, 2d.
        #[arg(long)]
        since: Option<String>,
    },
    /// Status ringkas komponen Hermes (batch 7 tier 1; mirip `hermes status` asli).
    Status {
        /// Tampilkan semua bagian (bukan ringkasan satu layar).
        #[arg(long, alias = "all")]
        full: bool,
        /// Pemeriksaan mendalam (implisit --full; bisa lebih lama).
        #[arg(long)]
        deep: bool,
    },
    /// Daftar & kelola tool agen (batch 7 tier 1; mirip `tools list` asli).
    Tools {
        #[command(subcommand)]
        cmd: ToolsCmd,
    },
    /// Daftar provider LLM yang dikonfigurasi (batch 8 tier 2).
    Providers {
        #[command(subcommand)]
        cmd: ProvidersCmd,
    },
    /// Wizard pengaturan awal.
    Setup,
    /// Tampilkan versi.
    Version,
}

#[derive(Subcommand, Debug)]
pub(crate) enum SkillsCmd {
    List,
    Read {
        name: String,
    },
    Create {
        name: String,
        description: String,
        body: String,
    },
    Delete {
        name: String,
    },
    /// Tampilkan metadata skill: deskripsi, path, trigger, ukuran
    /// (batch 8 tier 2; mirip `skills inspect` asli).
    Inspect {
        name: String,
    },
    /// Skills hub: cari & pasang skill dari sumber eksternal.
    Hub {
        #[command(subcommand)]
        cmd: HubCmd,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum HubCmd {
    /// Cari skill di GitHub (pencarian repositori).
    Search {
        /// Kueri pencarian.
        query: String,
        #[arg(long, default_value_t = 5)]
        limit: u8,
    },
    /// Pasang skill dari sumber: github:owner/repo[/path], URL git, atau path lokal.
    Install {
        /// Sumber skill.
        source: String,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum EvalCmd {
    /// Jalankan suite evaluasi (JSONL) dan laporkan skor.
    Run {
        /// File suite JSONL, atau `builtin:tools` untuk suite bawaan.
        suite: String,
        /// File output JSONL hasil (opsional).
        #[arg(long)]
        out: Option<String>,
        /// Ambang kelulusan dalam persen (0–100). Menimpa `[evals].threshold`
        /// di config (default 100). Skor di bawah ambang = exit non-nol.
        #[arg(long)]
        threshold: Option<f64>,
    },
    /// Daftar suite evaluasi bawaan beserta jumlah kasusnya (batch 8 tier 2).
    List,
}

#[derive(Subcommand, Debug)]
pub(crate) enum MemoryCmd {
    List {
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Target store: 'memory' (catatan agen, default) atau 'user' (profil pengguna).
        #[arg(long, default_value = "memory")]
        target: String,
    },
    Recall {
        query: String,
        #[arg(long, default_value_t = 8)]
        limit: usize,
        /// Target store: 'memory' (catatan agen, default) atau 'user' (profil pengguna).
        #[arg(long, default_value = "memory")]
        target: String,
    },
    Forget {
        id_or_query: String,
        /// Target store: 'memory' (catatan agen, default) atau 'user' (profil pengguna).
        #[arg(long, default_value = "memory")]
        target: String,
    },
    /// Tampilkan status backend memori: path database, jumlah entri
    /// memori, dan jumlah sesi tersimpan (batch 8 tier 2;
    /// mirip `memory status` asli).
    Status,
}

#[derive(Subcommand, Debug)]
pub(crate) enum ConfigCmd {
    Show,
    Path,
    Set {
        key: String,
        value: String,
    },
    /// Cetak nilai konfigurasi yang ter-resolve (batch 7 tier 1;
    /// mirip `config get <key>` asli). Tanpa kunci: daftar semua kunci profil.
    Get {
        /// Kunci konfigurasi (mis. model, provider, working_dir, temperature,
        /// max_iterations, reasoning, profile).
        key: Option<String>,
        /// Cetak sebagai JSON.
        #[arg(long)]
        json: bool,
    },
    /// Hapus nilai konfigurasi (kembalikan ke default; batch 7 tier 1;
    /// mirip `config unset <key>` asli).
    Unset {
        key: String,
    },
    /// Periksa kesehatan konfigurasi profil aktif: model, provider,
    /// protokol, working_dir, dan nilai numerik (batch 8 tier 2;
    /// mirip `config check` asli).
    Check,
}

/// Parser clap untuk `--overlap`: terima `skip`/`skip-if-running`/`allow`,
/// tolak nilai lain saat parsing dengan pesan yang jelas.
pub(crate) fn parse_overlap(s: &str) -> Result<hermes_cron::OverlapPolicy, String> {
    s.parse::<hermes_cron::OverlapPolicy>()
        .map_err(|e: hermes_core::HermesError| e.to_string())
}

/// Parser clap untuk `--timezone`: nama zona waktu harus IANA yang dikenal.
pub(crate) fn parse_timezone(s: &str) -> Result<String, String> {
    s.parse::<chrono_tz::Tz>()
        .map(|_| s.to_string())
        .map_err(|_| {
            format!(
            "nama zona waktu IANA tidak valid: `{s}` (contoh: Asia/Makassar, Asia/Jakarta, UTC)"
        )
        })
}

#[derive(Subcommand, Debug)]
pub(crate) enum CronCmd {
    List,
    Add {
        name: String,
        /// Jadwal: cron 6 kolom ("0 30 9 * * *"), cron 5 kolom ("30 9 * * *",
        /// detik=0), interval ("every 15m"), sekali jalan ("at
        /// 2026-10-22T09:00:00+08:00", otomatis dihapus setelah berjalan),
        /// atau kata kunci: hourly, daily, weekly, midnight, noon.
        schedule: String,
        prompt: String,
        #[arg(long, default_value = "telegram")]
        platform: String,
        #[arg(long)]
        target: String,
        /// Zona waktu IANA untuk jadwal (mis. "Asia/Makassar"). Default: zona waktu lokal sistem.
        #[arg(long, value_parser = parse_timezone)]
        timezone: Option<String>,
        /// Kebijakan bila tick tiba selagi run sebelumnya belum selesai: "skip" (default) atau "allow".
        #[arg(long, value_parser = parse_overlap)]
        overlap: Option<hermes_cron::OverlapPolicy>,
    },
    Remove {
        name: String,
    },
    Toggle {
        name: String,
        #[arg(long)]
        off: bool,
    },
    /// Jalankan scheduler (foreground).
    Serve,
}

#[derive(Subcommand, Debug)]
pub(crate) enum McpCmd {
    List,
}

#[derive(Subcommand, Debug)]
pub(crate) enum SessionCmd {
    /// Export sesi tersimpan sebagai trajectory (riset/eval) atau JSONL.
    Export {
        /// ID sesi.
        session: String,
        /// Format: trajectory (default) atau jsonl.
        #[arg(long, default_value = "trajectory")]
        format: String,
        /// File output (default: stdout).
        #[arg(long)]
        out: Option<String>,
    },
    /// Daftar sesi terbaru (batch 7 tier 1; mirip `sessions list` asli).
    List {
        /// Jumlah maksimum sesi (default 20).
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Filter profil/sumber (cli, telegram, discord, ...).
        #[arg(long)]
        source: Option<String>,
    },
    /// Tampilkan ringkasan dan riwayat pesan satu sesi (batch 7 tier 1).
    Show {
        /// ID sesi.
        session: String,
    },
    /// Hapus satu sesi (batch 7 tier 1; mirip `sessions delete` asli).
    Delete {
        /// ID sesi yang dihapus.
        session_id: String,
        /// Lewati konfirmasi.
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Impor sesi dari file ekspor (`session export`) kembali ke store
    /// (batch 8 tier 2; melengkapi round-trip ekspor/impor sesi).
    Import {
        /// File hasil `session export` (jsonl atau trajectory).
        file: String,
        /// Format file: jsonl (default) atau trajectory.
        #[arg(long, default_value = "jsonl")]
        format: String,
        /// Paksa ID sesi tertentu (default: ID baru agar tak bentrok).
        #[arg(long)]
        id: Option<String>,
        /// Judul sesi hasil impor (default: dari file, atau "Imported").
        #[arg(long)]
        title: Option<String>,
    },
    /// Ganti judul sesi (batch 8 tier 2; mirip `sessions rename` asli).
    Rename {
        /// ID sesi, `latest`, atau substring judul.
        session: String,
        /// Judul baru (boleh beberapa kata).
        title: Vec<String>,
    },
    /// Statistik store sesi: total sesi/pesan + rincian per profil
    /// (batch 8 tier 2; mirip `sessions stats` asli).
    Stats,
    /// Hapus sesi lama (batch 8 tier 2; mirip `sessions prune` asli:
    /// tanpa filter defaultnya 90 hari).
    Prune {
        /// Hapus sesi yang diperbarui sebelum usia ini: durasi seperti
        /// `5h`/`2d`/`1w`, atau angka polos = hari (default: 90).
        #[arg(long, default_value = "90")]
        older_than: String,
        /// Tampilkan yang akan dihapus tanpa menghapus.
        #[arg(long)]
        dry_run: bool,
        /// Lewati konfirmasi.
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum WebhookCmd {
    /// Jalankan server webhook masuk (POST /hook).
    Serve {
        /// Port server.
        #[arg(long, default_value_t = 8080)]
        port: u16,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum VoiceCmd {
    /// Bacakan teks menjadi file audio.
    Say {
        /// Teks yang dibacakan.
        text: String,
        /// File output (default: speech-<timestamp>.mp3).
        #[arg(long)]
        out: Option<String>,
        /// Suara: alloy, echo, fable, onyx, nova, shimmer.
        #[arg(long, default_value = "alloy")]
        voice: String,
    },
    /// Transkripsi file audio menjadi teks.
    Listen {
        /// Path file audio.
        file: String,
        /// Kode bahasa (opsional), mis. id.
        #[arg(long)]
        language: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum ToolsCmd {
    /// Daftar tool agen yang terdaftar (batch 7 tier 1; mirip `tools list` asli).
    List,
    /// Nonaktifkan skill bernama `<name>` (ditulis ke [skills].disabled_skills
    /// di config; tool inti agen selalu aktif).
    Disable {
        /// Nama skill yang dinonaktifkan.
        name: String,
    },
    /// Aktifkan kembali skill bernama `<name>` (dihapus dari disabled_skills).
    Enable {
        /// Nama skill yang diaktifkan.
        name: String,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum ProvidersCmd {
    /// Daftar provider yang dikonfigurasi: nama, protokol, base URL, model
    /// default, dan penanda provider aktif profil ini (batch 8 tier 2).
    List,
}
