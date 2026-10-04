//! Textos exibidos na interface (puros, para poderem ser testados).

use std::time::Duration;

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
}
