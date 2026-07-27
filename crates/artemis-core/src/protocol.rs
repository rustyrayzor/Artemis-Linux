use openssl::rand::rand_bytes;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use crate::http::XmlDocument;
use crate::{Application, Error, NvClient, Result, ServerInfo};

const AUDIO_CONFIGURATION_STEREO: i32 = 0x0003_02CA;
const SURROUND_AUDIO_INFO_STEREO: i32 = 0x0003_0002;

/// Stream profile passed to the GameStream launch and native transport layers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StreamProfile {
    width: i32,
    height: i32,
    fps: i32,
    bitrate_kbps: i32,
    packet_size: i32,
}

/// Supported H.264 SDR stream presets.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum StreamPreset {
    #[default]
    FullHd60,
    QuadHd60,
    UltraHd60,
}

impl StreamPreset {
    pub const ALL: [Self; 3] = [Self::FullHd60, Self::QuadHd60, Self::UltraHd60];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::FullHd60 => "1080p60",
            Self::QuadHd60 => "1440p60",
            Self::UltraHd60 => "4K60",
        }
    }

    #[must_use]
    pub const fn profile(self) -> StreamProfile {
        match self {
            Self::FullHd60 => StreamProfile::new(1920, 1080, 60, 20_000),
            Self::QuadHd60 => StreamProfile::new(2560, 1440, 60, 40_000),
            Self::UltraHd60 => StreamProfile::new(3840, 2160, 60, 80_000),
        }
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
}

/// Retrieves the paired host's current application list.
///
/// # Errors
///
/// Returns an error for TLS, HTTP, XML, or malformed application data.
pub fn list_applications(client: &NvClient) -> Result<Vec<Application>> {
    let xml = client.request_https("applist", &[], false)?;
    let document = XmlDocument::parse(&xml)?;
    let ids = document.all("ID");
    let titles = document.all("AppTitle");
    let hdr_values = document.all("IsHdrSupported").collect::<Vec<_>>();
    let mut applications = ids
        .zip(titles)
        .enumerate()
        .filter_map(|(index, (id, title))| {
            id.parse::<i32>().ok().map(|id| Application {
                id,
                title: title.to_owned(),
                hdr_supported: hdr_values.get(index).is_some_and(|value| *value == "1"),
            })
        })
        .collect::<Vec<_>>();
    applications.sort_by_key(|application| application.title.to_lowercase());
    Ok(applications)
}

/// Launches or resumes one application and returns stream transport parameters.
///
/// # Errors
///
/// Returns an error if the host is unpaired, another app is running, the requested app is
/// missing, or the launch request fails.
pub fn launch_application(
    client: &mut NvClient,
    app_id: i32,
    profile: StreamProfile,
) -> Result<LaunchResult> {
    let server_info = client.server_info()?;
    if !server_info.pair_status {
        return Err(Error::NotPaired);
    }
    let applications = list_applications(client)?;
    if !applications
        .iter()
        .any(|application| application.id == app_id)
    {
        return Err(Error::ApplicationNotFound(app_id));
    }
    let verb = if server_info.current_game == 0 {
        "launch"
    } else if server_info.current_game == app_id {
        "resume"
    } else {
        return Err(Error::AnotherApplicationRunning(server_info.current_game));
    };

    let remote_input = RemoteInputKey::generate()?;
    let gamepad_mask = 1;
    let parameters = vec![
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
        ("localAudioPlayMode", "0".to_owned()),
        ("surroundAudioInfo", SURROUND_AUDIO_INFO_STEREO.to_string()),
        ("remoteControllersBitmap", gamepad_mask.to_string()),
        ("gcmap", gamepad_mask.to_string()),
        ("gcpersist", "0".to_owned()),
        ("corever", "1".to_owned()),
    ];
    let xml = client.request_https(verb, &parameters, true)?;
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
    })
}

/// Requests termination of the application running on the host.
///
/// # Errors
///
/// Returns an error when the authenticated request fails or the host refuses cancellation.
pub fn cancel_host_application(client: &NvClient) -> Result<()> {
    let xml = client.request_https("cancel", &[], true)?;
    let document = XmlDocument::parse(&xml)?;
    if document.required("cancel")? == "0" {
        Err(Error::InvalidResponse(
            "host refused to cancel the running application".to_owned(),
        ))
    } else {
        Ok(())
    }
}

#[must_use]
pub const fn stereo_audio_configuration() -> i32 {
    AUDIO_CONFIGURATION_STEREO
}

#[cfg(test)]
mod tests {
    use super::{RemoteInputKey, StreamPreset, StreamProfile};

    #[test]
    fn default_profile_remains_1080p60() {
        let profile = StreamProfile::default();
        assert_eq!(
            (profile.width(), profile.height(), profile.fps()),
            (1920, 1080, 60)
        );
    }

    #[test]
    fn presets_match_moonlight_bitrate_defaults() {
        let expected = [
            (StreamPreset::FullHd60, 1920, 1080, 20_000),
            (StreamPreset::QuadHd60, 2560, 1440, 40_000),
            (StreamPreset::UltraHd60, 3840, 2160, 80_000),
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
    fn input_iv_contains_big_endian_key_id() {
        let key = RemoteInputKey::generate().expect("random key");
        assert_eq!(&key.iv()[..4], &key.key_id().to_be_bytes());
        assert!(key.iv()[4..].iter().all(|byte| *byte == 0));
    }
}
