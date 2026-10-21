//! Handler tools.
//! Dipindah dari main.rs (refactor batas ukuran file).

use super::cli::ToolsCmd;
use crate::app::save_config;
use crate::app::App;

// ---------------------------------------------------------------------------
// B7 (tier 1): `hermes tools` — daftar & kelola tool (paritas `tools list`
// aslinya; enable/disable memakai [skills].disabled_skills karena port ini
// belum punya pemetaan toolset sehalus aslinya).
// ---------------------------------------------------------------------------

pub(crate) async fn cmd_tools(app: &mut App, cmd: &ToolsCmd) -> anyhow::Result<()> {
    match cmd {
        ToolsCmd::List => {
            let mut names = app.tools.names();
            names.sort();
            for n in &names {
                match app.tools.get(n) {
                    Some(t) => {
                        let desc: String = t.description().chars().take(90).collect();
                        println!("{n}\n    {desc}");
                    }
                    None => println!("{n}"),
                }
            }
            println!("\n{} tool terdaftar.", names.len());
        }
        ToolsCmd::Disable { name } => {
            let disabled = &mut app.config_raw.skills.disabled_skills;
            if disabled.iter().any(|s| s.eq_ignore_ascii_case(name)) {
                println!("Skill `{name}` sudah nonaktif.");
            } else {
                disabled.push(name.clone());
                save_config(&app.config_raw, &app.hermes_home)?;
                println!("Skill `{name}` dinonaktifkan.");
            }
        }
        ToolsCmd::Enable { name } => {
            let disabled = &mut app.config_raw.skills.disabled_skills;
            let before = disabled.len();
            disabled.retain(|s| !s.eq_ignore_ascii_case(name));
            if disabled.len() != before {
                save_config(&app.config_raw, &app.hermes_home)?;
                println!("Skill `{name}` diaktifkan kembali.");
            } else {
                println!("Skill `{name}` tidak ada di daftar nonaktif.");
            }
        }
    }
    Ok(())
}
