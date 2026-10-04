//! Descrição dos pipelines GStreamer (pura e testável) para cada modo.

use std::os::fd::RawFd;
use std::path::Path;
use std::time::Duration;

use crate::config::Config;
use crate::format::{Codec, Container};

/// Duração de cada segmento do buffer circular.
pub const SEGMENT_DURATION: Duration = Duration::from_secs(5);

/// Fonte de vídeo entregue pelo portal de captura (PipeWire).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PipewireSource {
    pub fd: RawFd,
    pub node_id: u32,
}

/// Aspas para a sintaxe do `gst_parse_launch`.
fn quote(path: &Path) -> String {
    let raw = path.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{raw}\"")
}

/// Nome dos segmentos do replay, com `%05d` para o índice.
pub fn segment_pattern(container: Container) -> String {
    format!("seg_%05d.{}", container.extension())
}

/// Encoder + parser conforme o codec exigido pelo contêiner.
fn encoder(config: &Config) -> String {
    let kbps = config.quality.bitrate_kbps();
    match config.container.codec() {
        Codec::H264 => format!(
            "x264enc tune=zerolatency speed-preset={} bitrate={kbps} key-int-max={} ! \
             video/x-h264,profile=high ! h264parse",
            config.quality.x264_preset(),
            config.fps
        ),
        Codec::Vp9 => format!(
            "vp9enc deadline=1 cpu-used=6 threads=4 row-mt=true target-bitrate={} keyframe-max-dist={} ! vp9parse",
            u64::from(kbps) * 1000,
            config.fps
        ),
    }
}

/// Trecho comum: fonte → conversão → encoder com um keyframe por segundo.
fn encode_chain(src: PipewireSource, config: &Config) -> String {
    format!(
        "pipewiresrc fd={} path={} do-timestamp=true keepalive-time={} ! videoconvert ! videorate ! \
         video/x-raw,framerate={}/1 ! {}",
        src.fd,
        src.node_id,
        // No Wayland a fonte só entrega quadros quando a tela muda; reenviar o
        // último evita que uma tela parada deixe o vídeo (e os segmentos) vazio.
        1000 / config.fps,
        config.fps,
        encoder(config)
    )
}

/// Pipeline do modo replay: grava segmentos de ~5 s em `segment_dir`.
pub fn replay_pipeline(src: PipewireSource, config: &Config, segment_dir: &Path) -> String {
    let pattern = segment_dir.join(segment_pattern(config.container));
    format!(
        "{} ! splitmuxsink name=splitmux muxer-factory={} location={} max-size-time={}",
        encode_chain(src, config),
        config.container.muxer(),
        quote(&pattern),
        SEGMENT_DURATION.as_nanos()
    )
}

/// Pipeline do modo manual: grava um único arquivo em `output`.
pub fn manual_pipeline(src: PipewireSource, config: &Config, output: &Path) -> String {
    let muxer = match config.container {
        Container::Mp4 => "mp4mux faststart=true",
        other => other.muxer(),
    };
    format!("{} ! {muxer} ! filesink location={}", encode_chain(src, config), quote(output))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::Quality;
    use std::path::PathBuf;

    const SRC: PipewireSource = PipewireSource { fd: 7, node_id: 42 };

    #[test]
    fn replay_writes_five_second_segments_into_dir() {
        let p = replay_pipeline(SRC, &Config::default(), Path::new("/tmp/seg"));
        assert!(p.contains("splitmuxsink"), "{p}");
        assert!(p.contains("location=\"/tmp/seg/seg_%05d.mp4\""), "{p}");
        assert!(p.contains("max-size-time=5000000000"), "{p}");
        assert!(!p.contains("filesink"), "{p}");
    }

    #[test]
    fn manual_writes_single_file() {
        let p = manual_pipeline(SRC, &Config::default(), Path::new("/out/a.mp4"));
        assert!(p.contains("mp4mux"), "{p}");
        assert!(p.contains("filesink location=\"/out/a.mp4\""), "{p}");
        assert!(!p.contains("splitmuxsink"), "{p}");
    }

    #[test]
    fn resends_last_frame_so_a_static_screen_still_produces_video() {
        let c = Config { fps: 30, ..Config::default() };
        let p = replay_pipeline(SRC, &c, Path::new("/tmp/s"));
        assert!(p.contains("keepalive-time=33"), "{p}");
        let p = manual_pipeline(SRC, &Config::default(), Path::new("/o.mp4"));
        assert!(p.contains("keepalive-time=16"), "{p}");
    }

    #[test]
    fn uses_portal_fd_and_node() {
        let p = manual_pipeline(SRC, &Config::default(), Path::new("/o.mp4"));
        assert!(p.starts_with("pipewiresrc fd=7 path=42"), "{p}");
    }

    #[test]
    fn applies_fps_bitrate_and_one_keyframe_per_second() {
        let c = Config { fps: 30, quality: Quality::Light, ..Config::default() };
        let p = replay_pipeline(SRC, &c, Path::new("/tmp/s"));
        assert!(p.contains("framerate=30/1"), "{p}");
        assert!(p.contains("bitrate=8000"), "{p}");
        assert!(p.contains("speed-preset=veryfast"), "{p}");
        assert!(p.contains("key-int-max=30"), "{p}");
    }

    #[test]
    fn quotes_paths_with_spaces_and_quotes() {
        let out = PathBuf::from("/home/u/My \"Clips\"/a.mp4");
        let p = manual_pipeline(SRC, &Config::default(), &out);
        assert!(p.contains("location=\"/home/u/My \\\"Clips\\\"/a.mp4\""), "{p}");
    }

    fn with(container: Container, quality: Quality) -> Config {
        Config { container, quality, ..Config::default() }
    }

    #[test]
    fn quality_changes_bitrate_and_preset() {
        let p = manual_pipeline(SRC, &with(Container::Mp4, Quality::Max), Path::new("/o.mp4"));
        assert!(p.contains("bitrate=50000"), "{p}");
        assert!(p.contains("speed-preset=faster"), "{p}");
        let p = manual_pipeline(SRC, &with(Container::Mp4, Quality::High), Path::new("/o.mp4"));
        assert!(p.contains("bitrate=20000"), "{p}");
    }

    #[test]
    fn mkv_replay_uses_matroska_segments() {
        let p = replay_pipeline(SRC, &with(Container::Mkv, Quality::High), Path::new("/tmp/s"));
        assert!(p.contains("muxer-factory=matroskamux"), "{p}");
        assert!(p.contains("location=\"/tmp/s/seg_%05d.mkv\""), "{p}");
        assert!(p.contains("x264enc"), "{p}");
    }

    #[test]
    fn mkv_manual_uses_matroskamux() {
        let p = manual_pipeline(SRC, &with(Container::Mkv, Quality::High), Path::new("/o.mkv"));
        assert!(p.contains("! matroskamux ! filesink location=\"/o.mkv\""), "{p}");
        assert!(!p.contains("mp4mux"), "{p}");
    }

    #[test]
    fn webm_uses_vp9_and_webmmux() {
        let c = with(Container::WebM, Quality::High);
        let p = manual_pipeline(SRC, &c, Path::new("/o.webm"));
        assert!(p.contains("vp9enc"), "{p}");
        assert!(p.contains("target-bitrate=20000000"), "{p}"); // vp9enc usa bits/s
        assert!(p.contains("keyframe-max-dist=60"), "{p}");
        assert!(p.contains("webmmux"), "{p}");
        assert!(!p.contains("x264enc"), "{p}");
        let p = replay_pipeline(SRC, &c, Path::new("/tmp/s"));
        assert!(p.contains("muxer-factory=webmmux"), "{p}");
        assert!(p.contains("seg_%05d.webm"), "{p}");
    }

    #[test]
    fn segment_pattern_follows_container() {
        assert_eq!(segment_pattern(Container::Mp4), "seg_%05d.mp4");
        assert_eq!(segment_pattern(Container::WebM), "seg_%05d.webm");
    }
}
