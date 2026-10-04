//! Áudio da gravação: modos, descoberta de aplicativos e plano de captura.

use std::collections::BTreeMap;
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::config::Config;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioMode {
    #[default]
    Off,
    /// Só o som do jogo (ou do sistema).
    Game,
    GameAndMic,
    /// Só o microfone.
    Mic,
}

impl AudioMode {
    pub const ALL: [AudioMode; 4] = [AudioMode::Off, AudioMode::Game, AudioMode::GameAndMic, AudioMode::Mic];

    pub fn label(self) -> &'static str {
        match self {
            AudioMode::Off => "Sem áudio",
            AudioMode::Game => "Só o som do jogo",
            AudioMode::GameAndMic => "Som do jogo + microfone",
            AudioMode::Mic => "Só o microfone",
        }
    }

    pub fn has_game(self) -> bool {
        matches!(self, AudioMode::Game | AudioMode::GameAndMic)
    }

    pub fn has_mic(self) -> bool {
        matches!(self, AudioMode::GameAndMic | AudioMode::Mic)
    }
}

impl AudioMode {
    /// Combina "gravar o jogo" e "gravar o microfone" (as duas chaves da interface).
    pub fn from_flags(game: bool, mic: bool) -> Self {
        match (game, mic) {
            (false, false) => AudioMode::Off,
            (true, false) => AudioMode::Game,
            (true, true) => AudioMode::GameAndMic,
            (false, true) => AudioMode::Mic,
        }
    }
}

/// Apps de chamada de voz (a "call" com os amigos): não são jogo nem música.
const VOICE_CHAT: [&str; 11] = [
    "discord", "webcord", "vesktop", "armcord", "teamspeak", "mumble", "zoom", "teams", "skype", "slack",
    "webrtc voiceengine",
];

pub fn is_voice_chat(name: &str) -> bool {
    let name = name.to_lowercase();
    VOICE_CHAT.iter().any(|v| name.contains(v))
}

/// Aplicativo que está tocando áudio agora (um app pode ter vários fluxos).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioApp {
    pub name: String,
    pub serials: Vec<u32>,
    /// Algum fluxo do app está de fato tocando (não pausado/ocioso).
    pub playing: bool,
}

/// De onde vem o som do jogo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GameAudio {
    /// Tudo que sai nas caixas/fone (inclui a call do Discord).
    System,
    /// Apenas os fluxos (serial do PipeWire) do aplicativo escolhido.
    App(Vec<u32>),
}

/// O que gravar de áudio nesta captura.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioPlan {
    pub game: Option<GameAudio>,
    pub mic: bool,
    /// Ganho do jogo na gravação, em % (100 = sem alteração).
    pub game_volume: u32,
    /// Ganho do microfone na gravação, em %.
    pub mic_volume: u32,
    /// Microfone a usar (`node.name`); `None` = o padrão do sistema.
    pub mic_source: Option<String>,
    /// Fluxos (serial do PipeWire) dos apps de call a gravar; `None` = sem call.
    pub call: Option<Vec<u32>>,
    /// Ganho da call na gravação, em %.
    pub call_volume: u32,
    /// Grava jogo, sistema e microfone em faixas separadas, para decidir sobre a
    /// música só ao salvar. Só faz sentido com um aplicativo escolhido.
    pub separate_tracks: bool,
}

impl Default for AudioPlan {
    fn default() -> Self {
        Self {
            game: None,
            mic: false,
            game_volume: 100,
            mic_volume: 100,
            mic_source: None,
            call: None,
            call_volume: 100,
            separate_tracks: false,
        }
    }
}

/// Faixas de áudio no arquivo, na ordem em que são gravadas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    Game,
    /// Todo o áudio do sistema (jogo + música + call).
    System,
    /// Só os apps de voz (a call com os amigos).
    Call,
    Mic,
}

/// O que fazer com a música ao salvar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MusicChoice {
    /// Só o jogo (e o microfone): o que não for do jogo fica de fora.
    Drop,
    /// Todo o áudio do sistema (e o microfone).
    Keep,
}

/// Uma faixa do arquivo gravado com o ganho a aplicar na mixagem final.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackGain {
    /// Índice da faixa de áudio no arquivo (0 = primeira).
    pub index: usize,
    pub percent: u32,
}

impl AudioPlan {
    pub fn is_silent(&self) -> bool {
        self.game.is_none() && !self.mic && self.call.is_none()
    }

    /// Faixas gravadas, em ordem; vazio quando o áudio já sai misturado numa só.
    pub fn tracks(&self) -> Vec<TrackKind> {
        if !self.separate_tracks {
            return Vec::new();
        }
        let mut tracks = vec![TrackKind::Game, TrackKind::System];
        if self.call.is_some() {
            tracks.push(TrackKind::Call);
        }
        if self.mic {
            tracks.push(TrackKind::Mic);
        }
        tracks
    }

    /// Faixas (com ganho) que compõem o áudio final. `None` quando não há o que
    /// remixar, ou seja, o arquivo já tem a mixagem pronta.
    pub fn final_mix(&self, music: MusicChoice) -> Option<Vec<TrackGain>> {
        let tracks = self.tracks();
        if tracks.is_empty() {
            return None;
        }
        Some(
            tracks
                .iter()
                .enumerate()
                .filter_map(|(index, kind)| {
                    let percent = match (kind, music) {
                        (TrackKind::Game, MusicChoice::Drop) | (TrackKind::System, MusicChoice::Keep) => self.game_volume,
                        // Sem a música, a call entra pela faixa própria; ao manter, ela já está no sistema.
                        (TrackKind::Call, MusicChoice::Drop) => self.call_volume,
                        (TrackKind::Mic, _) => self.mic_volume,
                        _ => return None,
                    };
                    Some(TrackGain { index, percent })
                })
                .collect(),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AudioError {
    #[error("`{0}` não está tocando áudio agora; abra o jogo ou escolha o áudio do sistema")]
    AppNotPlaying(String),
    #[error("não foi possível listar os aplicativos de áudio: instale o `pactl` (libpulse) ou o `pw-dump` (pipewire)")]
    AppListUnavailable,
}

/// Nenhuma ferramenta (`pactl` ou `pw-dump`) respondeu.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("nem o `pactl` nem o `pw-dump` estão disponíveis")]
pub struct AppListError;

/// Resultado da listagem de aplicativos de áudio.
pub type AppList = Result<Vec<AudioApp>, AppListError>;

/// Interpreta a saída de `pactl -f json list sink-inputs`.
pub fn parse_apps(json: &str) -> Vec<AudioApp> {
    match serde_json::from_str(json) {
        Ok(serde_json::Value::Array(inputs)) => group_streams(&inputs),
        _ => Vec::new(),
    }
}

/// Agrupa fluxos (`{"properties": {...}}`) por aplicativo, ordenando por nome.
fn group_streams(streams: &[serde_json::Value]) -> Vec<AudioApp> {
    let mut by_name: BTreeMap<String, (Vec<u32>, bool)> = BTreeMap::new();
    for stream in streams {
        let props = &stream["properties"];
        let name = props["application.name"].as_str().or_else(|| props["node.name"].as_str());
        let serial = match &props["object.serial"] {
            serde_json::Value::String(s) => s.parse().ok(),
            serde_json::Value::Number(n) => n.as_u64().and_then(|n| u32::try_from(n).ok()),
            _ => None,
        };
        let playing = stream["corked"].as_bool() != Some(true);
        if let (Some(name), Some(serial)) = (name, serial) {
            let entry = by_name.entry(name.to_string()).or_default();
            entry.0.push(serial);
            entry.1 |= playing;
        }
    }
    by_name
        .into_iter()
        .map(|(name, (mut serials, playing))| {
            serials.sort_unstable();
            AudioApp { name, serials, playing }
        })
        .collect()
}

/// Interpreta a saída de `pw-dump` (nós `Stream/Output/Audio`).
pub fn parse_pw_dump(json: &str) -> Vec<AudioApp> {
    let Ok(serde_json::Value::Array(objects)) = serde_json::from_str(json) else {
        return Vec::new();
    };
    // Mesma estrutura do `pactl` (`properties`), então reaproveita o agrupamento.
    let streams: Vec<serde_json::Value> = objects
        .iter()
        .map(|o| (&o["info"]["props"], o["info"]["state"].as_str().unwrap_or("running")))
        .filter(|(props, _)| props["media.class"] == "Stream/Output/Audio")
        .map(|(props, state)| serde_json::json!({ "properties": props, "corked": state != "running" }))
        .collect();
    group_streams(&streams)
}

/// Executa um programa e devolve o stdout; `None` se faltar ou falhar.
pub type Runner<'a> = &'a dyn Fn(&str, &[&str]) -> Option<String>;

/// Lista os apps com `pactl` e, se ele não responder, com `pw-dump`.
pub fn list_apps_with(run: Runner) -> AppList {
    if let Some(out) = run("pactl", &["-f", "json", "list", "sink-inputs"]) {
        return Ok(parse_apps(&out));
    }
    run("pw-dump", &[]).map(|out| parse_pw_dump(&out)).ok_or(AppListError)
}

/// Roda `f` com um executor que chama os programas de verdade (`pactl`, `pw-dump`).
pub fn run_with_system_tools<T>(f: impl FnOnce(Runner) -> T) -> T {
    f(&|program, args| {
        Command::new(program)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
    })
}

/// Aplicativos tocando áudio agora.
pub fn list_apps() -> AppList {
    run_with_system_tools(list_apps_with)
}

/// Decide o que gravar a partir da configuração e dos apps em execução.
pub fn plan(config: &Config, apps: &AppList) -> Result<AudioPlan, AudioError> {
    let game = if config.audio.has_game() {
        Some(match &config.audio_app {
            None => GameAudio::System,
            Some(name) => {
                let apps = apps.as_ref().map_err(|_| AudioError::AppListUnavailable)?;
                let app = apps
                    .iter()
                    .find(|a| &a.name == name)
                    .ok_or_else(|| AudioError::AppNotPlaying(name.clone()))?;
                GameAudio::App(app.serials.clone())
            }
        })
    } else {
        None
    };
    // Com o áudio do sistema inteiro a call já está dentro; senão, pega os apps de voz.
    let call = (config.record_call && !matches!(game, Some(GameAudio::System)))
        .then(|| {
            let serials: Vec<u32> = apps
                .as_ref()
                .map(|apps| apps.iter().filter(|a| is_voice_chat(&a.name)).flat_map(|a| a.serials.clone()).collect())
                .unwrap_or_default();
            (!serials.is_empty()).then_some(serials)
        })
        .flatten();
    Ok(AudioPlan {
        game: game.clone(),
        mic: config.audio.has_mic(),
        call,
        call_volume: config.call_volume,
        game_volume: config.game_volume,
        mic_volume: config.mic_volume,
        mic_source: config.mic_source.clone(),
        separate_tracks: config.ask_music && matches!(game, Some(GameAudio::App(_))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PACTL: &str = r#"[
      {"index": 203, "properties": {"application.name": "pxgme-linux", "node.name": "pxgme-linux", "object.serial": "203"}},
      {"index": 300, "properties": {"application.name": "Firefox", "object.serial": "300"}},
      {"index": 301, "properties": {"application.name": "Firefox", "object.serial": 301}},
      {"index": 400, "properties": {"node.name": "so-node-name", "object.serial": "400"}},
      {"index": 500, "properties": {"object.serial": "500"}},
      {"index": 600, "properties": {"application.name": "SemSerial"}}
    ]"#;

    fn app(name: &str, serials: &[u32]) -> AudioApp {
        AudioApp { name: name.into(), serials: serials.to_vec(), playing: true }
    }

    fn config(audio: AudioMode, app: Option<&str>) -> Config {
        Config { audio, audio_app: app.map(String::from), ..Config::default() }
    }

    #[test]
    fn mode_flags() {
        let flags: Vec<_> = AudioMode::ALL.iter().map(|m| (m.has_game(), m.has_mic())).collect();
        assert_eq!(flags, [(false, false), (true, false), (true, true), (false, true)]);
    }

    #[test]
    fn mode_labels_are_distinct() {
        let labels: std::collections::HashSet<_> = AudioMode::ALL.iter().map(|m| m.label()).collect();
        assert_eq!(labels.len(), AudioMode::ALL.len());
    }

    #[test]
    fn parse_groups_streams_by_app_and_sorts_by_name() {
        let apps = parse_apps(PACTL);
        assert_eq!(apps, vec![app("Firefox", &[300, 301]), app("pxgme-linux", &[203]), app("so-node-name", &[400])]);
    }

    #[test]
    fn parse_garbage_gives_empty_list() {
        assert!(parse_apps("").is_empty());
        assert!(parse_apps("não é json").is_empty());
        assert!(parse_apps("{}").is_empty());
    }

    #[test]
    fn off_records_no_audio() {
        let p = plan(&config(AudioMode::Off, Some("x")), &Ok(vec![])).unwrap();
        assert!(p.is_silent());
    }

    #[test]
    fn mic_only_ignores_the_selected_app() {
        let p = plan(&config(AudioMode::Mic, Some("inexistente")), &Err(AppListError)).unwrap();
        assert_eq!(p, AudioPlan { game: None, mic: true, ..AudioPlan::default() });
    }

    #[test]
    fn game_without_app_uses_system_audio() {
        let p = plan(&config(AudioMode::Game, None), &Err(AppListError)).unwrap();
        assert_eq!(p, AudioPlan { game: Some(GameAudio::System), ..AudioPlan::default() });
    }

    #[test]
    fn game_with_app_uses_all_its_streams() {
        let apps = [app("Firefox", &[300, 301]), app("pxgme-linux", &[203])];
        let p = plan(&config(AudioMode::GameAndMic, Some("Firefox")), &Ok(apps.to_vec())).unwrap();
        assert_eq!(
            p,
            AudioPlan { game: Some(GameAudio::App(vec![300, 301])), mic: true, separate_tracks: true, ..AudioPlan::default() }
        );
    }

    #[test]
    fn missing_app_is_an_error_instead_of_silently_recording_nothing() {
        let err = plan(&config(AudioMode::Game, Some("pxgme-linux")), &Ok(vec![])).unwrap_err();
        assert_eq!(err, AudioError::AppNotPlaying("pxgme-linux".into()));
        assert!(err.to_string().contains("pxgme-linux"));
    }

    #[test]
    fn list_apps_never_panics() {
        let _ = list_apps();
    }

    #[test]
    fn plan_carries_the_configured_volumes() {
        let c = Config { game_volume: 70, mic_volume: 130, ..config(AudioMode::GameAndMic, None) };
        let p = plan(&c, &Ok(vec![])).unwrap();
        assert_eq!((p.game_volume, p.mic_volume), (70, 130));
    }

    #[test]
    fn paused_streams_are_not_playing() {
        let json = r#"[
          {"properties": {"application.name": "Spotify", "object.serial": "1"}, "corked": true},
          {"properties": {"application.name": "Firefox", "object.serial": "2"}, "corked": true},
          {"properties": {"application.name": "Firefox", "object.serial": "3"}, "corked": false},
          {"properties": {"application.name": "Jogo", "object.serial": "4"}}
        ]"#;
        let playing: Vec<_> = parse_apps(json).into_iter().map(|a| (a.name, a.playing)).collect();
        assert_eq!(
            playing,
            [("Firefox".into(), true), ("Jogo".into(), true), ("Spotify".into(), false)]
        );
    }

    #[test]
    fn pw_dump_idle_streams_are_not_playing() {
        let json = r#"[
          {"type": "PipeWire:Interface:Node", "info": {"state": "idle", "props": {
             "media.class": "Stream/Output/Audio", "application.name": "Spotify", "object.serial": 1}}},
          {"type": "PipeWire:Interface:Node", "info": {"state": "running", "props": {
             "media.class": "Stream/Output/Audio", "application.name": "Jogo", "object.serial": 2}}}
        ]"#;
        let playing: Vec<_> = parse_pw_dump(json).into_iter().map(|a| (a.name, a.playing)).collect();
        assert_eq!(playing, [("Jogo".into(), true), ("Spotify".into(), false)]);
    }

    fn cfg_with_app(ask_music: bool) -> Config {
        Config { ask_music, ..config(AudioMode::GameAndMic, Some("pxgme-linux")) }
    }

    #[test]
    fn separate_tracks_only_with_a_chosen_app_and_ask_music_on() {
        let apps = Ok(vec![app("pxgme-linux", &[203])]);
        assert!(plan(&cfg_with_app(true), &apps).unwrap().separate_tracks);
        assert!(!plan(&cfg_with_app(false), &apps).unwrap().separate_tracks);
        // sem app escolhido o áudio do sistema é uma faixa só: não há como separar a música
        let system = Config { ask_music: true, ..config(AudioMode::Game, None) };
        assert!(!plan(&system, &apps).unwrap().separate_tracks);
        // só microfone: nada a decidir
        let mic = Config { ask_music: true, ..config(AudioMode::Mic, Some("pxgme-linux")) };
        assert!(!plan(&mic, &apps).unwrap().separate_tracks);
    }

    fn separate(mic: bool) -> AudioPlan {
        AudioPlan {
            game: Some(GameAudio::App(vec![1])),
            mic,
            game_volume: 80,
            mic_volume: 120,
            mic_source: None,
            call: None,
            call_volume: 100,
            separate_tracks: true,
        }
    }

    #[test]
    fn track_order_is_game_system_then_mic() {
        assert_eq!(separate(true).tracks(), [TrackKind::Game, TrackKind::System, TrackKind::Mic]);
        assert_eq!(separate(false).tracks(), [TrackKind::Game, TrackKind::System]);
        assert!(AudioPlan::default().tracks().is_empty());
    }

    #[test]
    fn dropping_music_mixes_game_and_mic() {
        assert_eq!(
            separate(true).final_mix(MusicChoice::Drop),
            Some(vec![TrackGain { index: 0, percent: 80 }, TrackGain { index: 2, percent: 120 }])
        );
    }

    #[test]
    fn keeping_music_mixes_system_and_mic() {
        assert_eq!(
            separate(true).final_mix(MusicChoice::Keep),
            Some(vec![TrackGain { index: 1, percent: 80 }, TrackGain { index: 2, percent: 120 }])
        );
        // sem microfone o índice não "desliza"
        assert_eq!(separate(false).final_mix(MusicChoice::Drop), Some(vec![TrackGain { index: 0, percent: 80 }]));
    }

    #[test]
    fn already_mixed_audio_needs_no_remix() {
        assert_eq!(AudioPlan::default().final_mix(MusicChoice::Drop), None);
        assert_eq!(AudioPlan { mic: true, ..AudioPlan::default() }.final_mix(MusicChoice::Keep), None);
    }

    #[test]
    fn modes_come_from_the_game_and_mic_switches() {
        assert_eq!(AudioMode::from_flags(false, false), AudioMode::Off);
        assert_eq!(AudioMode::from_flags(true, false), AudioMode::Game);
        assert_eq!(AudioMode::from_flags(true, true), AudioMode::GameAndMic);
        assert_eq!(AudioMode::from_flags(false, true), AudioMode::Mic);
        for m in AudioMode::ALL {
            assert_eq!(AudioMode::from_flags(m.has_game(), m.has_mic()), m);
        }
    }

    #[test]
    fn voice_chat_apps_are_recognized() {
        for name in ["Discord", "WEBRTC VoiceEngine", "vesktop", "Zoom"] {
            assert!(is_voice_chat(name), "{name}");
        }
        for name in ["Spotify", "pxgme-linux", "Firefox", "Steam"] {
            assert!(!is_voice_chat(name), "{name}");
        }
    }

    fn call_config(record_call: bool) -> Config {
        Config { record_call, call_volume: 70, ..config(AudioMode::Game, Some("pxgme-linux")) }
    }

    fn with_discord() -> AppList {
        Ok(vec![app("Discord", &[9, 10]), app("Spotify", &[5]), app("pxgme-linux", &[203])])
    }

    #[test]
    fn call_takes_every_stream_of_the_voice_apps() {
        let p = plan(&call_config(true), &with_discord()).unwrap();
        assert_eq!(p.call, Some(vec![9, 10]));
        assert_eq!(p.call_volume, 70);
    }

    #[test]
    fn call_is_left_out_unless_requested() {
        assert_eq!(plan(&call_config(false), &with_discord()).unwrap().call, None);
    }

    #[test]
    fn requested_call_with_no_voice_app_running_yields_no_call() {
        let apps = Ok(vec![app("pxgme-linux", &[203])]);
        assert_eq!(plan(&call_config(true), &apps).unwrap().call, None);
        // sem como listar os apps (nem pactl nem pw-dump) também não há call a gravar
        let no_game = Config { record_call: true, ..config(AudioMode::Off, None) };
        assert_eq!(plan(&no_game, &Err(AppListError)).unwrap().call, None);
    }

    #[test]
    fn with_system_audio_the_call_is_already_inside_so_there_is_no_call_track() {
        let c = Config { record_call: true, ..config(AudioMode::Game, None) };
        assert_eq!(plan(&c, &with_discord()).unwrap().call, None);
    }

    #[test]
    fn the_call_alone_is_a_valid_recording() {
        let c = Config { record_call: true, ..config(AudioMode::Off, None) };
        let p = plan(&c, &with_discord()).unwrap();
        assert_eq!(p.call, Some(vec![9, 10]));
        assert!(!p.is_silent());
    }

    fn separate_with_call(mic: bool) -> AudioPlan {
        AudioPlan { call: Some(vec![9]), call_volume: 60, ..separate(mic) }
    }

    #[test]
    fn call_track_sits_between_system_and_mic() {
        assert_eq!(
            separate_with_call(true).tracks(),
            [TrackKind::Game, TrackKind::System, TrackKind::Call, TrackKind::Mic]
        );
        assert_eq!(separate_with_call(false).tracks(), [TrackKind::Game, TrackKind::System, TrackKind::Call]);
    }

    #[test]
    fn without_the_music_the_call_still_comes_through_its_own_track() {
        assert_eq!(
            separate_with_call(true).final_mix(MusicChoice::Drop),
            Some(vec![
                TrackGain { index: 0, percent: 80 },
                TrackGain { index: 2, percent: 60 },
                TrackGain { index: 3, percent: 120 },
            ])
        );
    }

    #[test]
    fn keeping_the_music_uses_the_system_track_which_already_has_the_call() {
        assert_eq!(
            separate_with_call(true).final_mix(MusicChoice::Keep),
            Some(vec![TrackGain { index: 1, percent: 80 }, TrackGain { index: 3, percent: 120 }])
        );
    }

    #[test]
    fn plan_carries_the_chosen_microphone() {
        let c = Config { mic_source: Some("bluez_input.x".into()), ..config(AudioMode::Mic, None) };
        assert_eq!(plan(&c, &Ok(vec![])).unwrap().mic_source.as_deref(), Some("bluez_input.x"));
        assert_eq!(plan(&config(AudioMode::Mic, None), &Ok(vec![])).unwrap().mic_source, None);
    }

    #[test]
    fn default_plan_has_unity_gain() {
        let p = AudioPlan::default();
        assert_eq!((p.game_volume, p.mic_volume), (100, 100));
    }

    #[test]
    fn choosing_an_app_without_any_listing_tool_explains_why() {
        let err = plan(&config(AudioMode::Game, Some("pxgme-linux")), &Err(AppListError)).unwrap_err();
        assert_eq!(err, AudioError::AppListUnavailable);
        assert!(err.to_string().contains("pactl") && err.to_string().contains("pw-dump"));
    }

    const PW_DUMP: &str = r#"[
      {"id": 40, "type": "PipeWire:Interface:Node", "info": {"props": {
         "media.class": "Stream/Output/Audio", "application.name": "pxgme-linux", "node.name": "pxgme-linux", "object.serial": 203}}},
      {"id": 41, "type": "PipeWire:Interface:Node", "info": {"props": {
         "media.class": "Stream/Output/Audio", "node.name": "Firefox", "object.serial": 300}}},
      {"id": 42, "type": "PipeWire:Interface:Node", "info": {"props": {
         "media.class": "Stream/Output/Audio", "node.name": "Firefox", "object.serial": 301}}},
      {"id": 50, "type": "PipeWire:Interface:Node", "info": {"props": {
         "media.class": "Audio/Sink", "node.name": "alsa_output", "object.serial": 54}}},
      {"id": 51, "type": "PipeWire:Interface:Node", "info": {"props": {
         "media.class": "Stream/Input/Audio", "application.name": "Discord", "object.serial": 77}}},
      {"id": 60, "type": "PipeWire:Interface:Port", "info": {"props": {"object.serial": 99}}}
    ]"#;

    #[test]
    fn pw_dump_keeps_only_output_audio_streams() {
        assert_eq!(parse_pw_dump(PW_DUMP), vec![app("Firefox", &[300, 301]), app("pxgme-linux", &[203])]);
    }

    #[test]
    fn pw_dump_garbage_gives_empty_list() {
        assert!(parse_pw_dump("").is_empty());
        assert!(parse_pw_dump("{}").is_empty());
    }

    #[test]
    fn pactl_is_preferred_when_it_works() {
        let run = |program: &str, _: &[&str]| (program == "pactl").then(|| PACTL.to_string());
        assert_eq!(list_apps_with(&run).unwrap().len(), 3);
    }

    #[test]
    fn falls_back_to_pw_dump_when_pactl_is_missing() {
        let run = |program: &str, _: &[&str]| (program == "pw-dump").then(|| PW_DUMP.to_string());
        assert_eq!(list_apps_with(&run).unwrap(), vec![app("Firefox", &[300, 301]), app("pxgme-linux", &[203])]);
    }

    #[test]
    fn no_tool_at_all_is_an_error_not_an_empty_list() {
        let run = |_: &str, _: &[&str]| None;
        assert_eq!(list_apps_with(&run), Err(AppListError));
    }

    #[test]
    fn a_working_tool_with_no_apps_is_just_an_empty_list() {
        let run = |program: &str, _: &[&str]| (program == "pactl").then(|| "[]".to_string());
        assert_eq!(list_apps_with(&run), Ok(vec![]));
    }
}
