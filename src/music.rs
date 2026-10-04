//! Detecção de música tocando fora do jogo (players MPRIS e fluxos de áudio ativos)
//! e registro do que tocou durante a captura.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use crate::audio::{is_voice_chat, AudioApp};

/// Player de mídia visto no D-Bus (MPRIS): Spotify, navegadores, VLC...
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MediaPlayer {
    pub identity: String,
    pub playing: bool,
    pub title: Option<String>,
    pub artist: Option<String>,
}

/// Tamanho máximo do texto de uma fonte (títulos de lives podem ser enormes).
const MAX_LABEL: usize = 80;

/// Texto para o usuário, ex.: `Spotify — Artista – Título`.
pub fn describe(player: &MediaPlayer) -> String {
    let text = describe_full(player);
    if text.chars().count() <= MAX_LABEL {
        return text;
    }
    let mut short: String = text.chars().take(MAX_LABEL - 1).collect();
    short.push('…');
    short
}

fn describe_full(player: &MediaPlayer) -> String {
    let clean = |s: &Option<String>| s.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(String::from);
    match (clean(&player.artist), clean(&player.title)) {
        (Some(artist), Some(title)) => format!("{} — {artist} – {title}", player.identity),
        (None, Some(what)) | (Some(what), None) => format!("{} — {what}", player.identity),
        (None, None) => player.identity.clone(),
    }
}

/// Mesmo aplicativo? Compara sem diferenciar maiúsculas e aceita um nome conter o
/// outro (`Mozilla Firefox` x `Firefox`, `Spotify` x `spotify`).
fn same_app(a: &str, b: &str) -> bool {
    let (a, b) = (a.trim().to_lowercase(), b.trim().to_lowercase());
    !a.is_empty() && !b.is_empty() && (a.contains(&b) || b.contains(&a))
}

/// O que está tocando fora do jogo `game` (nome do app escolhido), sem repetição.
/// Junta os players MPRIS tocando com os demais fluxos de áudio ativos, ignorando
/// apps de chamada de voz.
pub fn detect(players: &[MediaPlayer], apps: &[AudioApp], game: Option<&str>) -> Vec<String> {
    let is_game = |name: &str| game.is_some_and(|g| same_app(name, g));
    let playing: Vec<&MediaPlayer> = players.iter().filter(|p| p.playing && !is_game(&p.identity)).collect();

    let mut found: Vec<String> = playing.iter().map(|p| describe(p)).collect();
    let mut streams: Vec<String> = apps
        .iter()
        .filter(|a| a.playing && !is_game(&a.name) && !is_voice_chat(&a.name))
        .filter(|a| !playing.iter().any(|p| same_app(&p.identity, &a.name)))
        .map(|a| a.name.clone())
        .collect();
    streams.sort();
    found.extend(streams);
    found.dedup();
    found
}

/// Linha do tempo do que tocou desde o início da captura.
pub struct MusicLog {
    keep: Duration,
    samples: VecDeque<(Duration, Vec<String>)>,
}

impl MusicLog {
    /// `keep`: por quanto tempo guardar amostras (a janela do buffer).
    pub fn new(keep: Duration) -> Self {
        Self { keep, samples: VecDeque::new() }
    }

    /// Registra o que estava tocando no instante `at` (desde o início da captura).
    pub fn record(&mut self, at: Duration, playing: Vec<String>) {
        self.samples.push_back((at, playing));
        // Descarta o que saiu da janela, mas mantém a amostra mais recente antes
        // dela: é o estado que ainda vale no início da janela.
        let cutoff = at.saturating_sub(self.keep);
        while self.samples.len() >= 2 && self.samples[1].0 <= cutoff {
            self.samples.pop_front();
        }
    }

    /// Fontes ouvidas entre `from` e `to`, sem repetição, na ordem em que apareceram.
    /// O estado vale até a amostra seguinte, então inclui o que já tocava em `from`.
    pub fn heard_between(&self, from: Duration, to: Duration) -> Vec<String> {
        let carried = self.samples.iter().rev().find(|(at, _)| *at <= from);
        let inside = self.samples.iter().filter(|(at, _)| *at > from && *at <= to);
        let mut heard: Vec<String> = Vec::new();
        for (_, sources) in carried.into_iter().chain(inside) {
            for source in sources {
                if !heard.contains(source) {
                    heard.push(source.clone());
                }
            }
        }
        heard
    }
}

/// Lê os players MPRIS da sessão D-Bus (vazio se não houver barramento).
/// Faz chamadas bloqueantes: rode fora da thread da interface.
pub fn read_players() -> Vec<MediaPlayer> {
    mpris::read().unwrap_or_default()
}

mod mpris {
    use super::*;
    use zbus::blocking::Connection;
    use zbus::zvariant::OwnedValue;

    const PREFIX: &str = "org.mpris.MediaPlayer2.";
    const PATH: &str = "/org/mpris/MediaPlayer2";

    fn get_all(conn: &Connection, name: &str, interface: &str) -> zbus::Result<HashMap<String, OwnedValue>> {
        conn.call_method(Some(name), PATH, Some("org.freedesktop.DBus.Properties"), "GetAll", &(interface,))?
            .body()
            .deserialize()
    }

    fn string(value: Option<&OwnedValue>) -> Option<String> {
        value.and_then(|v| String::try_from(v.try_clone().ok()?).ok())
    }

    fn read_player(conn: &Connection, name: &str) -> zbus::Result<MediaPlayer> {
        let root = get_all(conn, name, "org.mpris.MediaPlayer2")?;
        let player = get_all(conn, name, "org.mpris.MediaPlayer2.Player")?;
        let metadata: HashMap<String, OwnedValue> = player
            .get("Metadata")
            .and_then(|m| HashMap::<String, OwnedValue>::try_from(m.try_clone().ok()?).ok())
            .unwrap_or_default();
        let artist = metadata
            .get("xesam:artist")
            .and_then(|a| Vec::<String>::try_from(a.try_clone().ok()?).ok())
            .map(|artists| artists.join(", "))
            .or_else(|| string(metadata.get("xesam:artist")));
        Ok(MediaPlayer {
            identity: string(root.get("Identity")).unwrap_or_else(|| name.trim_start_matches(PREFIX).to_string()),
            playing: string(player.get("PlaybackStatus")).as_deref() == Some("Playing"),
            title: string(metadata.get("xesam:title")),
            artist,
        })
    }

    pub fn read() -> zbus::Result<Vec<MediaPlayer>> {
        let conn = Connection::session()?;
        let names: Vec<String> = conn
            .call_method(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus"),
                "ListNames",
                &(),
            )?
            .body()
            .deserialize()?;
        // Um player que não responde não pode esconder os outros.
        Ok(names.iter().filter(|n| n.starts_with(PREFIX)).filter_map(|n| read_player(&conn, n).ok()).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn player(identity: &str, playing: bool, title: Option<&str>, artist: Option<&str>) -> MediaPlayer {
        MediaPlayer {
            identity: identity.into(),
            playing,
            title: title.map(Into::into),
            artist: artist.map(Into::into),
        }
    }

    fn app(name: &str, playing: bool) -> AudioApp {
        AudioApp { name: name.into(), serials: vec![1], playing }
    }

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    #[test]
    fn describe_uses_artist_and_title_when_known() {
        assert_eq!(describe(&player("Spotify", true, Some("Faixa"), Some("Banda"))), "Spotify — Banda – Faixa");
        assert_eq!(describe(&player("Firefox", true, Some("Vídeo"), None)), "Firefox — Vídeo");
        assert_eq!(describe(&player("VLC", true, None, None)), "VLC");
        assert_eq!(describe(&player("VLC", true, Some(""), Some(" ")), ), "VLC");
    }

    #[test]
    fn long_titles_are_shortened_with_an_ellipsis() {
        let long = "x".repeat(300);
        let text = describe(&player("Firefox", true, Some(&long), None));
        assert_eq!(text.chars().count(), MAX_LABEL);
        assert!(text.starts_with("Firefox — xxx") && text.ends_with('…'), "{text}");
        // texto curto não muda
        assert_eq!(describe(&player("VLC", true, Some("curto"), None)), "VLC — curto");
    }

    #[test]
    fn shortening_never_splits_a_multibyte_character() {
        let emojis = "😱".repeat(200);
        let text = describe(&player("Firefox", true, Some(&emojis), None));
        assert_eq!(text.chars().count(), MAX_LABEL);
    }

    #[test]
    fn playing_players_are_detected_and_paused_ones_are_not() {
        let players = [player("Spotify", true, Some("A"), Some("B")), player("Celluloid", false, None, None)];
        assert_eq!(detect(&players, &[], Some("pxgme-linux")), ["Spotify — B – A"]);
    }

    #[test]
    fn browsers_are_detected_through_mpris() {
        let players = [player("Mozilla Firefox", true, Some("Mix de Música - YouTube"), None)];
        assert_eq!(detect(&players, &[], Some("pxgme-linux")), ["Mozilla Firefox — Mix de Música - YouTube"]);
    }

    #[test]
    fn streams_without_mpris_are_still_caught_when_playing() {
        let apps = [app("pxgme-linux", true), app("Chromium", true), app("Parado", false)];
        assert_eq!(detect(&[], &apps, Some("pxgme-linux")), ["Chromium"]);
    }

    #[test]
    fn an_app_with_a_player_is_not_listed_twice() {
        let players = [player("Spotify", true, Some("A"), Some("B"))];
        let apps = [app("spotify", true), app("pxgme-linux", true)];
        assert_eq!(detect(&players, &apps, Some("pxgme-linux")), ["Spotify — B – A"]);
    }

    #[test]
    fn the_chosen_game_is_never_music() {
        let players = [player("pxgme-linux", true, None, None)];
        assert!(detect(&players, &[app("PXGME-Linux", true)], Some("pxgme-linux")).is_empty());
    }

    #[test]
    fn voice_chat_apps_are_not_music() {
        let apps = [app("Discord", true), app("WEBRTC VoiceEngine", true), app("Zoom", true)];
        assert!(detect(&[], &apps, Some("pxgme-linux")).is_empty());
    }

    #[test]
    fn without_a_chosen_game_every_playing_source_counts() {
        let apps = [app("pxgme-linux", true), app("Chromium", true)];
        assert_eq!(detect(&[], &apps, None), ["Chromium", "pxgme-linux"]);
    }

    #[test]
    fn nothing_playing_gives_nothing() {
        assert!(detect(&[], &[], Some("x")).is_empty());
    }

    #[test]
    fn log_reports_what_played_inside_the_window_without_duplicates() {
        let mut log = MusicLog::new(secs(600));
        log.record(secs(0), vec![]);
        log.record(secs(10), vec!["Spotify".into()]);
        log.record(secs(12), vec!["Spotify".into(), "Firefox".into()]);
        log.record(secs(14), vec!["Spotify".into()]);
        assert_eq!(log.heard_between(secs(9), secs(20)), ["Spotify", "Firefox"]);
    }

    #[test]
    fn log_carries_state_into_the_window_start() {
        let mut log = MusicLog::new(secs(600));
        log.record(secs(0), vec!["Spotify".into()]); // tocando desde antes, sem mudança depois
        log.record(secs(30), vec!["Spotify".into()]);
        assert_eq!(log.heard_between(secs(20), secs(25)), ["Spotify"]);
    }

    #[test]
    fn log_is_empty_when_music_stopped_before_the_window() {
        let mut log = MusicLog::new(secs(600));
        log.record(secs(0), vec!["Spotify".into()]);
        log.record(secs(5), vec![]);
        log.record(secs(30), vec![]);
        assert!(log.heard_between(secs(10), secs(30)).is_empty());
    }

    #[test]
    fn log_forgets_samples_older_than_the_buffer_window() {
        let mut log = MusicLog::new(secs(60));
        log.record(secs(0), vec!["Antigo".into()]);
        log.record(secs(100), vec![]);
        log.record(secs(200), vec![]);
        assert!(log.heard_between(secs(0), secs(200)).is_empty());
        assert!(log.samples.len() <= 3);
    }

    #[test]
    fn empty_log_hears_nothing() {
        assert!(MusicLog::new(secs(60)).heard_between(secs(0), secs(10)).is_empty());
    }

    struct FakeRoot {
        identity: String,
    }
    struct FakePlayer {
        status: &'static str,
    }

    #[zbus::interface(name = "org.mpris.MediaPlayer2")]
    impl FakeRoot {
        #[zbus(property)]
        fn identity(&self) -> String {
            self.identity.clone()
        }
    }

    #[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
    impl FakePlayer {
        #[zbus(property)]
        fn playback_status(&self) -> String {
            self.status.into()
        }

        #[zbus(property)]
        fn metadata(&self) -> HashMap<String, zbus::zvariant::OwnedValue> {
            use zbus::zvariant::{OwnedValue, Value};
            let mut m = HashMap::new();
            m.insert("xesam:title".to_string(), OwnedValue::try_from(Value::from("Faixa de Teste")).unwrap());
            m.insert("xesam:artist".to_string(), OwnedValue::try_from(Value::new(vec!["Banda A", "Banda B"])).unwrap());
            m
        }
    }

    /// Registra um player MPRIS falso no barramento de sessão; `None` sem D-Bus.
    /// A identidade é única por teste (eles rodam em paralelo).
    fn serve_fake(suffix: &str, status: &'static str) -> Option<zbus::blocking::Connection> {
        zbus::blocking::connection::Builder::session()
            .ok()?
            .name(format!("org.mpris.MediaPlayer2.catchback_teste_{suffix}_{}", std::process::id()))
            .ok()?
            .serve_at("/org/mpris/MediaPlayer2", FakeRoot { identity: format!("CatchbackTeste-{suffix}") })
            .ok()?
            .serve_at("/org/mpris/MediaPlayer2", FakePlayer { status })
            .ok()?
            .build()
            .ok()
    }

    #[test]
    fn reads_a_real_mpris_player_over_dbus() {
        let Some(_conn) = serve_fake("a", "Playing") else {
            eprintln!("sem D-Bus de sessão: teste ignorado");
            return;
        };
        let players = read_players();
        let ours = players.iter().find(|p| p.identity == "CatchbackTeste-a").expect("player falso não apareceu");
        assert!(ours.playing);
        assert_eq!(ours.title.as_deref(), Some("Faixa de Teste"));
        assert_eq!(ours.artist.as_deref(), Some("Banda A, Banda B"));
        assert_eq!(describe(ours), "CatchbackTeste-a — Banda A, Banda B – Faixa de Teste");
    }

    #[test]
    fn a_paused_mpris_player_is_read_as_not_playing() {
        let Some(_conn) = serve_fake("b", "Paused") else {
            return;
        };
        let players = read_players();
        let ours = players.iter().find(|p| p.identity == "CatchbackTeste-b").expect("player falso não apareceu");
        assert!(!ours.playing);
        assert!(detect(std::slice::from_ref(ours), &[], Some("jogo")).is_empty());
    }

    /// Mostra o que a detecção enxerga no sistema de verdade.
    /// Rodar com: `cargo test real_detection -- --ignored --nocapture`
    #[test]
    #[ignore = "depende do que estiver tocando agora; rode com --ignored"]
    fn real_detection_on_this_system() {
        let players = read_players();
        let apps = crate::audio::list_apps().unwrap_or_default();
        println!("players MPRIS: {players:#?}");
        println!("apps de áudio: {apps:?}");
        println!("detectado com jogo=pxgme-linux: {:?}", detect(&players, &apps, Some("pxgme-linux")));
        println!("detectado sem jogo escolhido:  {:?}", detect(&players, &apps, None));
    }
}
