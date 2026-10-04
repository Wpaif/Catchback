//! Execução real de pipelines GStreamer e o backend de captura.

use std::path::Path;
use std::time::Duration;

use gstreamer as gst;
use gstreamer::prelude::*;

use crate::audio::AudioPlan;
use crate::capture::{manual_pipeline, mic_probe_pipeline, replay_pipeline, PipewireSource};
use crate::config::Config;
use crate::recorder::{CaptureBackend, RecorderError};

/// Tempo para o splitmuxsink fechar o segmento após `split-now` (1 keyframe/s).
const ROTATE_WAIT: Duration = Duration::from_millis(1500);
/// Tempo máximo esperando o EOS ao parar.
const EOS_TIMEOUT: Duration = Duration::from_secs(10);

fn err(e: impl std::fmt::Display) -> RecorderError {
    RecorderError::Capture(e.to_string())
}

/// Um pipeline GStreamer em execução.
pub struct GstRunner {
    pipeline: gst::Pipeline,
}

impl GstRunner {
    pub fn launch(description: &str) -> Result<Self, RecorderError> {
        gst::init().map_err(err)?;
        let pipeline = gst::parse::launch(description)
            .map_err(err)?
            .downcast::<gst::Pipeline>()
            .map_err(|_| err("a descrição não gerou um pipeline"))?;
        let runner = Self { pipeline };
        runner.pipeline.set_state(gst::State::Playing).map_err(err)?;
        // Falhas de fonte (ex.: portal revogado) chegam logo no barramento.
        if let Some(msg) = runner.bus().timed_pop_filtered(
            gst::ClockTime::from_mseconds(300),
            &[gst::MessageType::Error],
        ) {
            return Err(err(error_text(&msg)));
        }
        Ok(runner)
    }

    fn bus(&self) -> gst::Bus {
        self.pipeline.bus().expect("pipeline sempre tem bus")
    }

    /// Fecha o segmento atual do elemento `splitmux` (se houver).
    pub fn rotate(&self) -> Result<(), RecorderError> {
        let splitmux = self
            .pipeline
            .by_name("splitmux")
            .ok_or_else(|| err("pipeline sem elemento `splitmux`"))?;
        splitmux.emit_by_name::<()>("split-now", &[]);
        std::thread::sleep(ROTATE_WAIT);
        Ok(())
    }

    /// Envia EOS, espera os arquivos serem finalizados e encerra.
    pub fn stop(self) -> Result<(), RecorderError> {
        self.pipeline.send_event(gst::event::Eos::new());
        let msg = self.bus().timed_pop_filtered(
            gst::ClockTime::from_seconds(EOS_TIMEOUT.as_secs()),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        );
        let result = match msg {
            Some(m) => match m.view() {
                gst::MessageView::Error(_) => Err(err(error_text(&m))),
                _ => Ok(()),
            },
            None => Err(err("tempo esgotado ao finalizar a gravação")),
        };
        let _ = self.pipeline.set_state(gst::State::Null);
        result
    }
}

impl Drop for GstRunner {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

fn error_text(msg: &gst::Message) -> String {
    match msg.view() {
        gst::MessageView::Error(e) => e.error().to_string(),
        _ => "erro desconhecido".into(),
    }
}

/// Grava cerca de `secs` segundos do microfone (`None` = padrão do sistema) e
/// devolve as amostras (mono, -1 a 1). Bloqueia: rode fora da thread da interface.
pub fn record_mic_sample(source: Option<&str>, secs: f64) -> Result<Vec<f32>, RecorderError> {
    let path = std::env::temp_dir().join(format!("catchback-mic-test-{}.wav", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let runner = GstRunner::launch(&mic_probe_pipeline(source, &path))?;
    std::thread::sleep(Duration::from_secs_f64(secs));
    let stopped = runner.stop();
    let bytes = std::fs::read(&path);
    let _ = std::fs::remove_file(&path);
    stopped?;
    Ok(crate::mic::parse_wav_s16(&bytes?))
}

/// Backend de produção: PipeWire → x264 → MP4.
#[derive(Default)]
pub struct GstBackend {
    runner: Option<GstRunner>,
}

impl CaptureBackend for GstBackend {
    fn start_replay(&mut self, src: PipewireSource, audio: &AudioPlan, config: &Config, segment_dir: &Path) -> Result<(), RecorderError> {
        self.runner = Some(GstRunner::launch(&replay_pipeline(src, audio, config, segment_dir))?);
        Ok(())
    }

    fn start_manual(&mut self, src: PipewireSource, audio: &AudioPlan, config: &Config, output: &Path) -> Result<(), RecorderError> {
        self.runner = Some(GstRunner::launch(&manual_pipeline(src, audio, config, output))?);
        Ok(())
    }

    fn rotate(&mut self) -> Result<(), RecorderError> {
        match &self.runner {
            Some(r) => r.rotate(),
            None => Err(err("nenhuma captura ativa")),
        }
    }

    fn stop(&mut self) -> Result<(), RecorderError> {
        match self.runner.take() {
            Some(r) => r.stop(),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gst_ready() -> bool {
        gst::init().is_ok()
            && ["videotestsrc", "x264enc", "splitmuxsink", "mp4mux"]
                .iter()
                .all(|e| gst::ElementFactory::find(e).is_some())
    }

    fn segments(dir: &Path) -> Vec<std::path::PathBuf> {
        let mut v: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == "mp4"))
            .collect();
        v.sort();
        v
    }

    #[test]
    fn invalid_description_is_an_error() {
        if !gst_ready() {
            return;
        }
        assert!(GstRunner::launch("elemento-que-nao-existe ! fakesink").is_err());
    }

    #[test]
    fn rotate_closes_segment_and_stop_finalizes_files() {
        if !gst_ready() {
            eprintln!("GStreamer incompleto: teste ignorado");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let desc = format!(
            "videotestsrc is-live=true ! video/x-raw,framerate=10/1,width=64,height=64 ! videoconvert ! \
             x264enc tune=zerolatency key-int-max=10 ! h264parse ! \
             splitmuxsink name=splitmux location=\"{}/seg_%05d.mp4\" max-size-time=60000000000",
            dir.path().display()
        );
        let runner = GstRunner::launch(&desc).unwrap();
        std::thread::sleep(Duration::from_millis(1200));
        runner.rotate().unwrap(); // sem rotate só existiria 1 segmento (limite de 60 s)
        std::thread::sleep(Duration::from_millis(500));
        runner.stop().unwrap();

        let segs = segments(dir.path());
        assert!(segs.len() >= 2, "esperava >= 2 segmentos, achei {segs:?}");
        for s in segs {
            assert!(std::fs::metadata(&s).unwrap().len() > 0, "{s:?} vazio");
        }
    }

    #[test]
    fn manual_pipeline_stop_produces_playable_file() {
        if !gst_ready() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("gravacao.mp4");
        let desc = format!(
            "videotestsrc is-live=true ! video/x-raw,framerate=10/1,width=64,height=64 ! videoconvert ! \
             x264enc tune=zerolatency key-int-max=10 ! h264parse ! mp4mux faststart=true ! filesink location=\"{}\"",
            out.display()
        );
        let runner = GstRunner::launch(&desc).unwrap();
        std::thread::sleep(Duration::from_millis(2000));
        runner.stop().unwrap();
        let probe = std::process::Command::new("ffprobe")
            .args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"])
            .arg(&out)
            .output()
            .unwrap();
        let dur: f64 = String::from_utf8_lossy(&probe.stdout).trim().parse().expect("arquivo inválido");
        assert!(dur > 1.0, "duração {dur}");
    }

    /// Troca só a fonte PipeWire por um `videotestsrc`, mantendo o resto do pipeline real.
    fn with_test_source(pipeline: &str) -> String {
        let (_, rest) = pipeline.split_once(" ! ").unwrap();
        format!("videotestsrc is-live=true ! video/x-raw,width=128,height=128 ! {rest}")
    }

    fn duration_of(path: &Path) -> f64 {
        let probe = std::process::Command::new("ffprobe")
            .args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"])
            .arg(path)
            .output()
            .unwrap();
        String::from_utf8_lossy(&probe.stdout).trim().parse().unwrap_or(0.0)
    }

    #[test]
    fn every_container_records_a_playable_manual_file() {
        if !gst_ready() {
            return;
        }
        use crate::format::{Container, Quality};
        let src = PipewireSource { fd: 0, node_id: 0 };
        for container in Container::ALL {
            let dir = tempfile::tempdir().unwrap();
            let out = dir.path().join(format!("a.{}", container.extension()));
            let config = Config { container, quality: Quality::Light, fps: 30, ..Config::default() };
            let desc = with_test_source(&manual_pipeline(src, &AudioPlan::default(), &config, &out));
            let runner = GstRunner::launch(&desc).unwrap_or_else(|e| panic!("{container:?}: {e}"));
            std::thread::sleep(Duration::from_millis(2500));
            runner.stop().unwrap_or_else(|e| panic!("{container:?}: {e}"));
            let dur = duration_of(&out);
            assert!(dur > 1.0, "{container:?}: duração {dur}");
        }
    }

    #[test]
    fn every_container_replay_segments_can_be_joined() {
        if !gst_ready() {
            return;
        }
        use crate::clip::{concat_list, ffmpeg_args};
        use crate::format::{Container, Quality};
        let src = PipewireSource { fd: 0, node_id: 0 };
        for container in Container::ALL {
            let dir = tempfile::tempdir().unwrap();
            let config = Config { container, quality: Quality::Light, fps: 30, ..Config::default() };
            let desc = with_test_source(&replay_pipeline(src, &AudioPlan::default(), &config, dir.path()));
            let runner = GstRunner::launch(&desc).unwrap_or_else(|e| panic!("{container:?}: {e}"));
            std::thread::sleep(Duration::from_millis(1500));
            runner.rotate().unwrap();
            std::thread::sleep(Duration::from_millis(500));
            runner.stop().unwrap();

            let mut segs: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().path()).collect();
            segs.sort();
            assert!(segs.len() >= 2, "{container:?}: {segs:?}");
            assert!(segs.iter().all(|p| p.extension().unwrap() == container.extension()));
            let list = dir.path().join("list.txt");
            let segments: Vec<_> = segs
                .iter()
                .map(|p| crate::buffer::Segment { path: p.clone(), duration: Duration::from_secs(1) })
                .collect();
            std::fs::write(&list, concat_list(&segments)).unwrap();
            let out = dir.path().join(format!("clip.{}", container.extension()));
            let status = std::process::Command::new("ffmpeg")
                .args(["-v", "error"])
                .args(ffmpeg_args(&list, &out))
                .status()
                .unwrap();
            assert!(status.success(), "{container:?}: ffmpeg falhou");
            assert!(duration_of(&out) > 1.5, "{container:?}");
        }
    }

    /// Troca o primeiro `pipewiresrc` (vídeo) por `videotestsrc`. Os de áudio viram
    /// `audio_src`, ou ficam como estão (PipeWire real) quando `audio_src` é `None`.
    fn with_test_sources(pipeline: &str, audio_src: Option<&str>) -> String {
        let mut out = String::new();
        let mut rest = pipeline;
        let mut first = true;
        while let Some(start) = rest.find("pipewiresrc") {
            let after = &rest[start..];
            let end = after.find(" ! ").unwrap_or(after.len());
            out.push_str(&rest[..start]);
            match (first, audio_src) {
                (true, _) => out.push_str("videotestsrc is-live=true ! video/x-raw,width=128,height=128"),
                (false, Some(src)) => out.push_str(src),
                (false, None) => out.push_str(&after[..end]),
            }
            first = false;
            rest = &after[end..];
        }
        out.push_str(rest);
        out
    }

    /// Como `with_test_sources`, mas cada fonte de áudio vem de `audio_src(índice)`.
    fn with_indexed_audio(pipeline: &str, audio_src: &dyn Fn(usize) -> String) -> String {
        let mut index = 0;
        let mut out = String::new();
        let mut rest = pipeline;
        let mut first = true;
        while let Some(start) = rest.find("pipewiresrc") {
            let after = &rest[start..];
            let end = after.find(" ! ").unwrap_or(after.len());
            out.push_str(&rest[..start]);
            if first {
                out.push_str("videotestsrc is-live=true ! video/x-raw,width=128,height=128");
            } else {
                out.push_str(&audio_src(index));
                index += 1;
            }
            first = false;
            rest = &after[end..];
        }
        out.push_str(rest);
        out
    }

    /// Pico (dB) da n-ésima faixa de áudio.
    fn track_peak_db(path: &Path, track: usize) -> f64 {
        let out = std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-nostats", "-i"])
            .arg(path)
            .args(["-map", &format!("0:a:{track}"), "-af", "volumedetect", "-f", "null", "-"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stderr)
            .lines()
            .find_map(|l| l.split("max_volume:").nth(1))
            .and_then(|v| v.trim().trim_end_matches(" dB").parse().ok())
            .unwrap_or_else(|| panic!("sem faixa {track} em {path:?}"))
    }

    /// Fontes de teste com amplitudes bem diferentes: 1ª baixa, 2ª média, 3ª alta.
    fn loudness_ladder(i: usize) -> String {
        let volume = [0.05, 0.3, 0.9][i];
        format!("audiotestsrc is-live=true wave=sine volume={volume}")
    }

    fn assert_ladder(path: &Path, tracks: usize, what: &str) {
        let peaks: Vec<f64> = (0..tracks).map(|t| track_peak_db(path, t)).collect();
        assert!(
            peaks.windows(2).all(|w| w[1] - w[0] > 5.0),
            "{what}: faixas fora de ordem ou faltando ({peaks:?} dB)"
        );
    }

    fn tracks_plan() -> AudioPlan {
        use crate::audio::GameAudio;
        AudioPlan { game: Some(GameAudio::App(vec![7])), mic: true, separate_tracks: true, ..AudioPlan::default() }
    }

    #[test]
    fn separate_tracks_keep_game_system_mic_order_in_every_container_manual() {
        if !gst_ready() || gst::ElementFactory::find("audiomixer").is_none() {
            return;
        }
        use crate::format::{Container, Quality};
        let src = PipewireSource { fd: 0, node_id: 0 };
        for container in Container::ALL {
            let dir = tempfile::tempdir().unwrap();
            let out = dir.path().join(format!("a.{}", container.extension()));
            let config = Config { container, quality: Quality::Light, fps: 30, ..Config::default() };
            let desc = with_indexed_audio(&manual_pipeline(src, &tracks_plan(), &config, &out), &loudness_ladder);
            let runner = GstRunner::launch(&desc).unwrap_or_else(|e| panic!("{container:?}: {e}\n{desc}"));
            std::thread::sleep(Duration::from_millis(2500));
            runner.stop().unwrap();
            assert_eq!(stream_types(&out), ["audio", "audio", "audio", "video"], "{container:?}");
            assert_ladder(&out, 3, &format!("manual {container:?}"));
        }
    }

    /// Quatro níveis bem diferentes: jogo < sistema < call < microfone.
    fn loudness_ladder4(i: usize) -> String {
        let volume = [0.02, 0.08, 0.3, 0.9][i];
        format!("audiotestsrc is-live=true wave=sine volume={volume}")
    }

    #[test]
    fn four_separate_tracks_with_the_call_keep_game_system_call_mic_order() {
        if !gst_ready() || gst::ElementFactory::find("audiomixer").is_none() {
            return;
        }
        use crate::audio::GameAudio;
        use crate::format::{Container, Quality};
        let src = PipewireSource { fd: 0, node_id: 0 };
        let plan = AudioPlan {
            game: Some(GameAudio::App(vec![7])),
            mic: true,
            call: Some(vec![9]),
            separate_tracks: true,
            ..AudioPlan::default()
        };
        for container in [Container::Mp4, Container::Mkv, Container::WebM] {
            let dir = tempfile::tempdir().unwrap();
            let out = dir.path().join(format!("a.{}", container.extension()));
            let config = Config { container, quality: Quality::Light, fps: 30, ..Config::default() };
            let desc = with_indexed_audio(&manual_pipeline(src, &plan, &config, &out), &loudness_ladder4);
            let runner = GstRunner::launch(&desc).unwrap_or_else(|e| panic!("{container:?}: {e}\n{desc}"));
            std::thread::sleep(Duration::from_millis(2500));
            runner.stop().unwrap();
            assert_eq!(stream_types(&out), ["audio", "audio", "audio", "audio", "video"], "{container:?}");
            assert_ladder(&out, 4, &format!("4 faixas {container:?}"));
        }
    }

    #[test]
    fn separate_tracks_keep_their_order_through_replay_segments_and_concat() {
        if !gst_ready() || gst::ElementFactory::find("audiomixer").is_none() {
            return;
        }
        use crate::clip::{concat_list, ffmpeg_args};
        use crate::format::{Container, Quality};
        let src = PipewireSource { fd: 0, node_id: 0 };
        for container in Container::ALL {
            let dir = tempfile::tempdir().unwrap();
            let config = Config { container, quality: Quality::Light, fps: 30, ..Config::default() };
            let desc = with_indexed_audio(&replay_pipeline(src, &tracks_plan(), &config, dir.path()), &loudness_ladder);
            let runner = GstRunner::launch(&desc).unwrap_or_else(|e| panic!("{container:?}: {e}\n{desc}"));
            std::thread::sleep(Duration::from_millis(1800));
            runner.rotate().unwrap();
            std::thread::sleep(Duration::from_millis(500));
            runner.stop().unwrap();

            let mut segs: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().path()).collect();
            segs.sort();
            assert!(segs.len() >= 2, "{container:?}: {segs:?}");
            for seg in &segs {
                assert_ladder(seg, 3, &format!("segmento {container:?}"));
            }
            let list = dir.path().join("list.txt");
            let segments: Vec<_> = segs
                .iter()
                .map(|p| crate::buffer::Segment { path: p.clone(), duration: Duration::from_secs(1) })
                .collect();
            std::fs::write(&list, concat_list(&segments)).unwrap();
            let out = dir.path().join(format!("clip.{}", container.extension()));
            let ok = std::process::Command::new("ffmpeg")
                .args(["-v", "error"])
                .args(ffmpeg_args(&list, &out))
                .status()
                .unwrap()
                .success();
            assert!(ok, "{container:?}: ffmpeg falhou");
            assert_ladder(&out, 3, &format!("clip {container:?}"));
        }
    }

    fn stream_types(path: &Path) -> Vec<String> {
        let probe = std::process::Command::new("ffprobe")
            .args(["-v", "error", "-show_entries", "stream=codec_type", "-of", "csv=p=0"])
            .arg(path)
            .output()
            .unwrap();
        let mut v: Vec<String> = String::from_utf8_lossy(&probe.stdout).lines().map(str::to_string).collect();
        v.sort();
        v
    }

    /// Duração de uma faixa pelo último pacote (o MKV não guarda duração por faixa).
    fn stream_duration(path: &Path, kind: &str) -> f64 {
        let probe = std::process::Command::new("ffprobe")
            .args(["-v", "error", "-select_streams", kind, "-show_entries", "packet=pts_time", "-of", "csv=p=0"])
            .arg(path)
            .output()
            .unwrap();
        String::from_utf8_lossy(&probe.stdout)
            .lines()
            .filter_map(|l| l.trim().parse::<f64>().ok())
            .fold(0.0, f64::max)
    }

    fn game_and_mic() -> AudioPlan {
        use crate::audio::GameAudio;
        AudioPlan { game: Some(GameAudio::System), mic: true, ..AudioPlan::default() }
    }

    const SINE: &str = "audiotestsrc is-live=true wave=sine";

    #[test]
    fn every_container_records_video_plus_mixed_audio() {
        if !gst_ready() || !["avenc_aac", "opusenc", "audiomixer"].iter().all(|e| gst::ElementFactory::find(e).is_some()) {
            return;
        }
        use crate::format::{Container, Quality};
        let src = PipewireSource { fd: 0, node_id: 0 };
        for container in Container::ALL {
            let dir = tempfile::tempdir().unwrap();
            let out = dir.path().join(format!("a.{}", container.extension()));
            let config = Config { container, quality: Quality::Light, fps: 30, ..Config::default() };
            let desc = with_test_sources(&manual_pipeline(src, &game_and_mic(), &config, &out), Some(SINE));
            let runner = GstRunner::launch(&desc).unwrap_or_else(|e| panic!("{container:?}: {e}\n{desc}"));
            std::thread::sleep(Duration::from_millis(3000));
            runner.stop().unwrap_or_else(|e| panic!("{container:?}: {e}"));
            assert_eq!(stream_types(&out), ["audio", "video"], "{container:?}");
            assert!(stream_duration(&out, "a") > 1.5, "{container:?}: áudio curto");
            assert!(stream_duration(&out, "v") > 1.5, "{container:?}: vídeo curto");
        }
    }

    /// Pico de volume (dB) da faixa de áudio, medido pelo `volumedetect` do ffmpeg.
    fn max_volume_db(path: &Path) -> f64 {
        let out = std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-nostats", "-i"])
            .arg(path)
            .args(["-vn", "-af", "volumedetect", "-f", "null", "-"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stderr)
            .lines()
            .find_map(|l| l.split("max_volume:").nth(1))
            .and_then(|v| v.trim().trim_end_matches(" dB").parse().ok())
            .expect("volumedetect não devolveu max_volume")
    }

    #[test]
    fn game_volume_really_changes_the_recorded_loudness() {
        if !gst_ready() || gst::ElementFactory::find("audiomixer").is_none() {
            return;
        }
        use crate::audio::GameAudio;
        let src = PipewireSource { fd: 0, node_id: 0 };
        let record = |percent: u32| {
            let dir = tempfile::tempdir().unwrap();
            let out = dir.path().join("a.mp4");
            let audio = AudioPlan { game: Some(GameAudio::System), game_volume: percent, ..AudioPlan::default() };
            let desc = with_test_sources(&manual_pipeline(src, &audio, &Config::default(), &out), Some(SINE));
            let runner = GstRunner::launch(&desc).unwrap();
            std::thread::sleep(Duration::from_millis(2500));
            runner.stop().unwrap();
            let db = max_volume_db(&out);
            drop(dir);
            db
        };
        let (full, quarter, muted) = (record(100), record(25), record(0));
        // 25% = -12 dB (tolerância larga: a codificação AAC altera um pouco o pico)
        assert!((full - quarter - 12.0).abs() < 3.0, "100%: {full} dB, 25%: {quarter} dB");
        assert!(muted < -60.0, "0% deveria ser silêncio, deu {muted} dB");
        assert!(full > -20.0, "100% inaudível: {full} dB");
    }

    #[test]
    fn stalled_game_source_does_not_stall_the_recording() {
        if !gst_ready() || gst::ElementFactory::find("audiomixer").is_none() {
            return;
        }
        use crate::audio::GameAudio;
        let src = PipewireSource { fd: 0, node_id: 0 };
        let audio = AudioPlan { game: Some(GameAudio::App(vec![1])), mic: false, ..AudioPlan::default() };
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("a.mp4");
        // fonte do jogo muda: viva mas sem entregar nenhum buffer (como um app que fechou)
        let stalled = format!("{SINE} ! identity drop-probability=1.0");
        let desc = with_test_sources(&manual_pipeline(src, &audio, &Config::default(), &out), Some(&stalled));
        let runner = GstRunner::launch(&desc).unwrap();
        std::thread::sleep(Duration::from_millis(3000));
        runner.stop().unwrap();
        assert!(stream_duration(&out, "v") > 1.5, "o vídeo parou junto com o áudio");
        assert!(stream_duration(&out, "a") > 1.5, "o silêncio de base não manteve o áudio");
    }

    #[test]
    fn replay_segments_carry_audio_and_join_into_a_clip() {
        if !gst_ready() || gst::ElementFactory::find("audiomixer").is_none() {
            return;
        }
        use crate::clip::{concat_list, ffmpeg_args};
        use crate::format::{Container, Quality};
        let src = PipewireSource { fd: 0, node_id: 0 };
        for container in Container::ALL {
            let dir = tempfile::tempdir().unwrap();
            let config = Config { container, quality: Quality::Light, fps: 30, ..Config::default() };
            let desc = with_test_sources(&replay_pipeline(src, &game_and_mic(), &config, dir.path()), Some(SINE));
            let runner = GstRunner::launch(&desc).unwrap_or_else(|e| panic!("{container:?}: {e}\n{desc}"));
            std::thread::sleep(Duration::from_millis(1800));
            runner.rotate().unwrap();
            std::thread::sleep(Duration::from_millis(500));
            runner.stop().unwrap();

            let mut segs: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().path()).collect();
            segs.sort();
            assert!(segs.len() >= 2, "{container:?}: {segs:?}");
            for seg in &segs {
                assert_eq!(stream_types(seg), ["audio", "video"], "{container:?}: {seg:?}");
            }
            let list = dir.path().join("list.txt");
            let segments: Vec<_> = segs
                .iter()
                .map(|p| crate::buffer::Segment { path: p.clone(), duration: Duration::from_secs(1) })
                .collect();
            std::fs::write(&list, concat_list(&segments)).unwrap();
            let out = dir.path().join(format!("clip.{}", container.extension()));
            let ok = std::process::Command::new("ffmpeg")
                .args(["-v", "error"])
                .args(ffmpeg_args(&list, &out))
                .status()
                .unwrap()
                .success();
            assert!(ok, "{container:?}: ffmpeg falhou");
            assert_eq!(stream_types(&out), ["audio", "video"], "{container:?}: clip final");
        }
    }

    /// Usa o PipeWire de verdade para o áudio (microfone, sistema e um app tocando).
    /// Rodar com: `cargo test real_pipewire_audio -- --ignored --nocapture`
    #[test]
    #[ignore = "precisa de PipeWire com áudio; rode com --ignored"]
    fn real_pipewire_audio_sources_record() {
        use crate::audio::{list_apps, GameAudio};
        assert!(gst_ready());
        let src = PipewireSource { fd: 0, node_id: 0 };
        let apps = list_apps().unwrap_or_default();
        println!("apps tocando: {apps:?}");
        let mut plans = vec![
            ("mic", AudioPlan { game: None, mic: true, ..AudioPlan::default() }),
            ("sistema", AudioPlan { game: Some(GameAudio::System), mic: false, ..AudioPlan::default() }),
            ("sistema+mic", AudioPlan { game: Some(GameAudio::System), mic: true, ..AudioPlan::default() }),
        ];
        if let Some(app) = apps.first() {
            plans.push(("app", AudioPlan { game: Some(GameAudio::App(app.serials.clone())), mic: true, ..AudioPlan::default() }));
        }        let tracks = apps.first().map(|app| AudioPlan {
            game: Some(GameAudio::App(app.serials.clone())),
            mic: true,
            separate_tracks: true,
            ..AudioPlan::default()
        });

        for (name, plan) in plans {
            let dir = tempfile::tempdir().unwrap();
            let out = dir.path().join("a.mp4");
            let desc = with_test_sources(&manual_pipeline(src, &plan, &Config::default(), &out), None);
            let runner = GstRunner::launch(&desc).unwrap_or_else(|e| panic!("{name}: {e}\n{desc}"));
            std::thread::sleep(Duration::from_millis(3000));
            runner.stop().unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(stream_types(&out), ["audio", "video"], "{name}");
            let audio = stream_duration(&out, "a");
            println!("{name}: áudio {audio:.1}s, vídeo {:.1}s", stream_duration(&out, "v"));
            assert!(audio > 1.5, "{name}: áudio curto ({audio})");
        }

        // Faixas separadas com as três fontes reais ao mesmo tempo.
        if let Some(plan) = tracks {
            let dir = tempfile::tempdir().unwrap();
            let out = dir.path().join("a.mp4");
            let desc = with_test_sources(&manual_pipeline(src, &plan, &Config::default(), &out), None);
            let runner = GstRunner::launch(&desc).unwrap_or_else(|e| panic!("faixas: {e}\n{desc}"));
            std::thread::sleep(Duration::from_millis(3000));
            runner.stop().unwrap();
            assert_eq!(stream_types(&out), ["audio", "audio", "audio", "video"], "faixas separadas");
            println!("faixas separadas: 3 faixas de áudio + vídeo, {:.1}s", stream_duration(&out, "a"));
        }
    }

    /// Decodifica a primeira faixa de áudio para PCM mono de 16 bits a 48 kHz.
    fn decode_mono(path: &Path) -> Vec<f64> {
        let out = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(path)
            .args(["-vn", "-map", "0:a:0", "-ac", "1", "-ar", "48000", "-f", "s16le", "-"])
            .output()
            .unwrap();
        out.stdout.as_chunks::<2>().0.iter().map(|b| f64::from(i16::from_le_bytes([b[0], b[1]])) / 32768.0).collect()
    }

    /// Relação sinal/ruído (dB) de um tom de `freq` Hz: ajusta a senoide e mede o
    /// que sobra. Estalos, buracos e saltos derrubam o valor (limpo: > 40 dB).
    fn tone_snr_db(samples: &[f64], freq: f64) -> f64 {
        let skip = 24_000.min(samples.len() / 4); // ignora o começo (partida do encoder)
        let x = &samples[skip..];
        let n = x.len() as f64;
        let w = 2.0 * std::f64::consts::PI * freq / 48_000.0;
        let (mut a, mut b) = (0.0, 0.0);
        for (i, v) in x.iter().enumerate() {
            a += v * (w * i as f64).sin();
            b += v * (w * i as f64).cos();
        }
        let (a, b) = (2.0 * a / n, 2.0 * b / n);
        let (mut signal, mut noise) = (0.0, 0.0);
        for (i, v) in x.iter().enumerate() {
            let fit = a * (w * i as f64).sin() + b * (w * i as f64).cos();
            signal += fit * fit;
            noise += (v - fit) * (v - fit);
        }
        10.0 * (signal / noise.max(1e-12)).log10()
    }

    /// Pior SNR local entre janelas de `window` amostras, depois de ignorar o começo
    /// (partida do encoder). Um único estalo no meio já derruba o valor.
    fn min_window_snr_db(samples: &[f64], freq: f64, skip: usize, window: usize) -> f64 {
        let w = 2.0 * std::f64::consts::PI * freq / 48_000.0;
        let mut worst = f64::INFINITY;
        let mut start = skip;
        while start + window <= samples.len() {
            let seg = &samples[start..start + window];
            let n = window as f64;
            let (mut a, mut b) = (0.0, 0.0);
            for (i, v) in seg.iter().enumerate() {
                let phase = w * (start + i) as f64;
                a += v * phase.sin();
                b += v * phase.cos();
            }
            let (a, b) = (2.0 * a / n, 2.0 * b / n);
            let (mut signal, mut noise) = (0.0, 0.0);
            for (i, v) in seg.iter().enumerate() {
                let phase = w * (start + i) as f64;
                let fit = a * phase.sin() + b * phase.cos();
                signal += fit * fit;
                noise += (v - fit) * (v - fit);
            }
            worst = worst.min(10.0 * (signal / noise.max(1e-12)).log10());
            start += window;
        }
        worst
    }

    #[test]
    fn window_snr_catches_a_single_click_in_the_middle() {
        let tone = |i: u32| 0.3 * (2.0 * std::f64::consts::PI * 440.0 * f64::from(i) / 48_000.0).sin();
        let mut audio: Vec<f64> = (0..240_000).map(tone).collect();
        assert!(min_window_snr_db(&audio, 440.0, 96_000, 12_000) > 60.0);
        // um "pop": 2 ms de amostras trocadas por um pico no meio da gravação
        for v in &mut audio[150_000..150_096] {
            *v = 0.9;
        }
        assert!(min_window_snr_db(&audio, 440.0, 96_000, 12_000) < 25.0);
    }

    #[test]
    fn tone_snr_detects_clean_and_damaged_audio() {
        let clean: Vec<f64> = (0..96_000).map(|i| 0.3 * (2.0 * std::f64::consts::PI * 440.0 * f64::from(i) / 48_000.0).sin()).collect();
        assert!(tone_snr_db(&clean, 440.0) > 60.0);
        let mut gaps = clean.clone();
        for chunk in gaps.chunks_mut(2_400).step_by(3) {
            chunk.iter_mut().for_each(|v| *v = 0.0); // buracos de 50 ms
        }
        assert!(tone_snr_db(&gaps, 440.0) < 15.0);
    }

    /// Reproduz a máquina que não acompanha a codificação: cada quadro leva muito
    /// mais que o tempo real. O vídeo pode perder quadros, o áudio não.
    fn slow_encoder(pipeline: &str, micros_per_frame: u32) -> String {
        pipeline.replace("x264enc", &format!("identity sleep-time={micros_per_frame} ! x264enc"))
    }

    fn record_tone_with_slow_encoder(audio: &AudioPlan, container: crate::format::Container, secs: u64) -> Vec<f64> {
        let src = PipewireSource { fd: 0, node_id: 0 };
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join(format!("a.{}", container.extension()));
        let config = Config { container, fps: 60, ..Config::default() };
        let delay = std::env::var("CATCHBACK_TEST_ENCODER_DELAY_US").ok().and_then(|v| v.parse().ok()).unwrap_or(60_000);
        let desc = slow_encoder(&manual_pipeline(src, audio, &config, &out), delay); // 60 ms = ~16 fps de verdade
        let desc = with_test_sources(&desc, Some("audiotestsrc is-live=true wave=sine freq=440 volume=0.3"));
        if std::env::var_os("CATCHBACK_PRINT_PIPELINE").is_some() {
            eprintln!("PIPELINE: {desc}");
        }
        let runner = GstRunner::launch(&desc).unwrap_or_else(|e| panic!("{e}\n{desc}"));
        std::thread::sleep(Duration::from_secs(secs));
        runner.stop().unwrap();
        if let Some(keep) = std::env::var_os("CATCHBACK_KEEP_RECORDINGS") {
            let name = format!("{}-{}.{}", std::process::id(), audio.mic, container.extension());
            let _ = std::fs::copy(&out, Path::new(&keep).join(name));
        }
        decode_mono(&out)
    }

    #[test]
    fn audio_stays_clean_when_the_video_encoder_cannot_keep_up() {
        if !gst_ready() || gst::ElementFactory::find("audiomixer").is_none() {
            return;
        }
        use crate::audio::GameAudio;
        use crate::format::Container;
        let plans = [
            ("sistema", AudioPlan { game: Some(GameAudio::System), ..AudioPlan::default() }),
            ("sistema+mic", AudioPlan { game: Some(GameAudio::System), mic: true, ..AudioPlan::default() }),
            ("app", AudioPlan { game: Some(GameAudio::App(vec![1])), ..AudioPlan::default() }),
        ];
        for (name, plan) in plans {
            let samples = record_tone_with_slow_encoder(&plan, Container::Mp4, 8);
            let seconds = samples.len() as f64 / 48_000.0;
            let zeros = samples.iter().filter(|v| v.abs() < 1e-5).count() as f64 / samples.len() as f64;
            // ignora os 2 s iniciais (partida) e olha cada janela de 250 ms do resto
            let snr = min_window_snr_db(&samples, 440.0, 96_000, 12_000);
            println!("{name}: {seconds:.1}s, {:.1}% de silêncio, pior janela de 250 ms: SNR {snr:.1} dB", zeros * 100.0);
            assert!(seconds > 6.5, "{name}: o áudio ficou curto ({seconds:.1}s)");
            // 'sistema+mic' soma dois tons iguais; ainda é uma senoide de 440 Hz
            assert!(zeros < 0.02, "{name}: {:.0}% do áudio é silêncio (buracos)", zeros * 100.0);
            assert!(snr > 30.0, "{name}: áudio estragado (SNR {snr:.1} dB)");
        }
    }

    /// Mede os microfones reais deste sistema.
    /// Rodar com: `cargo test real_mic_levels -- --ignored --nocapture`
    #[test]
    #[ignore = "usa o microfone de verdade; rode com --ignored"]
    fn real_mic_levels() {
        use crate::mic::{levels, list_mics, verdict, verdict_message};
        assert!(gst_ready());
        let mics = list_mics().expect("sem pactl/pw-dump");
        println!("microfones: {mics:#?}");
        let mut targets: Vec<(String, Option<String>)> = vec![("padrão do sistema".into(), None)];
        targets.extend(mics.iter().map(|m| (m.description.clone(), Some(m.name.clone()))));
        for (label, name) in targets {
            let samples = record_mic_sample(name.as_deref(), 2.0).expect("gravação do teste");
            let l = levels(&samples);
            println!(
                "{label}: {} amostras, pico {:.2}, rms {:.3}, {:.1}% no limite -> {:?}: {}",
                samples.len(),
                l.peak,
                l.rms,
                l.clipped * 100.0,
                verdict(&l),
                verdict_message(verdict(&l))
            );
        }
    }
}
