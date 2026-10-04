//! Orquestra sessão, buffer circular, backend de captura e exportação de clips.

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::NaiveDateTime;

use crate::buffer::{Segment, SegmentBuffer};
use crate::audio::{AudioPlan, MusicChoice, TrackGain};
use crate::capture::{PipewireSource, SEGMENT_DURATION};
use crate::clip::{clip_filename, unique_path};
use crate::config::Config;
use crate::format::Container;
use crate::session::{Mode, Session, SessionError, State};

#[derive(Debug, thiserror::Error)]
pub enum RecorderError {
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error("falha na captura: {0}")]
    Capture(String),
    #[error("falha ao exportar: {0}")]
    Export(String),
    #[error("ainda não há vídeo suficiente no buffer")]
    NothingBuffered,
    #[error("erro de E/S: {0}")]
    Io(#[from] std::io::Error),
}

/// Quem realmente captura a tela (GStreamer na prática, um fake nos testes).
pub trait CaptureBackend {
    fn start_replay(&mut self, src: PipewireSource, audio: &AudioPlan, config: &Config, segment_dir: &Path) -> Result<(), RecorderError>;
    fn start_manual(&mut self, src: PipewireSource, audio: &AudioPlan, config: &Config, output: &Path) -> Result<(), RecorderError>;
    /// Fecha o segmento atual para que ele possa entrar no clip.
    fn rotate(&mut self) -> Result<(), RecorderError>;
    /// Finaliza os arquivos (EOS) e para o pipeline.
    fn stop(&mut self) -> Result<(), RecorderError>;
}

/// Refazer o áudio do arquivo mixando só estas faixas (cada uma com seu ganho).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remix {
    pub tracks: Vec<TrackGain>,
    pub container: Container,
}

/// Junta segmentos em um único arquivo.
pub trait Exporter {
    /// Com `remix`, o vídeo é copiado e o áudio refeito; sem ele, tudo é copiado.
    fn export(&self, segments: &[Segment], output: &Path, remix: Option<&Remix>) -> Result<(), RecorderError>;
    /// Refaz o áudio de um arquivo já gravado em `output`.
    fn remix_file(&self, input: &Path, output: &Path, remix: &Remix) -> Result<(), RecorderError>;
}

pub struct Recorder<B, E> {
    session: Session,
    backend: B,
    exporter: E,
    config: Config,
    segment_dir: PathBuf,
    buffer: Option<SegmentBuffer>,
    last_index: Option<u32>,
    /// Contêiner da captura em andamento (ou da última).
    container: Container,
    /// Áudio da captura em andamento (ou da última).
    plan: AudioPlan,
    manual_output: Option<PathBuf>,
}

/// Extrai o índice de `seg_00012.<ext>`.
fn segment_index(name: &str, ext: &str) -> Option<u32> {
    name.strip_prefix("seg_")?.strip_suffix(ext)?.strip_suffix('.')?.parse().ok()
}

impl<B: CaptureBackend, E: Exporter> Recorder<B, E> {
    pub fn new(backend: B, exporter: E, config: Config, segment_dir: PathBuf) -> Self {
        Self {
            session: Session::new(),
            backend,
            exporter,
            config,
            segment_dir,
            buffer: None,
            last_index: None,
            container: Container::default(),
            plan: AudioPlan::default(),
            manual_output: None,
        }
    }

    pub fn state(&self) -> State {
        self.session.state()
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Vale a partir da próxima captura.
    pub fn set_config(&mut self, config: Config) {
        self.config = config;
    }

    pub fn start(&mut self, mode: Mode, src: PipewireSource, audio: &AudioPlan, now: NaiveDateTime) -> Result<(), RecorderError> {
        self.session.start(mode)?;
        self.container = self.config.container;
        self.plan = audio.clone();
        let result = match mode {
            Mode::Manual => self.start_manual(src, audio, now),
            Mode::Replay => self.start_replay(src, audio),
        };
        if result.is_err() {
            let _ = self.session.stop();
        }
        result
    }

    fn start_manual(&mut self, src: PipewireSource, audio: &AudioPlan, now: NaiveDateTime) -> Result<(), RecorderError> {
        std::fs::create_dir_all(&self.config.output_dir)?;
        let output = unique_path(&self.config.output_dir, &clip_filename(Mode::Manual, now, self.container));
        self.backend.start_manual(src, audio, &self.config, &output)?;
        self.manual_output = Some(output);
        Ok(())
    }

    fn start_replay(&mut self, src: PipewireSource, audio: &AudioPlan) -> Result<(), RecorderError> {
        self.clear_segments()?;
        std::fs::create_dir_all(&self.segment_dir)?;
        self.backend.start_replay(src, audio, &self.config, &self.segment_dir)?;
        self.buffer = Some(SegmentBuffer::new(self.config.buffer_window()));
        self.last_index = None;
        Ok(())
    }

    fn clear_segments(&self) -> Result<(), RecorderError> {
        match std::fs::remove_dir_all(&self.segment_dir) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }

    /// Para a captura. No modo manual devolve o arquivo gravado.
    pub fn stop(&mut self) -> Result<Option<PathBuf>, RecorderError> {
        let previous = self.session.stop()?;
        let result = self.backend.stop();
        let output = self.manual_output.take();
        if previous == State::Buffering {
            self.buffer = None;
            self.last_index = None;
            self.clear_segments()?;
        }
        result?;
        Ok(output)
    }

    /// Registra novos segmentos concluídos e apaga os expirados.
    pub fn poll(&mut self) -> Result<(), RecorderError> {
        let Some(buffer) = self.buffer.as_mut() else {
            return Ok(());
        };
        let mut indices: Vec<u32> = std::fs::read_dir(&self.segment_dir)?
            .filter_map(|e| segment_index(&e.ok()?.file_name().to_string_lossy(), self.container.extension()))
            .collect();
        indices.sort_unstable();
        indices.pop(); // o mais recente ainda está sendo escrito
        for index in indices {
            if self.last_index.is_some_and(|last| index <= last) {
                continue;
            }
            self.last_index = Some(index);
            let segment = Segment {
                path: self.segment_dir.join(format!("seg_{index:05}.{}", self.container.extension())),
                duration: SEGMENT_DURATION,
            };
            for expired in buffer.push(segment) {
                let _ = std::fs::remove_file(expired.path);
            }
        }
        Ok(())
    }

    /// A captura (em andamento ou a última) guardou jogo/sistema/mic em faixas
    /// separadas, então a escolha sobre a música ainda precisa ser aplicada.
    pub fn has_separate_tracks(&self) -> bool {
        self.plan.separate_tracks
    }

    fn remix_for(&self, music: MusicChoice) -> Option<Remix> {
        // Com faixas separadas o ganho só é aplicado aqui, então vale o volume atual.
        let live = AudioPlan {
            game_volume: self.config.game_volume,
            mic_volume: self.config.mic_volume,
            ..self.plan.clone()
        };
        live.final_mix(music).map(|tracks| Remix { tracks, container: self.container })
    }

    /// Aplica a escolha sobre a música numa gravação manual já finalizada,
    /// substituindo o arquivo. Não faz nada se o áudio já saiu misturado.
    pub fn mixdown_recording(&self, recording: &Path, music: MusicChoice) -> Result<(), RecorderError> {
        let Some(remix) = self.remix_for(music) else {
            return Ok(());
        };
        let temp = recording.with_extension(format!("mixdown.{}", self.container.extension()));
        let _ = std::fs::remove_file(&temp);
        if let Err(e) = self.exporter.remix_file(recording, &temp, &remix) {
            let _ = std::fs::remove_file(&temp);
            return Err(e);
        }
        std::fs::rename(&temp, recording)?;
        Ok(())
    }

    /// Salva os últimos `span` do buffer como clip.
    pub fn save_clip(&mut self, span: Duration, now: NaiveDateTime, music: MusicChoice) -> Result<PathBuf, RecorderError> {
        self.session.can_save_clip()?;
        self.backend.rotate()?;
        self.poll()?;
        let segments = self.buffer.as_ref().map(|b| b.last(span)).unwrap_or_default();
        if segments.is_empty() {
            return Err(RecorderError::NothingBuffered);
        }
        std::fs::create_dir_all(&self.config.output_dir)?;
        let output = unique_path(&self.config.output_dir, &clip_filename(Mode::Replay, now, self.container));
        self.exporter.export(&segments, &output, self.remix_for(music).as_ref())?;
        Ok(output)
    }

    /// Quanto o buffer já acumulou.
    pub fn buffered(&self) -> Duration {
        self.buffer.as_ref().map_or(Duration::ZERO, SegmentBuffer::total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use std::cell::RefCell;
    use std::rc::Rc;

    const SRC: PipewireSource = PipewireSource { fd: 1, node_id: 2 };

    fn now() -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 10, 4).unwrap().and_hms_opt(15, 30, 12).unwrap()
    }

    #[derive(Default)]
    struct Calls {
        log: Vec<String>,
        exported: Vec<(Vec<String>, PathBuf, Option<Remix>)>,
        remixed: Vec<(PathBuf, PathBuf, Remix)>,
        fail_remix: bool,
    }

    #[derive(Clone, Default)]
    struct Fake {
        calls: Rc<RefCell<Calls>>,
    }

    impl Fake {
        fn log(&self, s: &str) {
            self.calls.borrow_mut().log.push(s.into());
        }
        fn logged(&self) -> Vec<String> {
            self.calls.borrow().log.clone()
        }
    }

    impl CaptureBackend for Fake {
        fn start_replay(&mut self, _: PipewireSource, _: &AudioPlan, _: &Config, _: &Path) -> Result<(), RecorderError> {
            self.log("start_replay");
            Ok(())
        }
        fn start_manual(&mut self, _: PipewireSource, _: &AudioPlan, _: &Config, out: &Path) -> Result<(), RecorderError> {
            self.log(&format!("start_manual {}", out.display()));
            Ok(())
        }
        fn rotate(&mut self) -> Result<(), RecorderError> {
            self.log("rotate");
            Ok(())
        }
        fn stop(&mut self) -> Result<(), RecorderError> {
            self.log("stop");
            Ok(())
        }
    }

    impl Exporter for Fake {
        fn export(&self, segments: &[Segment], output: &Path, remix: Option<&Remix>) -> Result<(), RecorderError> {
            let names = segments
                .iter()
                .map(|s| s.path.file_name().unwrap().to_string_lossy().into_owned())
                .collect();
            self.calls.borrow_mut().exported.push((names, output.to_path_buf(), remix.cloned()));
            std::fs::write(output, b"clip")?;
            Ok(())
        }

        fn remix_file(&self, input: &Path, output: &Path, remix: &Remix) -> Result<(), RecorderError> {
            let fail = self.calls.borrow().fail_remix;
            self.calls.borrow_mut().remixed.push((input.to_path_buf(), output.to_path_buf(), remix.clone()));
            if fail {
                return Err(RecorderError::Export("ffmpeg falhou".into()));
            }
            std::fs::write(output, b"mixed")?;
            Ok(())
        }
    }

    struct Env {
        _tmp: tempfile::TempDir,
        fake: Fake,
        rec: Recorder<Fake, Fake>,
        seg_dir: PathBuf,
        out_dir: PathBuf,
    }

    fn env(buffer_minutes: u32) -> Env {
        let tmp = tempfile::tempdir().unwrap();
        let seg_dir = tmp.path().join("segs");
        let out_dir = tmp.path().join("out");
        let config = Config { buffer_minutes, output_dir: out_dir.clone(), ..Config::default() };
        let fake = Fake::default();
        let rec = Recorder::new(fake.clone(), fake.clone(), config, seg_dir.clone());
        Env { _tmp: tmp, fake, rec, seg_dir, out_dir }
    }

    fn touch(dir: &Path, index: u32) {
        std::fs::write(dir.join(format!("seg_{index:05}.mp4")), b"x").unwrap();
    }

    /// Arquivos que sobraram em `dir` (pasta removida conta como vazia).
    fn remaining(dir: &Path) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut v: Vec<_> = entries
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn manual_start_picks_output_in_output_dir_and_stop_returns_it() {
        let mut e = env(10);
        e.rec.start(Mode::Manual, SRC, &AudioPlan::default(), now()).unwrap();
        let expected = e.out_dir.join("Gravacao_2026-10-04_15-30-12.mp4");
        assert_eq!(e.fake.logged(), vec![format!("start_manual {}", expected.display())]);
        assert_eq!(e.rec.state(), State::Recording);
        assert_eq!(e.rec.stop().unwrap(), Some(expected));
        assert_eq!(e.rec.state(), State::Idle);
    }

    #[test]
    fn mkv_replay_reads_mkv_segments_and_names_clip_mkv() {
        let mut e = env(10);
        e.rec.set_config(Config { container: Container::Mkv, ..e.rec.config().clone() });
        e.rec.start(Mode::Replay, SRC, &AudioPlan::default(), now()).unwrap();
        for i in 0..3 {
            std::fs::write(e.seg_dir.join(format!("seg_{i:05}.mkv")), b"x").unwrap();
        }
        std::fs::write(e.seg_dir.join("seg_00000.mp4"), b"x").unwrap(); // outro formato: ignorado
        let path = e.rec.save_clip(Duration::from_secs(5), now(), MusicChoice::Drop).unwrap();
        assert_eq!(path, e.out_dir.join("Replay_2026-10-04_15-30-12.mkv"));
        assert_eq!(e.fake.calls.borrow().exported[0].0, vec!["seg_00001.mkv"]);
    }

    #[test]
    fn changing_format_mid_capture_does_not_affect_running_replay() {
        let mut e = env(10);
        e.rec.start(Mode::Replay, SRC, &AudioPlan::default(), now()).unwrap();
        e.rec.set_config(Config { container: Container::WebM, ..e.rec.config().clone() });
        for i in 0..3 {
            touch(&e.seg_dir, i); // .mp4, o formato com que a captura começou
        }
        let path = e.rec.save_clip(Duration::from_secs(5), now(), MusicChoice::Drop).unwrap();
        assert!(path.to_string_lossy().ends_with(".mp4"), "{path:?}");
    }

    #[test]
    fn replay_start_creates_clean_segment_dir() {
        let mut e = env(10);
        std::fs::create_dir_all(&e.seg_dir).unwrap();
        touch(&e.seg_dir, 0); // sobra de uma execução anterior
        e.rec.start(Mode::Replay, SRC, &AudioPlan::default(), now()).unwrap();
        assert!(remaining(&e.seg_dir).is_empty());
        assert_eq!(e.rec.state(), State::Buffering);
    }

    #[test]
    fn double_start_is_rejected_without_touching_backend() {
        let mut e = env(10);
        e.rec.start(Mode::Replay, SRC, &AudioPlan::default(), now()).unwrap();
        assert!(matches!(
            e.rec.start(Mode::Manual, SRC, &AudioPlan::default(), now()),
            Err(RecorderError::Session(SessionError::AlreadyRunning))
        ));
        assert_eq!(e.fake.logged(), vec!["start_replay"]);
    }

    #[test]
    fn poll_ignores_segment_still_being_written() {
        let mut e = env(10);
        e.rec.start(Mode::Replay, SRC, &AudioPlan::default(), now()).unwrap();
        touch(&e.seg_dir, 0);
        touch(&e.seg_dir, 1);
        e.rec.poll().unwrap();
        assert_eq!(e.rec.buffered(), SEGMENT_DURATION); // só o 0 está completo
        touch(&e.seg_dir, 2);
        e.rec.poll().unwrap();
        e.rec.poll().unwrap(); // idempotente
        assert_eq!(e.rec.buffered(), SEGMENT_DURATION * 2);
    }

    #[test]
    fn poll_deletes_expired_segments_from_disk() {
        let mut e = env(1); // janela de 60 s = 12 segmentos
        e.rec.start(Mode::Replay, SRC, &AudioPlan::default(), now()).unwrap();
        for i in 0..15 {
            touch(&e.seg_dir, i);
        }
        e.rec.poll().unwrap(); // 0..=13 completos; 0 e 1 expiram
        let files = remaining(&e.seg_dir);
        assert_eq!(files.len(), 13);
        assert_eq!(files[0], "seg_00002.mp4");
    }

    #[test]
    fn save_clip_rotates_then_exports_requested_span() {
        let mut e = env(10);
        e.rec.start(Mode::Replay, SRC, &AudioPlan::default(), now()).unwrap();
        for i in 0..5 {
            touch(&e.seg_dir, i);
        }
        let path = e.rec.save_clip(Duration::from_secs(10), now(), MusicChoice::Drop).unwrap();
        assert_eq!(path, e.out_dir.join("Replay_2026-10-04_15-30-12.mp4"));
        assert!(path.exists());
        assert!(e.fake.logged().contains(&"rotate".to_string()));
        let calls = e.fake.calls.borrow();
        assert_eq!(calls.exported[0].0, vec!["seg_00002.mp4", "seg_00003.mp4"]);
    }

    #[test]
    fn save_clip_fails_when_nothing_buffered() {
        let mut e = env(10);
        e.rec.start(Mode::Replay, SRC, &AudioPlan::default(), now()).unwrap();
        touch(&e.seg_dir, 0); // único segmento = ainda em escrita
        assert!(matches!(e.rec.save_clip(Duration::from_secs(10), now(), MusicChoice::Drop), Err(RecorderError::NothingBuffered)));
    }

    #[test]
    fn save_clip_only_in_replay_mode() {
        let mut e = env(10);
        assert!(matches!(
            e.rec.save_clip(Duration::from_secs(10), now(), MusicChoice::Drop),
            Err(RecorderError::Session(SessionError::NotBuffering))
        ));
    }

    #[test]
    fn stopping_replay_cleans_segments_and_returns_none() {
        let mut e = env(10);
        e.rec.start(Mode::Replay, SRC, &AudioPlan::default(), now()).unwrap();
        touch(&e.seg_dir, 0);
        touch(&e.seg_dir, 1);
        e.rec.poll().unwrap();
        assert_eq!(e.rec.stop().unwrap(), None);
        assert!(remaining(&e.seg_dir).is_empty());
        assert_eq!(e.rec.buffered(), Duration::ZERO);
    }

    use crate::audio::{GameAudio, TrackGain};

    fn tracks_plan() -> AudioPlan {
        AudioPlan {
            game: Some(GameAudio::App(vec![1])),
            mic: true,
            game_volume: 80,
            mic_volume: 120,
            mic_source: None,
            call: None,
            call_volume: 100,
            separate_tracks: true,
        }
    }

    fn start_replay_with(e: &mut Env, plan: &AudioPlan) {
        e.rec.set_config(Config { game_volume: 80, mic_volume: 120, ..e.rec.config().clone() });
        e.rec.start(Mode::Replay, SRC, plan, now()).unwrap();
        for i in 0..4 {
            touch(&e.seg_dir, i);
        }
    }

    #[test]
    fn already_mixed_audio_is_copied_without_remix() {
        let mut e = env(10);
        start_replay_with(&mut e, &AudioPlan { mic: true, ..AudioPlan::default() });
        e.rec.save_clip(Duration::from_secs(5), now(), MusicChoice::Keep).unwrap();
        assert_eq!(e.fake.calls.borrow().exported[0].2, None);
        assert!(!e.rec.has_separate_tracks());
    }

    #[test]
    fn dropping_music_remixes_game_and_mic() {
        let mut e = env(10);
        start_replay_with(&mut e, &tracks_plan());
        e.rec.save_clip(Duration::from_secs(5), now(), MusicChoice::Drop).unwrap();
        let remix = e.fake.calls.borrow().exported[0].2.clone().unwrap();
        assert_eq!(remix.tracks, vec![TrackGain { index: 0, percent: 80 }, TrackGain { index: 2, percent: 120 }]);
        assert_eq!(remix.container, Container::Mp4);
        assert!(e.rec.has_separate_tracks());
    }

    #[test]
    fn keeping_music_remixes_system_and_mic() {
        let mut e = env(10);
        start_replay_with(&mut e, &tracks_plan());
        e.rec.save_clip(Duration::from_secs(5), now(), MusicChoice::Keep).unwrap();
        let remix = e.fake.calls.borrow().exported[0].2.clone().unwrap();
        assert_eq!(remix.tracks, vec![TrackGain { index: 1, percent: 80 }, TrackGain { index: 2, percent: 120 }]);
    }

    #[test]
    fn manual_mixdown_replaces_the_recording_with_the_remix() {
        let mut e = env(10);
        e.rec.set_config(Config { game_volume: 80, ..e.rec.config().clone() });
        e.rec.start(Mode::Manual, SRC, &tracks_plan(), now()).unwrap();
        let file = e.rec.stop().unwrap().unwrap();
        std::fs::write(&file, b"original").unwrap();
        e.rec.mixdown_recording(&file, MusicChoice::Drop).unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"mixed");
        assert_eq!(remaining(file.parent().unwrap()), vec![file.file_name().unwrap().to_string_lossy().into_owned()]);
        let calls = e.fake.calls.borrow();
        assert_eq!(calls.remixed[0].0, file);
        assert_eq!(calls.remixed[0].2.tracks[0], TrackGain { index: 0, percent: 80 });
    }

    #[test]
    fn manual_mixdown_failure_keeps_the_original_and_cleans_up() {
        let mut e = env(10);
        e.rec.start(Mode::Manual, SRC, &tracks_plan(), now()).unwrap();
        let file = e.rec.stop().unwrap().unwrap();
        std::fs::write(&file, b"original").unwrap();
        e.fake.calls.borrow_mut().fail_remix = true;
        assert!(matches!(e.rec.mixdown_recording(&file, MusicChoice::Keep), Err(RecorderError::Export(_))));
        assert_eq!(std::fs::read(&file).unwrap(), b"original");
        assert_eq!(remaining(file.parent().unwrap()).len(), 1);
    }

    #[test]
    fn manual_mixdown_is_a_noop_when_audio_is_already_mixed() {
        let mut e = env(10);
        e.rec.start(Mode::Manual, SRC, &AudioPlan::default(), now()).unwrap();
        let file = e.rec.stop().unwrap().unwrap();
        std::fs::write(&file, b"original").unwrap();
        e.rec.mixdown_recording(&file, MusicChoice::Drop).unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"original");
        assert!(e.fake.calls.borrow().remixed.is_empty());
    }

    #[test]
    fn volume_moved_during_the_capture_applies_when_saving() {
        let mut e = env(10);
        start_replay_with(&mut e, &tracks_plan());
        e.rec.set_config(Config { game_volume: 30, mic_volume: 60, ..e.rec.config().clone() });
        e.rec.save_clip(Duration::from_secs(5), now(), MusicChoice::Drop).unwrap();
        let remix = e.fake.calls.borrow().exported[0].2.clone().unwrap();
        assert_eq!(remix.tracks, vec![TrackGain { index: 0, percent: 30 }, TrackGain { index: 2, percent: 60 }]);
    }
}
