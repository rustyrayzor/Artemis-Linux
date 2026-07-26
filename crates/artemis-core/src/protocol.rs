use openssl::rand::rand_bytes;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use crate::http::XmlDocument;
use crate::{Application, Error, NvClient, Result, ServerInfo};

const AUDIO_CONFIGURATION_STEREO: i32 = 0x0003_02CA;
const SURROUND_AUDIO_INFO_STEREO: i32 = 0x0003_0002;

/// Fixed first-release stream profile.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StreamProfile {
    pub width: i32,
    pub height: i32,
    pub fps: i32,
    pub bitrate_kbps: i32,
    pub packet_size: i32,
}

impl Default for StreamProfile {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_kbps: 20_000,
            packet_size: 1392,
        }
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
            format!("{}x{}x{}", profile.width, profile.height, profile.fps),
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
    use super::{RemoteInputKey, StreamProfile};

    #[test]
    fn profile_matches_reference_slice() {
        let profile = StreamProfile::default();
        assert_eq!(
            (profile.width, profile.height, profile.fps),
            (1920, 1080, 60)
        );
    }

    #[test]
    fn input_iv_contains_big_endian_key_id() {
        let key = RemoteInputKey::generate().expect("random key");
        assert_eq!(&key.iv()[..4], &key.key_id().to_be_bytes());
        assert!(key.iv()[4..].iter().all(|byte| *byte == 0));
    }
}
