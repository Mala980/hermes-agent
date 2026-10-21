//! Handler model.
//! Dipindah dari main.rs (refactor batas ukuran file).

use crate::app::expand_config_env;
use crate::app::save_config;
use crate::app::App;

pub(crate) async fn cmd_model(app: &mut App, name: Option<String>) -> anyhow::Result<()> {
    match name {
        Some(n) => {
            let profile = app
                .config_raw
                .profiles
                .entry(app.profile_name.clone())
                .or_default();
            profile.model = n.clone();
            save_config(&app.config_raw, &app.hermes_home)?;
            app.config = expand_config_env(app.config_raw.clone())?;
            println!("Model profil `{}` → {n}", app.profile_name);
        }
        None => {
            println!(
                "Provider: {}  |  Model: {}",
                app.profile.provider, app.profile.model
            );
        }
    }
    Ok(())
}
