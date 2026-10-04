//! Montagem de clips: nomes de arquivo, lista de concatenação e comando do ffmpeg.

use std::path::{Path, PathBuf};

use chrono::NaiveDateTime;

use crate::audio::TrackGain;
use crate::buffer::Segment;
use crate::format::{Codec, Container};
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
            // sem `-map 0` o ffmpeg guardaria só uma faixa de áudio
            "-map".into(),
            "0".into(),
            "-c".into(),
            "copy".into(),
            output.to_string_lossy().into_owned(),
        ])
        .collect()
}

/// De onde o `ffmpeg` lê para o remix.
#[derive(Debug, Clone, Copy)]
pub enum RemixInput<'a> {
    /// Lista de segmentos (modo `concat`).
    Concat(&'a Path),
    /// Um arquivo único (gravação manual).
    File(&'a Path),
}

/// Argumentos do ffmpeg que copiam o vídeo e refazem o áudio com uma única faixa,
/// mixando só `tracks` (cada uma com seu ganho em %).
pub fn remix_args(input: RemixInput, tracks: &[TrackGain], container: Container, output: &Path) -> Vec<String> {
    let gains: Vec<String> = tracks
        .iter()
        .enumerate()
        .map(|(k, t)| {
            let out = if tracks.len() == 1 { "a".to_string() } else { format!("t{k}") };
            format!("[0:a:{}]volume={:.2}[{out}]", t.index, f64::from(t.percent) / 100.0)
        })
        .collect();
    let mut filter = gains.join(";");
    if tracks.len() > 1 {
        let inputs: String = (0..tracks.len()).map(|k| format!("[t{k}]")).collect();
        filter.push_str(&format!(";{inputs}amix=inputs={}:normalize=0:duration=longest[a]", tracks.len()));
    }
    let audio_codec = match container.codec() {
        Codec::H264 => "aac",
        Codec::Vp9 => "libopus",
    };
    let mut args: Vec<String> = vec!["-n".into()];
    match input {
        RemixInput::Concat(list) => {
            args.extend(["-f", "concat", "-safe", "0", "-i"].map(String::from));
            args.push(list.to_string_lossy().into_owned());
        }
        RemixInput::File(file) => {
            args.push("-i".into());
            args.push(file.to_string_lossy().into_owned());
        }
    }
    args.extend(["-filter_complex".to_string(), filter]);
    args.extend(["-map", "0:v", "-map", "[a]", "-c:v", "copy", "-c:a"].map(String::from));
    args.extend([audio_codec.to_string(), "-b:a".into(), "192k".into(), output.to_string_lossy().into_owned()]);
    args
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
    fn ffmpeg_copies_all_streams_without_overwriting() {
        let args = ffmpeg_args(Path::new("/tmp/l.txt"), Path::new("/out/c.mp4"));
        assert_eq!(
            args,
            ["-n", "-f", "concat", "-safe", "0", "-i", "/tmp/l.txt", "-map", "0", "-c", "copy", "/out/c.mp4"]
        );
    }

    fn tg(index: usize, percent: u32) -> TrackGain {
        TrackGain { index, percent }
    }

    fn filter_of(args: &[String]) -> &str {
        let i = args.iter().position(|a| a == "-filter_complex").expect("sem -filter_complex");
        &args[i + 1]
    }

    #[test]
    fn remix_copies_video_and_encodes_one_audio_track() {
        let args = remix_args(
            RemixInput::Concat(Path::new("/tmp/l.txt")),
            &[tg(0, 100), tg(2, 100)],
            Container::Mp4,
            Path::new("/out/c.mp4"),
        );
        assert_eq!(&args[..7], ["-n", "-f", "concat", "-safe", "0", "-i", "/tmp/l.txt"]);
        let tail = &args[args.len() - 11..];
        assert_eq!(
            tail,
            ["-map", "0:v", "-map", "[a]", "-c:v", "copy", "-c:a", "aac", "-b:a", "192k", "/out/c.mp4"]
        );
    }

    #[test]
    fn remix_mixes_the_chosen_tracks_with_their_gains_without_normalizing() {
        let args = remix_args(
            RemixInput::File(Path::new("/in.mp4")),
            &[tg(1, 80), tg(2, 120)],
            Container::Mp4,
            Path::new("/out.mp4"),
        );
        assert_eq!(
            filter_of(&args),
            "[0:a:1]volume=0.80[t0];[0:a:2]volume=1.20[t1];[t0][t1]amix=inputs=2:normalize=0:duration=longest[a]"
        );
    }

    #[test]
    fn remix_with_a_single_track_just_applies_the_gain() {
        let args = remix_args(RemixInput::File(Path::new("/in.mp4")), &[tg(0, 50)], Container::Mkv, Path::new("/o.mkv"));
        assert_eq!(filter_of(&args), "[0:a:0]volume=0.50[a]");
    }

    #[test]
    fn remix_reads_a_plain_file_without_concat_options() {
        let args = remix_args(RemixInput::File(Path::new("/in.mp4")), &[tg(0, 100)], Container::Mp4, Path::new("/o.mp4"));
        assert_eq!(&args[..3], ["-n", "-i", "/in.mp4"]);
        assert!(!args.contains(&"concat".to_string()));
    }

    #[test]
    fn remix_audio_codec_follows_container() {
        let codec = |c| {
            let a = remix_args(RemixInput::File(Path::new("/i")), &[tg(0, 100)], c, Path::new("/o"));
            let i = a.iter().position(|x| x == "-c:a").unwrap();
            a[i + 1].clone()
        };
        assert_eq!(codec(Container::Mp4), "aac");
        assert_eq!(codec(Container::Mkv), "aac");
        assert_eq!(codec(Container::WebM), "libopus");
    }
}
