//! TUI interaktif (ratatui) — padanan Ink TUI dari Hermes asli.
//!
//! Gaya tampilan mengikuti TUI asli (banner logo ASCII, gradasi warna,
//! penanda aktivitas tool ●/✓) dan pola Reasonix: header satu baris,
//! feed aktivitas ringkas, status bar bawah yang informatif.
//! Tombol: Enter kirim · Esc keluar · PgUp/PgDn scroll · Ctrl+C keluar ·
//! F1 bantuan · F2 tema · F3 ganti sesi · F4 proses background.
use crate::app::App;
use crate::cmd::chat::AgentFactory;
use crossterm::{
    event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::StreamExt;
use hermes_core::{AgentEvent, DiffStat, Session};
use hermes_tools::processes::BgProcessSummary;
use pulldown_cmark::{CodeBlockKind, Event as MdEvent, Options, Parser, Tag, TagEnd};
use ratatui::{
    backend::Backend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Terminal,
};
use std::collections::HashMap;
use std::io::Stdout;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// Logo block-letter "HERMES AGENT" — verbatim dari `ui-tui/src/banner.ts`
/// repo asli (MIT). Dirender hanya bila terminal cukup lebar.
const LOGO_ART: &[&str] = &[
    "██╗  ██╗███████╗██████╗ ███╗   ███╗███████╗███████╗       █████╗  ██████╗ ███████╗███╗   ██╗████████╗",
    "██║  ██║██╔════╝██╔══██╗████╗ ████║██╔════╝██╔════╝      ██╔══██╗██╔════╝ ██╔════╝████╗  ██║╚══██╔══╝",
    "███████║█████╗  ██████╔╝██╔████╔██║█████╗  ███████╗█████╗███████║██║  ███╗█████╗  ██╔██╗ ██║   ██║   ",
    "██╔══██║██╔══╝  ██╔══██╗██║╚██╔╝██║██╔══╝  ╚════██║╚════╝██╔══██║██║   ██║██╔══╝  ██║╚██╗██║   ██║   ",
    "██║  ██║███████╗██║  ██║██║ ╚═╝ ██║███████╗███████║      ██║  ██║╚██████╔╝███████╗██║ ╚████║   ██║   ",
    "╚═╝  ╚═╝╚══════╝╚═╝  ╚═╝╚═╝     ╚═╝╚══════╝╚══════╝      ╚═╝  ╚═╝ ╚═════╝ ╚══════╝╚═╝  ╚═══╝   ╚═╝   ",
];
/// Palet warna TUI: gelap (default), terang (terminal berlatar terang),
/// dan kontras tinggi. Urutan = urutan toggle F2.
#[derive(Clone, Copy)]
struct Theme {
    name: &'static str,
    user: Color,
    assistant: Color,
    system: Color,
    accent: Color,
    tool_run: Color,
    tool_ok: Color,
    tool_err: Color,
    code_bg: Color,
    code_fg: Color,
    border: Color,
    header_glyph: Color,
}

const THEMES: &[Theme] = &[
    Theme {
        name: "gelap",
        user: Color::Cyan,
        assistant: Color::Green,
        system: Color::DarkGray,
        accent: Color::Yellow,
        tool_run: Color::Yellow,
        tool_ok: Color::Green,
        tool_err: Color::Red,
        code_bg: Color::Rgb(26, 26, 26),
        code_fg: Color::White,
        border: Color::DarkGray,
        header_glyph: Color::Cyan,
    },
    Theme {
        name: "terang",
        user: Color::Rgb(0, 0, 180),
        assistant: Color::Rgb(0, 130, 0),
        system: Color::Rgb(120, 120, 120),
        accent: Color::Rgb(150, 0, 150),
        tool_run: Color::Rgb(150, 100, 0),
        tool_ok: Color::Rgb(0, 130, 0),
        tool_err: Color::Rgb(180, 0, 0),
        code_bg: Color::Rgb(240, 240, 240),
        code_fg: Color::Rgb(30, 30, 30),
        border: Color::Rgb(150, 150, 150),
        header_glyph: Color::Rgb(0, 0, 180),
    },
    Theme {
        name: "kontras",
        user: Color::White,
        assistant: Color::LightYellow,
        system: Color::Gray,
        accent: Color::LightMagenta,
        tool_run: Color::LightYellow,
        tool_ok: Color::LightGreen,
        tool_err: Color::LightRed,
        code_bg: Color::Black,
        code_fg: Color::White,
        border: Color::White,
        header_glyph: Color::LightCyan,
    },
];

/// Petakan nama tema tersimpan (config `[tui]`) ke indeks THEMES;
/// nama tak dikenal/kosong → tema default (gelap, indeks 0).
fn theme_idx_for_name(name: &str) -> usize {
    THEMES.iter().position(|t| t.name == name).unwrap_or(0)
}

/// Gradasi logo asli dipetakan ke tema: [user, user, assistant, assistant,
/// border, border].
fn logo_colors(t: &Theme) -> [Color; 6] {
    [t.user, t.user, t.assistant, t.assistant, t.border, t.border]
}

/// Gradasi caduceus asli dipetakan ke tema.
fn caduc_colors(t: &Theme) -> [Color; 15] {
    [
        t.border,
        t.border,
        t.assistant,
        t.assistant,
        t.user,
        t.user,
        t.assistant,
        t.assistant,
        t.border,
        t.border,
        t.system,
        t.system,
        t.system,
        t.system,
        t.system,
    ]
}

/// Caduceus — verbatim dari `ui-tui/src/banner.ts` repo asli (MIT).
const CADUCEUS_ART: &[&str] = &[
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢀⣀⡀⠀⣀⣀⠀⢀⣀⡀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⢀⣠⣴⣾⣿⣿⣇⠸⣿⣿⠇⣸⣿⣿⣷⣦⣄⡀⠀⠀⠀⠀⠀⠀",
    "⠀⢀⣠⣴⣶⠿⠋⣩⡿⣿⡿⠻⣿⡇⢠⡄⢸⣿⠟⢿⣿⢿⣍⠙⠿⣶⣦⣄⡀⠀",
    "⠀⠀⠉⠉⠁⠶⠟⠋⠀⠉⠀⢀⣈⣁⡈⢁⣈⣁⡀⠀⠉⠀⠙⠻⠶⠈⠉⠉⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⣴⣿⡿⠛⢁⡈⠛⢿⣿⣦⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠿⣿⣦⣤⣈⠁⢠⣴⣿⠿⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠈⠉⠻⢿⣿⣦⡉⠁⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠘⢷⣦⣈⠛⠃⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢠⣴⠦⠈⠙⠿⣦⡄⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠸⣿⣤⡈⠁⢤⣿⠇⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠉⠛⠷⠄⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢀⣀⠑⢶⣄⡀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⣿⠁⢰⡆⠈⡿⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠈⠳⠈⣡⠞⠁⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠈⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
];

/// Lebar minimum terminal agar banner logo penuh (100 kolom + margin) tampil.
const WIDE_MIN: u16 = 104;

/// Frame spinner braille untuk indikator "sedang berpikir".
const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

struct TuiMsg {
    role: MsgRole,
    text: String,
    /// Hanya tampil di terminal lebar (>= WIDE_MIN): untuk banner splash.
    wide_only: bool,
    /// Override warna seluruh baris (banner splash bergradasi).
    fg: Option<Color>,
    /// Baris aktivitas tool (●/✓/✗), bila ada.
    tool: Option<ToolLine>,
}

#[derive(PartialEq, Clone, Copy, Debug)]
enum MsgRole {
    User,
    Assistant,
    System,
}

struct ToolLine {
    name: String,
    summary: String,
    running: bool,
    ok: bool,
    elapsed_ms: u64,
    /// Diff-stat dari hasil tool (Some hanya bila hasil membawa unified
    /// diff, mis. edit_file); dirender sebagai `+a −r` di baris ✓.
    diff_stat: Option<DiffStat>,
}

struct TuiState {
    messages: Vec<TuiMsg>,
    input: String,
    scroll: u16,
    thinking: bool,
    /// Catatan transien untuk status bar (mis. "menjalankan exec").
    note: String,
    stick_bottom: bool,
    started: Instant,
    spin_frame: usize,
    model: String,
    cwd: String,
    /// Waktu mulai tiap tool call yang sedang berjalan (untuk durasi).
    tools_running: HashMap<String, Instant>,
    /// Indeks tema aktif di THEMES.
    theme_idx: usize,
    /// Overlay bantuan sedang terbuka.
    show_help: bool,
    /// Panel daftar proses background (F4) sedang terbuka.
    show_agents: bool,
    /// Snapshot terakhir registri proses background; di-refresh tiap tick
    /// hanya saat panel terbuka.
    bg_procs: Vec<BgProcessSummary>,
    /// Jumlah turn pengguna yang dikirim sesi ini (goal/loop bar).
    turn_count: u32,
    /// Iterasi agent loop terakhir pada turn berjalan.
    iteration: u32,
    /// Teks goal sesi ini, disetel via `/goal`; None = belum disetel
    /// (goal bar menampilkan placeholder "—"). Agent/session tidak punya
    /// konsep goal, jadi ini murni state TUI.
    goal: Option<String>,
    /// Overlay ganti sesi (F3) sedang terbuka.
    show_sessions: bool,
    /// Daftar sesi dari session store, terbaru dulu (urutan list_sessions).
    sessions: Vec<Session>,
    /// Indeks sesi yang disorot di overlay.
    session_idx: usize,
    /// Id sesi yang sedang aktif (penanda ● di daftar).
    session_id: String,
}

impl TuiState {
    fn new(model: String, cwd: String) -> Self {
        Self {
            messages: Vec::new(),
            input: String::new(),
            scroll: 0,
            thinking: false,
            note: String::new(),
            stick_bottom: true,
            started: Instant::now(),
            spin_frame: 0,
            model,
            cwd,
            tools_running: HashMap::new(),
            theme_idx: 0,
            show_help: false,
            show_agents: false,
            bg_procs: Vec::new(),
            turn_count: 0,
            iteration: 0,
            goal: None,
            show_sessions: false,
            sessions: Vec::new(),
            session_idx: 0,
            session_id: String::new(),
        }
    }

    fn theme(&self) -> Theme {
        THEMES[self.theme_idx % THEMES.len()]
    }

    fn cycle_theme(&mut self) {
        self.theme_idx = (self.theme_idx + 1) % THEMES.len();
    }

    /// Setel tema dari nama (dipakai saat startup dari config tersimpan).
    fn set_theme_by_name(&mut self, name: &str) {
        self.theme_idx = theme_idx_for_name(name);
    }

    /// Buka overlay ganti sesi dengan daftar dari session store.
    fn open_session_overlay(&mut self, sessions: Vec<Session>) {
        self.sessions = sessions;
        self.session_idx = 0;
        self.show_sessions = true;
    }

    fn close_session_overlay(&mut self) {
        self.show_sessions = false;
        self.session_idx = 0;
    }

    /// Navigasi daftar sesi; melingkar di kedua ujung.
    fn session_nav(&mut self, dir: i32) {
        let n = self.sessions.len();
        if n == 0 {
            return;
        }
        let next = self.session_idx as i32 + dir;
        self.session_idx = next.rem_euclid(n as i32) as usize;
    }

    fn session_select(&self) -> Option<&Session> {
        self.sessions.get(self.session_idx)
    }

    /// Prioritas Esc: tutup overlay sesi → tutup bantuan → tutup panel
    /// proses → bersihkan input → keluar. Mengembalikan true bila TUI
    /// harus keluar.
    fn on_esc(&mut self) -> bool {
        if self.show_sessions {
            self.close_session_overlay();
            false
        } else if self.show_help {
            self.show_help = false;
            false
        } else if self.show_agents {
            self.show_agents = false;
            false
        } else if !self.input.is_empty() {
            self.input.clear();
            false
        } else {
            true
        }
    }

    /// Toggle panel proses background (F4). Saat dibuka, overlay lain
    /// ditutup agar tidak bertumpuk. Refresh snapshot dilakukan pemanggil
    /// karena butuh await.
    fn toggle_agents_panel(&mut self) {
        if self.show_agents {
            self.show_agents = false;
        } else {
            self.show_help = false;
            self.close_session_overlay();
            self.show_agents = true;
        }
    }

    /// Terapkan hasil ganti sesi ke state tampilan: tutup overlay,
    /// kosongkan feed lama (riwayat asli tetap di session store), umumkan
    /// sesi baru.
    fn apply_session_switch(&mut self, session: &Session) {
        self.close_session_overlay();
        self.sessions.clear();
        self.messages.clear();
        self.scroll = 0;
        self.stick_bottom = true;
        self.note.clear();
        self.thinking = false;
        // Turn/iterasi milik feed sesi lama; goal (disetel via /goal)
        // dipertahankan sebagai intent pengguna lintas sesi.
        self.turn_count = 0;
        self.iteration = 0;
        self.session_id = session.id.clone();
        self.push(
            MsgRole::System,
            format!(
                "beralih ke sesi “{}” — {} pesan, terakhir aktif {}",
                session.title,
                session.messages.len(),
                fmt_ago(session.updated_at)
            ),
        );
    }

    fn push(&mut self, role: MsgRole, text: String) {
        // Gabungkan delta teks asisten yang berurutan.
        if role == MsgRole::Assistant {
            if let Some(last) = self.messages.last_mut() {
                if last.role == MsgRole::Assistant && self.thinking {
                    last.text.push_str(&text);
                    return;
                }
            }
        }
        self.messages.push(TuiMsg {
            role,
            text,
            wide_only: false,
            fg: None,
            tool: None,
        });
    }

    fn push_art(&mut self, lines: &[&str], colors: &[Color]) {
        for (i, line) in lines.iter().enumerate() {
            self.messages.push(TuiMsg {
                role: MsgRole::System,
                text: line.to_string(),
                wide_only: true,
                fg: Some(colors[i % colors.len()]),
                tool: None,
            });
        }
    }

    fn last_assistant_mut(&mut self) -> Option<&mut TuiMsg> {
        self.messages
            .iter_mut()
            .rev()
            .find(|m| m.role == MsgRole::Assistant)
    }
}

/// Splash awal sesi: logo + caduceus dari TUI asli + baris petunjuk.
/// Warna mengikuti tema aktif (gradasi dipetakan di logo_colors/caduc_colors).
fn push_splash(state: &mut TuiState) {
    let theme = state.theme();
    state.push_art(LOGO_ART, &logo_colors(&theme));
    state.messages.push(TuiMsg {
        role: MsgRole::System,
        text: String::new(),
        wide_only: true,
        fg: None,
        tool: None,
    });
    state.push_art(CADUCEUS_ART, &caduc_colors(&theme));
    state.push(
        MsgRole::System,
        "Ketik pesan lalu tekan Enter · /quit keluar · /clear bersihkan riwayat".to_string(),
    );
}

/// Ringkasan singkat argumen tool untuk baris aktivitas.
fn tool_summary(args: &serde_json::Value) -> String {
    fn trunc(s: &str, n: usize) -> String {
        s.chars().take(n).collect()
    }
    match args {
        serde_json::Value::String(s) => trunc(s, 60),
        serde_json::Value::Object(map) => {
            for key in ["path", "command", "query", "prompt", "text", "file", "url"] {
                if let Some(v) = map.get(key).and_then(|v| v.as_str()) {
                    if !v.trim().is_empty() {
                        return trunc(v.trim(), 60);
                    }
                }
            }
            trunc(&serde_json::to_string(args).unwrap_or_default(), 60)
        }
        _ => trunc(&serde_json::to_string(args).unwrap_or_default(), 60),
    }
}

fn fmt_dur_ms(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

fn fmt_session(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}j {}m", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        format!("{}m {}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}

/// Status gaya inline saat memindai markdown.
#[derive(Default)]
struct MdInline {
    bold: u32,
    italic: u32,
    code: u32,
    link: u32,
    strike: u32,
    heading: bool,
}

impl MdInline {
    fn style(&self, theme: &Theme) -> Style {
        let mut s = Style::default();
        if self.code > 0 || self.heading {
            s = s.fg(theme.accent).add_modifier(Modifier::BOLD);
        } else if self.link > 0 {
            s = s.fg(theme.accent).add_modifier(Modifier::UNDERLINED);
        }
        if self.bold > 0 {
            s = s.add_modifier(Modifier::BOLD);
        }
        if self.italic > 0 {
            s = s.add_modifier(Modifier::ITALIC);
        }
        if self.strike > 0 {
            s = s.add_modifier(Modifier::CROSSED_OUT);
        }
        s
    }
}

/// Ubah teks markdown menjadi baris-baris ratatui bergaya:
/// heading tebal aksen, **bold**/*italic*, `kode` aksen, blok kode berpagar
/// dengan label bahasa + latar halus, dan daftar berpoin.
/// Semua String di-owned agar hasilnya 'static.
fn markdown_lines(md: &str, theme: &Theme) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut st = MdInline::default();
    let mut lists: Vec<Option<u64>> = Vec::new();
    let mut in_code_block = false;
    let mut code_lang: String;
    let mut code_buf = String::new();
    let mut quote: u32 = 0;

    // Dorong baris yang sedang dibangun ke hasil (tanpa baris kosong ganda).
    macro_rules! flush {
        () => {
            if !cur.is_empty() {
                let mut spans = std::mem::take(&mut cur);
                if quote > 0 {
                    let mut q = vec![Span::styled(
                        "▎ ".repeat(quote as usize),
                        Style::default().fg(theme.system),
                    )];
                    q.append(&mut spans);
                    spans = q;
                }
                lines.push(Line::from(spans));
            }
        };
    }
    // Pemisah baris kosong (hindari dua berurutan).
    macro_rules! gap {
        () => {
            if lines.last().map(|l| l.width() != 0).unwrap_or(false) {
                lines.push(Line::from(""));
            }
        };
    }

    let parser = Parser::new_ext(md, Options::empty());
    for ev in parser {
        match ev {
            MdEvent::Start(Tag::Heading { .. }) => {
                gap!();
                st.heading = true;
            }
            MdEvent::End(TagEnd::Heading(_)) => {
                flush!();
                gap!();
                st.heading = false;
            }
            MdEvent::Start(Tag::Paragraph) => {}
            MdEvent::End(TagEnd::Paragraph) => {
                flush!();
                gap!();
            }
            MdEvent::Start(Tag::CodeBlock(kind)) => {
                in_code_block = true;
                code_lang = match kind {
                    CodeBlockKind::Fenced(lang) => lang.into_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                let label = if code_lang.is_empty() {
                    "kode".to_string()
                } else {
                    code_lang.clone()
                };
                lines.push(Line::from(Span::styled(
                    format!("┌─ {label}"),
                    Style::default().fg(theme.border),
                )));
            }
            MdEvent::End(TagEnd::CodeBlock) => {
                in_code_block = false;
                let bg = Style::default().fg(theme.code_fg).bg(theme.code_bg);
                let mut any = false;
                for l in code_buf.lines() {
                    lines.push(Line::from(Span::styled(l.to_string(), bg)));
                    any = true;
                }
                if !any {
                    lines.push(Line::from(Span::styled(String::new(), bg)));
                }
                code_buf.clear();
                gap!();
            }
            MdEvent::Start(Tag::List(n)) => lists.push(n),
            MdEvent::End(TagEnd::List(_)) => {
                lists.pop();
                gap!();
            }
            MdEvent::Start(Tag::Item) => {
                let depth = lists.len().saturating_sub(1);
                let indent = "  ".repeat(depth);
                let bullet = match lists.last_mut() {
                    Some(slot @ Some(_)) => {
                        let n = (*slot).unwrap_or(1);
                        let s = format!("{n}. ");
                        *slot = Some(n + 1);
                        s
                    }
                    _ => "• ".to_string(),
                };
                cur.push(Span::styled(
                    format!("{indent}{bullet}"),
                    Style::default().fg(theme.system),
                ));
            }
            MdEvent::End(TagEnd::Item) => flush!(),
            MdEvent::Start(Tag::Emphasis) => st.italic += 1,
            MdEvent::End(TagEnd::Emphasis) => st.italic = st.italic.saturating_sub(1),
            MdEvent::Start(Tag::Strong) => st.bold += 1,
            MdEvent::End(TagEnd::Strong) => st.bold = st.bold.saturating_sub(1),
            MdEvent::Start(Tag::Strikethrough) => st.strike += 1,
            MdEvent::End(TagEnd::Strikethrough) => st.strike = st.strike.saturating_sub(1),
            MdEvent::Start(Tag::Link { .. }) => st.link += 1,
            MdEvent::End(TagEnd::Link) => st.link = st.link.saturating_sub(1),
            MdEvent::Start(Tag::BlockQuote(_)) => quote += 1,
            MdEvent::End(TagEnd::BlockQuote(_)) => {
                quote = quote.saturating_sub(1);
                flush!();
                gap!();
            }
            MdEvent::Text(t) => {
                if in_code_block {
                    code_buf.push_str(&t);
                } else {
                    cur.push(Span::styled(t.into_string(), st.style(theme)));
                }
            }
            MdEvent::Code(t) => cur.push(Span::styled(
                t.into_string(),
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            )),
            MdEvent::Html(t) | MdEvent::InlineHtml(t) => cur.push(Span::styled(
                t.into_string(),
                Style::default()
                    .fg(theme.system)
                    .add_modifier(Modifier::ITALIC),
            )),
            MdEvent::SoftBreak | MdEvent::HardBreak => flush!(),
            MdEvent::Rule => {
                gap!();
                lines.push(Line::from(Span::styled(
                    "─".repeat(24),
                    Style::default().fg(theme.border),
                )));
                gap!();
            }
            _ => {}
        }
    }
    flush!();
    while lines.last().map(|l| l.width() == 0).unwrap_or(false) {
        lines.pop();
    }
    if lines.is_empty() {
        lines.push(Line::from(""));
    }
    lines
}

pub async fn run_tui(app: &mut App) -> anyhow::Result<()> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_loop(app, &mut terminal).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

fn short_cwd() -> String {
    match std::env::current_dir() {
        Ok(p) => {
            let s = p.display().to_string();
            if let Ok(home) = std::env::var("HOME") {
                if let Some(rest) = s.strip_prefix(&home) {
                    return format!("~{rest}");
                }
            }
            s
        }
        Err(_) => String::new(),
    }
}

/// Simpan tema aktif ke config.toml agar bertahan antar-restart.
///
/// Ditulis lewat `config_raw` (bukan config efektif) supaya nilai hasil
/// ekspansi env (`${VAR}`) tidak "terbakar" permanen ke disk — konvensi
/// yang sama dipakai `save_config`. Kegagalan tulis hanya dilaporkan
/// sebagai pesan sistem, tidak fatal.
fn persist_theme(app: &mut App, state: &mut TuiState) {
    let name = state.theme().name.to_string();
    app.config.tui.theme = Some(name.clone());
    app.config_raw.tui.theme = Some(name);
    if let Err(e) = crate::app::save_config(&app.config_raw, &app.hermes_home) {
        state.push(
            MsgRole::System,
            format!("gagal menyimpan tema ke config: {e}"),
        );
    }
}

async fn run_loop(
    app: &mut App,
    terminal: &mut Terminal<ratatui::backend::CrosstermBackend<Stdout>>,
) -> anyhow::Result<()> {
    let factory = AgentFactory::from_app(app);
    let (event_tx, mut event_rx) = mpsc::channel::<AgentEvent>(512);
    let session = Session::new(app.profile_name.clone());
    let session_id = session.id.clone();
    let mut agent_opt = Some(factory.build_with_channel(session, event_tx));
    let (done_tx, mut done_rx) = mpsc::channel::<(hermes_core::Agent, anyhow::Result<String>)>(4);

    let mut state = TuiState::new(app.provider.default_model().to_string(), short_cwd());
    // Pulihkan tema dari config (disimpan tiap toggle F2).
    state.set_theme_by_name(app.config.tui.theme.as_deref().unwrap_or("gelap"));
    state.session_id = session_id;
    push_splash(&mut state);

    let mut events = EventStream::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(120));

    loop {
        draw(terminal, &state)?;

        tokio::select! {
            maybe_ev = events.next() => {
                let Some(Ok(ev)) = maybe_ev else { continue };
                if let Event::Key(key) = ev {
                    if key.kind != KeyEventKind::Press { continue }
                    // Overlay sesi terbuka: navigasi daftar dulu; Ctrl+C
                    // tetap keluar. Tombol lain tidak menyentuh input.
                    if state.show_sessions {
                        if key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL)
                        {
                            break;
                        }
                        session_key(&factory, app, &mut agent_opt, &mut event_rx, &mut state, key)
                            .await;
                        continue;
                    }
                    match key.code {
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                        KeyCode::Esc => {
                            if state.on_esc() {
                                break;
                            }
                        }
                        KeyCode::F(1) => {
                            state.show_help = true;
                        }
                        KeyCode::F(2) => {
                            // Toggle tema + simpan ke config agar bertahan
                            // antar-restart.
                            state.cycle_theme();
                            persist_theme(app, &mut state);
                        }
                        KeyCode::F(3) => {
                            // Overlay ganti sesi: muat ulang dari session
                            // store (terbaru dulu), tutup overlay lain agar
                            // tidak bertumpuk.
                            if state.show_sessions {
                                state.close_session_overlay();
                            } else {
                                state.show_help = false;
                                state.show_agents = false;
                                match app
                                    .memory
                                    .list_sessions(&app.profile_name, SESSION_LIST_LIMIT)
                                    .await
                                {
                                    Ok(list) => state.open_session_overlay(list),
                                    Err(e) => state.push(
                                        MsgRole::System,
                                        format!("gagal memuat daftar sesi: {e}"),
                                    ),
                                }
                            }
                        }
                        KeyCode::F(4) => {
                            // Panel proses background: tutup overlay lain
                            // agar tidak bertumpuk; refresh langsung agar
                            // langsung terisi walau tick berikutnya belum tiba.
                            state.toggle_agents_panel();
                            if state.show_agents {
                                state.bg_procs =
                                    hermes_tools::processes::bg_processes_snapshot().await;
                            }
                        }
                        KeyCode::Char('?') if state.input.is_empty() => {
                            state.show_help = true;
                        }
                        KeyCode::Enter => {
                            let text = state.input.trim().to_string();
                            if text.is_empty() || state.thinking { continue }
                            if text == "/quit" || text == "/exit" { break }
                            if text == "/clear" {
                                state.messages.clear();
                                state.input.clear();
                                continue;
                            }
                            if handle_goal_command(&mut state, &text) {
                                state.input.clear();
                                continue;
                            }
                            state.push(MsgRole::User, text.clone());
                            state.input.clear();
                            state.thinking = true;
                            state.turn_count += 1;
                            state.iteration = 0;
                            state.note = "berpikir...".to_string();
                            state.stick_bottom = true;
                            state.scroll = 0;
                            if let Some(agent) = agent_opt.take() {
                                let done_tx = done_tx.clone();
                                tokio::spawn(async move {
                                    let mut agent = agent;
                                    let res = agent.run_turn(&text).await.map_err(|e| anyhow::anyhow!(e.to_string()));
                                    let _ = done_tx.send((agent, res)).await;
                                });
                            }
                        }
                        KeyCode::Char(c) => { state.input.push(c); }
                        KeyCode::Backspace => { state.input.pop(); }
                        KeyCode::PageUp => {
                            state.stick_bottom = false;
                            state.scroll = state.scroll.saturating_add(10);
                        }
                        KeyCode::PageDown => {
                            state.scroll = state.scroll.saturating_sub(10);
                            if state.scroll == 0 { state.stick_bottom = true; }
                        }
                        _ => {}
                    }
                }
            }
            Some(ev) = event_rx.recv() => {
                handle_agent_event(&mut state, ev);
            }
            Some((agent, res)) = done_rx.recv() => {
                agent_opt = Some(agent);
                state.thinking = false;
                match res {
                    Ok(reply) => {
                        if reply.trim().is_empty() {
                            // Balasan sudah di-stream via TextDelta.
                        }
                        state.note.clear();
                    }
                    Err(e) => {
                        state.push(MsgRole::System, format!("Error: {e}"));
                        state.note = "error".to_string();
                    }
                }
            }
            _ = ticker.tick() => {
                // Majukan frame spinner selagi agent bekerja.
                if state.thinking {
                    state.spin_frame = state.spin_frame.wrapping_add(1);
                }
                // Refresh daftar proses background selagi panel terbuka
                // (registri global yang sama dipakai tool exec).
                if state.show_agents {
                    state.bg_procs = hermes_tools::processes::bg_processes_snapshot().await;
                }
            }
        }
    }
    Ok(())
}

/// Batas daftar sesi yang dimuat ke overlay ganti sesi — cukup untuk
/// navigasi cepat tanpa memindai seluruh DB.
const SESSION_LIST_LIMIT: usize = 30;

/// Tangani tombol saat overlay ganti sesi terbuka: navigasi daftar,
/// Enter = ganti, Esc/F3 = tutup.
async fn session_key(
    factory: &AgentFactory,
    app: &App,
    agent_opt: &mut Option<hermes_core::Agent>,
    event_rx: &mut mpsc::Receiver<AgentEvent>,
    state: &mut TuiState,
    key: KeyEvent,
) {
    match key.code {
        KeyCode::Esc | KeyCode::F(3) => state.close_session_overlay(),
        KeyCode::Up => state.session_nav(-1),
        KeyCode::Down => state.session_nav(1),
        KeyCode::Enter => {
            if let Some(sel) = state.session_select().cloned() {
                switch_session(factory, app, agent_opt, event_rx, state, sel).await;
            }
        }
        _ => {}
    }
}

/// Ganti sesi aktif TUI ke sesi terpilih dari session store.
/// Penjaga: hanya bila tidak ada turn yang berjalan (agen tersedia di
/// `agent_opt` dan `thinking` mati). Sesi aktif disimpan dulu agar urutan
/// "terbaru" di overlay tetap benar, lalu agen dibangun ulang dari
/// factory dengan channel event baru — event sisa milik sesi lama dibuang
/// agar tidak mengotori tampilan sesi baru.
async fn switch_session(
    factory: &AgentFactory,
    app: &App,
    agent_opt: &mut Option<hermes_core::Agent>,
    event_rx: &mut mpsc::Receiver<AgentEvent>,
    state: &mut TuiState,
    selected: Session,
) {
    let ready = agent_opt.is_some() && !state.thinking;
    if !ready {
        state.push(
            MsgRole::System,
            "tunggu turn yang berjalan selesai sebelum ganti sesi".to_string(),
        );
        return;
    }
    if let Some(agent) = agent_opt.as_ref() {
        if let Err(e) = app.memory.save_session(agent.session()).await {
            state.push(
                MsgRole::System,
                format!("peringatan: gagal menyimpan sesi aktif: {e}"),
            );
        }
    }
    let (new_tx, new_rx) = mpsc::channel::<AgentEvent>(512);
    *agent_opt = Some(factory.build_with_channel(selected, new_tx));
    *event_rx = new_rx;
    let session = agent_opt
        .as_ref()
        .expect("agen baru saja dibangun")
        .session()
        .clone();
    state.apply_session_switch(&session);
}

fn handle_agent_event(state: &mut TuiState, ev: AgentEvent) {
    match ev {
        AgentEvent::SessionStart { .. } => {}
        AgentEvent::TextDelta(d) => {
            let thinking = state.thinking;
            if let Some(m) = state.last_assistant_mut() {
                if thinking {
                    m.text.push_str(&d);
                    return;
                }
            }
            state.messages.push(TuiMsg {
                role: MsgRole::Assistant,
                text: d,
                wide_only: false,
                fg: None,
                tool: None,
            });
            state.thinking = true;
        }
        AgentEvent::ToolCallStart { name, arguments } => {
            let summary = tool_summary(&arguments);
            state.tools_running.insert(name.clone(), Instant::now());
            state.messages.push(TuiMsg {
                role: MsgRole::System,
                text: String::new(),
                wide_only: false,
                fg: None,
                tool: Some(ToolLine {
                    name: name.clone(),
                    summary,
                    running: true,
                    ok: true,
                    elapsed_ms: 0,
                    diff_stat: None,
                }),
            });
            state.note = format!("menjalankan {name}");
        }
        AgentEvent::ToolCallResult {
            name,
            ok,
            preview,
            diff_stat,
        } => {
            let elapsed_ms = state
                .tools_running
                .remove(&name)
                .map(|t| t.elapsed().as_millis().min(u128::from(u64::MAX)) as u64)
                .unwrap_or(0);
            let preview: String = preview.chars().take(120).collect();
            let updated = state
                .messages
                .iter_mut()
                .rev()
                .find(|m| matches!(&m.tool, Some(t) if t.name == name && t.running));
            match updated {
                Some(m) => {
                    if let Some(t) = m.tool.as_mut() {
                        t.running = false;
                        t.ok = ok;
                        t.elapsed_ms = elapsed_ms;
                        t.diff_stat = diff_stat;
                    }
                    m.text = preview;
                }
                None => {
                    // Tidak ada baris start yang cocok (mis. riwayat dibersihkan).
                    state.messages.push(TuiMsg {
                        role: MsgRole::System,
                        text: preview,
                        wide_only: false,
                        fg: None,
                        tool: Some(ToolLine {
                            name,
                            summary: String::new(),
                            running: false,
                            ok,
                            elapsed_ms,
                            diff_stat,
                        }),
                    });
                }
            }
            state.note = "berpikir...".to_string();
        }
        AgentEvent::IterationStart { n } => {
            state.iteration = n;
            state.note = format!("iterasi {n}");
        }
        AgentEvent::Compaction {
            before_tokens,
            after_tokens,
        } => {
            state.push(
                MsgRole::System,
                format!("🗜 konteks dikompresi: {before_tokens} → {after_tokens} token"),
            );
        }
        AgentEvent::SkillCreated { name } => {
            state.push(MsgRole::System, format!("📚 skill baru: {name}"));
        }
        AgentEvent::MemorySaved { kind } => {
            state.push(MsgRole::System, format!("🧠 memori tersimpan ({kind})"));
        }
        AgentEvent::Warning(w) => {
            state.push(MsgRole::System, format!("⚠ {w}"));
        }
        AgentEvent::Done { usage, .. } => {
            state.note = format!("↑{} ↓{}", usage.input_tokens, usage.output_tokens);
        }
        AgentEvent::Error(e) => {
            state.push(MsgRole::System, format!("Error: {e}"));
        }
    }
}

/// Perintah slash `/goal`: `/goal <teks>` menyetel teks goal sesi,
/// `/goal` (tanpa argumen) menampilkan goal saat ini, `/goal clear`
/// mengosongkannya. Kembalikan true bila input adalah perintah goal
/// (sudah ditangani — jangan diteruskan ke agent).
fn handle_goal_command(state: &mut TuiState, text: &str) -> bool {
    let rest = if text == "/goal" {
        None
    } else if let Some(r) = text.strip_prefix("/goal ") {
        Some(r)
    } else {
        return false;
    };
    match rest {
        None => {
            let cur = state.goal.clone().unwrap_or_else(|| "—".to_string());
            state.push(MsgRole::System, format!("goal saat ini: {cur}"));
        }
        Some(r) if r.trim() == "clear" || r.trim() == "-" => {
            state.goal = None;
            state.push(MsgRole::System, "goal dikosongkan".to_string());
        }
        Some(r) => {
            let g = r.trim().to_string();
            state.goal = Some(g.clone());
            state.push(MsgRole::System, format!("goal disetel: {g}"));
        }
    }
    true
}

/// Satu baris teks goal/loop bar: jumlah turn + iterasi berjalan
/// (bila agent sedang berpikir) + jumlah proses background yang hidup
/// + teks goal (atau placeholder "—" bila belum disetel).
fn goal_bar_text(state: &TuiState) -> String {
    let mut parts = vec![format!("turn {}", state.turn_count)];
    if state.thinking && state.iteration > 0 {
        parts.push(format!("iterasi {}", state.iteration));
    }
    let bg_running = state.bg_procs.iter().filter(|p| p.running).count();
    if bg_running > 0 {
        parts.push(format!("bg {bg_running}"));
    }
    let goal = state.goal.as_deref().unwrap_or("—");
    format!("{} · goal: {goal}", parts.join(" · "))
}

/// Isi panel proses background: satu baris per proses dengan penanda
/// status (● berjalan kuning, ✓ selesai hijau, ✗ gagal merah), atau pesan
/// state kosong. `inner_width` = lebar area teks (tanpa border); perintah
/// dipotong agar muat satu baris.
fn agents_panel_lines(
    theme: &Theme,
    procs: &[BgProcessSummary],
    inner_width: u16,
) -> Vec<Line<'static>> {
    fn trunc(s: &str, n: usize) -> String {
        let t: String = s.chars().take(n).collect();
        if s.chars().count() > n {
            format!("{t}…")
        } else {
            t
        }
    }
    let dim = Style::default().fg(theme.system);
    let mut out = Vec::new();
    out.push(Line::from(Span::styled(
        "Proses background (exec background=true) — F4/Esc tutup",
        Style::default().fg(theme.user).add_modifier(Modifier::BOLD),
    )));
    out.push(Line::from(""));
    if procs.is_empty() {
        out.push(Line::from(Span::styled(
            "Tidak ada proses background.",
            dim,
        )));
        out.push(Line::from(Span::styled(
            "exec dengan background: true mendaftarkan proses di sini.",
            dim,
        )));
        return out;
    }
    let w = inner_width as usize;
    for p in procs.iter().take(20) {
        let (glyph, color, status) = if p.running {
            ("●", theme.tool_run, "berjalan".to_string())
        } else {
            match p.exit_code {
                Some(0) => ("✓", theme.tool_ok, "selesai".to_string()),
                Some(c) => ("✗", theme.tool_err, format!("exit {c}")),
                None => ("·", theme.system, "selesai".to_string()),
            }
        };
        // ID dipadatkan (8 karakter pertama cukup unik untuk dikenali),
        // perintah dipotong agar baris muat dalam lebar panel.
        let short_id: String = p.id.chars().take(8).collect();
        let fixed = short_id.chars().count() + status.chars().count() + 8;
        let cmd = trunc(&p.command, w.saturating_sub(fixed).max(8));
        out.push(Line::from(vec![
            Span::styled(
                format!("{glyph} "),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("{short_id} "), dim),
            Span::styled(cmd, Style::default().fg(color)),
            Span::styled(format!(" · {status}"), dim),
        ]));
    }
    if procs.len() > 20 {
        out.push(Line::from(Span::styled(
            format!(
                "… dan {} lainnya (pakai process_manage untuk daftar penuh)",
                procs.len() - 20
            ),
            dim,
        )));
    }
    out
}

/// Kotak terpusat untuk overlay (bantuan): lebar = persen dari area,
/// tinggi tetap.
fn centered_rect(percent_x: u16, height: u16, r: Rect) -> Rect {
    let w = (r.width * percent_x / 100).max(24).min(r.width);
    let h = height.min(r.height);
    Rect {
        x: r.x + r.width.saturating_sub(w) / 2,
        y: r.y + r.height.saturating_sub(h) / 2,
        width: w,
        height: h,
    }
}

/// Waktu relatif ringkas untuk daftar sesi ("5 mnt lalu").
fn fmt_ago(dt: chrono::DateTime<chrono::Utc>) -> String {
    let s = chrono::Utc::now()
        .signed_duration_since(dt)
        .num_seconds()
        .max(0);
    if s < 60 {
        format!("{s} dtk lalu")
    } else if s < 3600 {
        format!("{} mnt lalu", s / 60)
    } else if s < 86400 {
        format!("{} jam lalu", s / 3600)
    } else {
        format!("{} hari lalu", s / 86400)
    }
}

/// Isi overlay ganti sesi: satu baris per sesi (judul + waktu terakhir
/// aktif), sesi terpilih disorot, sesi aktif bertanda ●. `inner_width` =
/// lebar area teks (tanpa border); judul dipotong agar muat satu baris.
fn session_overlay_lines(state: &TuiState, inner_width: u16) -> Vec<Line<'static>> {
    let theme = state.theme();
    let dim = Style::default().fg(theme.system);
    let mut out = Vec::new();
    out.push(Line::from(Span::styled(
        "Ganti sesi percakapan — Enter pilih · Esc/F3 tutup",
        Style::default().fg(theme.user).add_modifier(Modifier::BOLD),
    )));
    out.push(Line::from(""));
    if state.sessions.is_empty() {
        out.push(Line::from(Span::styled(
            "Belum ada sesi tersimpan untuk profil ini.",
            dim.add_modifier(Modifier::ITALIC),
        )));
        return out;
    }
    // Judul dipadatkan: marker 2 + " — " + kolom waktu ~14 + margin.
    let w = (inner_width as usize).saturating_sub(20).max(10);
    for (i, s) in state.sessions.iter().enumerate() {
        let selected = i == state.session_idx;
        let mut title: String = s.title.chars().take(w).collect();
        if s.title.chars().count() > w {
            title.push('…');
        }
        if title.trim().is_empty() {
            title = "(tanpa judul)".to_string();
        }
        let marker_style = Style::default()
            .fg(if selected { theme.accent } else { theme.system })
            .add_modifier(Modifier::BOLD);
        let mut spans = vec![
            Span::styled(if selected { "▸ " } else { "  " }, marker_style),
            Span::styled(
                title,
                if selected {
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                },
            ),
        ];
        if s.id == state.session_id {
            spans.push(Span::styled(" ●", Style::default().fg(theme.tool_ok)));
        }
        spans.push(Span::styled(format!(" — {}", fmt_ago(s.updated_at)), dim));
        out.push(Line::from(spans));
    }
    out
}

/// Isi overlay bantuan: tombol + perintah yang benar-benar didukung TUI.
fn help_lines(theme: &Theme) -> Vec<Line<'static>> {
    let dim = Style::default().fg(theme.system);
    let key = |k: &str, d: &str| {
        Line::from(vec![
            Span::styled(
                format!("  {k:<14}"),
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(d.to_string(), dim),
        ])
    };
    vec![
        Line::from(Span::styled(
            "Tombol",
            Style::default().fg(theme.user).add_modifier(Modifier::BOLD),
        )),
        key("Enter", "kirim pesan"),
        key("Esc", "bersihkan input / tutup popup / keluar"),
        key("Ctrl+C", "keluar"),
        key("PgUp / PgDn", "gulir riwayat"),
        key("F1 / ?", "bantuan ini (? saat input kosong)"),
        key("F2", &format!("ganti tema (aktif: {})", theme.name)),
        key("F3", "ganti sesi percakapan"),
        key("F4", "panel proses background (buka/tutup)"),
        Line::from(""),
        Line::from(Span::styled(
            "Perintah",
            Style::default().fg(theme.user).add_modifier(Modifier::BOLD),
        )),
        key("/quit, /exit", "keluar dari TUI"),
        key("/clear", "bersihkan riwayat pesan"),
        key("/goal <teks>", "atur teks goal (tampil di goal bar)"),
        key("/goal clear", "kosongkan goal"),
        Line::from(""),
        Line::from(Span::styled(
            "Esc untuk menutup",
            Style::default()
                .fg(theme.system)
                .add_modifier(Modifier::ITALIC),
        )),
    ]
}

fn draw<B: Backend>(terminal: &mut Terminal<B>, state: &TuiState) -> anyhow::Result<()> {
    terminal.draw(|f| {
        let area = f.area();
        let width = area.width;
        let theme = state.theme();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // header
                Constraint::Min(1),    // riwayat pesan (tanpa border)
                Constraint::Length(1), // goal/loop bar
                Constraint::Length(3), // input
                Constraint::Length(1), // status bar
            ])
            .split(area);

        // 1) Header satu baris ala Reasonix.
        let header = Line::from(vec![
            Span::styled(
                "◆ ",
                Style::default()
                    .fg(theme.header_glyph)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "hermes {} · {} · {}",
                    env!("CARGO_PKG_VERSION"),
                    state.model,
                    state.cwd
                ),
                Style::default().fg(theme.system),
            ),
        ]);
        f.render_widget(Paragraph::new(header), chunks[0]);

        // 2) Riwayat pesan tanpa border berat.
        let mut lines: Vec<Line> = Vec::new();
        for m in &state.messages {
            if m.wide_only && width < WIDE_MIN {
                continue;
            }
            if let Some(fg) = m.fg {
                // Baris splash art: warna gradasi, tanpa label.
                lines.push(Line::from(Span::styled(
                    m.text.clone(),
                    Style::default().fg(fg),
                )));
            } else if let Some(t) = &m.tool {
                lines.push(tool_line_with_preview(&theme, t, &m.text));
            } else if m.text.is_empty() {
                // Baris spacer kosong: tanpa label.
                lines.push(Line::from(""));
                lines.push(Line::from(""));
                continue;
            } else {
                let (label, color) = match m.role {
                    MsgRole::User => ("Anda", theme.user),
                    MsgRole::Assistant => ("Hermes", theme.assistant),
                    MsgRole::System => ("·", theme.system),
                };
                let label_span = Span::styled(
                    format!("{label}: "),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                );
                if m.role == MsgRole::Assistant {
                    // Pesan asisten dirender sebagai markdown.
                    let md = markdown_lines(&m.text, &theme);
                    for (i, line) in md.into_iter().enumerate() {
                        if i == 0 {
                            let mut spans = vec![label_span.clone()];
                            spans.extend(line.spans);
                            lines.push(Line::from(spans));
                        } else {
                            lines.push(line);
                        }
                    }
                } else {
                    lines.push(Line::from(vec![label_span, Span::raw(m.text.clone())]));
                }
            }
            lines.push(Line::from(""));
        }
        if state.thinking {
            let spin = SPINNER[state.spin_frame % SPINNER.len()];
            lines.push(Line::from(Span::styled(
                format!("{spin} Hermes sedang mengetik…"),
                Style::default()
                    .fg(theme.system)
                    .add_modifier(Modifier::ITALIC),
            )));
        }

        let visible_h = chunks[1].height as usize;
        let total = lines.len();
        let scroll: usize = if state.stick_bottom {
            total.saturating_sub(visible_h)
        } else {
            let from_bottom = state.scroll as usize;
            total.saturating_sub(visible_h).saturating_sub(from_bottom)
        };
        let msg_widget = Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((scroll as u16, 0));
        f.render_widget(msg_widget, chunks[1]);

        // 2b) Goal/loop bar: satu baris di atas input — turn, iterasi
        // berjalan, jumlah proses background yang hidup, dan teks goal
        // (placeholder "—" bila belum disetel via /goal).
        let mut goal_text = goal_bar_text(state);
        // Sisakan ruang untuk prefix "◈ " dan elipsis.
        let max_goal = (width as usize).saturating_sub(3);
        if goal_text.chars().count() > max_goal {
            goal_text = format!("{}…", goal_text.chars().take(max_goal).collect::<String>());
        }
        let goal_line = Line::from(vec![
            Span::styled(
                "◈ ",
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(goal_text, Style::default().fg(theme.system)),
        ]);
        f.render_widget(Paragraph::new(goal_line), chunks[2]);

        // 3) Input ramping dengan prefix › dan judul kontekstual.
        let input_title = if state.thinking {
            " Input (tunggu…) "
        } else {
            " Input "
        };
        let input_line = Line::from(vec![
            Span::styled(
                "› ".to_string(),
                Style::default().fg(theme.user).add_modifier(Modifier::BOLD),
            ),
            Span::raw(state.input.clone()),
        ]);
        let input_widget = Paragraph::new(input_line).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.border))
                .title(input_title),
        );
        f.render_widget(input_widget, chunks[3]);
        // Kursor di akhir input (setelah prefix "› "); sembunyikan saat
        // overlay bantuan/panel agen/overlay sesi terbuka.
        if !state.show_help && !state.show_agents && !state.show_sessions {
            f.set_cursor_position((
                chunks[3].x + 1 + 2 + state.input.chars().count() as u16,
                chunks[3].y + 1,
            ));
        }

        // 4) Status bar bawah ala Reasonix: info kiri, petunjuk kanan.
        // Di terminal sempit, ringkas bagian kiri dan potong dengan elipsis
        // agar petunjuk kanan tidak terpotong.
        let mut left = format!(
            "model {} · sesi {}",
            state.model,
            fmt_session(state.started.elapsed())
        );
        if width >= 100 {
            left.push_str(&format!(" · pesan {}", state.messages.len()));
        }
        if !state.note.is_empty() {
            left.push_str(" · ");
            left.push_str(&state.note);
        }
        let right =
            "Enter kirim · Esc keluar · PgUp/PgDn · F1 bantuan · F2 tema · F3 sesi · F4 agen";
        let max_left = (width as usize).saturating_sub(right.chars().count() + 3);
        let left: String = if left.chars().count() > max_left {
            let mut s: String = left.chars().take(max_left.saturating_sub(1)).collect();
            s.push('…');
            s
        } else {
            left
        };
        let gap = (width as usize).saturating_sub(left.chars().count() + right.chars().count() + 2);
        let status = Line::from(vec![
            Span::styled(
                format!(" {left}{} ", " ".repeat(gap)),
                Style::default().fg(theme.system),
            ),
            Span::styled(right, Style::default().fg(theme.system)),
        ]);
        f.render_widget(Paragraph::new(status), chunks[4]);

        // 5) Overlay bantuan di tengah layar (F1 / ?), di atas konten utama.
        if state.show_help {
            let popup = centered_rect(72, 21, area);
            f.render_widget(Clear, popup);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.border))
                .title(" Bantuan — Esc untuk tutup ");
            f.render_widget(Paragraph::new(help_lines(&theme)).block(block), popup);
        }

        // 6) Overlay panel proses background (F4), di atas overlay bantuan.
        if state.show_agents {
            let rows = state.bg_procs.len().min(20) + 5;
            let h = (rows as u16).clamp(7, area.height.saturating_sub(2).max(7));
            let popup = centered_rect(72, h, area);
            f.render_widget(Clear, popup);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.border))
                .title(" Proses background ");
            let inner = block.inner(popup);
            f.render_widget(block, popup);
            f.render_widget(
                Paragraph::new(agents_panel_lines(&theme, &state.bg_procs, inner.width)),
                inner,
            );
        }

        // 7) Overlay ganti sesi (F3), paling atas dari semua popup.
        if state.show_sessions {
            let rows = state.sessions.len().max(1) + 4;
            let h = (rows as u16).clamp(6, area.height.saturating_sub(2).max(6));
            let popup = centered_rect(72, h, area);
            f.render_widget(Clear, popup);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.border))
                .title(" Sesi ");
            let inner = block.inner(popup);
            f.render_widget(block, popup);
            f.render_widget(
                Paragraph::new(session_overlay_lines(state, inner.width)),
                inner,
            );
        }
    })?;
    Ok(())
}

/// Baris aktivitas tool + cuplikan hasil (m.text) bila ada.
fn tool_line_with_preview(theme: &Theme, t: &ToolLine, preview: &str) -> Line<'static> {
    if t.running {
        Line::from(vec![
            Span::styled(
                "● ",
                Style::default()
                    .fg(theme.tool_run)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                if t.summary.is_empty() {
                    t.name.clone()
                } else {
                    format!("{}: {}", t.name, t.summary)
                },
                Style::default().fg(theme.tool_run),
            ),
        ])
    } else if t.ok {
        let mut spans = vec![
            Span::styled(
                "✓ ",
                Style::default()
                    .fg(theme.tool_ok)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{} selesai ({})", t.name, fmt_dur_ms(t.elapsed_ms)),
                Style::default().fg(theme.tool_ok),
            ),
        ];
        // Diff-stat per-tool ala aslinya: `+24 −3` (hijau/merah),
        // hanya bila hasil tool membawa unified diff.
        if let Some(ds) = t.diff_stat {
            spans.push(Span::styled(
                format!(" +{}", ds.added),
                Style::default().fg(theme.tool_ok),
            ));
            spans.push(Span::styled(
                format!(" −{}", ds.removed),
                Style::default().fg(theme.tool_err),
            ));
        }
        if !preview.is_empty() {
            spans.push(Span::styled(
                format!(": {preview}"),
                Style::default().fg(theme.system),
            ));
        }
        Line::from(spans)
    } else {
        Line::from(vec![
            Span::styled(
                "✗ ",
                Style::default()
                    .fg(theme.tool_err)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{} gagal", t.name),
                Style::default().fg(theme.tool_err),
            ),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    #[test]
    fn tool_activity_line_updates_in_place() {
        let mut state = TuiState::new("m".to_string(), "/tmp".to_string());
        handle_agent_event(
            &mut state,
            AgentEvent::ToolCallStart {
                name: "exec".to_string(),
                arguments: serde_json::json!({"command": "ls -la"}),
            },
        );
        assert_eq!(state.messages.len(), 1);
        let t = state.messages[0].tool.as_ref().expect("baris tool");
        assert!(t.running);
        assert_eq!(t.summary, "ls -la");

        handle_agent_event(
            &mut state,
            AgentEvent::ToolCallResult {
                name: "exec".to_string(),
                ok: true,
                preview: "total 0".to_string(),
                diff_stat: None,
            },
        );
        // Diupdate di tempat, bukan baris baru.
        assert_eq!(state.messages.len(), 1);
        let t = state.messages[0].tool.as_ref().expect("baris tool");
        assert!(!t.running && t.ok);
        assert_eq!(state.messages[0].text, "total 0");
    }

    #[test]
    fn tool_summary_picks_common_fields() {
        assert_eq!(
            tool_summary(&serde_json::json!({"path": "/a/b.txt", "x": 1})),
            "/a/b.txt"
        );
        assert_eq!(
            tool_summary(&serde_json::json!("teks langsung")),
            "teks langsung"
        );
    }

    #[test]
    fn fmt_helpers() {
        assert_eq!(fmt_dur_ms(42), "42ms");
        assert_eq!(fmt_dur_ms(1500), "1.5s");
        assert_eq!(fmt_session(Duration::from_secs(5)), "5s");
        assert_eq!(fmt_session(Duration::from_secs(125)), "2m 5s");
        assert_eq!(fmt_session(Duration::from_secs(3700)), "1j 1m");
    }

    #[test]
    fn markdown_inline_styles() {
        let theme = THEMES[0];
        let lines = markdown_lines("teks **tebal** dan *miring* serta `kode`", &theme);
        assert_eq!(lines.len(), 1, "satu paragraf = satu baris");
        let spans = &lines[0].spans;
        let bold = spans
            .iter()
            .find(|s| s.content == "tebal")
            .expect("span tebal");
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
        let italic = spans
            .iter()
            .find(|s| s.content == "miring")
            .expect("span miring");
        assert!(italic.style.add_modifier.contains(Modifier::ITALIC));
        let code = spans
            .iter()
            .find(|s| s.content == "kode")
            .expect("span kode");
        assert_eq!(code.style.fg, Some(theme.accent));
    }

    #[test]
    fn markdown_heading_list_and_fence() {
        let theme = THEMES[0];
        let lines = markdown_lines(
            "# Judul\n\n- apel\n- jeruk\n\n1. satu\n2. dua\n\n```rust\nlet x = 1;\n```",
            &theme,
        );
        // Heading: baris pertama, tebal + warna aksen.
        let head = &lines[0].spans[0];
        assert_eq!(head.content, "Judul");
        assert_eq!(head.style.fg, Some(theme.accent));
        assert!(head.style.add_modifier.contains(Modifier::BOLD));
        // Daftar: cari baris berpoin dan bernomor.
        let bullet: Vec<&Line> = lines
            .iter()
            .filter(|l| {
                l.spans
                    .first()
                    .map(|s| s.content.starts_with('•'))
                    .unwrap_or(false)
            })
            .collect();
        assert_eq!(bullet.len(), 2, "dua poin daftar");
        let numbered: Vec<&Line> = lines
            .iter()
            .filter(|l| {
                l.spans
                    .first()
                    .map(|s| s.content.starts_with("1. ") || s.content.starts_with("2. "))
                    .unwrap_or(false)
            })
            .collect();
        assert_eq!(numbered.len(), 2, "dua item bernomor");
        // Blok kode: label bahasa + baris berlatar.
        let label = lines
            .iter()
            .find(|l| {
                l.spans
                    .first()
                    .map(|s| s.content == "┌─ rust")
                    .unwrap_or(false)
            })
            .expect("label bahasa blok kode");
        assert_eq!(label.spans[0].style.fg, Some(theme.border));
        let code = lines
            .iter()
            .find(|l| {
                l.spans
                    .first()
                    .map(|s| s.content == "let x = 1;")
                    .unwrap_or(false)
            })
            .expect("isi blok kode");
        assert_eq!(code.spans[0].style.bg, Some(theme.code_bg));
        assert_eq!(code.spans[0].style.fg, Some(theme.code_fg));
    }

    #[test]
    fn theme_toggle_cycles() {
        let mut state = TuiState::new("m".to_string(), "/tmp".to_string());
        assert_eq!(state.theme().name, "gelap");
        state.cycle_theme();
        assert_eq!(state.theme().name, "terang");
        state.cycle_theme();
        assert_eq!(state.theme().name, "kontras");
        state.cycle_theme();
        assert_eq!(state.theme().name, "gelap");
    }

    #[test]
    fn theme_idx_for_name_maps_and_falls_back() {
        assert_eq!(theme_idx_for_name("gelap"), 0);
        assert_eq!(theme_idx_for_name("terang"), 1);
        assert_eq!(theme_idx_for_name("kontras"), 2);
        // Nama tak dikenal / kosong → default gelap.
        assert_eq!(theme_idx_for_name("neon"), 0);
        assert_eq!(theme_idx_for_name(""), 0);
    }

    #[test]
    fn set_theme_by_name_restores_saved_theme() {
        let mut state = TuiState::new("m".to_string(), "/tmp".to_string());
        state.set_theme_by_name("terang");
        assert_eq!(state.theme().name, "terang");
        state.set_theme_by_name("tidak-ada");
        assert_eq!(state.theme().name, "gelap");
    }

    #[test]
    fn draw_renders_help_overlay() {
        let mut state = TuiState::new("m".to_string(), "/tmp".to_string());
        state.show_help = true;
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal uji");
        draw(&mut terminal, &state).expect("draw");
        let has_bantuan = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .any(|c| c.symbol() == "B");
        assert!(has_bantuan, "judul overlay Bantuan harus ter-render");
    }

    #[test]
    fn draw_assistant_markdown_code_styled() {
        let mut state = TuiState::new("m".to_string(), "/tmp".to_string());
        state.push(MsgRole::Assistant, "ini `kode` inline".to_string());
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal uji");
        draw(&mut terminal, &state).expect("draw");
        let theme = state.theme();
        let styled = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .any(|c| c.symbol() == "k" && c.fg == theme.accent);
        assert!(styled, "span kode inline harus berwarna aksen");
    }

    fn screen_text(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    fn sample_proc(
        id: &str,
        command: &str,
        running: bool,
        exit_code: Option<i32>,
    ) -> BgProcessSummary {
        BgProcessSummary {
            id: id.to_string(),
            command: command.to_string(),
            started_at: "0".to_string(),
            running,
            exit_code,
        }
    }

    #[test]
    fn goal_command_sets_clears_and_shows_goal() {
        let mut state = TuiState::new("m".to_string(), "/tmp".to_string());
        // Bukan perintah goal → tidak ditangani.
        assert!(!handle_goal_command(&mut state, "/goals"));
        assert!(!handle_goal_command(&mut state, "/quit"));
        assert!(!handle_goal_command(&mut state, "goal tanpa slash"));
        // Setel.
        assert!(handle_goal_command(
            &mut state,
            "/goal tulis laporan mingguan"
        ));
        assert_eq!(state.goal.as_deref(), Some("tulis laporan mingguan"));
        assert!(state
            .messages
            .iter()
            .any(|m| m.text.contains("goal disetel")));
        // Tampilkan tanpa mengubah.
        assert!(handle_goal_command(&mut state, "/goal"));
        assert_eq!(state.goal.as_deref(), Some("tulis laporan mingguan"));
        // Kosongkan.
        assert!(handle_goal_command(&mut state, "/goal clear"));
        assert!(state.goal.is_none());
        assert!(handle_goal_command(&mut state, "/goal x"));
        assert!(handle_goal_command(&mut state, "/goal -"));
        assert!(state.goal.is_none());
    }

    #[test]
    fn goal_bar_text_includes_turn_goal_iteration_bg() {
        let mut state = TuiState::new("m".to_string(), "/tmp".to_string());
        // Placeholder saat goal belum disetel.
        let t = goal_bar_text(&state);
        assert!(t.contains("turn 0"), "{t}");
        assert!(t.contains("goal: —"), "{t}");
        assert!(
            !t.contains("iterasi"),
            "tanpa thinking tak ada iterasi: {t}"
        );
        assert!(!t.contains("bg"), "tanpa proses tak ada bg: {t}");

        state.turn_count = 3;
        state.goal = Some("tulis laporan".to_string());
        state.thinking = true;
        state.iteration = 5;
        state.bg_procs = vec![sample_proc("proc-1", "sleep 60", true, None)];
        let t = goal_bar_text(&state);
        assert!(t.contains("turn 3"), "{t}");
        assert!(t.contains("iterasi 5"), "{t}");
        assert!(t.contains("bg 1"), "{t}");
        assert!(t.contains("goal: tulis laporan"), "{t}");
    }

    #[test]
    fn agents_panel_renders_with_data() {
        let mut state = TuiState::new("m".to_string(), "/tmp".to_string());
        state.show_agents = true;
        state.bg_procs = vec![
            sample_proc("proc-12345678", "sleep 300", true, None),
            sample_proc("proc-abcdefgh", "ls -la /tmp", false, Some(0)),
            sample_proc("proc-ijklmnop", "false", false, Some(1)),
        ];
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal uji");
        draw(&mut terminal, &state).expect("draw");
        let screen = screen_text(&terminal);
        assert!(screen.contains("Proses background"), "judul panel");
        assert!(screen.contains("sleep 300"), "perintah proses");
        assert!(screen.contains("berjalan"), "status berjalan");
        assert!(screen.contains("selesai"), "status selesai");
        assert!(screen.contains("exit 1"), "status gagal");
        assert!(
            screen.contains('●') && screen.contains('✓') && screen.contains('✗'),
            "glyph status ●/✓/✗"
        );
    }

    #[test]
    fn agents_panel_toggle_closes_other_overlays() {
        let mut state = TuiState::new("m".to_string(), "/tmp".to_string());
        assert!(!state.show_agents);
        state.show_help = true;
        state.toggle_agents_panel();
        assert!(state.show_agents, "F4 membuka panel");
        assert!(!state.show_help, "overlay bantuan ditutup");
        state.toggle_agents_panel();
        assert!(!state.show_agents, "F4 kedua menutup panel");
    }

    #[test]
    fn agents_panel_renders_empty_state() {
        let mut state = TuiState::new("m".to_string(), "/tmp".to_string());
        state.show_agents = true;
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal uji");
        draw(&mut terminal, &state).expect("draw");
        let screen = screen_text(&terminal);
        assert!(
            screen.contains("Tidak ada proses background"),
            "empty state: {screen}"
        );
    }

    #[test]
    fn draw_small_terminal_with_overlays_no_panic() {
        let mut state = TuiState::new("m".to_string(), "/tmp".to_string());
        state.show_agents = true;
        state.bg_procs = vec![sample_proc(
            "proc-12345678",
            "perintah-yang-sangat-panjang-untuk-menguji-pemotongan",
            true,
            None,
        )];
        state.turn_count = 12;
        state.goal = Some(
            "goal yang cukup panjang untuk menguji pemotongan baris goal bar di terminal sempit"
                .to_string(),
        );
        let backend = TestBackend::new(60, 16);
        let mut terminal = Terminal::new(backend).expect("terminal uji");
        draw(&mut terminal, &state).expect("draw tidak boleh panic di 60x16");
        let screen = screen_text(&terminal);
        assert!(screen.contains('◈'), "goal bar ter-render");
        assert!(screen.contains('…'), "teks panjang dipotong dengan elipsis");
        assert!(screen.contains("Proses background"), "panel ter-render");
    }

    #[test]
    fn help_overlay_mentions_new_bindings() {
        let theme = THEMES[0];
        let text: String = help_lines(&theme)
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(text.contains("F4"), "bantuan menyebut F4");
        assert!(text.contains("F3"), "bantuan menyebut F3");
        assert!(text.contains("/goal"), "bantuan menyebut /goal");
    }

    fn mock_session(title: &str, secs_ago: i64) -> Session {
        let mut s = Session::new("test");
        s.title = title.to_string();
        s.updated_at = chrono::Utc::now() - chrono::Duration::seconds(secs_ago);
        s
    }

    #[test]
    fn session_overlay_open_close() {
        let mut state = TuiState::new("m".to_string(), "/tmp".to_string());
        assert!(!state.show_sessions);
        state.open_session_overlay(vec![
            mock_session("Sesi Alpha", 30),
            mock_session("Sesi Beta", 3600),
        ]);
        assert!(state.show_sessions);
        assert_eq!(state.sessions.len(), 2);
        assert_eq!(state.session_idx, 0);
        // Judul terbaru dulu (urutan list_sessions).
        assert_eq!(
            state.session_select().map(|s| s.title.as_str()),
            Some("Sesi Alpha")
        );
        state.close_session_overlay();
        assert!(!state.show_sessions);
        assert_eq!(state.session_idx, 0);
    }

    #[test]
    fn session_nav_wraps_around() {
        let mut state = TuiState::new("m".to_string(), "/tmp".to_string());
        state.open_session_overlay(vec![
            mock_session("satu", 1),
            mock_session("dua", 2),
            mock_session("tiga", 3),
        ]);
        state.session_nav(1);
        assert_eq!(state.session_idx, 1);
        state.session_nav(1);
        assert_eq!(state.session_idx, 2);
        state.session_nav(1); // melingkar ke awal
        assert_eq!(state.session_idx, 0);
        state.session_nav(-1); // melingkar ke akhir
        assert_eq!(state.session_idx, 2);
        assert_eq!(
            state.session_select().map(|s| s.title.as_str()),
            Some("tiga")
        );
    }

    #[test]
    fn session_overlay_empty_safe() {
        let mut state = TuiState::new("m".to_string(), "/tmp".to_string());
        state.open_session_overlay(vec![]);
        assert!(state.show_sessions);
        assert!(state.session_select().is_none());
        state.session_nav(1); // tidak boleh panic
        state.session_nav(-1);
        assert_eq!(state.session_idx, 0);
    }

    #[test]
    fn esc_priority_sessions_first() {
        let mut state = TuiState::new("m".to_string(), "/tmp".to_string());
        state.show_sessions = true;
        state.show_help = true;
        state.show_agents = true;
        state.input = "teks".to_string();
        // Urutan tutup: sesi → bantuan → agen → bersihkan input → keluar.
        assert!(!state.on_esc());
        assert!(!state.show_sessions && state.show_help);
        assert!(!state.on_esc());
        assert!(!state.show_help && state.show_agents);
        assert!(!state.on_esc());
        assert!(!state.show_agents && state.input == "teks");
        assert!(!state.on_esc());
        assert!(state.input.is_empty());
        assert!(state.on_esc(), "input kosong → keluar");
    }

    #[test]
    fn apply_session_switch_resets_feed() {
        let mut state = TuiState::new("m".to_string(), "/tmp".to_string());
        state.push(MsgRole::User, "halo lama".to_string());
        state.push(MsgRole::Assistant, "balasan lama".to_string());
        state.open_session_overlay(vec![mock_session("Sesi Baru", 10)]);
        let sel = state.session_select().cloned().expect("ada sesi");
        state.apply_session_switch(&sel);
        assert!(!state.show_sessions);
        assert!(state.sessions.is_empty());
        assert_eq!(state.session_id, sel.id);
        // Feed lama diganti satu pengumuman sistem berisi judul sesi.
        assert_eq!(state.messages.len(), 1);
        assert_eq!(state.messages[0].role, MsgRole::System);
        assert!(
            state.messages[0].text.contains("Sesi Baru"),
            "pengumuman menyebut judul: {}",
            state.messages[0].text
        );
    }

    #[test]
    fn fmt_ago_units() {
        let now = chrono::Utc::now();
        assert_eq!(fmt_ago(now), "0 dtk lalu");
        assert_eq!(fmt_ago(now - chrono::Duration::seconds(45)), "45 dtk lalu");
        assert_eq!(fmt_ago(now - chrono::Duration::seconds(150)), "2 mnt lalu");
        assert_eq!(fmt_ago(now - chrono::Duration::seconds(7200)), "2 jam lalu");
        assert_eq!(
            fmt_ago(now - chrono::Duration::seconds(90000)),
            "1 hari lalu"
        );
    }

    #[test]
    fn draw_renders_session_overlay() {
        let mut state = TuiState::new("m".to_string(), "/tmp".to_string());
        state.session_id = "id-alpha".to_string();
        let mut a = mock_session("Sesi Alpha", 30);
        a.id = "id-alpha".to_string();
        state.open_session_overlay(vec![a, mock_session("Sesi Beta", 4000)]);
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal uji");
        draw(&mut terminal, &state).expect("draw");
        let screen = screen_text(&terminal);
        assert!(screen.contains("Sesi Alpha"), "judul sesi ter-render");
        assert!(screen.contains("Sesi Beta"), "sesi kedua ter-render");
        assert!(screen.contains("▸"), "penanda pilihan ter-render");
        assert!(screen.contains('●'), "penanda sesi aktif ter-render");
        assert!(screen.contains("dtk lalu"), "waktu relatif ter-render");
    }

    fn tool_line_for_test(diff_stat: Option<DiffStat>) -> ToolLine {
        ToolLine {
            name: "edit_file".to_string(),
            summary: String::new(),
            running: false,
            ok: true,
            elapsed_ms: 1200,
            diff_stat,
        }
    }

    fn line_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn tool_ok_line_renders_diff_stat() {
        let theme = THEMES[0];
        let t = tool_line_for_test(Some(DiffStat {
            added: 24,
            removed: 3,
        }));
        let text = line_text(&tool_line_with_preview(&theme, &t, ""));
        assert!(text.contains("✓"), "{text}");
        assert!(text.contains("+24"), "penambahan ter-render: {text}");
        assert!(text.contains("−3"), "penghapusan ter-render: {text}");
    }

    #[test]
    fn tool_ok_line_hides_diff_stat_when_none() {
        let theme = THEMES[0];
        let t = tool_line_for_test(None);
        let text = line_text(&tool_line_with_preview(&theme, &t, ""));
        assert!(text.contains("selesai"), "{text}");
        assert!(!text.contains('+'), "tanpa diff-stat tak ada '+': {text}");
        assert!(!text.contains('−'), "tanpa diff-stat tak ada '−': {text}");
    }

    #[test]
    fn tool_result_event_carries_diff_stat_to_tool_line() {
        let mut state = TuiState::new("m".to_string(), "/tmp".to_string());
        handle_agent_event(
            &mut state,
            AgentEvent::ToolCallStart {
                name: "edit_file".to_string(),
                arguments: serde_json::json!({"path": "a.txt"}),
            },
        );
        handle_agent_event(
            &mut state,
            AgentEvent::ToolCallResult {
                name: "edit_file".to_string(),
                ok: true,
                preview: "diganti".to_string(),
                diff_stat: Some(DiffStat {
                    added: 2,
                    removed: 1,
                }),
            },
        );
        let t = state.messages[0].tool.as_ref().expect("baris tool");
        assert!(!t.running && t.ok);
        assert_eq!(
            t.diff_stat,
            Some(DiffStat {
                added: 2,
                removed: 1
            })
        );
    }
}
