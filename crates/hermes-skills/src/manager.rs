use async_trait::async_trait;
use hermes_core::{HermesError, Result, Skill, SkillStore, SkillsConfig};
use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Frontmatter SKILL.md ala agentskills.io.
///
/// Selain `name`/`description`, aslinya mendukung gate frontmatter:
/// - `platforms:` — gate kompatibilitas: skill dilewati total bila OS saat
///   ini tidak cocok (absen = semua OS). Bentuk list atau skalar.
/// - `environments:` — gate relevansi untuk permukaan penawaran (list,
///   pencarian) saja; pemuatan eksplisit (`get`) selalu lolos — meniru
///   semantik aslinya.
#[derive(Debug, Deserialize, Default)]
struct Frontmatter {
    name: Option<String>,
    description: Option<String>,
    #[serde(default)]
    platforms: Option<PlatformList>,
    #[serde(default)]
    environments: Option<PlatformList>,
}

/// Nilai frontmatter yang boleh berupa skalar tunggal atau list
/// (aslinya: `if not isinstance(platforms, list): platforms = [platforms]`).
#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
enum PlatformList {
    Single(String),
    Multiple(Vec<String>),
}

impl PlatformList {
    fn values(&self) -> Vec<String> {
        match self {
            PlatformList::Single(s) => vec![s.clone()],
            PlatformList::Multiple(v) => v.clone(),
        }
    }
}

/// Skill yang tidak bisa dinonaktifkan — meniru ESSENTIAL_SKILLS aslinya
/// (`hermes-agent` adalah manual operasi agen itu sendiri).
const ESSENTIAL_SKILLS: &[&str] = &["hermes-agent"];

/// Normalisasi nama platform frontmatter ke nama OS kanonis
/// ala `std::env::consts::OS` ("linux", "macos", "windows").
///
/// Meniru PLATFORM_MAP aslinya (`macos`→`darwin`, `windows`→`win32`) dari
/// sisi sebaliknya: entri frontmatter dinormalisasi ke nama OS Rust,
/// lalu dibandingkan persis dengan OS saat ini.
fn normalize_platform(name: &str) -> String {
    let n = name.trim().to_lowercase();
    match n.as_str() {
        "darwin" | "macos" | "macosx" | "osx" => "macos".to_string(),
        "win32" | "win64" | "windows" | "win" => "windows".to_string(),
        "linux" => "linux".to_string(),
        _ => n,
    }
}

/// Gate kompatibilitas platform: true bila skill boleh dimuat di OS ini.
/// Absen/kosong = semua OS (meniru `skill_matches_platform` aslinya).
fn platform_matches(platforms: &Option<PlatformList>) -> bool {
    let Some(list) = platforms else {
        return true;
    };
    let values = list.values();
    if values.is_empty() {
        return true;
    }
    let current = std::env::consts::OS;
    values.iter().any(|p| normalize_platform(p) == current)
}

/// Deteksi environment runtime untuk gate `environments:` — meniru
/// `_ENV_DETECTORS` aslinya (kanban/docker/s6). Tag tak dikenal fail-open
/// (true), persis seperti aslinya.
fn detect_environment(name: &str) -> bool {
    match name.trim().to_lowercase().as_str() {
        "kanban" => {
            std::env::var_os("HERMES_KANBAN_TASK").is_some()
                || std::env::var_os("HERMES_KANBAN_BOARD").is_some()
        }
        "docker" => Path::new("/.dockerenv").exists() || Path::new("/run/.containerenv").exists(),
        "s6" => Path::new("/run/s6").is_dir() || Path::new("/package/admin/s6-overlay").is_dir(),
        _ => true,
    }
}

/// Gate relevansi environment: true bila SALAH SATU tag aktif
/// (kosong = semua). Hanya dipakai di permukaan penawaran (`list`,
/// `find_relevant`); tag tak dikenal fail-open via `detect_environment`.
fn environment_tags_match(tags: &[String]) -> bool {
    tags.is_empty() || tags.iter().any(|e| detect_environment(e))
}

/// Manajer skill berbasis file: ~/.hermes/skills/<nama>/SKILL.md
/// plus skill proyek di <workdir>/.hermes/skills/.
///
/// Gate yang diterapkan saat load (meniru aslinya):
/// - `platforms:` di frontmatter — skill dilewati total bila OS tak cocok.
/// - `disabled_skills` di config — skill tak pernah dimuat/didaftar
///   (kecuali skill esensial `hermes-agent`).
/// - `environments:` di frontmatter — hanya filter permukaan penawaran
///   (`list`, `find_relevant`); `get` eksplisit selalu lolos.
pub struct SkillManager {
    /// Direktori skill global + proyek + ekstra, berurutan prioritas.
    dirs: Vec<PathBuf>,
    /// Direktori tulis untuk `create` (tak berubah oleh direktori ekstra).
    primary: PathBuf,
    /// Nama skill yang dinonaktifkan (lowercase).
    disabled: HashSet<String>,
}

impl SkillManager {
    pub fn new(hermes_home: &std::path::Path, workdir: &std::path::Path) -> Self {
        let primary = hermes_home.join("skills");
        Self {
            dirs: vec![workdir.join(".hermes").join("skills"), primary.clone()],
            primary,
            disabled: HashSet::new(),
        }
    }

    /// Untuk testing: direktori-dir saja.
    pub fn with_dirs(dirs: Vec<PathBuf>) -> Self {
        let primary = dirs.last().cloned().unwrap_or_default();
        Self {
            dirs,
            primary,
            disabled: HashSet::new(),
        }
    }

    /// Terapkan konfigurasi `[skills]`: daftar disable + direktori ekstra.
    ///
    /// Direktori ekstra dipindai dengan parsing yang sama seperti direktori
    /// bawaan, pada prioritas paling rendah (meniru tier `external_dirs`
    /// aslinya: project > local > external). Entri relatif dijangkarkan ke
    /// `hermes_home`; entri yang tidak ada dilewati diam-diam.
    pub fn with_skills_config(mut self, hermes_home: &Path, cfg: &SkillsConfig) -> Self {
        self.disabled = cfg
            .disabled_skills
            .iter()
            .map(|s| s.to_lowercase())
            .filter(|s| !ESSENTIAL_SKILLS.contains(&s.as_str()))
            .collect();
        let mut seen_dirs: HashSet<PathBuf> = self.dirs.iter().cloned().collect();
        for entry in &cfg.extra_skill_dirs {
            let p = resolve_skill_dir(hermes_home, entry);
            if p.is_dir() && seen_dirs.insert(p.clone()) {
                self.dirs.push(p);
            }
        }
        self
    }

    /// Pindai semua direktori; terapkan gate platform + disable.
    /// Mengembalikan skill beserta tag `environments:`-nya (untuk filter
    /// permukaan penawaran di `list`/`find_relevant`).
    fn scan(&self) -> Vec<(Skill, Vec<String>)> {
        let mut skills = Vec::new();
        let mut seen = HashSet::new();
        for dir in &self.dirs {
            let Ok(rd) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in rd.flatten() {
                let skill_dir = entry.path();
                if !skill_dir.is_dir() {
                    continue;
                }
                let md = skill_dir.join("SKILL.md");
                if !md.is_file() {
                    continue;
                }
                let Ok(body_raw) = std::fs::read_to_string(&md) else {
                    continue;
                };
                let (fm, body) = parse_frontmatter(&body_raw);
                let name = fm
                    .name
                    .or_else(|| {
                        skill_dir
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                    })
                    .unwrap_or_else(|| "unnamed".to_string());
                if !seen.insert(name.clone()) {
                    continue;
                }
                // Gate kompatibilitas platform: dilewati total saat load.
                if !platform_matches(&fm.platforms) {
                    continue;
                }
                // Gate disable dari config.
                if self.disabled.contains(&name.to_lowercase()) {
                    continue;
                }
                let description = fm.description.unwrap_or_default();
                let triggers = extract_triggers(&name, &description, &body);
                let environments = fm.environments.map(|e| e.values()).unwrap_or_default();
                skills.push((
                    Skill {
                        name,
                        description,
                        path: skill_dir,
                        body,
                        triggers,
                    },
                    environments,
                ));
            }
        }
        skills.sort_by(|a, b| a.0.name.cmp(&b.0.name));
        skills
    }

    /// Hasil scan yang lolos gate environment (permukaan penawaran).
    fn offerable(&self) -> Vec<Skill> {
        self.scan()
            .into_iter()
            .filter(|(_, envs)| environment_tags_match(envs))
            .map(|(s, _)| s)
            .collect()
    }

    fn primary_dir(&self) -> &PathBuf {
        &self.primary
    }
}

/// Selesaikan satu entri `extra_skill_dirs`: ekspansi `~`/`$VAR`/`${VAR}`,
/// jangkar path relatif ke `hermes_home` (meniru `_home_relative` aslinya).
fn resolve_skill_dir(hermes_home: &Path, entry: &str) -> PathBuf {
    let expanded = expand_env(entry.trim());
    let p = PathBuf::from(&expanded);
    if p.is_absolute() {
        return p;
    }
    if expanded == "~" || expanded.starts_with("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            let rest = expanded.trim_start_matches('~').trim_start_matches('/');
            return PathBuf::from(home).join(rest);
        }
    }
    hermes_home.join(p)
}

/// Ekspansi `$VAR` dan `${VAR}` sederhana dari environment.
/// Variabel tak dikenal menjadi string kosong (ala shell); `$` soliter
/// dipertahankan literal.
fn expand_env(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        if chars.peek() == Some(&'{') {
            chars.next(); // konsumsi '{'
            let mut name = String::new();
            let mut closed = false;
            for ch in chars.by_ref() {
                if ch == '}' {
                    closed = true;
                    break;
                }
                name.push(ch);
            }
            if closed {
                if let Some(v) = std::env::var_os(&name) {
                    out.push_str(&v.to_string_lossy());
                }
            } else {
                out.push_str("${");
                out.push_str(&name);
            }
            continue;
        }
        let mut name = String::new();
        while let Some(&ch) = chars.peek() {
            if ch.is_alphanumeric() || ch == '_' {
                name.push(ch);
                chars.next();
            } else {
                break;
            }
        }
        if name.is_empty() {
            out.push('$');
        } else if let Some(v) = std::env::var_os(&name) {
            out.push_str(&v.to_string_lossy());
        }
    }
    out
}

/// Blok frontmatter mentah (termasuk pagar `---`), untuk dipertahankan
/// utuh saat `update`. Mengembalikan None bila tidak ada frontmatter.
fn frontmatter_block(raw: &str) -> Option<String> {
    let mut lines = raw.lines();
    if lines.next().map(|l| l.trim()) != Some("---") {
        return None;
    }
    let mut block = String::from("---");
    for line in lines {
        block.push('\n');
        block.push_str(line);
        if line.trim() == "---" {
            return Some(block);
        }
    }
    None
}

/// Pisahkan frontmatter YAML dari body markdown.
fn parse_frontmatter(raw: &str) -> (Frontmatter, String) {
    let mut lines = raw.lines();
    if lines.next().map(|l| l.trim()) != Some("---") {
        return (Frontmatter::default(), raw.to_string());
    }
    let mut fm_text = String::new();
    let mut body_start = 0;
    for (i, line) in raw.lines().enumerate().skip(1) {
        if line.trim() == "---" {
            body_start = i + 1;
            break;
        }
        fm_text.push_str(line);
        fm_text.push('\n');
    }
    let fm: Frontmatter = serde_yaml::from_str(&fm_text).unwrap_or_default();
    let body = raw.lines().skip(body_start).collect::<Vec<_>>().join("\n");
    (fm, body)
}

/// Kata kunci pemicu: token dari nama + deskripsi.
fn extract_triggers(name: &str, description: &str, _body: &str) -> Vec<String> {
    let mut triggers = Vec::new();
    for token in name.split(|c: char| !c.is_alphanumeric()) {
        if token.len() > 2 {
            triggers.push(token.to_lowercase());
        }
    }
    for token in description.split(|c: char| !c.is_alphanumeric()) {
        if token.len() > 3 {
            let t = token.to_lowercase();
            if !triggers.contains(&t) {
                triggers.push(t);
            }
        }
    }
    triggers
}

fn slugify(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

#[async_trait]
impl SkillStore for SkillManager {
    async fn list(&self) -> Result<Vec<Skill>> {
        // Permukaan penawaran: gate platform + disable + environments.
        Ok(self.offerable())
    }

    async fn get(&self, name: &str) -> Result<Option<Skill>> {
        // Pemuatan eksplisit: gate platform + disable berlaku, tetapi
        // gate `environments:` TIDAK (meniru skill_view aslinya).
        Ok(self
            .scan()
            .into_iter()
            .map(|(s, _)| s)
            .find(|s| s.name == name))
    }

    async fn create(&self, name: &str, description: &str, body: &str) -> Result<Skill> {
        let slug = slugify(name);
        if slug.is_empty() {
            return Err(HermesError::Skill("nama skill tidak valid".to_string()));
        }
        let dir = self.primary_dir().join(&slug);
        if dir.exists() {
            return Err(HermesError::Skill(format!("skill `{slug}` sudah ada")));
        }
        std::fs::create_dir_all(&dir)?;
        let content = format!("---\nname: {slug}\ndescription: {description}\n---\n\n{body}\n");
        std::fs::write(dir.join("SKILL.md"), &content)?;
        Ok(Skill {
            name: slug.clone(),
            description: description.to_string(),
            path: dir,
            body: body.to_string(),
            triggers: extract_triggers(&slug, description, body),
        })
    }

    async fn update(&self, name: &str, body: &str) -> Result<()> {
        let skill = self
            .get(name)
            .await?
            .ok_or_else(|| HermesError::Skill(format!("skill `{name}` tidak ditemukan")))?;
        // Pertahankan blok frontmatter yang ada utuh (termasuk
        // platforms:/environments: bila ada), ganti body saja.
        let raw = std::fs::read_to_string(skill.path.join("SKILL.md"))?;
        let fm_block = frontmatter_block(&raw).unwrap_or_else(|| {
            format!(
                "---\nname: {}\ndescription: {}\n---",
                skill.name, skill.description
            )
        });
        std::fs::write(
            skill.path.join("SKILL.md"),
            format!("{fm_block}\n\n{body}\n"),
        )?;
        Ok(())
    }

    async fn delete(&self, name: &str) -> Result<()> {
        let skill = self
            .get(name)
            .await?
            .ok_or_else(|| HermesError::Skill(format!("skill `{name}` tidak ditemukan")))?;
        std::fs::remove_dir_all(&skill.path)?;
        Ok(())
    }

    async fn find_relevant(&self, text: &str, limit: usize) -> Result<Vec<Skill>> {
        let text_lc = text.to_lowercase();
        let mut scored: Vec<(usize, Skill)> = self
            .offerable()
            .into_iter()
            .map(|s| {
                let mut score = 0;
                for t in &s.triggers {
                    if text_lc.contains(t.as_str()) {
                        score += 2;
                    }
                }
                // Bonus bila nama skill disebut eksplisit.
                if text_lc.contains(&s.name) {
                    score += 5;
                }
                (score, s)
            })
            .filter(|(score, _)| *score > 0)
            .collect();
        scored.sort_by_key(|a| std::cmp::Reverse(a.0));
        Ok(scored.into_iter().take(limit).map(|(_, s)| s).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("hermes-skill-test-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write_skill(dir: &Path, name: &str, frontmatter_extra: &str) {
        let sd = dir.join(name);
        std::fs::create_dir_all(&sd).unwrap();
        let fm = if frontmatter_extra.is_empty() {
            format!("---\nname: {name}\ndescription: skill {name}\n---\n")
        } else {
            format!("---\nname: {name}\ndescription: skill {name}\n{frontmatter_extra}---\n")
        };
        std::fs::write(sd.join("SKILL.md"), format!("{fm}\n# {name}\nbody\n")).unwrap();
    }

    fn names(skills: &[Skill]) -> Vec<&str> {
        skills.iter().map(|s| s.name.as_str()).collect()
    }

    #[tokio::test]
    async fn create_and_find() {
        let dir = tmp("create");
        let mgr = SkillManager::with_dirs(vec![dir.clone()]);
        mgr.create(
            "deploy-fly",
            "Deploy aplikasi ke Fly.io",
            "# Deploy\n1. fly deploy",
        )
        .await
        .unwrap();
        assert!(dir.join("deploy-fly").join("SKILL.md").is_file());
        let found = mgr
            .find_relevant("bagaimana cara deploy ke fly?", 3)
            .await
            .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "deploy-fly");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn frontmatter_parsed() {
        let raw = "---\nname: x\ndescription: d\n---\n\nbody here";
        let (fm, body) = parse_frontmatter(raw);
        assert_eq!(fm.name.as_deref(), Some("x"));
        assert!(body.contains("body here"));
    }

    #[test]
    fn frontmatter_platforms_parsed_list_and_scalar() {
        let raw = "---\nname: x\nplatforms: [macos, linux]\n---\nbody";
        let (fm, _) = parse_frontmatter(raw);
        assert!(matches!(fm.platforms, Some(PlatformList::Multiple(_))));

        let raw = "---\nname: x\nplatforms: darwin\n---\nbody";
        let (fm, _) = parse_frontmatter(raw);
        assert!(matches!(fm.platforms, Some(PlatformList::Single(_))));
    }

    #[test]
    fn platform_normalization() {
        assert_eq!(normalize_platform("darwin"), "macos");
        assert_eq!(normalize_platform("Darwin"), "macos");
        assert_eq!(normalize_platform("macos"), "macos");
        assert_eq!(normalize_platform("win32"), "windows");
        assert_eq!(normalize_platform("windows"), "windows");
        assert_eq!(normalize_platform("linux"), "linux");
        // Tak dikenal: dipertahankan apa adanya (tak akan cocok).
        assert_eq!(normalize_platform("plan9"), "plan9");
    }

    #[tokio::test]
    async fn platform_gate_skips_mismatching_os() {
        let dir = tmp("platform");
        let current = std::env::consts::OS;
        write_skill(&dir, "plan9-only", "platforms: [plan9]\n");
        write_skill(&dir, "scalar-plan9", "platforms: plan9\n");
        write_skill(&dir, "current-os", &format!("platforms: [{current}]\n"));
        write_skill(&dir, "darwin-alias", "platforms: [darwin]\n");
        write_skill(&dir, "no-gate", "");

        let mgr = SkillManager::with_dirs(vec![dir.clone()]);
        let listed = mgr.list().await.unwrap();
        let n = names(&listed);
        assert!(!n.contains(&"plan9-only"), "plan9 harus dilewati: {n:?}");
        assert!(
            !n.contains(&"scalar-plan9"),
            "skalar plan9 harus dilewati: {n:?}"
        );
        assert!(n.contains(&"current-os"), "OS saat ini harus lolos: {n:?}");
        assert!(n.contains(&"no-gate"), "tanpa platforms harus lolos: {n:?}");
        // darwin -> macos: lolos hanya bila OS saat ini macos.
        assert_eq!(
            n.contains(&"darwin-alias"),
            current == "macos",
            "normalisasi darwin salah: {n:?}"
        );

        // get() juga menerapkan gate platform.
        assert!(mgr.get("plan9-only").await.unwrap().is_none());
        assert!(mgr.get("current-os").await.unwrap().is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn skills_config(disabled: &[&str], extra_dirs: &[String]) -> SkillsConfig {
        SkillsConfig {
            disabled_skills: disabled.iter().map(|s| s.to_string()).collect(),
            extra_skill_dirs: extra_dirs.to_vec(),
        }
    }

    #[tokio::test]
    async fn disabled_skills_absent_from_list() {
        let dir = tmp("disabled");
        write_skill(&dir, "gone-skill", "");
        write_skill(&dir, "keep-skill", "");

        let mgr = SkillManager::with_dirs(vec![dir.clone()])
            .with_skills_config(&dir, &skills_config(&["gone-skill"], &[]));
        let listed = mgr.list().await.unwrap();
        let n = names(&listed);
        assert!(
            !n.contains(&"gone-skill"),
            "skill disable harus absen: {n:?}"
        );
        assert!(n.contains(&"keep-skill"), "skill lain harus ada: {n:?}");
        assert!(mgr.get("gone-skill").await.unwrap().is_none());
        assert!(mgr
            .find_relevant("gone-skill keep-skill", 5)
            .await
            .unwrap()
            .iter()
            .all(|s| s.name != "gone-skill"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn essential_skill_cannot_be_disabled() {
        let dir = tmp("essential");
        write_skill(&dir, "hermes-agent", "");

        let mgr = SkillManager::with_dirs(vec![dir.clone()])
            .with_skills_config(&dir, &skills_config(&["hermes-agent"], &[]));
        let listed = mgr.list().await.unwrap();
        assert!(
            names(&listed).contains(&"hermes-agent"),
            "hermes-agent tak bisa dinonaktifkan"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn extra_skill_dirs_discovered() {
        let main = tmp("extra-main");
        let extra = tmp("extra-dir");
        write_skill(&main, "main-skill", "");
        write_skill(&extra, "extra-skill", "");
        // Nama sama di extra dir: tidak membayangi main dir (prioritas).
        write_skill(&extra, "main-skill", "");

        let cfg = skills_config(&[], &[extra.to_string_lossy().into_owned()]);
        let mgr = SkillManager::with_dirs(vec![main.clone()]).with_skills_config(&main, &cfg);
        let listed = mgr.list().await.unwrap();
        let n = names(&listed);
        assert!(n.contains(&"main-skill"), "skill main harus ada: {n:?}");
        assert!(
            n.contains(&"extra-skill"),
            "skill extra dir harus ketemu: {n:?}"
        );
        // Prioritas: versi main dir yang dipakai.
        let main_skill = mgr.get("main-skill").await.unwrap().unwrap();
        assert!(
            main_skill.path.starts_with(&main),
            "bayangan extra dir tak boleh menang: {:?}",
            main_skill.path
        );

        let _ = std::fs::remove_dir_all(&main);
        let _ = std::fs::remove_dir_all(&extra);
    }

    #[tokio::test]
    async fn extra_skill_dir_missing_is_skipped() {
        let dir = tmp("extra-missing");
        write_skill(&dir, "real-skill", "");
        let missing = dir.join("tidak-ada");

        let cfg = skills_config(&[], &[missing.to_string_lossy().into_owned()]);
        let mgr = SkillManager::with_dirs(vec![dir.clone()]).with_skills_config(&dir, &cfg);
        let listed = mgr.list().await.unwrap();
        assert_eq!(names(&listed), vec!["real-skill"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn environments_gate_offer_only() {
        // Pastikan detektor kanban nonaktif di test ini.
        std::env::remove_var("HERMES_KANBAN_TASK");
        std::env::remove_var("HERMES_KANBAN_BOARD");

        let dir = tmp("env");
        write_skill(&dir, "kanban-skill", "environments: [kanban]\n");
        write_skill(&dir, "unknown-env", "environments: [frobnicate]\n");
        write_skill(&dir, "plain-skill", "");

        let mgr = SkillManager::with_dirs(vec![dir.clone()]);
        let listed = mgr.list().await.unwrap();
        let n = names(&listed);
        assert!(
            !n.contains(&"kanban-skill"),
            "environment tak aktif harus disaring dari list: {n:?}"
        );
        assert!(
            n.contains(&"unknown-env"),
            "tag tak dikenal fail-open: {n:?}"
        );
        assert!(n.contains(&"plain-skill"));

        // find_relevant juga permukaan penawaran.
        let rel = mgr.find_relevant("kanban-skill", 5).await.unwrap();
        assert!(rel.iter().all(|s| s.name != "kanban-skill"));

        // get() eksplisit: gate environments TIDAK berlaku (meniru aslinya).
        assert!(mgr.get("kanban-skill").await.unwrap().is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn update_preserves_frontmatter_gates() {
        let dir = tmp("update");
        write_skill(&dir, "gated", "platforms: [linux, macos, windows]\n");

        let mgr = SkillManager::with_dirs(vec![dir.clone()]);
        mgr.update("gated", "body baru").await.unwrap();
        let raw = std::fs::read_to_string(dir.join("gated").join("SKILL.md")).unwrap();
        assert!(
            raw.contains("platforms:"),
            "frontmatter harus dipertahankan:\n{raw}"
        );
        assert!(raw.contains("body baru"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn expand_env_basic() {
        std::env::set_var("HERMES_SKILL_TEST_VAR", "nilaitest");
        assert_eq!(expand_env("$HERMES_SKILL_TEST_VAR/sub"), "nilaitest/sub");
        assert_eq!(expand_env("${HERMES_SKILL_TEST_VAR}/sub"), "nilaitest/sub");
        // Variabel tak dikenal -> kosong (ala shell); `$` soliter literal.
        assert_eq!(expand_env("$HERMES_SKILL_TEST_VAR_TAK_ADA/x"), "/x");
        assert_eq!(expand_env("harga 5$"), "harga 5$");
        std::env::remove_var("HERMES_SKILL_TEST_VAR");
    }
}
