//! Descrição dos pipelines GStreamer (pura e testável) para cada modo.

use std::os::fd::RawFd;
use std::path::Path;
use std::time::Duration;

use crate::audio::{AudioPlan, GameAudio, TrackKind};
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

/// Fila antes do encoder: guarda só 2 quadros e descarta os mais antigos. Assim a
/// captura nunca espera por um encoder lento (o vídeo perde quadros, o áudio não).
const VIDEO_SHED_QUEUE: &str = "queue leaky=downstream max-size-buffers=2 max-size-bytes=0 max-size-time=0";

/// Fila folgada (10 s) depois dos encoders, sem descartar nada: absorve o atraso de
/// um encoder lento para que o muxer não trave o áudio.
const MUX_QUEUE: &str = "queue max-size-buffers=0 max-size-bytes=0 max-size-time=10000000000";

/// Trecho comum: fonte → conversão → encoder com um keyframe por segundo.
fn encode_chain(src: PipewireSource, config: &Config) -> String {
    format!(
        "pipewiresrc fd={} path={} do-timestamp=true keepalive-time={} ! {VIDEO_SHED_QUEUE} ! videoconvert ! videorate drop-only=true ! \
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

const AUDIO_CAPS: &str = "audio/x-raw,rate=48000,channels=2";

fn system_source() -> String {
    "pipewiresrc client-name=catchback-system do-timestamp=true \
     stream-properties=\"props,stream.capture.sink=true\""
        .to_string()
}

fn app_source(serial: u32) -> String {
    format!("pipewiresrc client-name=catchback-app-{serial} do-timestamp=true target-object={serial}")
}

/// Um fluxo de app de voz (a call com os amigos).
fn call_source(serial: u32) -> String {
    format!("pipewiresrc client-name=catchback-call-{serial} do-timestamp=true target-object={serial}")
}

/// Microfone: o escolhido (`target-object`) ou, sem escolha, o padrão do sistema.
fn mic_source(name: Option<&str>) -> String {
    match name {
        Some(name) => format!(
            "pipewiresrc client-name=catchback-mic do-timestamp=true target-object={}",
            quote(Path::new(name))
        ),
        None => "pipewiresrc client-name=catchback-mic do-timestamp=true".to_string(),
    }
}

/// Pipeline do teste de microfone: grava um WAV mono de 16 bits em `output`.
pub fn mic_probe_pipeline(name: Option<&str>, output: &Path) -> String {
    format!(
        "{} ! audioconvert ! audioresample ! audio/x-raw,format=S16LE,rate=48000,channels=1 ! \
         wavenc ! filesink location={}",
        mic_source(name).replace("catchback-mic", "catchback-mic-test"),
        quote(output)
    )
}

/// Fontes do áudio misturado numa faixa só, cada uma com seu ganho em %.
fn mixed_sources(audio: &AudioPlan) -> Vec<(String, Option<u32>)> {
    let mut sources = Vec::new();
    match &audio.game {
        Some(GameAudio::System) => sources.push((system_source(), Some(audio.game_volume))),
        Some(GameAudio::App(serials)) => {
            sources.extend(serials.iter().map(|&serial| (app_source(serial), Some(audio.game_volume))));
        }
        None => {}
    }
    if let Some(serials) = &audio.call {
        sources.extend(serials.iter().map(|&serial| (call_source(serial), Some(audio.call_volume))));
    }
    if audio.mic {
        sources.push((mic_source(audio.mic_source.as_deref()), Some(audio.mic_volume)));
    }
    sources
}

/// Fontes de cada faixa separada (o ganho fica para a exportação).
fn track_sources(audio: &AudioPlan, kind: TrackKind) -> Vec<(String, Option<u32>)> {
    let sources = match kind {
        TrackKind::Game => match &audio.game {
            Some(GameAudio::App(serials)) => serials.iter().map(|&s| app_source(s)).collect(),
            _ => Vec::new(),
        },
        TrackKind::System => vec![system_source()],
        TrackKind::Call => audio.call.iter().flatten().map(|&s| call_source(s)).collect(),
        TrackKind::Mic => vec![mic_source(audio.mic_source.as_deref())],
    };
    sources.into_iter().map(|source| (source, None)).collect()
}

fn audio_encoder(container: Container) -> &'static str {
    match container.codec() {
        Codec::H264 => "avenc_aac bitrate=192000 ! aacparse",
        Codec::Vp9 => "opusenc bitrate=128000 ! opusparse",
    }
}

/// Um mixer com as `sources` mais silêncio contínuo (para o muxer nunca ficar
/// esperando, ex.: jogo fechado no meio), codificado e ligado em `sink_pad`.
fn mixer_chain(name: &str, sources: &[(String, Option<u32>)], container: Container, sink_pad: &str) -> String {
    let mut chain = format!(
        " audiomixer name={name} ! audioconvert ! audioresample ! {AUDIO_CAPS} ! {} ! {MUX_QUEUE} ! {sink_pad} \
         audiotestsrc wave=silence is-live=true ! {AUDIO_CAPS} ! queue ! {name}.",
        audio_encoder(container)
    );
    for (source, percent) in sources {
        // `None`: sem estágio de volume (o ganho é aplicado na exportação).
        let gain = percent.map_or(String::new(), |p| format!(" volume volume={:.2} !", f64::from(p) / 100.0));
        chain.push_str(&format!(
            " {source} ! audioconvert ! audioresample ! {AUDIO_CAPS} !{gain} queue leaky=downstream max-size-buffers=0 max-size-bytes=0 max-size-time=2000000000 ! {name}."
        ));
    }
    chain
}

/// Ramos de áudio. `pad(i)` é onde entra a faixa `i` já codificada
/// (`splitmux.audio_i` no replay, `mux.` na gravação manual).
fn audio_branches(audio: &AudioPlan, config: &Config, pad: &dyn Fn(usize) -> String) -> String {
    if audio.is_silent() {
        return String::new();
    }
    if audio.separate_tracks {
        return audio
            .tracks()
            .into_iter()
            .enumerate()
            .map(|(i, kind)| mixer_chain(&format!("amix{i}"), &track_sources(audio, kind), config.container, &pad(i)))
            .collect();
    }
    mixer_chain("amix", &mixed_sources(audio), config.container, &pad(0))
}

/// Pipeline do modo replay: grava segmentos de ~5 s em `segment_dir`.
pub fn replay_pipeline(src: PipewireSource, audio: &AudioPlan, config: &Config, segment_dir: &Path) -> String {
    let pattern = segment_dir.join(segment_pattern(config.container));
    format!(
        "{} ! {MUX_QUEUE} ! splitmuxsink name=splitmux muxer-factory={} location={} max-size-time={}{}",
        encode_chain(src, config),
        config.container.muxer(),
        quote(&pattern),
        SEGMENT_DURATION.as_nanos(),
        audio_branches(audio, config, &|i| format!("splitmux.audio_{i}"))
    )
}

/// Pipeline do modo manual: grava um único arquivo em `output`.
pub fn manual_pipeline(src: PipewireSource, audio: &AudioPlan, config: &Config, output: &Path) -> String {
    let muxer = match config.container {
        Container::Mp4 => "mp4mux faststart=true",
        other => other.muxer(),
    };
    format!(
        "{} ! {MUX_QUEUE} ! {muxer} name=mux ! filesink location={}{}",
        encode_chain(src, config),
        quote(output),
        audio_branches(audio, config, &|_| "mux.".to_string())
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::Quality;
    use std::path::PathBuf;

    const SRC: PipewireSource = PipewireSource { fd: 7, node_id: 42 };

    #[test]
    fn replay_writes_five_second_segments_into_dir() {
        let p = replay_pipeline(SRC, &AudioPlan::default(), &Config::default(), Path::new("/tmp/seg"));
        assert!(p.contains("splitmuxsink"), "{p}");
        assert!(p.contains("location=\"/tmp/seg/seg_%05d.mp4\""), "{p}");
        assert!(p.contains("max-size-time=5000000000"), "{p}");
        assert!(!p.contains("filesink"), "{p}");
    }

    #[test]
    fn manual_writes_single_file() {
        let p = manual_pipeline(SRC, &AudioPlan::default(), &Config::default(), Path::new("/out/a.mp4"));
        assert!(p.contains("mp4mux"), "{p}");
        assert!(p.contains("filesink location=\"/out/a.mp4\""), "{p}");
        assert!(!p.contains("splitmuxsink"), "{p}");
    }

    #[test]
    fn resends_last_frame_so_a_static_screen_still_produces_video() {
        let c = Config { fps: 30, ..Config::default() };
        let p = replay_pipeline(SRC, &AudioPlan::default(), &c, Path::new("/tmp/s"));
        assert!(p.contains("keepalive-time=33"), "{p}");
        let p = manual_pipeline(SRC, &AudioPlan::default(), &Config::default(), Path::new("/o.mp4"));
        assert!(p.contains("keepalive-time=16"), "{p}");
    }

    #[test]
    fn uses_portal_fd_and_node() {
        let p = manual_pipeline(SRC, &AudioPlan::default(), &Config::default(), Path::new("/o.mp4"));
        assert!(p.starts_with("pipewiresrc fd=7 path=42"), "{p}");
    }

    #[test]
    fn applies_fps_bitrate_and_one_keyframe_per_second() {
        let c = Config { fps: 30, quality: Quality::Light, ..Config::default() };
        let p = replay_pipeline(SRC, &AudioPlan::default(), &c, Path::new("/tmp/s"));
        assert!(p.contains("framerate=30/1"), "{p}");
        assert!(p.contains("bitrate=8000"), "{p}");
        assert!(p.contains("speed-preset=veryfast"), "{p}");
        assert!(p.contains("key-int-max=30"), "{p}");
    }

    #[test]
    fn quotes_paths_with_spaces_and_quotes() {
        let out = PathBuf::from("/home/u/My \"Clips\"/a.mp4");
        let p = manual_pipeline(SRC, &AudioPlan::default(), &Config::default(), &out);
        assert!(p.contains("location=\"/home/u/My \\\"Clips\\\"/a.mp4\""), "{p}");
    }

    fn with(container: Container, quality: Quality) -> Config {
        Config { container, quality, ..Config::default() }
    }

    #[test]
    fn quality_changes_bitrate_and_preset() {
        let p = manual_pipeline(SRC, &AudioPlan::default(), &with(Container::Mp4, Quality::Max), Path::new("/o.mp4"));
        assert!(p.contains("bitrate=50000"), "{p}");
        assert!(p.contains("speed-preset=faster"), "{p}");
        let p = manual_pipeline(SRC, &AudioPlan::default(), &with(Container::Mp4, Quality::High), Path::new("/o.mp4"));
        assert!(p.contains("bitrate=20000"), "{p}");
    }

    #[test]
    fn mkv_replay_uses_matroska_segments() {
        let p = replay_pipeline(SRC, &AudioPlan::default(), &with(Container::Mkv, Quality::High), Path::new("/tmp/s"));
        assert!(p.contains("muxer-factory=matroskamux"), "{p}");
        assert!(p.contains("location=\"/tmp/s/seg_%05d.mkv\""), "{p}");
        assert!(p.contains("x264enc"), "{p}");
    }

    #[test]
    fn mkv_manual_uses_matroskamux() {
        let p = manual_pipeline(SRC, &AudioPlan::default(), &with(Container::Mkv, Quality::High), Path::new("/o.mkv"));
        assert!(p.contains("! matroskamux name=mux ! filesink location=\"/o.mkv\""), "{p}");
        assert!(!p.contains("mp4mux"), "{p}");
    }

    #[test]
    fn webm_uses_vp9_and_webmmux() {
        let c = with(Container::WebM, Quality::High);
        let p = manual_pipeline(SRC, &AudioPlan::default(), &c, Path::new("/o.webm"));
        assert!(p.contains("vp9enc"), "{p}");
        assert!(p.contains("target-bitrate=20000000"), "{p}"); // vp9enc usa bits/s
        assert!(p.contains("keyframe-max-dist=60"), "{p}");
        assert!(p.contains("webmmux"), "{p}");
        assert!(!p.contains("x264enc"), "{p}");
        let p = replay_pipeline(SRC, &AudioPlan::default(), &c, Path::new("/tmp/s"));
        assert!(p.contains("muxer-factory=webmmux"), "{p}");
        assert!(p.contains("seg_%05d.webm"), "{p}");
    }

    #[test]
    fn segment_pattern_follows_container() {
        assert_eq!(segment_pattern(Container::Mp4), "seg_%05d.mp4");
        assert_eq!(segment_pattern(Container::WebM), "seg_%05d.webm");
    }

    fn plan(game: Option<GameAudio>, mic: bool) -> AudioPlan {
        AudioPlan { game, mic, ..AudioPlan::default() }
    }

    fn replay_with(audio: &AudioPlan, config: &Config) -> String {
        replay_pipeline(SRC, audio, config, Path::new("/tmp/s"))
    }

    #[test]
    fn silent_plan_has_no_audio_chain() {
        let p = replay_with(&AudioPlan::default(), &Config::default());
        assert!(!p.contains("audiomixer"), "{p}");
        assert!(!p.contains("audio_0"), "{p}");
    }

    #[test]
    fn system_audio_taps_the_sink_monitor() {
        let p = replay_with(&plan(Some(GameAudio::System), false), &Config::default());
        assert!(p.contains("stream-properties=\"props,stream.capture.sink=true\""), "{p}");
        assert!(!p.contains("target-object"), "{p}");
    }

    #[test]
    fn app_audio_targets_each_of_its_streams() {
        let p = replay_with(&plan(Some(GameAudio::App(vec![300, 301])), false), &Config::default());
        assert!(p.contains("target-object=300"), "{p}");
        assert!(p.contains("target-object=301"), "{p}");
        assert!(!p.contains("capture.sink"), "{p}");
    }

    #[test]
    fn mic_uses_the_default_source() {
        let p = replay_with(&plan(None, true), &Config::default());
        assert!(p.contains("client-name=catchback-mic"), "{p}");
        assert!(!p.contains("target-object") && !p.contains("capture.sink"), "{p}");
    }

    #[test]
    fn game_and_mic_are_mixed_together() {
        let p = replay_with(&plan(Some(GameAudio::System), true), &Config::default());
        assert!(p.contains("catchback-system") && p.contains("catchback-mic"), "{p}");
        assert_eq!(p.matches("! amix.").count(), 3, "silêncio + jogo + mic: {p}"); // 3 entradas no mixer
    }

    #[test]
    fn mixer_always_has_a_silent_base_so_muxing_never_stalls() {
        let p = replay_with(&plan(None, true), &Config::default());
        assert!(p.contains("audiotestsrc wave=silence is-live=true"), "{p}");
    }

    #[test]
    fn audio_codec_follows_container() {
        let audio = plan(Some(GameAudio::System), false);
        let p = replay_with(&audio, &with(Container::Mp4, Quality::High));
        assert!(p.contains("avenc_aac") && p.contains("aacparse"), "{p}");
        let p = replay_with(&audio, &with(Container::Mkv, Quality::High));
        assert!(p.contains("avenc_aac"), "{p}");
        let p = replay_with(&audio, &with(Container::WebM, Quality::High));
        assert!(p.contains("opusenc") && p.contains("opusparse"), "{p}");
        assert!(!p.contains("avenc_aac"), "{p}");
    }

    #[test]
    fn replay_feeds_the_splitmux_audio_pad() {
        let p = replay_with(&plan(Some(GameAudio::System), false), &Config::default());
        assert!(p.contains("! splitmux.audio_0"), "{p}");
    }

    #[test]
    fn manual_feeds_the_named_muxer() {
        let audio = plan(None, true);
        for c in Container::ALL {
            let p = manual_pipeline(SRC, &audio, &with(c, Quality::High), Path::new("/o"));
            assert!(p.contains(&format!("{} ", c.muxer())) && p.contains("name=mux"), "{p}");
            assert!(p.contains("! mux."), "{p}");
        }
    }

    #[test]
    fn each_source_gets_a_volume_stage_at_unity_by_default() {
        let p = replay_with(&plan(Some(GameAudio::System), true), &Config::default());
        assert_eq!(p.matches("volume volume=1.00").count(), 2, "{p}");
    }

    #[test]
    fn game_and_mic_volumes_are_independent() {
        let audio = AudioPlan {
            game: Some(GameAudio::System),
            mic: true,
            game_volume: 50,
            mic_volume: 150,
            ..AudioPlan::default()
        };
        let p = replay_with(&audio, &Config::default());
        let game = p.split("catchback-system").nth(1).unwrap().split("amix.").next().unwrap();
        let mic = p.split("catchback-mic").nth(1).unwrap().split("amix.").next().unwrap();
        assert!(game.contains("volume volume=0.50"), "{game}");
        assert!(mic.contains("volume volume=1.50"), "{mic}");
    }

    #[test]
    fn muted_source_stays_in_the_mix_at_zero() {
        let audio = AudioPlan { mic: true, mic_volume: 0, ..AudioPlan::default() };
        let p = replay_with(&audio, &Config::default());
        assert!(p.contains("volume volume=0.00"), "{p}");
    }

    #[test]
    fn every_stream_of_an_app_shares_the_game_volume() {
        let audio = AudioPlan { game: Some(GameAudio::App(vec![1, 2])), game_volume: 80, ..AudioPlan::default() };
        let p = replay_with(&audio, &Config::default());
        assert_eq!(p.matches("volume volume=0.80").count(), 2, "{p}");
    }

    fn tracks_plan(mic: bool) -> AudioPlan {
        AudioPlan { game: Some(GameAudio::App(vec![7])), mic, separate_tracks: true, ..AudioPlan::default() }
    }

    #[test]
    fn separate_tracks_get_one_mixer_and_one_pad_each() {
        let p = replay_with(&tracks_plan(true), &Config::default());
        for i in 0..3 {
            assert!(p.contains(&format!("audiomixer name=amix{i}")), "{p}");
            assert!(p.contains(&format!("! splitmux.audio_{i}")), "{p}");
        }
        assert!(!p.contains("splitmux.audio_3"), "{p}");
        assert!(!p.contains("name=amix "), "não deve sobrar o mixer único: {p}");
    }

    #[test]
    fn tracks_are_game_then_system_then_mic() {
        let p = replay_with(&tracks_plan(true), &Config::default());
        let pos = |needle: &str| p.find(needle).unwrap_or_else(|| panic!("{needle} em {p}"));
        assert!(pos("target-object=7") < pos("catchback-system"), "{p}");
        assert!(pos("catchback-system") < pos("catchback-mic"), "{p}");
        // o app aparece só na faixa do jogo; o sistema é sua própria faixa
        assert_eq!(p.matches("target-object=7").count(), 1, "{p}");
        assert_eq!(p.matches("catchback-system").count(), 1, "{p}");
    }

    #[test]
    fn without_mic_there_are_two_tracks() {
        let p = replay_with(&tracks_plan(false), &Config::default());
        assert!(p.contains("splitmux.audio_1") && !p.contains("splitmux.audio_2"), "{p}");
        assert!(!p.contains("catchback-mic"), "{p}");
    }

    #[test]
    fn gain_is_left_to_the_export_when_tracks_are_separate() {
        let audio = AudioPlan { game_volume: 50, mic_volume: 150, ..tracks_plan(true) };
        let p = replay_with(&audio, &Config::default());
        assert!(!p.contains("volume volume="), "{p}");
    }

    #[test]
    fn manual_tracks_all_feed_the_named_muxer() {
        let p = manual_pipeline(SRC, &tracks_plan(true), &Config::default(), Path::new("/o.mp4"));
        assert_eq!(p.matches("! mux.").count(), 3, "{p}");
        assert!(p.contains("name=amix0") && p.contains("name=amix2"), "{p}");
    }

    #[test]
    fn every_track_has_its_own_silent_base() {
        let p = replay_with(&tracks_plan(true), &Config::default());
        assert_eq!(p.matches("wave=silence").count(), 3, "{p}");
    }

    /// Vídeo: só ele pode perder quadros; o áudio nunca.
    #[test]
    fn video_sheds_frames_before_the_encoder_instead_of_blocking_the_capture() {
        let p = replay_with(&AudioPlan::default(), &Config::default());
        let before_convert = p.split("! videoconvert").next().unwrap();
        assert!(before_convert.ends_with("queue leaky=downstream max-size-buffers=2 max-size-bytes=0 max-size-time=0 "), "{p}");
    }

    #[test]
    fn encoded_video_gets_a_roomy_non_leaky_queue_before_the_muxer() {
        let p = replay_with(&AudioPlan::default(), &Config::default());
        assert!(p.contains("h264parse ! queue max-size-buffers=0 max-size-bytes=0 max-size-time=10000000000 ! splitmuxsink"), "{p}");
        let p = manual_pipeline(SRC, &AudioPlan::default(), &Config::default(), Path::new("/o.mp4"));
        assert!(p.contains("h264parse ! queue max-size-buffers=0 max-size-bytes=0 max-size-time=10000000000 ! mp4mux"), "{p}");
    }

    #[test]
    fn encoded_audio_gets_a_roomy_non_leaky_queue_before_the_muxer() {
        let p = replay_with(&plan(Some(GameAudio::System), true), &Config::default());
        assert!(
            p.contains("aacparse ! queue max-size-buffers=0 max-size-bytes=0 max-size-time=10000000000 ! splitmux.audio_0"),
            "{p}"
        );
    }

    #[test]
    fn audio_sources_only_drop_after_a_generous_backlog() {
        let p = replay_with(&plan(Some(GameAudio::System), true), &Config::default());
        let sources = p.matches("queue leaky=downstream max-size-buffers=0 max-size-bytes=0 max-size-time=2000000000 ! amix.").count();
        assert_eq!(sources, 2, "{p}");
        // o vídeo é o único com fila de poucos quadros
        assert_eq!(p.matches("max-size-buffers=2").count(), 1, "{p}");
    }

    /// Duplicar quadros multiplica o trabalho do encoder justo quando ele já não
    /// dá conta, e o atraso resultante faz o áudio ser descartado.
    #[test]
    fn videorate_never_duplicates_frames() {
        for p in [
            replay_with(&AudioPlan::default(), &Config::default()),
            manual_pipeline(SRC, &AudioPlan::default(), &Config::default(), Path::new("/o.mp4")),
        ] {
            assert!(p.contains("videorate drop-only=true ! video/x-raw,framerate=60/1"), "{p}");
        }
    }

    fn mic_plan(source: Option<&str>) -> AudioPlan {
        AudioPlan { mic: true, mic_source: source.map(String::from), ..AudioPlan::default() }
    }

    #[test]
    fn chosen_microphone_is_targeted_with_quoting() {
        let p = replay_with(&mic_plan(Some("bluez_input.84:AC:60:D7:C8:B2")), &Config::default());
        assert!(p.contains("catchback-mic do-timestamp=true target-object=\"bluez_input.84:AC:60:D7:C8:B2\""), "{p}");
    }

    #[test]
    fn microphone_name_with_spaces_and_quotes_is_escaped() {
        let p = replay_with(&mic_plan(Some("Meu \"mic\" USB")), &Config::default());
        assert!(p.contains("target-object=\"Meu \\\"mic\\\" USB\""), "{p}");
    }

    #[test]
    fn default_microphone_has_no_target() {
        let p = replay_with(&mic_plan(None), &Config::default());
        assert!(!p.contains("target-object"), "{p}");
    }

    #[test]
    fn separate_mic_track_uses_the_chosen_microphone_too() {
        let plan = AudioPlan {
            game: Some(GameAudio::App(vec![7])),
            mic: true,
            mic_source: Some("alsa_input.x".into()),
            separate_tracks: true,
            ..AudioPlan::default()
        };
        let p = replay_with(&plan, &Config::default());
        assert!(p.contains("target-object=\"alsa_input.x\""), "{p}");
        assert!(p.contains("target-object=7"), "o app do jogo continua na faixa do jogo: {p}");
    }

    #[test]
    fn mic_probe_records_a_mono_wav_from_the_chosen_source() {
        let p = mic_probe_pipeline(Some("alsa_input.x"), Path::new("/tmp/t.wav"));
        assert!(p.starts_with("pipewiresrc client-name=catchback-mic-test"), "{p}");
        assert!(p.contains("target-object=\"alsa_input.x\""), "{p}");
        assert!(p.contains("channels=1") && p.contains("wavenc ! filesink location=\"/tmp/t.wav\""), "{p}");
        assert!(!mic_probe_pipeline(None, Path::new("/t.wav")).contains("target-object"));
    }

    #[test]
    fn mixed_audio_includes_the_call_streams_with_their_own_gain() {
        let audio = AudioPlan {
            game: Some(GameAudio::App(vec![7])),
            call: Some(vec![9, 10]),
            call_volume: 60,
            ..AudioPlan::default()
        };
        let p = replay_with(&audio, &Config::default());
        assert!(p.contains("target-object=7"), "{p}");
        for serial in [9, 10] {
            let part = p.split(&format!("catchback-call-{serial}")).nth(1).unwrap().split("amix.").next().unwrap();
            assert!(part.contains(&format!("target-object={serial}")), "{part}");
            assert!(part.contains("volume volume=0.60"), "o ganho da call é o dela: {part}");
        }
        assert_eq!(p.matches("! amix.").count(), 4, "silêncio + jogo + 2 fluxos da call: {p}");
    }

    #[test]
    fn the_call_alone_still_gets_a_mixer() {
        let audio = AudioPlan { call: Some(vec![9]), ..AudioPlan::default() };
        let p = replay_with(&audio, &Config::default());
        assert!(p.contains("audiomixer name=amix") && p.contains("target-object=9"), "{p}");
    }

    #[test]
    fn separate_tracks_put_the_call_on_its_own_track_between_system_and_mic() {
        let audio = AudioPlan {
            game: Some(GameAudio::App(vec![7])),
            mic: true,
            call: Some(vec![9]),
            separate_tracks: true,
            ..AudioPlan::default()
        };
        let p = replay_with(&audio, &Config::default());
        for i in 0..4 {
            assert!(p.contains(&format!("audiomixer name=amix{i}")) && p.contains(&format!("! splitmux.audio_{i}")), "{p}");
        }
        let pos = |needle: &str| p.find(needle).unwrap_or_else(|| panic!("{needle} em {p}"));
        assert!(pos("target-object=7") < pos("catchback-system"), "{p}");
        assert!(pos("catchback-system") < pos("catchback-call-9"), "{p}");
        assert!(pos("catchback-call-9") < pos("catchback-mic"), "{p}");
        assert_eq!(p.matches("catchback-call-9").count(), 1, "{p}");
        assert!(!p.contains("volume volume="), "o ganho fica para a exportação: {p}");
    }
}
