//! Formatos de saída: contêiner (e o codec que ele exige) e perfis de qualidade.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Container {
    #[default]
    Mp4,
    Mkv,
    WebM,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    Vp9,
}

impl Container {
    pub const ALL: [Container; 3] = [Container::Mp4, Container::Mkv, Container::WebM];

    pub fn extension(self) -> &'static str {
        match self {
            Container::Mp4 => "mp4",
            Container::Mkv => "mkv",
            Container::WebM => "webm",
        }
    }

    /// Elemento GStreamer que escreve o contêiner.
    pub fn muxer(self) -> &'static str {
        match self {
            Container::Mp4 => "mp4mux",
            Container::Mkv => "matroskamux",
            Container::WebM => "webmmux",
        }
    }

    /// WebM só aceita VP9/AV1; MP4 e MKV usam H.264.
    pub fn codec(self) -> Codec {
        match self {
            Container::WebM => Codec::Vp9,
            _ => Codec::H264,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Container::Mp4 => "MP4 (H.264)",
            Container::Mkv => "MKV (H.264)",
            Container::WebM => "WebM (VP9)",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Quality {
    Light,
    #[default]
    High,
    Max,
}

impl Quality {
    pub const ALL: [Quality; 3] = [Quality::Light, Quality::High, Quality::Max];

    pub fn bitrate_kbps(self) -> u32 {
        match self {
            Quality::Light => 8_000,
            Quality::High => 20_000,
            Quality::Max => 50_000,
        }
    }

    /// Preset do x264: o perfil máximo troca CPU por menos artefatos.
    pub fn x264_preset(self) -> &'static str {
        match self {
            Quality::Light | Quality::High => "veryfast",
            Quality::Max => "faster",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Quality::Light => "Leve (8 Mbps)",
            Quality::High => "Alta (20 Mbps)",
            Quality::Max => "Máxima (50 Mbps, mais CPU)",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_extensions_and_muxers() {
        assert_eq!(Container::Mp4.extension(), "mp4");
        assert_eq!(Container::Mkv.extension(), "mkv");
        assert_eq!(Container::WebM.extension(), "webm");
        assert_eq!(Container::Mp4.muxer(), "mp4mux");
        assert_eq!(Container::Mkv.muxer(), "matroskamux");
        assert_eq!(Container::WebM.muxer(), "webmmux");
    }

    #[test]
    fn webm_requires_vp9_others_use_h264() {
        assert_eq!(Container::WebM.codec(), Codec::Vp9);
        assert_eq!(Container::Mp4.codec(), Codec::H264);
        assert_eq!(Container::Mkv.codec(), Codec::H264);
    }

    #[test]
    fn defaults_favor_quality_over_the_old_8mbps() {
        assert_eq!(Container::default(), Container::Mp4);
        assert_eq!(Quality::default(), Quality::High);
        assert!(Quality::default().bitrate_kbps() > 8_000);
    }

    #[test]
    fn quality_bitrate_strictly_increases() {
        let rates: Vec<_> = Quality::ALL.iter().map(|q| q.bitrate_kbps()).collect();
        assert!(rates.windows(2).all(|w| w[0] < w[1]), "{rates:?}");
    }

    #[test]
    fn all_lists_every_variant_with_distinct_labels() {
        let labels: std::collections::HashSet<_> = Container::ALL.iter().map(|c| c.label()).collect();
        assert_eq!(labels.len(), Container::ALL.len());
        let labels: std::collections::HashSet<_> = Quality::ALL.iter().map(|q| q.label()).collect();
        assert_eq!(labels.len(), Quality::ALL.len());
    }
}
