//! The supported MP3 presets, shared by settings, estimates and the encoder.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mp3Quality {
    Smaller,
    Balanced,
    #[default]
    Best,
    V0, V1, V2, V3, V4, V5, V6, V7, V8, V9,
}

impl Mp3Quality {
    pub const CBR: [Self; 3] = [Self::Smaller, Self::Balanced, Self::Best];
    pub const VBR: [Self; 10] = [Self::V0, Self::V1, Self::V2, Self::V3, Self::V4, Self::V5, Self::V6, Self::V7, Self::V8, Self::V9];
    #[cfg(test)]
    pub const ALL: [Self; 13] = [Self::Smaller, Self::Balanced, Self::Best, Self::V0, Self::V1, Self::V2, Self::V3, Self::V4, Self::V5, Self::V6, Self::V7, Self::V8, Self::V9];
    pub fn vbr(self) -> Option<usize> { Self::VBR.iter().position(|&q| q == self) }
    /// Indicative music bitrate for estimates only; VBR is not a bitrate guarantee.
    pub fn kbps(self) -> u32 { match self { Self::Smaller => 192, Self::Balanced => 256, Self::Best => 320, _ => [245,225,190,175,165,130,115,100,85,65][self.vbr().unwrap()] } }
    pub fn label(self) -> String { self.vbr().map_or_else(|| self.kbps().to_string(), |v| format!("V{v}")) }
    pub fn encoder_args(self) -> [&'static str; 4] {
        let (flag, value) = match self {
            Self::Smaller => ("-b:a", "192k"), Self::Balanced => ("-b:a", "256k"), Self::Best => ("-b:a", "320k"),
            _ => ("-q:a", ["0","1","2","3","4","5","6","7","8","9"][self.vbr().unwrap()]),
        };
        ["-c:a", "libmp3lame", flag, value]
    }
    /// Reserve the maximum MP3 bitrate for VBR, whose actual size depends on the music.
    pub fn reserve_kbps(self) -> u32 { if self.vbr().is_some() { 320 } else { self.kbps() } }
    pub fn album_mb(self) -> f64 { self.kbps() as f64 * 125.0 * 45.0 * 60.0 / 1_000_000.0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn vbr_uses_quality_not_a_fixed_bitrate_and_reserves_enough_space() {
        for (v, quality) in Mp3Quality::VBR.into_iter().enumerate() {
            let args = quality.encoder_args();
            assert_eq!(&args[..3], &["-c:a", "libmp3lame", "-q:a"]);
            assert_eq!(args[3], v.to_string());
            assert_eq!(quality.reserve_kbps(), 320);
        }
        for quality in Mp3Quality::CBR { assert_eq!(quality.encoder_args()[2], "-b:a"); }
    }
    #[test]
    fn old_preferences_keep_their_output_and_new_presets_round_trip() {
        for keep in [true, false] {
            let old: crate::config::Config = serde_json::from_str(&format!(r#"{{"keep_lossless":{keep}}}"#)).unwrap();
            assert_eq!(old.keep_lossless, keep);
            assert_eq!(old.mp3_quality, Mp3Quality::Best);
            for quality in Mp3Quality::ALL {
                let cfg = crate::config::Config { mp3_quality: quality, ..old.clone() };
                let saved: crate::config::Config = serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
                assert_eq!(saved.mp3_quality, quality);
                assert_eq!(saved.keep_lossless, keep);
            }
        }
    }
}
