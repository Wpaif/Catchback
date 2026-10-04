//! Microfones: lista de dispositivos e verificação do nível (saturação).

use crate::audio::{AppListError, Runner};

/// Entrada de áudio (microfone) do sistema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MicDevice {
    /// `node.name` do PipeWire: o que se passa em `target-object`.
    pub name: String,
    pub description: String,
}

/// Nome legível: o `pactl` devolve `(null)` quando a descrição tem acentos, então
/// cai para os outros campos até achar um útil.
fn friendly_name(source: &serde_json::Value, name: &str) -> String {
    let props = &source["properties"];
    [
        source["description"].as_str(),
        props["device.description"].as_str(),
        props["node.description"].as_str(),
        props["node.nick"].as_str(),
        props["device.nick"].as_str(),
        props["alsa.card_name"].as_str(),
    ]
    .into_iter()
    .flatten()
    .map(str::trim)
    .find(|d| !d.is_empty() && *d != "(null)")
    .unwrap_or(name)
    .to_string()
}

/// Interpreta `pactl -f json list sources`, ignorando os "monitores" de saída.
pub fn parse_sources(json: &str) -> Vec<MicDevice> {
    let Ok(serde_json::Value::Array(sources)) = serde_json::from_str(json) else {
        return Vec::new();
    };
    sources
        .iter()
        .filter_map(|s| {
            let name = s["name"].as_str()?;
            let is_monitor = name.ends_with(".monitor")
                || !s["monitor_of_sink"].is_null()
                || s["properties"]["device.class"] == "monitor";
            (!is_monitor).then(|| MicDevice { name: name.to_string(), description: friendly_name(s, name) })
        })
        .collect()
}

/// Interpreta `pw-dump` (nós `Audio/Source`).
pub fn parse_pw_sources(json: &str) -> Vec<MicDevice> {
    let Ok(serde_json::Value::Array(objects)) = serde_json::from_str(json) else {
        return Vec::new();
    };
    objects
        .iter()
        .map(|o| &o["info"]["props"])
        .filter(|p| p["media.class"] == "Audio/Source")
        .filter_map(|p| {
            let name = p["node.name"].as_str()?;
            Some(MicDevice {
                name: name.to_string(),
                description: p["node.description"].as_str().unwrap_or(name).to_string(),
            })
        })
        .collect()
}

/// Lista os microfones com `pactl` e, se ele não responder, com `pw-dump`.
pub fn list_mics_with(run: Runner) -> Result<Vec<MicDevice>, AppListError> {
    if let Some(out) = run("pactl", &["-f", "json", "list", "sources"]) {
        return Ok(parse_sources(&out));
    }
    run("pw-dump", &[]).map(|out| parse_pw_sources(&out)).ok_or(AppListError)
}

/// Microfones do sistema (erro se nem `pactl` nem `pw-dump` estiverem disponíveis).
pub fn list_mics() -> Result<Vec<MicDevice>, AppListError> {
    crate::audio::run_with_system_tools(list_mics_with)
}

/// Estatísticas de uma amostra de áudio (valores de -1 a 1).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MicLevels {
    pub peak: f32,
    pub rms: f32,
    /// Fração das amostras coladas no limite (≥ 0,98 em módulo).
    pub clipped: f32,
}

pub fn levels(samples: &[f32]) -> MicLevels {
    if samples.is_empty() {
        return MicLevels { peak: 0.0, rms: 0.0, clipped: 0.0 };
    }
    let n = samples.len() as f32;
    let peak = samples.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    let rms = (samples.iter().map(|v| v * v).sum::<f32>() / n).sqrt();
    let clipped = samples.iter().filter(|v| v.abs() >= 0.98).count() as f32 / n;
    MicLevels { peak, rms, clipped }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicVerdict {
    /// Sem sinal: microfone mudo ou desligado.
    Silent,
    Ok,
    /// Cortando no limite: o ganho de entrada está alto demais (estalos e ruído).
    Saturated,
}

/// Mais de 1 % das amostras coladas no limite já é distorção audível; um ou outro
/// pico isolado é normal.
const SATURATED_FRACTION: f32 = 0.01;
/// Abaixo de cerca de -66 dBFS não há sinal útil.
const SILENT_RMS: f32 = 0.0005;

pub fn verdict(levels: &MicLevels) -> MicVerdict {
    if levels.clipped > SATURATED_FRACTION {
        MicVerdict::Saturated
    } else if levels.rms < SILENT_RMS {
        MicVerdict::Silent
    } else {
        MicVerdict::Ok
    }
}

/// Mensagem para o usuário sobre o resultado do teste.
pub fn verdict_message(verdict: MicVerdict) -> &'static str {
    match verdict {
        MicVerdict::Silent => {
            "Sem sinal: o microfone está mudo ou desligado. Se for um fone Bluetooth, troque o perfil dele \
             para chamada (o perfil de música não tem microfone)."
        }
        MicVerdict::Ok => "O microfone está com um nível bom.",
        MicVerdict::Saturated => {
            "O microfone está saturado (cortando no limite). Baixe o volume de entrada do sistema \
             ou escolha outro microfone."
        }
    }
}

/// Amostras (mono, 16 bits) do trecho `data` de um WAV.
pub fn parse_wav_s16(bytes: &[u8]) -> Vec<f32> {
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Vec::new();
    }
    // Percorre os chunks (`id` de 4 bytes + tamanho de 4 bytes) até achar `data`.
    let mut at = 12;
    while at + 8 <= bytes.len() {
        let id = &bytes[at..at + 4];
        let size = u32::from_le_bytes([bytes[at + 4], bytes[at + 5], bytes[at + 6], bytes[at + 7]]) as usize;
        let body = at + 8;
        if id == b"data" {
            // o `wavenc` em streaming pode deixar o tamanho como 0 ou maior que o arquivo
            let end = if size == 0 { bytes.len() } else { (body + size).min(bytes.len()) };
            return bytes[body..end]
                .as_chunks::<2>().0.iter()
                .map(|b| f32::from(i16::from_le_bytes([b[0], b[1]])) / 32768.0)
                .collect();
        }
        at = body + size + (size & 1); // chunks têm alinhamento de 2 bytes
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PACTL: &str = r#"[
      {"index": 55, "name": "alsa_input.pci-0000_00_1f.3.analog-stereo", "description": "Áudio interno Estéreo analógico",
       "properties": {"device.class": "sound"}},
      {"index": 54, "name": "alsa_output.pci-0000_00_1f.3.analog-stereo.monitor", "description": "Monitor de Áudio interno",
       "monitor_of_sink": 54, "properties": {"device.class": "monitor"}},
      {"index": 141, "name": "bluez_input.84:AC:60:D7:C8:B2", "description": "Fones Bluetooth", "properties": {}},
      {"index": 9, "name": "so.monitor", "description": "Outro monitor", "properties": {}},
      {"index": 10, "description": "sem nome"}
    ]"#;

    fn dev(name: &str, description: &str) -> MicDevice {
        MicDevice { name: name.into(), description: description.into() }
    }

    #[test]
    fn pactl_sources_skip_monitors_and_nameless_entries() {
        assert_eq!(
            parse_sources(PACTL),
            vec![
                dev("alsa_input.pci-0000_00_1f.3.analog-stereo", "Áudio interno Estéreo analógico"),
                dev("bluez_input.84:AC:60:D7:C8:B2", "Fones Bluetooth"),
            ]
        );
    }

    #[test]
    fn a_null_description_falls_back_to_the_next_useful_name() {
        let json = r#"[
          {"name": "alsa_input.a", "description": "(null)",
           "properties": {"device.description": "(null)", "node.nick": "ALC255 Analog", "alsa.card_name": "HDA Intel PCH"}},
          {"name": "alsa_input.b", "description": "", "properties": {"alsa.card_name": "Placa B"}},
          {"name": "alsa_input.c", "description": "(null)", "properties": {}}
        ]"#;
        let names: Vec<_> = parse_sources(json).into_iter().map(|d| d.description).collect();
        assert_eq!(names, ["ALC255 Analog", "Placa B", "alsa_input.c"]);
    }

    #[test]
    fn garbage_gives_an_empty_list() {
        assert!(parse_sources("").is_empty());
        assert!(parse_sources("{}").is_empty());
        assert!(parse_pw_sources("não é json").is_empty());
    }

    #[test]
    fn pw_dump_keeps_audio_sources_only() {
        let json = r#"[
          {"type": "PipeWire:Interface:Node", "info": {"props": {
             "media.class": "Audio/Source", "node.name": "alsa_input.x", "node.description": "Microfone X"}}},
          {"type": "PipeWire:Interface:Node", "info": {"props": {
             "media.class": "Audio/Sink", "node.name": "alsa_output.x", "node.description": "Saída"}}},
          {"type": "PipeWire:Interface:Node", "info": {"props": {
             "media.class": "Stream/Input/Audio", "node.name": "app"}}}
        ]"#;
        assert_eq!(parse_pw_sources(json), vec![dev("alsa_input.x", "Microfone X")]);
    }

    #[test]
    fn pw_dump_falls_back_to_the_node_name_as_description() {
        let json = r#"[{"info": {"props": {"media.class": "Audio/Source", "node.name": "so-nome"}}}]"#;
        assert_eq!(parse_pw_sources(json), vec![dev("so-nome", "so-nome")]);
    }

    #[test]
    fn listing_prefers_pactl_then_pw_dump_then_errors() {
        let only_pactl = |p: &str, _: &[&str]| (p == "pactl").then(|| PACTL.to_string());
        assert_eq!(list_mics_with(&only_pactl).unwrap().len(), 2);
        let pw = r#"[{"info": {"props": {"media.class": "Audio/Source", "node.name": "n", "node.description": "d"}}}]"#;
        let only_pw = |p: &str, _: &[&str]| (p == "pw-dump").then(|| pw.to_string());
        assert_eq!(list_mics_with(&only_pw).unwrap(), vec![dev("n", "d")]);
        assert_eq!(list_mics_with(&|_: &str, _: &[&str]| None), Err(AppListError));
    }

    fn sine(amplitude: f32, n: usize) -> Vec<f32> {
        (0..n).map(|i| amplitude * (i as f32 * 0.05).sin()).collect()
    }

    #[test]
    fn levels_of_a_normal_signal() {
        let l = levels(&sine(0.3, 48_000));
        assert!((l.peak - 0.3).abs() < 0.01, "{l:?}");
        assert!((l.rms - 0.3 / 2f32.sqrt()).abs() < 0.01, "{l:?}");
        assert_eq!(l.clipped, 0.0);
        assert_eq!(verdict(&l), MicVerdict::Ok);
    }

    #[test]
    fn a_square_wave_at_full_scale_is_saturated() {
        let loud: Vec<f32> = (0..48_000).map(|i| if i % 40 < 20 { 1.0 } else { -1.0 }).collect();
        let l = levels(&loud);
        assert!(l.clipped > 0.99, "{l:?}");
        assert_eq!(verdict(&l), MicVerdict::Saturated);
    }

    #[test]
    fn hard_clipping_a_hot_signal_is_saturated() {
        // seno com ganho 8x cortado em ±1, como um microfone com ganho exagerado
        let hot: Vec<f32> = sine(8.0, 48_000).into_iter().map(|v| v.clamp(-1.0, 1.0)).collect();
        assert_eq!(verdict(&levels(&hot)), MicVerdict::Saturated);
    }

    #[test]
    fn a_few_isolated_peaks_are_not_saturation() {
        let mut s = sine(0.3, 48_000);
        s[100] = 1.0;
        s[5_000] = -1.0;
        assert_eq!(verdict(&levels(&s)), MicVerdict::Ok);
    }

    #[test]
    fn digital_silence_is_silent() {
        assert_eq!(verdict(&levels(&vec![0.0; 48_000])), MicVerdict::Silent);
        assert_eq!(verdict(&levels(&[])), MicVerdict::Silent);
    }

    #[test]
    fn each_verdict_has_a_distinct_helpful_message() {
        let msgs = [MicVerdict::Silent, MicVerdict::Ok, MicVerdict::Saturated].map(verdict_message);
        assert!(msgs.iter().all(|m| !m.is_empty()));
        assert_eq!(msgs.iter().collect::<std::collections::HashSet<_>>().len(), 3);
        assert!(verdict_message(MicVerdict::Saturated).to_lowercase().contains("volume"));
        // fones Bluetooth costumam ficar sem microfone no perfil de música
        assert!(verdict_message(MicVerdict::Silent).contains("Bluetooth"));
    }

    fn wav(samples: &[i16]) -> Vec<u8> {
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut out = b"RIFF".to_vec();
        out.extend((36 + data.len() as u32).to_le_bytes());
        out.extend(b"WAVEfmt ");
        out.extend(16u32.to_le_bytes());
        out.extend([1, 0, 1, 0]); // PCM, mono
        out.extend(48_000u32.to_le_bytes());
        out.extend(96_000u32.to_le_bytes());
        out.extend([2, 0, 16, 0]);
        out.extend(b"data");
        out.extend((data.len() as u32).to_le_bytes());
        out.extend(data);
        out
    }

    #[test]
    fn wav_data_chunk_is_decoded_to_floats() {
        let parsed = parse_wav_s16(&wav(&[0, 16384, -16384, 32767, -32768]));
        assert_eq!(parsed.len(), 5);
        assert!((parsed[1] - 0.5).abs() < 1e-4 && (parsed[2] + 0.5).abs() < 1e-4);
        assert!((parsed[4] + 1.0).abs() < 1e-4);
    }

    #[test]
    fn wav_with_an_extra_chunk_before_data_is_still_read() {
        let mut bytes = wav(&[1000, 2000]);
        // insere um chunk "LIST" de 4 bytes antes de "data"
        let at = bytes.windows(4).position(|w| w == b"data").unwrap();
        let mut extra = b"LIST".to_vec();
        extra.extend(4u32.to_le_bytes());
        extra.extend([0, 0, 0, 0]);
        bytes.splice(at..at, extra);
        assert_eq!(parse_wav_s16(&bytes).len(), 2);
    }

    #[test]
    fn truncated_or_invalid_wav_gives_what_it_can_or_nothing() {
        assert!(parse_wav_s16(b"").is_empty());
        assert!(parse_wav_s16(b"RIFFxxxxWAVE").is_empty());
        let mut bytes = wav(&[1, 2, 3, 4]);
        bytes.truncate(bytes.len() - 3); // sobram 5 bytes: 2 amostras inteiras e meia
        assert_eq!(parse_wav_s16(&bytes).len(), 2);
    }

}
