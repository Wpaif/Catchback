//! Exportador real: junta segmentos com o `ffmpeg` (sem reencodar).

use std::path::Path;
use std::process::Command;

use crate::buffer::Segment;
use crate::clip::{concat_list, ffmpeg_args};
use crate::recorder::{Exporter, RecorderError};

pub struct FfmpegExporter;

impl Exporter for FfmpegExporter {
    fn export(&self, segments: &[Segment], output: &Path) -> Result<(), RecorderError> {
        let list = output.with_extension("concat.txt");
        std::fs::write(&list, concat_list(segments))?;
        let result = Command::new("ffmpeg")
            .arg("-v")
            .arg("error")
            .args(ffmpeg_args(&list, output))
            .output();
        let _ = std::fs::remove_file(&list);
        let result = result.map_err(|e| RecorderError::Export(format!("não foi possível executar o ffmpeg: {e}")))?;
        if result.status.success() {
            Ok(())
        } else {
            let _ = std::fs::remove_file(output);
            Err(RecorderError::Export(String::from_utf8_lossy(&result.stderr).trim().to_string()))
        }
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
        FfmpegExporter.export(&[seg(&a), seg(&b)], &out).unwrap();
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
        let err = FfmpegExporter.export(&[missing], &out).unwrap_err();
        assert!(matches!(err, RecorderError::Export(_)));
        assert!(!out.exists());
    }
}
