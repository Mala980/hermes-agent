//! Handler voice.
//! Dipindah dari main.rs (refactor batas ukuran file).

use super::cli::VoiceCmd;

pub(crate) async fn cmd_voice(cmd: VoiceCmd) -> anyhow::Result<()> {
    match cmd {
        VoiceCmd::Say { text, out, voice } => {
            let audio = hermes_tools::media::tts_bytes(&text, "tts-1", &voice, "mp3").await?;
            let path = out.unwrap_or_else(|| {
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                format!("speech-{ts}.mp3")
            });
            std::fs::write(&path, &audio)?;
            println!("Audio disimpan: {path} ({} bytes)", audio.len());
            Ok(())
        }
        VoiceCmd::Listen { file, language } => {
            let bytes = std::fs::read(&file)?;
            let filename = std::path::Path::new(&file)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("audio.bin");
            let text = hermes_tools::media::transcribe_bytes(
                &bytes,
                filename,
                "whisper-1",
                language.as_deref(),
            )
            .await?;
            println!("{text}");
            Ok(())
        }
    }
}
