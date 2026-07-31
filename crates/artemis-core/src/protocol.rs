use openssl::rand::rand_bytes;
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use crate::http::XmlDocument;
use crate::{Application, Error, NvClient, Result, ServerInfo};

const AUDIO_CONFIGURATION_STEREO: i32 = 0x0003_02CA;
const AUDIO_CONFIGURATION_51_SURROUND: i32 = 0x003F_06CA;

/// Stream profile passed to the `GameStream` launch and native transport layers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StreamProfile {
    width: i32,
    height: i32,
    fps: i32,
    bitrate_kbps: i32,
    packet_size: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LaunchOptions {
    pub mute_host_audio: bool,
    pub audio_configuration: StreamAudioConfiguration,
}

impl Default for LaunchOptions {
    fn default() -> Self {
        Self {
            mute_host_audio: true,
            audio_configuration: StreamAudioConfiguration::default(),
        }
    }
}

/// Speaker layout requested from the `GameStream` host and native transport.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub enum StreamAudioConfiguration {
    #[default]
    Stereo,
    Surround51,
}

impl StreamAudioConfiguration {
    pub const ALL: [Self; 2] = [Self::Stereo, Self::Surround51];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Stereo => "Stereo",
            Self::Surround51 => "5.1 surround",
        }
    }

    #[must_use]
    pub const fn channels(self) -> i32 {
        match self {
            Self::Stereo => 2,
            Self::Surround51 => 6,
        }
    }

    #[must_use]
    pub const fn moonlight_value(self) -> i32 {
        match self {
            Self::Stereo => AUDIO_CONFIGURATION_STEREO,
            Self::Surround51 => AUDIO_CONFIGURATION_51_SURROUND,
        }
    }

    const fn surround_audio_info(self) -> i32 {
        let configuration = self.moonlight_value();
        let channels = (configuration >> 8) & 0xFF;
        let channel_mask = (configuration >> 16) & 0xFFFF;
        (channel_mask << 16) | channels
    }
}

/// Validated video bitrate for one stream profile.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StreamBitrate {
    kbps: i32,
}

/// Supported SDR output resolutions.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub enum StreamPreset {
    Hd60,
    #[default]
    FullHd60,
    QuadHd60,
    UltraHd60,
}

impl StreamPreset {
    pub const ALL: [Self; 4] = [Self::Hd60, Self::FullHd60, Self::QuadHd60, Self::UltraHd60];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Hd60 => "720p60",
            Self::FullHd60 => "1080p60",
            Self::QuadHd60 => "1440p60",
            Self::UltraHd60 => "4K60",
        }
    }

    #[must_use]
    pub const fn resolution_label(self) -> &'static str {
        match self {
            Self::Hd60 => "720p",
            Self::FullHd60 => "1080p",
            Self::QuadHd60 => "1440p",
            Self::UltraHd60 => "4K",
        }
    }

    #[must_use]
    pub const fn default_bitrate(self) -> StreamBitrate {
        match self {
            Self::Hd60 => StreamBitrate::new_unchecked(7_000),
            Self::FullHd60 => StreamBitrate::new_unchecked(15_000),
            Self::QuadHd60 => StreamBitrate::new_unchecked(27_000),
            Self::UltraHd60 => StreamBitrate::new_unchecked(60_000),
        }
    }

    #[must_use]
    pub const fn profile(self) -> StreamProfile {
        self.profile_with_bitrate(self.default_bitrate())
    }

    #[must_use]
    pub const fn profile_with_bitrate(self, bitrate: StreamBitrate) -> StreamProfile {
        self.profile_with_bitrate_and_fps(bitrate, StreamFrameRate::Fps60)
    }

    #[must_use]
    pub const fn profile_with_bitrate_and_fps(
        self,
        bitrate: StreamBitrate,
        frame_rate: StreamFrameRate,
    ) -> StreamProfile {
        match self {
            Self::Hd60 => StreamProfile::new(1280, 720, frame_rate.fps(), bitrate.kbps()),
            Self::FullHd60 => StreamProfile::new(1920, 1080, frame_rate.fps(), bitrate.kbps()),
            Self::QuadHd60 => StreamProfile::new(2560, 1440, frame_rate.fps(), bitrate.kbps()),
            Self::UltraHd60 => StreamProfile::new(3840, 2160, frame_rate.fps(), bitrate.kbps()),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub enum StreamFrameRate {
    Fps30,
    #[default]
    Fps60,
    Fps90,
    Fps120,
}

impl StreamFrameRate {
    pub const ALL: [Self; 4] = [Self::Fps30, Self::Fps60, Self::Fps90, Self::Fps120];

    #[must_use]
    pub const fn fps(self) -> i32 {
        match self {
            Self::Fps30 => 30,
            Self::Fps60 => 60,
            Self::Fps90 => 90,
            Self::Fps120 => 120,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Fps30 => "30 FPS",
            Self::Fps60 => "60 FPS",
            Self::Fps90 => "90 FPS",
            Self::Fps120 => "120 FPS",
        }
    }
}

impl StreamBitrate {
    pub const MIN_MBPS: i32 = 1;
    pub const MAX_MBPS: i32 = 300;

    const fn new_unchecked(kbps: i32) -> Self {
        Self { kbps }
    }

    #[must_use]
    pub fn from_mbps(mbps: i32) -> Option<Self> {
        if !(Self::MIN_MBPS..=Self::MAX_MBPS).contains(&mbps) {
            return None;
        }
        Some(Self::new_unchecked(mbps * 1000))
    }

    #[must_use]
    pub const fn mbps(self) -> i32 {
        self.kbps / 1000
    }

    #[must_use]
    pub const fn kbps(self) -> i32 {
        self.kbps
    }
}

impl StreamProfile {
    const PACKET_SIZE: i32 = 1392;

    const fn new(width: i32, height: i32, fps: i32, bitrate_kbps: i32) -> Self {
        Self {
            width,
            height,
            fps,
            bitrate_kbps,
            packet_size: Self::PACKET_SIZE,
        }
    }

    #[must_use]
    pub const fn width(self) -> i32 {
        self.width
    }

    #[must_use]
    pub const fn height(self) -> i32 {
        self.height
    }

    #[must_use]
    pub const fn fps(self) -> i32 {
        self.fps
    }

    #[must_use]
    pub const fn bitrate_kbps(self) -> i32 {
        self.bitrate_kbps
    }

    #[must_use]
    pub const fn packet_size(self) -> i32 {
        self.packet_size
    }
}

impl Default for StreamProfile {
    fn default() -> Self {
        StreamPreset::default().profile()
    }
}

/// Per-launch AES key material for the encrypted remote-input stream.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct RemoteInputKey {
    key: Zeroizing<[u8; 16]>,
    iv: [u8; 16],
    key_id: i32,
}

impl RemoteInputKey {
    fn generate() -> Result<Self> {
        let mut key = Zeroizing::new([0_u8; 16]);
        rand_bytes(key.as_mut())?;
        let mut identifier = [0_u8; 4];
        rand_bytes(&mut identifier)?;
        let key_id = i32::from_be_bytes(identifier);
        let mut iv = [0_u8; 16];
        iv[..4].copy_from_slice(&identifier);
        Ok(Self { key, iv, key_id })
    }

    #[must_use]
    pub fn key(&self) -> &[u8; 16] {
        &self.key
    }

    #[must_use]
    pub const fn iv(&self) -> &[u8; 16] {
        &self.iv
    }

    #[must_use]
    pub const fn key_id(&self) -> i32 {
        self.key_id
    }
}

/// Control-plane output needed to start `moonlight-common-c`.
pub struct LaunchResult {
    pub server_info: ServerInfo,
    pub rtsp_session_url: Option<String>,
    pub remote_input: RemoteInputKey,
    pub profile: StreamProfile,
    pub audio_configuration: StreamAudioConfiguration,
}

/// Retrieves the paired host's current application list.
///
/// # Errors
///
/// Returns an error for TLS, HTTP, XML, or malformed application data.
pub fn list_applications(client: &NvClient) -> Result<Vec<Application>> {
    let xml = client.request_https("applist", &[], false)?;
    let document = XmlDocument::parse(&xml)?;
    Ok(applications_from_document(&document))
}

/// Retrieves the box art advertised by the host for an application.
///
/// # Errors
///
/// Returns an error for TLS pin, HTTP, or transport failures.
pub fn application_asset(client: &NvClient, application: &Application) -> Result<Vec<u8>> {
    client.request_https_bytes(
        "appasset",
        &[
            ("appid", application.id.to_string()),
            ("AssetType", "2".to_owned()),
            ("AssetIdx", "0".to_owned()),
        ],
        false,
    )
}

fn applications_from_document(document: &XmlDocument) -> Vec<Application> {
    let ids = document.all("ID");
    let titles = document.all("AppTitle");
    let uuids = document.all("UUID").collect::<Vec<_>>();
    let hdr_values = document.all("IsHdrSupported").collect::<Vec<_>>();
    let mut applications = ids
        .zip(titles)
        .enumerate()
        .filter_map(|(index, (id, title))| {
            id.parse::<i32>().ok().map(|id| Application {
                id,
                uuid: uuids
                    .get(index)
                    .filter(|value| !value.is_empty())
                    .map(|value| (*value).to_owned()),
                title: title.to_owned(),
                hdr_supported: hdr_values.get(index).is_some_and(|value| *value == "1"),
            })
        })
        .collect::<Vec<_>>();
    applications.sort_by_key(|application| application.title.to_lowercase());
    applications
}

/// Launches or resumes one application and returns stream transport parameters.
///
/// # Errors
///
/// Returns an error if the host is unpaired, another app is running, the requested app is
/// missing, or the launch request fails.
pub fn launch_application(
    client: &mut NvClient,
    application: &Application,
    profile: StreamProfile,
    options: LaunchOptions,
) -> Result<LaunchResult> {
    let server_info = client.server_info()?;
    if !server_info.pair_status {
        return Err(Error::NotPaired);
    }
    let applications = list_applications(client)?;
    if !applications
        .iter()
        .any(|candidate| applications_match(candidate, application))
    {
        return Err(Error::ApplicationNotFound(application.id));
    }
    let app_id = application.id;
    let verb = if server_info.current_game == 0 {
        "launch"
    } else if server_info.current_game == app_id {
        "resume"
    } else {
        return Err(Error::AnotherApplicationRunning(server_info.current_game));
    };

    let remote_input = RemoteInputKey::generate()?;
    let gamepad_mask = 1;
    let mut parameters = vec![
        ("appid", app_id.to_string()),
        (
            "mode",
            format!("{}x{}x{}", profile.width(), profile.height(), profile.fps()),
        ),
        ("scaleFactor", "100".to_owned()),
        ("additionalStates", "1".to_owned()),
        ("sops", "1".to_owned()),
        ("rikey", hex::encode_upper(remote_input.key())),
        ("rikeyid", remote_input.key_id().to_string()),
        (
            "localAudioPlayMode",
            local_audio_play_mode(options).to_string(),
        ),
        (
            "surroundAudioInfo",
            options
                .audio_configuration
                .surround_audio_info()
                .to_string(),
        ),
        ("remoteControllersBitmap", gamepad_mask.to_string()),
        ("gcmap", gamepad_mask.to_string()),
        ("gcpersist", "0".to_owned()),
        ("corever", "1".to_owned()),
    ];
    if let Some(uuid) = application.uuid.as_ref() {
        parameters.push(("appuuid", uuid.clone()));
    }
    let xml = client.request_https(verb, &parameters, false)?;
    let document = XmlDocument::parse(&xml)?;
    let result_field = if verb == "launch" {
        "gamesession"
    } else {
        "resume"
    };
    if document.required(result_field)? == "0" {
        return Err(Error::InvalidResponse(format!(
            "host refused to {verb} application {app_id}"
        )));
    }
    let rtsp_session_url = document.optional("sessionUrl0").map(str::to_owned);
    Ok(LaunchResult {
        server_info,
        rtsp_session_url,
        remote_input,
        profile,
        audio_configuration: options.audio_configuration,
    })
}

fn applications_match(left: &Application, right: &Application) -> bool {
    left.id == right.id
        || left.uuid.as_deref().is_some_and(|left_uuid| {
            right
                .uuid
                .as_deref()
                .is_some_and(|right_uuid| left_uuid.eq_ignore_ascii_case(right_uuid))
        })
}

/// Requests termination of the application running on the host.
///
/// # Errors
///
/// Returns an error when the authenticated request fails or the host refuses cancellation.
pub fn cancel_host_application(client: &NvClient) -> Result<()> {
    let xml = client.request_https("cancel", &[], false)?;
    let document = XmlDocument::parse(&xml)?;
    if document.required("cancel")? == "0" {
        Err(Error::InvalidResponse(
            "host refused to cancel the running application".to_owned(),
        ))
    } else {
        Ok(())
    }
}

const fn local_audio_play_mode(options: LaunchOptions) -> i32 {
    if options.mute_host_audio { 0 } else { 1 }
}

#[cfg(test)]
mod tests {
    use super::{
        LaunchOptions, RemoteInputKey, StreamAudioConfiguration, StreamBitrate, StreamFrameRate,
        StreamPreset, StreamProfile, applications_from_document, local_audio_play_mode,
    };
    use crate::http::XmlDocument;

    #[test]
    fn default_profile_remains_1080p60() {
        let profile = StreamProfile::default();
        assert_eq!(
            (profile.width(), profile.height(), profile.fps()),
            (1920, 1080, 60)
        );
    }

    #[test]
    fn audio_configurations_match_moonlight_common_c() {
        assert_eq!(
            StreamAudioConfiguration::Stereo.moonlight_value(),
            0x0003_02CA
        );
        assert_eq!(
            StreamAudioConfiguration::Surround51.moonlight_value(),
            0x003F_06CA
        );
        assert_eq!(
            StreamAudioConfiguration::Stereo.surround_audio_info(),
            0x0003_0002
        );
        assert_eq!(
            StreamAudioConfiguration::Surround51.surround_audio_info(),
            0x003F_0006
        );
    }

    #[test]
    fn presets_match_the_sdr_h264_sixty_fps_defaults() {
        let expected = [
            (StreamPreset::Hd60, 1280, 720, 7_000),
            (StreamPreset::FullHd60, 1920, 1080, 15_000),
            (StreamPreset::QuadHd60, 2560, 1440, 27_000),
            (StreamPreset::UltraHd60, 3840, 2160, 60_000),
        ];

        for (preset, width, height, bitrate_kbps) in expected {
            let profile = preset.profile();
            assert_eq!(
                (
                    profile.width(),
                    profile.height(),
                    profile.fps(),
                    profile.bitrate_kbps(),
                    profile.packet_size(),
                ),
                (width, height, 60, bitrate_kbps, 1392)
            );
        }
    }

    #[test]
    fn custom_bitrate_is_validated_up_to_three_hundred_mbps() {
        let bitrate = StreamBitrate::from_mbps(300).expect("valid maximum bitrate");
        let profile = StreamPreset::UltraHd60.profile_with_bitrate(bitrate);

        assert_eq!(profile.bitrate_kbps(), 300_000);
        assert_eq!(bitrate.mbps(), 300);
        assert!(StreamBitrate::from_mbps(0).is_none());
        assert_eq!(
            StreamBitrate::from_mbps(1).expect("valid minimum").kbps(),
            1_000
        );
        assert!(StreamBitrate::from_mbps(301).is_none());
    }

    #[test]
    fn application_list_preserves_apollo_uuids() {
        let document = XmlDocument::parse(
            r#"<root status_code="200">
                <App>
                    <AppTitle>Steam Big Picture</AppTitle>
                    <UUID>steam-uuid</UUID>
                    <ID>2</ID>
                    <IsHdrSupported>0</IsHdrSupported>
                </App>
            </root>"#,
        )
        .expect("Apollo application list");

        let applications = applications_from_document(&document);
        assert_eq!(applications.len(), 1);
        assert_eq!(applications[0].uuid.as_deref(), Some("steam-uuid"));
    }

    #[test]
    fn presets_accept_a_one_hundred_twenty_fps_stream_profile() {
        let profile = StreamPreset::UltraHd60.profile_with_bitrate_and_fps(
            StreamBitrate::from_mbps(100).expect("valid bitrate"),
            StreamFrameRate::Fps120,
        );

        assert_eq!(
            (
                profile.width(),
                profile.height(),
                profile.fps(),
                profile.bitrate_kbps(),
            ),
            (3840, 2160, 120, 100_000)
        );
    }

    #[test]
    fn presets_accept_a_ninety_fps_stream_profile() {
        let profile = StreamPreset::Hd60.profile_with_bitrate_and_fps(
            StreamBitrate::from_mbps(10).expect("valid bitrate"),
            StreamFrameRate::Fps90,
        );

        assert_eq!(
            (profile.width(), profile.height(), profile.fps()),
            (1280, 720, 90)
        );
    }

    #[test]
    fn host_audio_mode_matches_the_mute_host_setting() {
        assert_eq!(
            local_audio_play_mode(LaunchOptions {
                mute_host_audio: true,
                ..LaunchOptions::default()
            }),
            0
        );
        assert_eq!(
            local_audio_play_mode(LaunchOptions {
                mute_host_audio: false,
                ..LaunchOptions::default()
            }),
            1
        );
    }

    #[test]
    fn input_iv_contains_big_endian_key_id() {
        let key = RemoteInputKey::generate().expect("random key");
        assert_eq!(&key.iv()[..4], &key.key_id().to_be_bytes());
        assert!(key.iv()[4..].iter().all(|byte| *byte == 0));
    }
}
