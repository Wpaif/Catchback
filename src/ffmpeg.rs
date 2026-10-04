//! Exportador real: junta segmentos com o `ffmpeg` (sem reencodar).

use std::path::Path;
use std::process::Command;

use crate::buffer::Segment;
use crate::clip::{concat_list, ffmpeg_args, remix_args, RemixInput};
use crate::recorder::{Exporter, RecorderError, Remix};

pub struct FfmpegExporter;

/// Roda o ffmpeg; em caso de falha apaga o arquivo parcial e devolve o erro dele.
fn run(args: &[String], output: &Path) -> Result<(), RecorderError> {
    let result = Command::new("ffmpeg")
        .arg("-v")
        .arg("error")
        .args(args)
        .output()
        .map_err(|e| RecorderError::Export(format!("não foi possível executar o ffmpeg: {e}")))?;
    if result.status.success() {
        Ok(())
    } else {
        let _ = std::fs::remove_file(output);
        Err(RecorderError::Export(String::from_utf8_lossy(&result.stderr).trim().to_string()))
    }
}

impl Exporter for FfmpegExporter {
    fn export(&self, segments: &[Segment], output: &Path, remix: Option<&Remix>) -> Result<(), RecorderError> {
        let list = output.with_extension("concat.txt");
        std::fs::write(&list, concat_list(segments))?;
        let args = match remix {
            Some(r) => remix_args(RemixInput::Concat(&list), &r.tracks, r.container, output),
            None => ffmpeg_args(&list, output),
        };
        let result = run(&args, output);
        let _ = std::fs::remove_file(&list);
        result
    }

    fn remix_file(&self, input: &Path, output: &Path, remix: &Remix) -> Result<(), RecorderError> {
        run(&remix_args(RemixInput::File(input), &remix.tracks, remix.container, output), output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn ffmpeg_available() -> bool {
        Command::new("ffmpeg").arg("-version").output().is_ok()
    }

    /// Gera um vídeo de teste de `secs` segundos.
    fn make_video(path: &Path, secs: u32) {
        let status = Command::new("ffmpeg")
            .args(["-v", "error", "-f", "lavfi", "-i"])
            .arg(format!("testsrc=duration={secs}:size=64x64:rate=10"))
            .args(["-pix_fmt", "yuv420p", "-g", "10"])
            .arg(path)
            .status()
            .unwrap();
        assert!(status.success());
    }

    fn duration_of(path: &Path) -> f64 {
        let out = Command::new("ffprobe")
            .args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"])
            .arg(path)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
    }

    fn seg(path: &Path) -> Segment {
        Segment { path: path.to_path_buf(), duration: Duration::from_secs(2) }
    }

    #[test]
    fn joins_segments_into_one_file() {
        if !ffmpeg_available() {
            eprintln!("ffmpeg ausente: teste ignorado");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("it's a.mp4"), dir.path().join("b.mp4"));
        make_video(&a, 2);
        make_video(&b, 2);
        let out = dir.path().join("clip.mp4");
        FfmpegExporter.export(&[seg(&a), seg(&b)], &out, None).unwrap();
        assert!((duration_of(&out) - 4.0).abs() < 0.3);
    }

    #[test]
    fn reports_ffmpeg_failure_and_leaves_no_partial_file() {
        if !ffmpeg_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("clip.mp4");
        let missing = seg(&dir.path().join("nao-existe.mp4"));
        let err = FfmpegExporter.export(&[missing], &out, None).unwrap_err();
        assert!(matches!(err, RecorderError::Export(_)));
        assert!(!out.exists());
    }

    use crate::audio::TrackGain;
    use crate::format::Container;

    /// Vídeo com 3 faixas de áudio de níveis bem diferentes (jogo < sistema < mic).
    fn make_three_track_video(path: &Path, secs: u32) {
        let status = Command::new("ffmpeg")
            .args(["-v", "error", "-f", "lavfi", "-i"])
            .arg(format!("testsrc=duration={secs}:size=64x64:rate=10"))
            .args(["-f", "lavfi", "-i"])
            .arg(format!("sine=frequency=300:duration={secs}"))
            .args(["-f", "lavfi", "-i"])
            .arg(format!("sine=frequency=700:duration={secs}"))
            .args(["-f", "lavfi", "-i"])
            .arg(format!("sine=frequency=1100:duration={secs}"))
            .args(["-map", "0:v", "-map", "1:a", "-map", "2:a", "-map", "3:a"])
            .args(["-filter:a:0", "volume=0.05", "-filter:a:1", "volume=0.5", "-filter:a:2", "volume=1.0"])
            .args(["-pix_fmt", "yuv420p", "-g", "10", "-c:a", "aac"])
            .arg(path)
            .status()
            .unwrap();
        assert!(status.success());
    }

    fn audio_tracks(path: &Path) -> usize {
        let out = Command::new("ffprobe")
            .args(["-v", "error", "-select_streams", "a", "-show_entries", "stream=index", "-of", "csv=p=0"])
            .arg(path)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).lines().count()
    }

    fn peak_db(path: &Path) -> f64 {
        let out = Command::new("ffmpeg")
            .args(["-hide_banner", "-nostats", "-i"])
            .arg(path)
            .args(["-vn", "-af", "volumedetect", "-f", "null", "-"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stderr)
            .lines()
            .find_map(|l| l.split("max_volume:").nth(1))
            .and_then(|v| v.trim().trim_end_matches(" dB").parse().ok())
            .expect("sem max_volume")
    }

    fn remix(tracks: &[(usize, u32)], container: Container) -> Remix {
        Remix { tracks: tracks.iter().map(|&(index, percent)| TrackGain { index, percent }).collect(), container }
    }

    #[test]
    fn remix_keeps_one_audio_track_with_only_the_chosen_sources() {
        if !ffmpeg_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a.mp4"), dir.path().join("b.mp4"));
        make_three_track_video(&a, 2);
        make_three_track_video(&b, 2);
        let segs = [seg(&a), seg(&b)];
        // mic (faixa 2) a 0% para isolar o efeito: jogo (0.05) x sistema (0.5)
        let drop = dir.path().join("drop.mp4");
        FfmpegExporter.export(&segs, &drop, Some(&remix(&[(0, 100), (2, 0)], Container::Mp4))).unwrap();
        let keep = dir.path().join("keep.mp4");
        FfmpegExporter.export(&segs, &keep, Some(&remix(&[(1, 100), (2, 0)], Container::Mp4))).unwrap();

        for out in [&drop, &keep] {
            assert_eq!(audio_tracks(out), 1, "{out:?}");
            assert!((duration_of(out) - 4.0).abs() < 0.4, "{out:?}");
        }
        let (d, k) = (peak_db(&drop), peak_db(&keep));
        assert!(k - d > 12.0, "sem música {d} dB, com música {k} dB: a escolha não mudou o áudio");
    }

    #[test]
    fn remix_applies_gain_per_track() {
        if !ffmpeg_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.mkv");
        make_three_track_video(&a, 2);
        let loud = dir.path().join("loud.mkv");
        let quiet = dir.path().join("quiet.mkv");
        FfmpegExporter.export(&[seg(&a)], &loud, Some(&remix(&[(1, 100)], Container::Mkv))).unwrap();
        FfmpegExporter.export(&[seg(&a)], &quiet, Some(&remix(&[(1, 25)], Container::Mkv))).unwrap();
        let diff = peak_db(&loud) - peak_db(&quiet);
        assert!((diff - 12.0).abs() < 3.0, "25% deveria ficar ~12 dB abaixo, ficou {diff}");
    }

    #[test]
    fn remix_file_rewrites_a_single_recording() {
        if !ffmpeg_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let (input, output) = (dir.path().join("in.mp4"), dir.path().join("out.mp4"));
        make_three_track_video(&input, 2);
        FfmpegExporter.remix_file(&input, &output, &remix(&[(0, 100), (2, 100)], Container::Mp4)).unwrap();
        assert_eq!(audio_tracks(&output), 1);
        assert!((duration_of(&output) - 2.0).abs() < 0.4);
    }

    #[test]
    fn remix_failure_leaves_no_partial_file() {
        if !ffmpeg_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out.mp4");
        let err = FfmpegExporter
            .remix_file(&dir.path().join("nao-existe.mp4"), &out, &remix(&[(0, 100)], Container::Mp4))
            .unwrap_err();
        assert!(matches!(err, RecorderError::Export(_)));
        assert!(!out.exists());
    }
}
