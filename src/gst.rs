//! Execução real de pipelines GStreamer e o backend de captura.

use std::path::Path;
use std::time::Duration;

use gstreamer as gst;
use gstreamer::prelude::*;

use crate::capture::{manual_pipeline, replay_pipeline, PipewireSource};
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

/// Backend de produção: PipeWire → x264 → MP4.
#[derive(Default)]
pub struct GstBackend {
    runner: Option<GstRunner>,
}

impl CaptureBackend for GstBackend {
    fn start_replay(&mut self, src: PipewireSource, config: &Config, segment_dir: &Path) -> Result<(), RecorderError> {
        self.runner = Some(GstRunner::launch(&replay_pipeline(src, config, segment_dir))?);
        Ok(())
    }

    fn start_manual(&mut self, src: PipewireSource, config: &Config, output: &Path) -> Result<(), RecorderError> {
        self.runner = Some(GstRunner::launch(&manual_pipeline(src, config, output))?);
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
            let desc = with_test_source(&manual_pipeline(src, &config, &out));
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
            let desc = with_test_source(&replay_pipeline(src, &config, dir.path()));
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
}
