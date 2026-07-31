use std::fs;
use std::path::{Path, PathBuf};

use artemis_core::{StreamAudioConfiguration, StreamBitrate, StreamFrameRate, StreamPreset};
use serde::{Deserialize, Serialize};

const SETTINGS_SCHEMA_VERSION: u32 = 2;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub enum VideoCodecPreference {
    #[default]
    Automatic,
    Av1,
    Hevc,
    H264,
}

impl VideoCodecPreference {
    pub const ALL: [Self; 4] = [Self::Automatic, Self::Av1, Self::Hevc, Self::H264];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Automatic => "Automatic",
            Self::Av1 => "Prefer AV1",
            Self::Hevc => "Prefer HEVC",
            Self::H264 => "H.264 compatibility",
        }
    }

    pub const fn environment_value(self) -> &'static str {
        match self {
            Self::Automatic => "auto",
            Self::Av1 => "av1",
            Self::Hevc => "hevc",
            Self::H264 => "h264",
        }
    }

    pub const fn bitrate_label(self) -> &'static str {
        match self {
            Self::Automatic | Self::H264 => "H.264",
            Self::Av1 => "AV1",
            Self::Hevc => "HEVC",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub enum VideoBitDepthPreference {
    #[default]
    EightBit,
    TenBit,
}

impl VideoBitDepthPreference {
    pub const fn label(self) -> &'static str {
        match self {
            Self::EightBit => "SDR (8-bit)",
            Self::TenBit => "HDR10 (Main10)",
        }
    }
}

// Source: the SDR rows in the user-selected Moonlight bitrate table:
// https://docs.google.com/spreadsheets/d/1XF01BCk_syQeiqugPUqTl-pNTDDA6dHlZCpMhGwcv0w
// Rows are 720p, 1080p, 1440p, and 4K. Columns are 30, 60, 90, and 120 FPS.
const H264_SDR_KBPS: [[i32; 4]; 4] = [
    [3_327, 6_655, 9_982, 13_310],
    [7_487, 14_974, 22_460, 29_947],
    [13_310, 26_620, 39_930, 53_240],
    [29_947, 59_895, 89_842, 119_789],
];
const HEVC_SDR_KBPS: [[i32; 4]; 4] = [
    [1_996, 3_993, 5_989, 7_986],
    [4_492, 8_984, 13_476, 17_968],
    [7_986, 15_972, 23_958, 31_944],
    [17_968, 35_937, 53_905, 71_873],
];
const AV1_SDR_KBPS: [[i32; 4]; 4] = [
    [1_331, 2_662, 3_993, 5_324],
    [2_995, 5_989, 8_984, 11_979],
    [5_324, 10_648, 15_972, 21_296],
    [11_979, 23_958, 35_937, 47_916],
];
const HEVC_HDR_KBPS: [[i32; 4]; 4] = [
    [2_502, 5_004, 7_506, 10_008],
    [5_629, 11_259, 16_888, 22_518],
    [10_008, 20_015, 30_023, 40_030],
    [22_517, 45_034, 67_551, 90_068],
];
const AV1_HDR_KBPS: [[i32; 4]; 4] = [
    [1_668, 3_336, 5_004, 6_672],
    [3_753, 7_506, 11_259, 15_012],
    [6_672, 13_343, 20_015, 26_687],
    [15_011, 30_023, 45_034, 60_046],
];

pub const AVAILABLE_FRAME_RATES: [StreamFrameRate; 2] =
    [StreamFrameRate::Fps30, StreamFrameRate::Fps60];

fn table_bitrate_kbps(
    preset: StreamPreset,
    frame_rate: StreamFrameRate,
    codec: VideoCodecPreference,
) -> i32 {
    let resolution_index = match preset {
        StreamPreset::Hd60 => 0,
        StreamPreset::FullHd60 => 1,
        StreamPreset::QuadHd60 => 2,
        StreamPreset::UltraHd60 => 3,
    };
    let frame_rate_index = match frame_rate {
        StreamFrameRate::Fps30 => 0,
        StreamFrameRate::Fps60 => 1,
        StreamFrameRate::Fps90 => 2,
        StreamFrameRate::Fps120 => 3,
    };
    let table = match codec {
        VideoCodecPreference::Automatic | VideoCodecPreference::H264 => H264_SDR_KBPS,
        VideoCodecPreference::Hevc => HEVC_SDR_KBPS,
        VideoCodecPreference::Av1 => AV1_SDR_KBPS,
    };
    table[resolution_index][frame_rate_index]
}

fn hdr_table_bitrate_kbps(
    preset: StreamPreset,
    frame_rate: StreamFrameRate,
    codec: VideoCodecPreference,
) -> Option<i32> {
    let resolution_index = match preset {
        StreamPreset::Hd60 => 0,
        StreamPreset::FullHd60 => 1,
        StreamPreset::QuadHd60 => 2,
        StreamPreset::UltraHd60 => 3,
    };
    let frame_rate_index = match frame_rate {
        StreamFrameRate::Fps30 => 0,
        StreamFrameRate::Fps60 => 1,
        StreamFrameRate::Fps90 => 2,
        StreamFrameRate::Fps120 => 3,
    };
    let table = match codec {
        VideoCodecPreference::Automatic | VideoCodecPreference::Av1 => AV1_HDR_KBPS,
        VideoCodecPreference::Hevc => HEVC_HDR_KBPS,
        VideoCodecPreference::H264 => return None,
    };
    Some(table[resolution_index][frame_rate_index])
}

#[must_use]
pub fn recommended_bitrate_mbps_for_range(
    preset: StreamPreset,
    frame_rate: StreamFrameRate,
    codec: VideoCodecPreference,
    dynamic_range: VideoBitDepthPreference,
) -> Option<i32> {
    let bitrate_kbps = match dynamic_range {
        VideoBitDepthPreference::EightBit => table_bitrate_kbps(preset, frame_rate, codec),
        VideoBitDepthPreference::TenBit => hdr_table_bitrate_kbps(preset, frame_rate, codec)?,
    };
    Some((bitrate_kbps + 999) / 1_000)
}

#[must_use]
pub fn high_quality_bitrate_mbps_for_range(
    preset: StreamPreset,
    frame_rate: StreamFrameRate,
    codec: VideoCodecPreference,
    dynamic_range: VideoBitDepthPreference,
) -> Option<i32> {
    let bitrate_kbps = match dynamic_range {
        VideoBitDepthPreference::EightBit => table_bitrate_kbps(preset, frame_rate, codec),
        VideoBitDepthPreference::TenBit => hdr_table_bitrate_kbps(preset, frame_rate, codec)?,
    };
    Some((bitrate_kbps * 5 + 3_999) / 4_000)
}

#[must_use]
pub fn recommended_bitrate_mbps(
    preset: StreamPreset,
    frame_rate: StreamFrameRate,
    codec: VideoCodecPreference,
) -> i32 {
    let bitrate_kbps = table_bitrate_kbps(preset, frame_rate, codec);
    (bitrate_kbps + 999) / 1_000
}

#[must_use]
pub fn high_quality_bitrate_mbps(
    preset: StreamPreset,
    frame_rate: StreamFrameRate,
    codec: VideoCodecPreference,
) -> i32 {
    let bitrate_kbps = table_bitrate_kbps(preset, frame_rate, codec);
    (bitrate_kbps * 5 + 3_999) / 4_000
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub enum BitrateMode {
    Balanced,
    HighQualityLan,
    #[default]
    Custom,
}

impl BitrateMode {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Balanced => "Balanced",
            Self::HighQualityLan => "High Quality LAN",
            Self::Custom => "Custom",
        }
    }

    pub fn bitrate_mbps_for_range(
        self,
        preset: StreamPreset,
        frame_rate: StreamFrameRate,
        codec: VideoCodecPreference,
        dynamic_range: VideoBitDepthPreference,
    ) -> Option<i32> {
        match self {
            Self::Balanced => {
                recommended_bitrate_mbps_for_range(preset, frame_rate, codec, dynamic_range)
            }
            Self::HighQualityLan => {
                high_quality_bitrate_mbps_for_range(preset, frame_rate, codec, dynamic_range)
            }
            Self::Custom => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub enum StreamDisplayMode {
    #[default]
    Fullscreen,
    Windowed,
}

impl StreamDisplayMode {
    pub const ALL: [Self; 2] = [Self::Fullscreen, Self::Windowed];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Fullscreen => "Fullscreen (Recommended)",
            Self::Windowed => "Windowed",
        }
    }

    pub const fn fullscreen(self) -> bool {
        matches!(self, Self::Fullscreen)
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub enum KeyboardCaptureMode {
    Never,
    #[default]
    Fullscreen,
}

impl KeyboardCaptureMode {
    pub const ALL: [Self; 2] = [Self::Fullscreen, Self::Never];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Never => "Never",
            Self::Fullscreen => "In fullscreen",
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default)]
#[allow(clippy::struct_excessive_bools)]
pub struct AppSettings {
    #[serde(default)]
    settings_schema_version: u32,
    pub resolution: StreamPreset,
    pub frame_rate: StreamFrameRate,
    pub bitrate_mbps: i32,
    #[serde(default)]
    pub bitrate_mode: BitrateMode,
    pub video_codec: VideoCodecPreference,
    pub video_bit_depth: VideoBitDepthPreference,
    pub audio_configuration: StreamAudioConfiguration,
    pub display_mode: StreamDisplayMode,
    pub vsync: bool,
    pub frame_pacing: bool,
    pub mute_host_audio: bool,
    pub mute_audio_when_inactive: bool,
    pub optimize_mouse_for_desktop: bool,
    pub keyboard_capture: KeyboardCaptureMode,
    pub swap_mouse_buttons: bool,
    pub reverse_scrolling: bool,
    pub swap_gamepad_buttons: bool,
    pub force_gamepad_one: bool,
    pub gamepad_mouse_control: bool,
    pub gamepad_background_input: bool,
    pub show_performance_diagnostics: bool,
}

impl Default for AppSettings {
    fn default() -> Self {
        let resolution = StreamPreset::default();
        let frame_rate = StreamFrameRate::default();
        let video_codec = VideoCodecPreference::default();
        Self {
            settings_schema_version: SETTINGS_SCHEMA_VERSION,
            resolution,
            frame_rate,
            bitrate_mbps: recommended_bitrate_mbps(resolution, frame_rate, video_codec),
            bitrate_mode: BitrateMode::Balanced,
            video_codec,
            video_bit_depth: VideoBitDepthPreference::default(),
            audio_configuration: StreamAudioConfiguration::default(),
            display_mode: StreamDisplayMode::default(),
            vsync: true,
            frame_pacing: true,
            mute_host_audio: true,
            mute_audio_when_inactive: false,
            optimize_mouse_for_desktop: false,
            keyboard_capture: KeyboardCaptureMode::default(),
            swap_mouse_buttons: false,
            reverse_scrolling: false,
            swap_gamepad_buttons: false,
            force_gamepad_one: false,
            gamepad_mouse_control: false,
            gamepad_background_input: false,
            show_performance_diagnostics: false,
        }
    }
}

impl AppSettings {
    pub fn load(config_dir: &Path) -> Result<Self, String> {
        let path = settings_path(config_dir);
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(error.to_string()),
        };
        let mut settings =
            serde_json::from_slice::<Self>(&bytes).map_err(|error| error.to_string())?;
        if settings.settings_schema_version < SETTINGS_SCHEMA_VERSION {
            // Frame pacing was previously a disabled placeholder, so a false value from an older
            // schema cannot represent an intentional user choice.
            settings.frame_pacing = true;
            settings.settings_schema_version = SETTINGS_SCHEMA_VERSION;
        }
        if !AVAILABLE_FRAME_RATES.contains(&settings.frame_rate) {
            settings.frame_rate = StreamFrameRate::Fps60;
            if let Some(bitrate_mbps) = settings.bitrate_mode.bitrate_mbps_for_range(
                settings.resolution,
                settings.frame_rate,
                settings.video_codec,
                settings.video_bit_depth,
            ) {
                settings.bitrate_mbps = bitrate_mbps;
            }
        }
        if StreamBitrate::from_mbps(settings.bitrate_mbps).is_none() {
            settings.bitrate_mode = BitrateMode::Balanced;
        }
        if settings.video_bit_depth == VideoBitDepthPreference::TenBit
            && settings.video_codec == VideoCodecPreference::H264
        {
            settings.video_bit_depth = VideoBitDepthPreference::EightBit;
        }
        if let Some(bitrate_mbps) = settings.bitrate_mode.bitrate_mbps_for_range(
            settings.resolution,
            settings.frame_rate,
            settings.video_codec,
            settings.video_bit_depth,
        ) {
            settings.bitrate_mbps = bitrate_mbps;
        }
        Ok(settings)
    }

    pub fn save(&self, config_dir: &Path) -> Result<(), String> {
        fs::create_dir_all(config_dir).map_err(|error| error.to_string())?;
        let contents = serde_json::to_vec_pretty(self).map_err(|error| error.to_string())?;
        fs::write(settings_path(config_dir), contents).map_err(|error| error.to_string())
    }

    pub fn bitrate(&self) -> StreamBitrate {
        StreamBitrate::from_mbps(self.bitrate_mbps).unwrap_or_else(|| {
            let bitrate_mbps = self
                .bitrate_mode
                .bitrate_mbps_for_range(
                    self.resolution,
                    self.frame_rate,
                    self.video_codec,
                    self.video_bit_depth,
                )
                .unwrap_or_else(|| {
                    recommended_bitrate_mbps_for_range(
                        self.resolution,
                        self.frame_rate,
                        self.video_codec,
                        self.video_bit_depth,
                    )
                    .unwrap_or_else(|| {
                        recommended_bitrate_mbps(self.resolution, self.frame_rate, self.video_codec)
                    })
                });
            StreamBitrate::from_mbps(bitrate_mbps)
                .unwrap_or_else(|| self.resolution.default_bitrate())
        })
    }
}

fn settings_path(config_dir: &Path) -> PathBuf {
    config_dir.join("settings.json")
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use artemis_core::{StreamAudioConfiguration, StreamFrameRate, StreamPreset};

    use super::{
        AppSettings, BitrateMode, StreamDisplayMode, VideoBitDepthPreference, VideoCodecPreference,
        high_quality_bitrate_mbps, recommended_bitrate_mbps, recommended_bitrate_mbps_for_range,
    };

    #[test]
    fn settings_round_trip_without_losing_stream_preferences() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("artemis-settings-{}-{nonce}", std::process::id()));
        let settings = AppSettings {
            bitrate_mbps: 150,
            bitrate_mode: BitrateMode::Custom,
            video_codec: VideoCodecPreference::Av1,
            video_bit_depth: VideoBitDepthPreference::TenBit,
            audio_configuration: StreamAudioConfiguration::Surround51,
            display_mode: StreamDisplayMode::Windowed,
            swap_mouse_buttons: true,
            show_performance_diagnostics: true,
            ..AppSettings::default()
        };

        settings.save(&directory).expect("save settings");
        let restored = AppSettings::load(&directory).expect("load settings");

        assert_eq!(restored, settings);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn sdr_recommendations_match_the_selected_bitrate_table() {
        let codecs = [
            VideoCodecPreference::H264,
            VideoCodecPreference::Hevc,
            VideoCodecPreference::Av1,
        ];
        let expected = [
            [[4, 2, 2], [7, 4, 3], [10, 6, 4], [14, 8, 6]],
            [[8, 5, 3], [15, 9, 6], [23, 14, 9], [30, 18, 12]],
            [[14, 8, 6], [27, 16, 11], [40, 24, 16], [54, 32, 22]],
            [[30, 18, 12], [60, 36, 24], [90, 54, 36], [120, 72, 48]],
        ];

        for (resolution_index, preset) in StreamPreset::ALL.into_iter().enumerate() {
            for (frame_rate_index, frame_rate) in StreamFrameRate::ALL.into_iter().enumerate() {
                for (codec_index, codec) in codecs.into_iter().enumerate() {
                    assert_eq!(
                        recommended_bitrate_mbps(preset, frame_rate, codec),
                        expected[resolution_index][frame_rate_index][codec_index]
                    );
                }
            }
        }
    }

    #[test]
    fn automatic_recommendation_is_safe_for_h264_fallback() {
        assert_eq!(
            recommended_bitrate_mbps(
                StreamPreset::UltraHd60,
                StreamFrameRate::Fps120,
                VideoCodecPreference::Automatic,
            ),
            120
        );
    }

    #[test]
    fn hdr_recommendations_use_main10_table_and_reject_h264() {
        assert_eq!(
            recommended_bitrate_mbps_for_range(
                StreamPreset::UltraHd60,
                StreamFrameRate::Fps60,
                VideoCodecPreference::Av1,
                VideoBitDepthPreference::TenBit,
            ),
            Some(31)
        );
        assert_eq!(
            recommended_bitrate_mbps_for_range(
                StreamPreset::UltraHd60,
                StreamFrameRate::Fps60,
                VideoCodecPreference::Hevc,
                VideoBitDepthPreference::TenBit,
            ),
            Some(46)
        );
        assert_eq!(
            recommended_bitrate_mbps_for_range(
                StreamPreset::UltraHd60,
                StreamFrameRate::Fps60,
                VideoCodecPreference::H264,
                VideoBitDepthPreference::TenBit,
            ),
            None
        );
    }

    #[test]
    fn high_quality_lan_adds_twenty_five_percent_before_rounding() {
        let codecs = [
            VideoCodecPreference::H264,
            VideoCodecPreference::Hevc,
            VideoCodecPreference::Av1,
        ];
        let frame_rates = [StreamFrameRate::Fps30, StreamFrameRate::Fps60];
        let expected = [
            [[5, 3, 2], [9, 5, 4]],
            [[10, 6, 4], [19, 12, 8]],
            [[17, 10, 7], [34, 20, 14]],
            [[38, 23, 15], [75, 45, 30]],
        ];

        for (resolution_index, preset) in StreamPreset::ALL.into_iter().enumerate() {
            for (frame_rate_index, frame_rate) in frame_rates.into_iter().enumerate() {
                for (codec_index, codec) in codecs.into_iter().enumerate() {
                    assert_eq!(
                        high_quality_bitrate_mbps(preset, frame_rate, codec),
                        expected[resolution_index][frame_rate_index][codec_index]
                    );
                }
            }
        }
    }

    #[test]
    fn legacy_bitrate_without_a_mode_is_preserved_as_custom() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("artemis-legacy-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&directory).expect("create settings directory");
        let mut json = serde_json::to_value(AppSettings::default()).expect("serialize settings");
        json.as_object_mut()
            .expect("settings object")
            .remove("bitrate_mode");
        json["bitrate_mbps"] = serde_json::json!(100);
        fs::write(
            directory.join("settings.json"),
            serde_json::to_vec_pretty(&json).expect("serialize legacy settings"),
        )
        .expect("write legacy settings");

        let restored = AppSettings::load(&directory).expect("load legacy settings");

        assert_eq!(restored.bitrate_mode, BitrateMode::Custom);
        assert_eq!(restored.bitrate_mbps, 100);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn legacy_settings_enable_the_new_frame_pacing_path_once() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "artemis-pacing-migration-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).expect("create settings directory");
        let mut json = serde_json::to_value(AppSettings::default()).expect("serialize settings");
        json.as_object_mut()
            .expect("settings object")
            .remove("settings_schema_version");
        json["frame_pacing"] = serde_json::json!(false);
        fs::write(
            directory.join("settings.json"),
            serde_json::to_vec(&json).expect("encode settings"),
        )
        .expect("write settings");

        let migrated = AppSettings::load(&directory).expect("load legacy settings");
        assert!(migrated.frame_pacing);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn current_settings_preserve_an_explicitly_disabled_pacing_choice() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "artemis-pacing-choice-{}-{nonce}",
            std::process::id()
        ));
        let settings = AppSettings {
            frame_pacing: false,
            ..AppSettings::default()
        };
        settings.save(&directory).expect("save settings");

        let restored = AppSettings::load(&directory).expect("load current settings");
        assert!(!restored.frame_pacing);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn saved_high_refresh_profile_is_migrated_to_sixty_fps() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "artemis-fps-migration-{}-{nonce}",
            std::process::id()
        ));
        let settings = AppSettings {
            resolution: StreamPreset::UltraHd60,
            frame_rate: StreamFrameRate::Fps120,
            bitrate_mbps: 48,
            bitrate_mode: BitrateMode::Balanced,
            video_codec: VideoCodecPreference::Av1,
            ..AppSettings::default()
        };
        settings.save(&directory).expect("save settings");

        let restored = AppSettings::load(&directory).expect("load settings");

        assert_eq!(restored.frame_rate, StreamFrameRate::Fps60);
        assert_eq!(restored.bitrate_mode, BitrateMode::Balanced);
        assert_eq!(restored.bitrate_mbps, 24);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn saved_balanced_hdr_profile_refreshes_to_hdr_table() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "artemis-hdr-profile-{}-{nonce}",
            std::process::id()
        ));
        let settings = AppSettings {
            resolution: StreamPreset::UltraHd60,
            frame_rate: StreamFrameRate::Fps60,
            bitrate_mbps: 24,
            bitrate_mode: BitrateMode::Balanced,
            video_codec: VideoCodecPreference::Av1,
            video_bit_depth: VideoBitDepthPreference::TenBit,
            ..AppSettings::default()
        };
        settings.save(&directory).expect("save settings");

        let restored = AppSettings::load(&directory).expect("load settings");

        assert_eq!(restored.bitrate_mbps, 31);
        let _ = fs::remove_dir_all(directory);
    }
}
