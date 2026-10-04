//! Montagem de clips: nomes de arquivo, lista de concatenação e comando do ffmpeg.

use std::path::{Path, PathBuf};

use chrono::NaiveDateTime;

use crate::buffer::Segment;
use crate::format::Container;
use crate::session::Mode;

/// Nome do arquivo, ex.: `Replay_2026-10-04_15-30-12.mp4`.
pub fn clip_filename(mode: Mode, at: NaiveDateTime, container: Container) -> String {
    let prefix = match mode {
        Mode::Replay => "Replay",
        Mode::Manual => "Gravacao",
    };
    format!("{prefix}_{}.{}", at.format("%Y-%m-%d_%H-%M-%S"), container.extension())
}

/// Devolve `dir/name`; se já existir, acrescenta `_2`, `_3`... antes da extensão.
pub fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let path = Path::new(name);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or(name);
    let ext = path.extension().and_then(|s| s.to_str());
    (2..)
        .map(|n| match ext {
            Some(ext) => dir.join(format!("{stem}_{n}.{ext}")),
            None => dir.join(format!("{stem}_{n}")),
        })
        .find(|p| !p.exists())
        .expect("intervalo infinito")
}

/// Conteúdo do arquivo de lista para o demuxer `concat` do ffmpeg.
pub fn concat_list(segments: &[Segment]) -> String {
    segments
        .iter()
        .map(|s| format!("file '{}'\n", s.path.to_string_lossy().replace('\'', "'\\''")))
        .collect()
}

/// Argumentos do ffmpeg para juntar os segmentos sem reencodar.
pub fn ffmpeg_args(list: &Path, output: &Path) -> Vec<String> {
    ["-n", "-f", "concat", "-safe", "0", "-i"]
        .into_iter()
        .map(String::from)
        .chain([
            list.to_string_lossy().into_owned(),
            "-c".into(),
            "copy".into(),
            output.to_string_lossy().into_owned(),
        ])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use std::time::Duration;

    fn at() -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 10, 4).unwrap().and_hms_opt(15, 30, 12).unwrap()
    }

    fn seg(path: &str) -> Segment {
        Segment { path: path.into(), duration: Duration::from_secs(5) }
    }

    #[test]
    fn filename_depends_on_mode() {
        assert_eq!(clip_filename(Mode::Replay, at(), Container::Mp4), "Replay_2026-10-04_15-30-12.mp4");
        assert_eq!(clip_filename(Mode::Manual, at(), Container::Mp4), "Gravacao_2026-10-04_15-30-12.mp4");
    }

    #[test]
    fn filename_extension_follows_container() {
        assert!(clip_filename(Mode::Replay, at(), Container::Mkv).ends_with(".mkv"));
        assert!(clip_filename(Mode::Manual, at(), Container::WebM).ends_with(".webm"));
    }

    #[test]
    fn unique_path_returns_plain_name_when_free() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(unique_path(dir.path(), "a.mp4"), dir.path().join("a.mp4"));
    }

    #[test]
    fn unique_path_adds_counter_on_collision() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.mp4"), b"").unwrap();
        assert_eq!(unique_path(dir.path(), "a.mp4"), dir.path().join("a_2.mp4"));
        std::fs::write(dir.path().join("a_2.mp4"), b"").unwrap();
        assert_eq!(unique_path(dir.path(), "a.mp4"), dir.path().join("a_3.mp4"));
    }

    #[test]
    fn concat_list_has_one_line_per_segment() {
        let list = concat_list(&[seg("/tmp/s1.mp4"), seg("/tmp/s2.mp4")]);
        assert_eq!(list, "file '/tmp/s1.mp4'\nfile '/tmp/s2.mp4'\n");
    }

    #[test]
    fn concat_list_escapes_single_quotes() {
        assert_eq!(concat_list(&[seg("/tmp/it's.mp4")]), "file '/tmp/it'\\''s.mp4'\n");
    }

    #[test]
    fn ffmpeg_copies_streams_without_overwriting() {
        let args = ffmpeg_args(Path::new("/tmp/l.txt"), Path::new("/out/c.mp4"));
        assert_eq!(
            args,
            ["-n", "-f", "concat", "-safe", "0", "-i", "/tmp/l.txt", "-c", "copy", "/out/c.mp4"]
        );
    }
}
