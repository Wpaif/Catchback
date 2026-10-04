//! Textos exibidos na interface (puros, para poderem ser testados).

use std::time::Duration;

use crate::audio::AudioMode;
use crate::config::Config;
use crate::session::State;

/// `mm:ss`, ou `h:mm:ss` a partir de uma hora.
pub fn format_clock(d: Duration) -> String {
    let total = d.as_secs();
    let (h, m, s) = (total / 3600, total % 3600 / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub icon: &'static str,
    pub title: String,
    pub subtitle: String,
}

/// `progress` é o tempo já bufferizado (replay) ou decorrido (manual).
pub fn status(state: State, progress: Duration, window: Duration) -> Status {
    match state {
        State::Idle => Status {
            icon: "camera-video-symbolic",
            title: "Pronto para gravar".into(),
            subtitle: "Escolha um modo e inicie a captura".into(),
        },
        State::Buffering => Status {
            icon: "media-playlist-repeat-symbolic",
            title: "Buffer ativo".into(),
            subtitle: format!("{} / {}", format_clock(progress.min(window)), format_clock(window)),
        },
        State::Recording => Status {
            icon: "media-record-symbolic",
            title: "Gravando".into(),
            subtitle: format_clock(progress),
        },
    }
}

/// Rótulo do botão que salva o clip, ex.: `Salvar últimos 2 min`.
pub fn save_button_label(span: Duration) -> String {
    let secs = span.as_secs();
    match (secs.is_multiple_of(60), secs / 60) {
        (true, 1) => "Salvar último minuto".into(),
        (true, m) => format!("Salvar últimos {m} min"),
        _ => format!("Salvar últimos {secs} s"),
    }
}

/// Aviso compacto sobre música tocando fora do jogo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MusicNotice {
    pub title: &'static str,
    /// O que está tocando (pode ser longo: a interface corta em uma linha).
    pub detail: String,
    /// O que vai acontecer com isso, em poucas palavras.
    pub outcome: &'static str,
    /// Vale chamar a atenção: o que toca será gravado.
    pub warn: bool,
}

/// Aviso sobre música tocando, ou `None` se não há o que avisar.
/// `app_chosen`: o som do jogo vem de um aplicativo específico (e não do sistema).
pub fn music_notice(mode: AudioMode, app_chosen: bool, ask_music: bool, detected: &[String]) -> Option<MusicNotice> {
    if !mode.has_game() || detected.is_empty() {
        return None;
    }
    let detail = detected.join(", ");
    Some(match (app_chosen, ask_music) {
        (false, _) => MusicNotice { title: "Tocando agora", detail, outcome: "Será gravado", warn: true },
        (true, true) => MusicNotice {
            title: "Tocando fora do jogo",
            detail,
            outcome: "Você escolhe ao salvar",
            warn: false,
        },
        (true, false) => MusicNotice { title: "Tocando fora do jogo", detail, outcome: "Fica de fora", warn: false },
    })
}

/// O que vai (e o que não vai) para o clip, em linguagem simples.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioOverview {
    /// O que será gravado.
    pub records: String,
    /// O que você decide na hora de salvar (a música), se for o caso.
    pub decides: Option<&'static str>,
    /// O que fica de fora.
    pub leaves_out: String,
    /// Algo que impede de gravar o que foi pedido.
    pub warning: Option<&'static str>,
}

const GAME: &str = "Som do jogo";
const MIC: &str = "Seu microfone";
const CALL: &str = "Call dos amigos";
const MUSIC: &str = "Música e outros sons";
const SYSTEM: &str = "Tudo que toca no computador (jogo, música e call)";

/// `call_available`: há um app de voz (Discord...) aberto agora.
pub fn audio_overview(config: &Config, call_available: bool) -> AudioOverview {
    let system = config.audio.has_game() && config.audio_app.is_none();
    let mut records: Vec<String> = Vec::new();
    let mut leaves: Vec<&str> = Vec::new();

    if config.audio.has_game() {
        records.push(match &config.audio_app {
            Some(app) => format!("Jogo ({app})"),
            None => SYSTEM.to_string(),
        });
    } else {
        leaves.push(GAME);
    }
    if config.audio.has_mic() {
        records.push(MIC.to_string());
    } else {
        leaves.push(MIC);
    }
    // No áudio do sistema a call já vem junto; senão ela é uma fonte à parte.
    if !system {
        if config.record_call {
            records.push("Call dos amigos".to_string());
        } else {
            leaves.push(CALL);
        }
    }
    let app_game = config.audio.has_game() && config.audio_app.is_some();
    let decides = (app_game && config.ask_music).then_some(MUSIC);
    if !system && decides.is_none() {
        leaves.push(MUSIC);
    }
    let warning = (config.record_call && !system && !call_available)
        .then_some("Nenhum app de call (Discord...) aberto agora: abra e entre na call antes de iniciar.");
    AudioOverview {
        records: if records.is_empty() { "Só o vídeo, sem áudio".to_string() } else { records.join(" · ") },
        decides,
        leaves_out: leaves.join(" · "),
        warning,
    }
}

/// Explicação do interruptor "perguntar sobre a música": cita só o que é gravado.
pub fn ask_music_hint(config: &Config) -> String {
    let mut parts = vec!["jogo", "sistema"];
    if config.record_call {
        parts.push("call");
    }
    if config.audio.has_mic() {
        parts.push("microfone");
    }
    let (last, rest) = parts.split_last().expect("sempre há jogo e sistema");
    format!("Grava {} e {last} em faixas separadas e deixa você escolher", rest.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    #[test]
    fn clock_formats_minutes_and_seconds() {
        assert_eq!(format_clock(secs(0)), "00:00");
        assert_eq!(format_clock(secs(205)), "03:25");
        assert_eq!(format_clock(secs(599)), "09:59");
    }

    #[test]
    fn clock_switches_to_hours() {
        assert_eq!(format_clock(secs(3600)), "1:00:00");
        assert_eq!(format_clock(secs(3725)), "1:02:05");
    }

    #[test]
    fn clock_ignores_sub_second() {
        assert_eq!(format_clock(Duration::from_millis(59_999)), "00:59");
    }

    #[test]
    fn idle_status() {
        let s = status(State::Idle, Duration::ZERO, secs(600));
        assert_eq!(s.title, "Pronto para gravar");
        assert!(!s.subtitle.is_empty());
    }

    #[test]
    fn buffering_status_shows_progress_capped_at_window() {
        let s = status(State::Buffering, secs(205), secs(600));
        assert_eq!(s.title, "Buffer ativo");
        assert_eq!(s.subtitle, "03:25 / 10:00");
        let full = status(State::Buffering, secs(650), secs(600));
        assert_eq!(full.subtitle, "10:00 / 10:00");
    }

    #[test]
    fn recording_status_shows_elapsed() {
        let s = status(State::Recording, secs(75), secs(600));
        assert_eq!(s.title, "Gravando");
        assert_eq!(s.subtitle, "01:15");
    }

    #[test]
    fn states_have_distinct_icons() {
        let icons: Vec<_> = [State::Idle, State::Buffering, State::Recording]
            .into_iter()
            .map(|st| status(st, Duration::ZERO, secs(600)).icon)
            .collect();
        assert_ne!(icons[0], icons[1]);
        assert_ne!(icons[1], icons[2]);
        assert_ne!(icons[0], icons[2]);
    }

    #[test]
    fn save_label_uses_minutes_or_seconds() {
        assert_eq!(save_button_label(secs(120)), "Salvar últimos 2 min");
        assert_eq!(save_button_label(secs(60)), "Salvar último minuto");
        assert_eq!(save_button_label(secs(90)), "Salvar últimos 90 s");
        assert_eq!(save_button_label(secs(30)), "Salvar últimos 30 s");
    }

    fn found() -> Vec<String> {
        vec!["Spotify — Banda – Faixa".into(), "Firefox".into()]
    }

    #[test]
    fn notice_is_hidden_when_nothing_plays_or_game_audio_is_off() {
        assert_eq!(music_notice(AudioMode::Game, true, true, &[]), None);
        assert_eq!(music_notice(AudioMode::Off, true, true, &found()), None);
        assert_eq!(music_notice(AudioMode::Mic, false, true, &found()), None);
    }

    #[test]
    fn system_audio_warns_that_everything_is_recorded() {
        let n = music_notice(AudioMode::Game, false, true, &found()).unwrap();
        assert_eq!(n.title, "Tocando agora");
        assert_eq!(n.detail, "Spotify — Banda – Faixa, Firefox");
        assert_eq!(n.outcome, "Será gravado");
        assert!(n.warn);
    }

    #[test]
    fn chosen_app_with_ask_says_you_decide_when_saving() {
        let n = music_notice(AudioMode::GameAndMic, true, true, &found()).unwrap();
        assert_eq!(n.title, "Tocando fora do jogo");
        assert_eq!(n.outcome, "Você escolhe ao salvar");
        assert!(!n.warn);
    }

    #[test]
    fn chosen_app_without_ask_says_it_is_left_out() {
        let n = music_notice(AudioMode::Game, true, false, &found()).unwrap();
        assert_eq!(n.outcome, "Fica de fora");
        assert!(!n.warn);
    }

    #[test]
    fn notice_texts_are_short_enough_for_one_line() {
        for (app, ask) in [(false, true), (true, true), (true, false)] {
            let n = music_notice(AudioMode::Game, app, ask, &found()).unwrap();
            assert!(n.title.chars().count() <= 24 && n.outcome.chars().count() <= 24, "{n:?}");
        }
    }


    fn cfg(audio: AudioMode, app: Option<&str>, call: bool, ask: bool) -> Config {
        Config { audio, audio_app: app.map(String::from), record_call: call, ask_music: ask, ..Config::default() }
    }

    #[test]
    fn game_app_mic_and_call_with_ask_say_exactly_what_is_recorded() {
        let o = audio_overview(&cfg(AudioMode::GameAndMic, Some("pxgme-linux"), true, true), true);
        assert_eq!(o.records, "Jogo (pxgme-linux) · Seu microfone · Call dos amigos");
        assert_eq!(o.decides, Some("Música e outros sons"));
        assert_eq!(o.leaves_out, "");
        assert_eq!(o.warning, None);
    }

    #[test]
    fn the_game_is_named_once_and_never_repeated_as_a_mode() {
        let o = audio_overview(&cfg(AudioMode::GameAndMic, Some("pxgme-linux"), false, true), true);
        assert_eq!(o.records.matches("pxgme-linux").count(), 1);
        assert!(!o.records.contains("Som do jogo"), "{}", o.records);
    }

    #[test]
    fn without_the_call_switch_the_friends_are_explicitly_left_out() {
        let o = audio_overview(&cfg(AudioMode::Game, Some("pxgme-linux"), false, true), true);
        assert_eq!(o.records, "Jogo (pxgme-linux)");
        assert_eq!(o.leaves_out, "Seu microfone · Call dos amigos");
        assert_eq!(o.decides, Some("Música e outros sons"));
    }

    #[test]
    fn without_asking_the_music_is_left_out() {
        let o = audio_overview(&cfg(AudioMode::Game, Some("pxgme-linux"), false, false), true);
        assert_eq!(o.decides, None);
        assert_eq!(o.leaves_out, "Seu microfone · Call dos amigos · Música e outros sons");
    }

    #[test]
    fn system_audio_includes_everything_so_the_call_is_not_a_separate_choice() {
        let o = audio_overview(&cfg(AudioMode::Game, None, false, true), true);
        assert_eq!(o.records, "Tudo que toca no computador (jogo, música e call)");
        assert_eq!(o.leaves_out, "Seu microfone");
        assert_eq!(o.decides, None);
        assert_eq!(o.warning, None);
        // mesmo pedindo a call, ela já está dentro: sem aviso nem item duplicado
        let o = audio_overview(&cfg(AudioMode::Game, None, true, true), false);
        assert_eq!(o.warning, None);
        assert!(!o.records.contains("Call dos amigos"));
    }

    #[test]
    fn mic_only_leaves_the_game_the_call_and_the_music_out() {
        let o = audio_overview(&cfg(AudioMode::Mic, Some("pxgme-linux"), false, true), true);
        assert_eq!(o.records, "Seu microfone");
        assert_eq!(o.leaves_out, "Som do jogo · Call dos amigos · Música e outros sons");
        assert_eq!(o.decides, None);
    }

    #[test]
    fn no_audio_says_video_only() {
        let o = audio_overview(&cfg(AudioMode::Off, None, false, true), true);
        assert_eq!(o.records, "Só o vídeo, sem áudio");
    }

    #[test]
    fn the_call_alone_is_possible() {
        let o = audio_overview(&cfg(AudioMode::Off, None, true, true), true);
        assert_eq!(o.records, "Call dos amigos");
    }

    #[test]
    fn asking_for_the_call_with_no_voice_app_open_warns() {
        let c = cfg(AudioMode::Game, Some("pxgme-linux"), true, true);
        assert!(audio_overview(&c, false).warning.unwrap().contains("Discord"));
        assert_eq!(audio_overview(&c, true).warning, None);
    }

    #[test]
    fn ask_hint_only_names_what_is_recorded() {
        let hint = |mode, call| ask_music_hint(&cfg(mode, Some("x"), call, true));
        assert_eq!(hint(AudioMode::Game, false), "Grava jogo e sistema em faixas separadas e deixa você escolher");
        assert_eq!(
            hint(AudioMode::GameAndMic, false),
            "Grava jogo, sistema e microfone em faixas separadas e deixa você escolher"
        );
        assert_eq!(
            hint(AudioMode::GameAndMic, true),
            "Grava jogo, sistema, call e microfone em faixas separadas e deixa você escolher"
        );
        assert!(!hint(AudioMode::Game, false).contains("microfone"));
    }
}
