//! Configuração persistida em TOML.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::audio::AudioMode;
use crate::format::{Container, Quality};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("TOML inválido: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("não foi possível serializar: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("erro de E/S: {0}")]
    Io(#[from] std::io::Error),
    #[error("valor inválido em `{field}`: {reason}")]
    Invalid { field: &'static str, reason: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Quantos minutos o modo replay mantém.
    pub buffer_minutes: u32,
    pub output_dir: PathBuf,
    pub fps: u32,
    pub container: Container,
    pub quality: Quality,
    pub audio: AudioMode,
    /// Aplicativo (`application.name`) cujo som é gravado; `None` = áudio do sistema.
    pub audio_app: Option<String>,
    /// Microfone (`node.name` do PipeWire); `None` = o padrão do sistema.
    pub mic_source: Option<String>,
    /// Com um app escolhido, grava faixas separadas e pergunta sobre a música ao salvar.
    pub ask_music: bool,
    /// Ganho do jogo na gravação, em % (0 a 200).
    pub game_volume: u32,
    /// Ganho do microfone na gravação, em % (0 a 200).
    pub mic_volume: u32,
    /// Grava também a call (apps de voz como o Discord).
    pub record_call: bool,
    /// Ganho da call na gravação, em % (0 a 200).
    pub call_volume: u32,
    /// Quantos segundos o botão "salvar clip" do modo replay recupera.
    pub clip_seconds: u32,
}

impl Default for Config {
    fn default() -> Self {
        let home = std::env::var("HOME").unwrap_or_default();
        let config_home = std::env::var("XDG_CONFIG_HOME").ok().filter(|d| !d.is_empty());
        let user_dirs = config_home
            .map_or_else(|| Path::new(&home).join(".config"), PathBuf::from)
            .join("user-dirs.dirs");
        let videos = std::fs::read_to_string(user_dirs)
            .ok()
            .and_then(|c| xdg_videos_dir(&c, &home))
            .unwrap_or_else(|| Path::new(&home).join("Videos"));
        Self {
            buffer_minutes: 10,
            output_dir: videos.join("Catchback"),
            fps: 60,
            container: Container::default(),
            quality: Quality::default(),
            audio: AudioMode::default(),
            audio_app: None,
            mic_source: None,
            ask_music: true,
            game_volume: 100,
            mic_volume: 100,
            record_call: false,
            call_volume: 100,
            clip_seconds: 120,
        }
    }
}

fn check(field: &'static str, value: u32, range: std::ops::RangeInclusive<u32>) -> Result<(), ConfigError> {
    if range.contains(&value) {
        Ok(())
    } else {
        Err(ConfigError::Invalid {
            field,
            reason: format!("{value} fora do intervalo {}..={}", range.start(), range.end()),
        })
    }
}

impl Config {
    pub fn buffer_window(&self) -> Duration {
        Duration::from_secs(u64::from(self.buffer_minutes) * 60)
    }

    /// Quanto o clip salvo recupera: `clip_seconds`, limitado pela janela do buffer.
    pub fn clip_span(&self) -> Duration {
        Duration::from_secs(u64::from(self.clip_seconds)).min(self.buffer_window())
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        check("buffer_minutes", self.buffer_minutes, 1..=60)?;
        check("fps", self.fps, 1..=240)?;
        check("clip_seconds", self.clip_seconds, 5..=3600)?;
        check("game_volume", self.game_volume, 0..=200)?;
        check("mic_volume", self.mic_volume, 0..=200)?;
        check("call_volume", self.call_volume, 0..=200)
    }

    pub fn from_toml(s: &str) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(s)?;
        config.validate()?;
        Ok(config)
    }

    pub fn to_toml(&self) -> Result<String, ConfigError> {
        Ok(toml::to_string_pretty(self)?)
    }

    /// Lê o arquivo; se não existir, devolve o padrão.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(s) => Self::from_toml(&s),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Grava o arquivo, criando as pastas necessárias.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, self.to_toml()?)?;
        Ok(())
    }
}

/// Pasta de vídeos do usuário a partir do conteúdo de `user-dirs.dirs`.
pub fn xdg_videos_dir(user_dirs: &str, home: &str) -> Option<PathBuf> {
    let value = user_dirs
        .lines()
        .find_map(|l| l.trim().strip_prefix("XDG_VIDEOS_DIR="))?
        .trim()
        .trim_matches('"');
    (!value.is_empty()).then(|| PathBuf::from(value.replace("$HOME", home)))
}

/// Caminho padrão do arquivo de configuração (padrão XDG).
pub fn default_config_path(xdg_config_home: Option<&str>, home: &str) -> PathBuf {
    let base = match xdg_config_home {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => Path::new(home).join(".config"),
    };
    base.join("catchback").join("config.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn videos_dir_is_read_from_user_dirs_and_expands_home() {
        let content = "# comentário\nXDG_MUSIC_DIR=\"$HOME/Música\"\nXDG_VIDEOS_DIR=\"$HOME/Vídeos\"\n";
        assert_eq!(xdg_videos_dir(content, "/home/u"), Some(PathBuf::from("/home/u/Vídeos")));
    }

    #[test]
    fn videos_dir_accepts_absolute_paths() {
        assert_eq!(xdg_videos_dir("XDG_VIDEOS_DIR=\"/mnt/clips\"", "/home/u"), Some(PathBuf::from("/mnt/clips")));
    }

    #[test]
    fn videos_dir_missing_gives_none() {
        assert_eq!(xdg_videos_dir("XDG_MUSIC_DIR=\"$HOME/Música\"", "/home/u"), None);
        assert_eq!(xdg_videos_dir("", "/home/u"), None);
    }

    #[test]
    fn format_defaults_to_mp4_high_quality() {
        let c = Config::default();
        assert_eq!((c.container, c.quality), (Container::Mp4, Quality::High));
    }

    #[test]
    fn format_is_read_from_toml_in_lowercase() {
        let c = Config::from_toml("container = \"webm\"\nquality = \"max\"").unwrap();
        assert_eq!((c.container, c.quality), (Container::WebM, Quality::Max));
        assert!(Config::from_toml("container = \"avi\"").is_err());
    }

    #[test]
    fn format_survives_roundtrip() {
        let c = Config { container: Container::Mkv, quality: Quality::Light, ..Config::default() };
        assert_eq!(Config::from_toml(&c.to_toml().unwrap()).unwrap(), c);
    }

    #[test]
    fn old_config_with_bitrate_still_loads() {
        let c = Config::from_toml("buffer_minutes = 5\nbitrate_kbps = 8000").unwrap();
        assert_eq!(c.buffer_minutes, 5);
        assert_eq!(c.quality, Quality::High);
    }

    #[test]
    fn ask_music_is_on_by_default_and_roundtrips() {
        assert!(Config::default().ask_music);
        let c = Config { ask_music: false, ..Config::default() };
        assert_eq!(Config::from_toml(&c.to_toml().unwrap()).unwrap(), c);
        assert!(Config::from_toml("buffer_minutes = 5").unwrap().ask_music);
    }

    #[test]
    fn mic_source_defaults_to_the_system_default_and_roundtrips() {
        assert_eq!(Config::default().mic_source, None);
        let c = Config { mic_source: Some("bluez_input.84:AC:60:D7:C8:B2".into()), ..Config::default() };
        let toml = c.to_toml().unwrap();
        assert!(toml.contains("mic_source = \"bluez_input.84:AC:60:D7:C8:B2\""), "{toml}");
        assert_eq!(Config::from_toml(&toml).unwrap(), c);
    }

    #[test]
    fn the_call_is_not_recorded_by_default_since_it_has_other_people_voices() {
        let c = Config::default();
        assert!(!c.record_call);
        assert_eq!(c.call_volume, 100);
        let c = Config { record_call: true, call_volume: 70, ..Config::default() };
        assert_eq!(Config::from_toml(&c.to_toml().unwrap()).unwrap(), c);
        assert!(matches!(
            Config { call_volume: 201, ..Config::default() }.validate(),
            Err(ConfigError::Invalid { field: "call_volume", .. })
        ));
    }

    #[test]
    fn volumes_default_to_unity_and_accept_zero_to_200() {
        let c = Config::default();
        assert_eq!((c.game_volume, c.mic_volume), (100, 100));
        for v in [0, 200] {
            assert!(Config { game_volume: v, mic_volume: v, ..Config::default() }.validate().is_ok());
        }
    }

    #[test]
    fn rejects_volume_above_200() {
        assert!(matches!(
            Config { game_volume: 201, ..Config::default() }.validate(),
            Err(ConfigError::Invalid { field: "game_volume", .. })
        ));
        assert!(matches!(
            Config { mic_volume: 201, ..Config::default() }.validate(),
            Err(ConfigError::Invalid { field: "mic_volume", .. })
        ));
    }

    #[test]
    fn volumes_survive_roundtrip() {
        let c = Config { game_volume: 60, mic_volume: 150, ..Config::default() };
        assert_eq!(Config::from_toml(&c.to_toml().unwrap()).unwrap(), c);
    }

    #[test]
    fn audio_is_off_by_default_so_the_mic_is_never_recorded_unasked() {
        let c = Config::default();
        assert_eq!(c.audio, AudioMode::Off);
        assert_eq!(c.audio_app, None);
    }

    #[test]
    fn audio_settings_survive_roundtrip() {
        let c = Config { audio: AudioMode::GameAndMic, audio_app: Some("pxgme-linux".into()), ..Config::default() };
        let toml = c.to_toml().unwrap();
        assert!(toml.contains("audio = \"game_and_mic\""), "{toml}");
        assert_eq!(Config::from_toml(&toml).unwrap(), c);
        let c = Config { audio_app: None, ..c };
        assert_eq!(Config::from_toml(&c.to_toml().unwrap()).unwrap(), c);
    }

    #[test]
    fn clip_span_defaults_to_two_minutes() {
        assert_eq!(Config::default().clip_span(), Duration::from_secs(120));
    }

    #[test]
    fn clip_span_is_capped_by_buffer_window() {
        let c = Config { buffer_minutes: 1, clip_seconds: 300, ..Config::default() };
        assert_eq!(c.clip_span(), Duration::from_secs(60));
    }

    #[test]
    fn rejects_out_of_range_clip_seconds() {
        for v in [0, 4, 3601] {
            let c = Config { clip_seconds: v, ..Config::default() };
            assert!(matches!(c.validate(), Err(ConfigError::Invalid { field: "clip_seconds", .. })), "{v}");
        }
    }

    #[test]
    fn config_path_prefers_xdg_then_home() {
        assert_eq!(
            default_config_path(Some("/x/cfg"), "/home/u"),
            PathBuf::from("/x/cfg/catchback/config.toml")
        );
        assert_eq!(
            default_config_path(None, "/home/u"),
            PathBuf::from("/home/u/.config/catchback/config.toml")
        );
        assert_eq!(
            default_config_path(Some(""), "/home/u"),
            PathBuf::from("/home/u/.config/catchback/config.toml")
        );
    }

    #[test]
    fn default_is_valid_and_ten_minutes() {
        let c = Config::default();
        assert!(c.validate().is_ok());
        assert_eq!(c.buffer_minutes, 10);
        assert_eq!(c.buffer_window(), Duration::from_secs(600));
    }

    #[test]
    fn partial_toml_falls_back_to_defaults() {
        let c = Config::from_toml("buffer_minutes = 5").unwrap();
        assert_eq!(c.buffer_minutes, 5);
        assert_eq!(c.fps, Config::default().fps);
    }

    #[test]
    fn toml_roundtrip() {
        let c = Config { buffer_minutes: 3, fps: 30, ..Config::default() };
        assert_eq!(Config::from_toml(&c.to_toml().unwrap()).unwrap(), c);
    }

    #[test]
    fn malformed_toml_is_an_error() {
        assert!(matches!(Config::from_toml("buffer_minutes = \"x\""), Err(ConfigError::Parse(_))));
    }

    #[test]
    fn rejects_out_of_range_values() {
        for c in [
            Config { buffer_minutes: 0, ..Config::default() },
            Config { buffer_minutes: 61, ..Config::default() },
            Config { fps: 0, ..Config::default() },
            Config { fps: 241, ..Config::default() },
        ] {
            assert!(matches!(c.validate(), Err(ConfigError::Invalid { .. })), "{c:?}");
        }
    }

    #[test]
    fn from_toml_validates() {
        assert!(matches!(Config::from_toml("fps = 0"), Err(ConfigError::Invalid { field: "fps", .. })));
    }

    #[test]
    fn load_missing_file_gives_default() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Config::load(&dir.path().join("nope.toml")).unwrap(), Config::default());
    }

    #[test]
    fn save_then_load_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b/config.toml");
        let c = Config { buffer_minutes: 7, ..Config::default() };
        c.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), c);
    }
}
