mod artwork;
mod browser;

use std::thread;
use std::time::{Duration, Instant};

use artemis_core::{
    Application, ClientIdentity, HostAddress, HostRecord, HostStore, LaunchOptions, LaunchResult,
    NvClient, PairingOutcome, ServerInfo, StreamBitrate, StreamFrameRate, StreamPreset,
    cancel_host_application, discover, generate_pin, launch_application, list_applications, pair,
};
use artemis_moonlight::{
    ConnectionQuality, EventReceiver, Session, StreamConfig, StreamEvent, VIDEO_FORMAT_AV1,
    VIDEO_FORMAT_AV1_MAIN10, VIDEO_FORMAT_H264, VIDEO_FORMAT_HEVC, VIDEO_FORMAT_HEVC_MAIN10,
    VideoCodec,
};
use crossbeam_channel::{Receiver, Sender, unbounded};
use eframe::egui::{self, Color32, RichText};

use self::artwork::{ArtworkKey, ArtworkStore, DecodedArtwork};
use self::browser::{BrowserDialog, BrowserPage, BrowserState};
use crate::controller::{ControllerManager, ControllerPreferences};
use crate::deep_link::ApolloLaunchRequest;
use crate::input::{InputPreferences, InputRouter};
use crate::media::{
    DecodedFrame, DecoderCapabilities, GlInteropContext, HdrDisplayCapabilities, MediaRuntime,
    StreamDiagnostics, decoder_capabilities, hdr_display_capabilities,
};
use crate::settings::{
    AppSettings, BitrateMode, StreamDisplayMode, VideoBitDepthPreference, VideoCodecPreference,
    recommended_bitrate_mbps, recommended_bitrate_mbps_for_range,
};
use crate::video_texture::StreamTexture;

const DEFAULT_HTTP_PORT: u16 = 47_989;
const AUTOSTART_HOST_ENV: &str = "ARTEMIS_AUTOSTART_HOST";
const AUTOSTART_APP_ENV: &str = "ARTEMIS_AUTOSTART_APP";
const AUTOSTART_PRESET_ENV: &str = "ARTEMIS_AUTOSTART_PRESET";
const AUTOSTART_FPS_ENV: &str = "ARTEMIS_AUTOSTART_FPS";
const AUTOSTART_BITRATE_ENV: &str = "ARTEMIS_AUTOSTART_BITRATE_MBPS";
const AUTOSTART_CODEC_ENV: &str = "ARTEMIS_AUTOSTART_CODEC";
const AUTOSTART_FULLSCREEN_ENV: &str = "ARTEMIS_AUTOSTART_FULLSCREEN";
const AUTOSTOP_AFTER_ENV: &str = "ARTEMIS_AUTOSTOP_AFTER_SECONDS";
const AUTOSTOP_CANCEL_HOST_ENV: &str = "ARTEMIS_AUTOSTOP_CANCEL_HOST";

pub struct ArtemisApp {
    identity: ClientIdentity,
    store: HostStore,
    browser: BrowserState,
    paired_hosts: Vec<HostRecord>,
    discovered_hosts: Vec<artemis_core::DiscoveredHost>,
    selected_address: Option<HostAddress>,
    selected_record: Option<HostRecord>,
    selected_info: Option<ServerInfo>,
    applications: Vec<Application>,
    artwork: ArtworkStore,
    manual_host: String,
    passphrase: String,
    pairing_pin: Option<String>,
    status: String,
    busy: bool,
    tasks: Sender<TaskMessage>,
    task_results: Receiver<TaskMessage>,
    active_stream: Option<ActiveStream>,
    gl_interop: Option<GlInteropContext>,
    texture: Option<StreamTexture>,
    settings: AppSettings,
    decoder_capabilities: DecoderCapabilities,
    hdr_display_capabilities: HdrDisplayCapabilities,
    fullscreen: bool,
    diagnostics_overlay: DiagnosticsOverlay,
    pending_autostart: Option<AutostartRequest>,
    pending_apollo_launch: Option<ApolloLaunchRequest>,
    fullscreen_on_connect: bool,
    autostop_after_connect: Option<Duration>,
    autostop_deadline: Option<Instant>,
    autostop_action: AutostopAction,
    cancel_completion: CancelCompletion,
}

struct ActiveStream {
    session: Session,
    events: EventReceiver,
    media: MediaRuntime,
    controller: ControllerManager,
    input: InputRouter,
    record: HostRecord,
    app_title: String,
    profile_label: String,
    connected: bool,
    connection_quality: ConnectionQuality,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum DiagnosticsOverlay {
    Hidden,
    Visible,
}

#[derive(Clone, Copy, Default, Eq, PartialEq)]
enum AutostopAction {
    #[default]
    Disconnect,
    CancelHost,
}

impl AutostopAction {
    const fn cancel_host(self) -> bool {
        matches!(self, Self::CancelHost)
    }
}

#[derive(Clone, Copy, Default, Eq, PartialEq)]
enum CancelCompletion {
    #[default]
    RemainOpen,
    CloseApplication,
}

impl VideoCodecPreference {
    fn supported_video_formats(
        self,
        bit_depth: VideoBitDepthPreference,
        capabilities: DecoderCapabilities,
    ) -> i32 {
        if bit_depth == VideoBitDepthPreference::TenBit {
            return match self {
                Self::Automatic | Self::Av1 => {
                    let mut formats = 0;
                    if capabilities.main10_ready(VideoCodec::Av1) {
                        formats |= VIDEO_FORMAT_AV1_MAIN10;
                    }
                    if capabilities.main10_ready(VideoCodec::Hevc) {
                        formats |= VIDEO_FORMAT_HEVC_MAIN10;
                    }
                    formats
                }
                Self::Hevc => {
                    if capabilities.main10_ready(VideoCodec::Hevc) {
                        VIDEO_FORMAT_HEVC_MAIN10
                    } else {
                        0
                    }
                }
                Self::H264 => 0,
            };
        }
        let mut formats = 0;
        let h264 = capabilities.support(VideoCodec::H264);
        let hevc = capabilities.support(VideoCodec::Hevc);
        let av1 = capabilities.support(VideoCodec::Av1);

        match self {
            Self::Automatic => {
                if av1.hardware {
                    formats |= VIDEO_FORMAT_AV1;
                }
                if hevc.hardware {
                    formats |= VIDEO_FORMAT_HEVC;
                }
                if h264.available {
                    formats |= VIDEO_FORMAT_H264;
                }
                if formats == 0 {
                    if av1.available {
                        formats |= VIDEO_FORMAT_AV1;
                    } else if hevc.available {
                        formats |= VIDEO_FORMAT_HEVC;
                    }
                }
            }
            Self::Av1 => {
                if av1.available {
                    formats |= VIDEO_FORMAT_AV1;
                }
                if hevc.available {
                    formats |= VIDEO_FORMAT_HEVC;
                }
                if h264.available {
                    formats |= VIDEO_FORMAT_H264;
                }
            }
            Self::Hevc => {
                if hevc.available {
                    formats |= VIDEO_FORMAT_HEVC;
                }
                if h264.available {
                    formats |= VIDEO_FORMAT_H264;
                }
            }
            Self::H264 => {
                if h264.available {
                    formats |= VIDEO_FORMAT_H264;
                }
            }
        }
        formats
    }

    fn bitrate_preference(self, capabilities: DecoderCapabilities) -> Self {
        if self != Self::Automatic {
            return self;
        }
        if capabilities.av1.hardware {
            Self::Av1
        } else if capabilities.hevc.hardware {
            Self::Hevc
        } else if capabilities.h264.available {
            Self::H264
        } else if capabilities.av1.available {
            Self::Av1
        } else if capabilities.hevc.available {
            Self::Hevc
        } else {
            Self::H264
        }
    }
}

impl DiagnosticsOverlay {
    fn is_visible(self) -> bool {
        self == Self::Visible
    }

    fn toggle(&mut self) {
        *self = if self.is_visible() {
            Self::Hidden
        } else {
            Self::Visible
        };
    }

    fn from_visible(visible: bool) -> Self {
        if visible { Self::Visible } else { Self::Hidden }
    }
}

#[derive(Debug)]
struct AutostartRequest {
    address: HostAddress,
    application_title: String,
    preset: StreamPreset,
    frame_rate: StreamFrameRate,
    bitrate_override: Option<StreamBitrate>,
    codec: VideoCodecPreference,
    fullscreen: bool,
    autostop_after: Option<Duration>,
    autostop_cancel_host: bool,
}

#[derive(Clone, Copy, Default)]
struct AutostartValues<'a> {
    host: Option<&'a str>,
    application: Option<&'a str>,
    preset: Option<&'a str>,
    frame_rate: Option<&'a str>,
    bitrate_mbps: Option<&'a str>,
    codec: Option<&'a str>,
    fullscreen: Option<&'a str>,
    autostop_after_seconds: Option<&'a str>,
    autostop_cancel_host: Option<&'a str>,
}

enum TaskMessage {
    Discovered(std::result::Result<Vec<artemis_core::DiscoveredHost>, String>),
    Inspected {
        address: HostAddress,
        record: Option<HostRecord>,
        result: std::result::Result<ServerInfo, String>,
    },
    Paired(std::result::Result<(HostRecord, ServerInfo), String>),
    Applications(std::result::Result<(HostRecord, Vec<Application>), String>),
    ArtworkLoaded {
        result: std::result::Result<DecodedArtwork, (ArtworkKey, String)>,
    },
    Launched {
        record: HostRecord,
        title: String,
        supported_video_formats: i32,
        codec_label: &'static str,
        bit_depth_label: &'static str,
        result: std::result::Result<LaunchResult, String>,
    },
    NativeConnected {
        record: HostRecord,
        title: String,
        profile_label: String,
        result: std::result::Result<(Session, EventReceiver), String>,
    },
    NetworkTested {
        title: String,
        result: std::result::Result<String, String>,
    },
    Cancelled(std::result::Result<(), String>),
}

fn load_app_settings(config_dir: &std::path::Path) -> (AppSettings, Option<String>) {
    match AppSettings::load(config_dir) {
        Ok(settings) => (settings, None),
        Err(error) => {
            tracing::warn!(%error, "using default application settings");
            (AppSettings::default(), Some(error))
        }
    }
}

fn apply_autostart_settings(
    settings: &mut AppSettings,
    pending_autostart: Option<&AutostartRequest>,
    capabilities: DecoderCapabilities,
) {
    let Some(request) = pending_autostart else {
        return;
    };
    settings.resolution = request.preset;
    settings.frame_rate = request.frame_rate;
    settings.video_codec = request.codec;
    settings.bitrate_mode = if request.bitrate_override.is_some() {
        BitrateMode::Custom
    } else {
        BitrateMode::Balanced
    };
    settings.bitrate_mbps = request.bitrate_override.map_or_else(
        || {
            recommended_bitrate_mbps_for_range(
                request.preset,
                request.frame_rate,
                request.codec.bitrate_preference(capabilities),
                settings.video_bit_depth,
            )
            .unwrap_or_else(|| {
                recommended_bitrate_mbps(
                    request.preset,
                    request.frame_rate,
                    request.codec.bitrate_preference(capabilities),
                )
            })
        },
        StreamBitrate::mbps,
    );
    settings.display_mode = if request.fullscreen {
        StreamDisplayMode::Fullscreen
    } else {
        StreamDisplayMode::Windowed
    };
}

fn initialize_gl_interop(context: &eframe::CreationContext<'_>) -> Option<GlInteropContext> {
    match GlInteropContext::new(context) {
        Ok(Some(context)) => {
            tracing::info!(
                target: "artemis::media",
                "EGL video interop is available"
            );
            Some(context)
        }
        Ok(None) => {
            tracing::info!(
                target: "artemis::media",
                "EGL video interop is unavailable; using CPU video upload"
            );
            None
        }
        Err(error) => {
            tracing::warn!(
                target: "artemis::media",
                %error,
                "could not initialize EGL video interop; using CPU video upload"
            );
            None
        }
    }
}

impl ArtemisApp {
    pub fn new(
        context: &eframe::CreationContext<'_>,
        identity: ClientIdentity,
        store: HostStore,
        start_in_settings: bool,
        apollo_launch: std::result::Result<Option<ApolloLaunchRequest>, String>,
    ) -> Self {
        configure_style(&context.egui_ctx);
        let paired_hosts = store.load().unwrap_or_default();
        let mut browser = BrowserState::load(identity.config_dir());
        if start_in_settings {
            browser.page = BrowserPage::Settings;
        }
        let (mut settings, settings_error) = load_app_settings(identity.config_dir());
        let (tasks, task_results) = unbounded();
        let (pending_apollo_launch, apollo_launch_error) = match apollo_launch {
            Ok(request) => (request, None),
            Err(error) => {
                tracing::warn!(%error, "ignoring invalid Apollo WebUI launch link");
                (None, Some(error))
            }
        };
        let (mut pending_autostart, mut autostart_error) = match autostart_from_environment() {
            Ok(request) => (request, None),
            Err(error) => {
                tracing::warn!(%error, "ignoring invalid diagnostic autostart configuration");
                (None, Some(error))
            }
        };
        if pending_apollo_launch.is_some() {
            pending_autostart = None;
            autostart_error = None;
        }
        let gl_interop = initialize_gl_interop(context);
        let mut decoder_capabilities = decoder_capabilities();
        decoder_capabilities.presentation_bit_depth = gl_interop
            .as_ref()
            .map_or(8, GlInteropContext::presentation_bit_depth);
        trace_decoder_capabilities(decoder_capabilities);
        let hdr_display_capabilities = hdr_display_capabilities();
        tracing::info!(
            target: "artemis::media",
            output = ?hdr_display_capabilities.output_name,
            display_hdr10 = hdr_display_capabilities.display_hdr10,
            native_hdr_presentation = hdr_display_capabilities.native_hdr_presentation,
            reason = %hdr_display_capabilities.presentation_reason,
            "HDR display capability probe complete"
        );
        apply_autostart_settings(
            &mut settings,
            pending_autostart.as_ref(),
            decoder_capabilities,
        );
        let diagnostics_overlay =
            DiagnosticsOverlay::from_visible(settings.show_performance_diagnostics);
        let mut app = Self {
            identity,
            store,
            browser,
            paired_hosts,
            discovered_hosts: Vec::new(),
            selected_address: None,
            selected_record: None,
            selected_info: None,
            applications: Vec::new(),
            artwork: ArtworkStore::default(),
            manual_host: String::new(),
            passphrase: String::new(),
            pairing_pin: None,
            status: apollo_launch_error
                .or(autostart_error)
                .or(settings_error)
                .unwrap_or_else(|| "Ready".to_owned()),
            busy: false,
            tasks,
            task_results,
            active_stream: None,
            gl_interop,
            texture: None,
            settings,
            decoder_capabilities,
            hdr_display_capabilities,
            fullscreen: false,
            diagnostics_overlay,
            pending_autostart,
            pending_apollo_launch,
            fullscreen_on_connect: false,
            autostop_after_connect: None,
            autostop_deadline: None,
            autostop_action: AutostopAction::Disconnect,
            cancel_completion: CancelCompletion::RemainOpen,
        };
        app.begin_initial_navigation();
        app
    }

    fn begin_initial_navigation(&mut self) {
        if let Some(request) = &self.pending_apollo_launch {
            let record = self
                .paired_hosts
                .iter()
                .find(|record| {
                    record
                        .server_unique_id
                        .eq_ignore_ascii_case(&request.host_uuid)
                })
                .cloned();
            if let Some(record) = record {
                tracing::info!(
                    host = %record.name,
                    host_uuid = %request.host_uuid,
                    application = %request.application_label(),
                    "Apollo WebUI launch is configured"
                );
                self.inspect(record.address);
            } else {
                let host = request.host_name.as_deref().unwrap_or(&request.host_uuid);
                self.status = format!(
                    "Apollo requested a launch on {host}, but that host is not paired with Artemis."
                );
                self.pending_apollo_launch = None;
                self.start_discovery();
            }
        } else if let Some(request) = &self.pending_autostart {
            tracing::info!(
                host = %request.address.host,
                application = %request.application_title,
                resolution = request.preset.resolution_label(),
                fps = request.frame_rate.fps(),
                bitrate_mbps = self.settings.bitrate_mbps,
                codec = request.codec.label(),
                fullscreen = request.fullscreen,
                "diagnostic autostart is configured"
            );
            self.inspect(request.address.clone());
        } else {
            self.start_discovery();
        }
    }

    fn start_discovery(&mut self) {
        self.busy = true;
        "Discovering Apollo and Sunshine hosts…".clone_into(&mut self.status);
        let sender = self.tasks.clone();
        thread::spawn(move || {
            let result = discover(Duration::from_secs(3)).map_err(|error| error.to_string());
            let _ = sender.send(TaskMessage::Discovered(result));
        });
    }

    fn inspect(&mut self, address: HostAddress) {
        self.busy = true;
        self.status = format!("Contacting {}…", address.host);
        self.selected_address = Some(address.clone());
        self.selected_info = None;
        self.applications.clear();
        self.pairing_pin = None;
        let record = self
            .paired_hosts
            .iter()
            .find(|record| record.address == address)
            .cloned();
        let identity = self.identity.clone();
        let sender = self.tasks.clone();
        thread::spawn(move || {
            let mut client = NvClient::new(
                address.clone(),
                identity,
                record.as_ref().map(|value| value.https_port),
                record.as_ref().map(|value| value.certificate_der.clone()),
            );
            let result = client.server_info().map_err(|error| error.to_string());
            let _ = sender.send(TaskMessage::Inspected {
                address,
                record,
                result,
            });
        });
    }

    fn start_pairing(&mut self) {
        let (Some(address), Some(info)) =
            (self.selected_address.clone(), self.selected_info.clone())
        else {
            return;
        };
        let pin = match generate_pin() {
            Ok(pin) => pin,
            Err(error) => {
                self.status = error.to_string();
                return;
            }
        };
        self.pairing_pin = Some(pin.clone());
        self.busy = true;
        self.status = format!("Enter PIN {pin} on {}.", info.name);
        let passphrase = (!self.passphrase.is_empty()).then(|| self.passphrase.clone());
        let identity = self.identity.clone();
        let store = self.store.clone();
        let sender = self.tasks.clone();
        thread::spawn(move || {
            let mut client = NvClient::new(address.clone(), identity, Some(info.https_port), None);
            let result = (|| {
                let certificate_der = match pair(&mut client, &info, &pin, passphrase.as_deref())? {
                    PairingOutcome::Paired { certificate_der } => certificate_der,
                    PairingOutcome::IncorrectPin => {
                        return Err(artemis_core::Error::Pairing(
                            "the host rejected the PIN or passphrase".to_owned(),
                        ));
                    }
                    PairingOutcome::AlreadyInProgress => {
                        return Err(artemis_core::Error::Pairing(
                            "another client is already pairing with this host".to_owned(),
                        ));
                    }
                };
                let record = HostRecord {
                    address,
                    name: info.name.clone(),
                    server_unique_id: info.unique_id.clone(),
                    https_port: info.https_port,
                    certificate_der,
                };
                store.upsert(record.clone())?;
                Ok((record, info))
            })()
            .map_err(|error| error.to_string());
            let _ = sender.send(TaskMessage::Paired(result));
        });
    }

    fn refresh_applications(&mut self) {
        let Some(record) = self.selected_record.clone() else {
            return;
        };
        self.busy = true;
        self.status = format!("Loading applications from {}…", record.name);
        let identity = self.identity.clone();
        let sender = self.tasks.clone();
        thread::spawn(move || {
            let client = NvClient::new(
                record.address.clone(),
                identity,
                Some(record.https_port),
                Some(record.certificate_der.clone()),
            );
            let result = list_applications(&client)
                .map(|applications| (record, applications))
                .map_err(|error| error.to_string());
            let _ = sender.send(TaskMessage::Applications(result));
        });
    }

    fn refresh_application_artwork(&mut self, record: &HostRecord, applications: &[Application]) {
        let pending = self
            .artwork
            .begin_host_load(&record.server_unique_id, applications);
        if pending.is_empty() {
            return;
        }
        let record = record.clone();
        let identity = self.identity.clone();
        let config_dir = self.identity.config_dir().to_owned();
        let sender = self.tasks.clone();
        thread::spawn(move || {
            let client = NvClient::new(
                record.address,
                identity,
                Some(record.https_port),
                Some(record.certificate_der),
            );
            for application in pending {
                let key = ArtworkKey::new(&record.server_unique_id, application.id);
                let result =
                    artwork::load(&config_dir, &record.server_unique_id, &client, &application)
                        .map_err(|error| (key, error));
                if sender.send(TaskMessage::ArtworkLoaded { result }).is_err() {
                    break;
                }
            }
        });
    }

    fn launch(&mut self, application: Application) {
        let Some(record) = self.selected_record.clone() else {
            return;
        };
        let supported_video_formats = self
            .settings
            .video_codec
            .supported_video_formats(self.settings.video_bit_depth, self.decoder_capabilities);
        if supported_video_formats == 0 {
            self.status = if self.settings.video_bit_depth == VideoBitDepthPreference::TenBit {
                format!(
                    "10-bit Main10 is unavailable for {} on this decoder/display path.",
                    self.settings.video_codec.label()
                )
            } else {
                "No compatible H.264, HEVC, or AV1 GStreamer decoder is installed.".to_owned()
            };
            return;
        }
        self.busy = true;
        let preset = self.settings.resolution;
        let frame_rate = self.settings.frame_rate;
        let bitrate = self.settings.bitrate();
        let codec_label = self.settings.video_codec.label();
        let bit_depth_label = self.settings.video_bit_depth.label();
        let launch_options = LaunchOptions {
            mute_host_audio: self.settings.mute_host_audio,
            audio_configuration: self.settings.audio_configuration,
        };
        self.fullscreen_on_connect = self.settings.display_mode.fullscreen();
        self.status = format!(
            "Launching {} at {} {}, {} Mbps, using {}…",
            application.title,
            preset.resolution_label(),
            frame_rate.label(),
            bitrate.mbps(),
            codec_label
        );
        let identity = self.identity.clone();
        let sender = self.tasks.clone();
        thread::spawn(move || {
            let mut client = NvClient::new(
                record.address.clone(),
                identity,
                Some(record.https_port),
                Some(record.certificate_der.clone()),
            );
            let title = application.title.clone();
            let result = launch_application(
                &mut client,
                &application,
                preset.profile_with_bitrate_and_fps(bitrate, frame_rate),
                launch_options,
            )
            .map_err(|error| error.to_string());
            let _ = sender.send(TaskMessage::Launched {
                record,
                title,
                supported_video_formats,
                codec_label,
                bit_depth_label,
                result,
            });
        });
    }

    fn begin_native_connection(
        &mut self,
        record: HostRecord,
        title: String,
        launch: LaunchResult,
        supported_video_formats: i32,
        codec_label: &str,
        bit_depth_label: &str,
    ) {
        let profile_label = format!(
            "{}x{} at {} FPS · {} Mbps · {} · {} · {}",
            launch.profile.width(),
            launch.profile.height(),
            launch.profile.fps(),
            launch.profile.bitrate_kbps() / 1000,
            codec_label,
            bit_depth_label,
            launch.audio_configuration.label(),
        );
        self.status = format!("Connecting {title} at {profile_label}…");
        let config = StreamConfig {
            address: record.address.host.clone(),
            app_version: launch.server_info.app_version,
            gfe_version: launch.server_info.gfe_version,
            rtsp_session_url: launch.rtsp_session_url,
            server_codec_mode_support: launch.server_info.codec_mode_support,
            supported_video_formats,
            width: launch.profile.width(),
            height: launch.profile.height(),
            fps: launch.profile.fps(),
            bitrate_kbps: launch.profile.bitrate_kbps(),
            packet_size: launch.profile.packet_size(),
            audio_configuration: launch.audio_configuration.moonlight_value(),
            client_refresh_rate_x100: launch.profile.fps() * 100,
            hdr_enabled: self.settings.video_bit_depth == VideoBitDepthPreference::TenBit,
            remote_input_key: *launch.remote_input.key(),
            remote_input_iv: *launch.remote_input.iv(),
        };
        let sender = self.tasks.clone();
        thread::spawn(move || {
            let result = Session::connect(config).map_err(|error| error.to_string());
            let _ = sender.send(TaskMessage::NativeConnected {
                record,
                title,
                profile_label,
                result,
            });
        });
    }

    #[allow(clippy::too_many_lines)]
    fn drain_tasks(&mut self, context: &egui::Context) {
        while let Ok(message) = self.task_results.try_recv() {
            match message {
                TaskMessage::Discovered(result) => {
                    self.busy = false;
                    match result {
                        Ok(hosts) => {
                            self.status = if hosts.is_empty() {
                                "No local hosts found. Manual entry is available.".to_owned()
                            } else {
                                format!("Found {} local host(s).", hosts.len())
                            };
                            self.discovered_hosts = hosts;
                        }
                        Err(error) => self.status = error,
                    }
                }
                TaskMessage::Inspected {
                    address,
                    record,
                    result,
                } => {
                    self.busy = false;
                    match result {
                        Ok(info) => {
                            self.status = format!("{} is online.", info.name);
                            self.selected_address = Some(address);
                            self.selected_record = record.or_else(|| {
                                self.paired_hosts
                                    .iter()
                                    .find(|candidate| candidate.server_unique_id == info.unique_id)
                                    .cloned()
                            });
                            self.selected_info = Some(info);
                            if self.selected_record.is_some() {
                                if self.browser.open_apps_after_inspect
                                    && self.pending_autostart.is_none()
                                    && self.pending_apollo_launch.is_none()
                                {
                                    self.browser.page = BrowserPage::Applications;
                                }
                                self.refresh_applications();
                            } else if self.browser.open_apps_after_inspect {
                                self.browser.open_apps_after_inspect = false;
                                self.browser.dialog = Some(BrowserDialog::PairHost);
                            }
                        }
                        Err(error) => {
                            self.browser.open_apps_after_inspect = false;
                            self.status = error;
                        }
                    }
                }
                TaskMessage::Paired(result) => {
                    self.busy = false;
                    self.pairing_pin = None;
                    match result {
                        Ok((record, mut info)) => {
                            info.pair_status = true;
                            self.status = format!("Paired with {}.", record.name);
                            self.paired_hosts = self.store.load().unwrap_or_default();
                            self.selected_record = Some(record);
                            self.selected_info = Some(info);
                            self.browser.dialog = None;
                            self.browser.page = BrowserPage::Applications;
                            self.browser.open_apps_after_inspect = false;
                            self.refresh_applications();
                        }
                        Err(error) => self.status = error,
                    }
                }
                TaskMessage::Applications(result) => {
                    self.busy = false;
                    match result {
                        Ok((record, applications)) => {
                            self.status = format!(
                                "{} application(s) available on {}.",
                                applications.len(),
                                record.name
                            );
                            self.refresh_application_artwork(&record, &applications);
                            self.selected_record = Some(record);
                            self.applications = applications;
                            if self.pending_autostart.is_none()
                                && self.pending_apollo_launch.is_none()
                            {
                                self.browser.page = BrowserPage::Applications;
                            }
                            self.browser.open_apps_after_inspect = false;
                            if let Some(request) = self.pending_apollo_launch.take() {
                                let application =
                                    application_for_apollo_launch(&self.applications, &request);
                                if let Some(application) = application {
                                    tracing::info!(
                                        application = %application.title,
                                        "launching application requested by Apollo WebUI"
                                    );
                                    self.launch(application);
                                } else {
                                    self.status = format!(
                                        "Apollo application '{}' was not found on this host.",
                                        request.application_label()
                                    );
                                }
                            } else if let Some(request) = self.pending_autostart.take() {
                                let application = self
                                    .applications
                                    .iter()
                                    .find(|application| {
                                        application
                                            .title
                                            .eq_ignore_ascii_case(&request.application_title)
                                    })
                                    .cloned();
                                if let Some(application) = application {
                                    tracing::info!(
                                        application = %application.title,
                                        "launching diagnostic autostart application"
                                    );
                                    self.fullscreen_on_connect = request.fullscreen;
                                    self.autostop_after_connect = request.autostop_after;
                                    self.autostop_action = if request.autostop_cancel_host {
                                        AutostopAction::CancelHost
                                    } else {
                                        AutostopAction::Disconnect
                                    };
                                    self.launch(application);
                                } else {
                                    self.status = format!(
                                        "Autostart application '{}' was not found.",
                                        request.application_title
                                    );
                                }
                            }
                        }
                        Err(error) => self.status = error,
                    }
                }
                TaskMessage::ArtworkLoaded { result } => match result {
                    Ok(decoded) => {
                        self.artwork.finish(context, decoded);
                        context.request_repaint();
                    }
                    Err((key, error)) => {
                        tracing::warn!(%error, "application artwork is unavailable; using fallback");
                        self.artwork.fail(key);
                    }
                },
                TaskMessage::Launched {
                    record,
                    title,
                    supported_video_formats,
                    codec_label,
                    bit_depth_label,
                    result,
                } => match result {
                    Ok(launch) => self.begin_native_connection(
                        record,
                        title,
                        launch,
                        supported_video_formats,
                        codec_label,
                        bit_depth_label,
                    ),
                    Err(error) => {
                        tracing::error!(%error, "host application launch failed");
                        self.busy = false;
                        self.status = error;
                    }
                },
                TaskMessage::NativeConnected {
                    record,
                    title,
                    profile_label,
                    result,
                } => {
                    self.busy = false;
                    match result {
                        Ok((mut session, mut events)) => {
                            let audio_events = events.take_audio();
                            let video_events = events.take_video();
                            match MediaRuntime::new(
                                audio_events,
                                video_events,
                                self.gl_interop.clone(),
                                self.settings.frame_pacing,
                            ) {
                                Ok(media) => {
                                    self.status =
                                        format!("Stream connected: {title} at {profile_label}");
                                    self.active_stream = Some(ActiveStream {
                                        session,
                                        events,
                                        media,
                                        controller: ControllerManager::new(ControllerPreferences {
                                            swap_face_buttons: self.settings.swap_gamepad_buttons,
                                            force_gamepad_one: self.settings.force_gamepad_one,
                                            background_input: self
                                                .settings
                                                .gamepad_background_input,
                                        }),
                                        input: InputRouter::new(InputPreferences {
                                            optimize_mouse_for_desktop: self
                                                .settings
                                                .optimize_mouse_for_desktop,
                                            swap_mouse_buttons: self.settings.swap_mouse_buttons,
                                            reverse_scrolling: self.settings.reverse_scrolling,
                                        }),
                                        record,
                                        app_title: title,
                                        profile_label,
                                        connected: false,
                                        connection_quality: ConnectionQuality::Okay,
                                    });
                                    if self.fullscreen_on_connect {
                                        self.set_fullscreen(context, true);
                                        self.fullscreen_on_connect = false;
                                    }
                                    context.send_viewport_cmd(egui::ViewportCommand::CursorGrab(
                                        egui::CursorGrab::Locked,
                                    ));
                                    context.send_viewport_cmd(
                                        egui::ViewportCommand::CursorVisible(false),
                                    );
                                }
                                Err(error) => {
                                    tracing::error!(%error, "media runtime initialization failed");
                                    session.stop();
                                    self.status = error;
                                }
                            }
                        }
                        Err(error) => {
                            tracing::error!(%error, "native streaming connection failed");
                            self.status = error;
                        }
                    }
                }
                TaskMessage::NetworkTested { title, result } => {
                    self.show_network_result(title, result);
                }
                TaskMessage::Cancelled(result) => {
                    self.busy = false;
                    if let Err(error) = &result {
                        tracing::error!(%error, "host application cancellation failed");
                    } else {
                        tracing::info!("host application ended cleanly");
                    }
                    self.status = result.map_or_else(
                        |error| error,
                        |()| "The host application ended cleanly.".to_owned(),
                    );
                    if self.cancel_completion == CancelCompletion::CloseApplication {
                        self.cancel_completion = CancelCompletion::RemainOpen;
                        context.send_viewport_cmd(egui::ViewportCommand::Close);
                    } else if self.selected_record.is_some() {
                        self.refresh_applications();
                    }
                }
            }
        }
    }

    fn pump_stream(
        &mut self,
        context: &egui::Context,
        frame: &mut eframe::Frame,
        suppress_escape: bool,
    ) {
        let mut events = Vec::new();
        if let Some(active) = &self.active_stream {
            while let Ok(event) = active.events.try_recv() {
                events.push(event);
            }
        }
        let mut terminated = None;
        for event in events {
            let Some(active) = &mut self.active_stream else {
                break;
            };
            match event {
                StreamEvent::StageStarting(name) => {
                    self.status = format!("Connecting: {name}…");
                }
                StreamEvent::StageComplete(name) => {
                    self.status = format!("Connected stage: {name}");
                }
                StreamEvent::StageFailed { name, error } => {
                    self.status = format!("{name} failed with code {error}");
                }
                StreamEvent::Connected => {
                    active.connected = true;
                    self.autostop_deadline = self
                        .autostop_after_connect
                        .take()
                        .map(|duration| Instant::now() + duration);
                    self.status = format!(
                        "Streaming {} at {}.",
                        active.app_title, active.profile_label
                    );
                }
                StreamEvent::ConnectionStatus(ConnectionQuality::Okay) => {
                    active.connection_quality = ConnectionQuality::Okay;
                    self.status = format!(
                        "Streaming {} at {}.",
                        active.app_title, active.profile_label
                    );
                }
                StreamEvent::ConnectionStatus(ConnectionQuality::Poor) => {
                    active.connection_quality = ConnectionQuality::Poor;
                    "The stream connection is unstable.".clone_into(&mut self.status);
                }
                StreamEvent::Terminated(error) => terminated = Some(error),
                StreamEvent::HdrModeChanged(color) => {
                    tracing::info!(
                        target: "artemis::media",
                        hdr_active = color.hdr_active,
                        color_space = color.color_space.label(),
                        hdr_metadata = ?color.hdr_metadata,
                        "Apollo display HDR mode changed"
                    );
                }
                StreamEvent::VideoSetup { .. }
                | StreamEvent::VideoFrame { .. }
                | StreamEvent::AudioSetup { .. }
                | StreamEvent::AudioPacket(_) => {}
            }
        }
        if self.handle_autostop(context) {
            return;
        }

        let window_focused = context.input(|input| input.viewport().focused.unwrap_or(true));
        if let Some(active) = &mut self.active_stream {
            active
                .media
                .set_audio_muted(self.settings.mute_audio_when_inactive && !window_focused);
            active.controller.poll(&mut active.session, window_focused);
            if active.connected {
                active
                    .input
                    .forward(context, &mut active.session, suppress_escape);
            }
        }
        self.present_media_frame(context, frame);
        if let Some(active) = &mut self.active_stream {
            active.media.report_stream_stats(&active.session);
            if let Some(error) = active.media.poll_error() {
                self.status = error;
                active.session.request_idr();
            }
        }
        if let Some(error) = terminated {
            self.disconnect(context, false);
            self.status = if error == 0 {
                "The host ended the stream.".to_owned()
            } else {
                format!("The stream terminated with code {error}.")
            };
        }
    }

    fn present_media_frame(&mut self, context: &egui::Context, frame: &mut eframe::Frame) {
        let decoded_frame = self
            .active_stream
            .as_ref()
            .and_then(|active| active.media.try_frame());
        if let Some(decoded_frame) = decoded_frame {
            if let Err(error) = self.upload_stream_frame(frame, &decoded_frame) {
                self.status = error;
            } else if let Some(active) = &mut self.active_stream {
                active.media.record_presented(&decoded_frame);
            }
        }
        context.request_repaint();
    }

    fn handle_autostop(&mut self, context: &egui::Context) -> bool {
        if self
            .autostop_deadline
            .is_none_or(|deadline| Instant::now() < deadline)
        {
            return false;
        }
        let cancel_host = self.autostop_action.cancel_host();
        tracing::info!(
            cancel_host,
            "diagnostic autostop deadline reached; disconnecting cleanly"
        );
        self.autostop_deadline = None;
        self.autostop_action = AutostopAction::Disconnect;
        self.cancel_completion = if cancel_host {
            CancelCompletion::CloseApplication
        } else {
            CancelCompletion::RemainOpen
        };
        self.disconnect(context, cancel_host);
        if !cancel_host {
            context.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        true
    }

    fn upload_stream_frame(
        &mut self,
        frame: &mut eframe::Frame,
        decoded: &DecodedFrame,
    ) -> Result<(), String> {
        if let Some(texture) = &mut self.texture {
            texture.upload(decoded, self.fullscreen)?;
        } else {
            self.texture = Some(StreamTexture::new(frame, decoded, self.fullscreen)?);
        }
        let native_hdr = self
            .texture
            .as_ref()
            .is_some_and(StreamTexture::native_hdr_active);
        if let Some(active) = &self.active_stream {
            active
                .media
                .set_hdr_presentation(decoded.color.hdr_active, native_hdr);
        }
        Ok(())
    }

    fn handle_stream_shortcuts(&mut self, context: &egui::Context) -> bool {
        if self.active_stream.is_none() {
            return false;
        }
        if context.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::F11)) {
            self.set_fullscreen(context, !self.fullscreen);
        }
        if context.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::F10)) {
            self.toggle_diagnostics_preference();
        }
        if self.fullscreen
            && context
                .input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
        {
            self.set_fullscreen(context, false);
            return true;
        }
        false
    }

    fn set_fullscreen(&mut self, context: &egui::Context, fullscreen: bool) {
        self.fullscreen = fullscreen;
        context.send_viewport_cmd(egui::ViewportCommand::Fullscreen(fullscreen));
    }

    fn toggle_diagnostics_preference(&mut self) {
        self.diagnostics_overlay.toggle();
        self.settings.show_performance_diagnostics = self.diagnostics_overlay.is_visible();
        if let Err(error) = self.settings.save(self.identity.config_dir()) {
            tracing::warn!(%error, "could not save diagnostics preference");
        }
    }

    fn disconnect(&mut self, context: &egui::Context, cancel_host: bool) {
        let Some(mut active) = self.active_stream.take() else {
            return;
        };
        active.input.release_all(&mut active.session);
        active.controller.disconnect(&mut active.session);
        active.media.shutdown();
        active.session.stop();
        self.texture = None;
        self.autostop_deadline = None;
        self.autostop_after_connect = None;
        self.autostop_action = AutostopAction::Disconnect;
        self.set_fullscreen(context, false);
        context.send_viewport_cmd(egui::ViewportCommand::CursorGrab(egui::CursorGrab::None));
        context.send_viewport_cmd(egui::ViewportCommand::CursorVisible(true));
        self.status = format!("Disconnected from {}.", active.app_title);

        if cancel_host {
            self.busy = true;
            "Ending the application on the host…".clone_into(&mut self.status);
            let record = active.record;
            let identity = self.identity.clone();
            let sender = self.tasks.clone();
            thread::spawn(move || {
                let client = NvClient::new(
                    record.address.clone(),
                    identity,
                    Some(record.https_port),
                    Some(record.certificate_der),
                );
                let result = cancel_host_application(&client).map_err(|error| error.to_string());
                let _ = sender.send(TaskMessage::Cancelled(result));
            });
        }
    }

    fn stream_ui(&mut self, context: &egui::Context) {
        if !self.fullscreen {
            egui::TopBottomPanel::top("stream_controls").show(context, |ui| {
                ui.horizontal(|ui| {
                    let title = self.active_stream.as_ref().map_or_else(
                        || "Stream".to_owned(),
                        |active| format!("{} · {}", active.app_title, active.profile_label),
                    );
                    ui.label(RichText::new(title).strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("End host app").clicked() {
                            self.disconnect(context, true);
                        }
                        if ui.button("Disconnect").clicked() {
                            self.disconnect(context, false);
                        }
                        if ui.button("Fullscreen").clicked() {
                            self.set_fullscreen(context, true);
                        }
                        if ui
                            .button(if self.diagnostics_overlay.is_visible() {
                                "Hide diagnostics"
                            } else {
                                "Diagnostics"
                            })
                            .on_hover_text("Toggle performance diagnostics (F10)")
                            .clicked()
                        {
                            self.toggle_diagnostics_preference();
                        }
                    });
                });
            });
        }
        let stream_panel = egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(Color32::BLACK))
            .show(context, |ui| {
                if let Some(texture) = &self.texture {
                    let available = ui.available_size();
                    let source = texture.size_vec2();
                    let scale = (available.x / source.x)
                        .min(available.y / source.y)
                        .max(0.01);
                    let size = source * scale;
                    ui.centered_and_justified(|ui| {
                        let rect = egui::Rect::from_center_size(ui.max_rect().center(), size);
                        if let Some(callback) = texture.hdr_paint_callback(rect) {
                            ui.painter().add(callback);
                        } else {
                            ui.image((texture.id(), size));
                        }
                    });
                } else {
                    ui.centered_and_justified(|ui| {
                        ui.spinner();
                    });
                }
            });
        if self.diagnostics_overlay.is_visible() {
            self.performance_overlay(context);
        }
        let pointer_over_stream = context.input(|input| {
            input
                .pointer
                .hover_pos()
                .is_some_and(|position| stream_panel.response.rect.contains(position))
        });
        if self.fullscreen || pointer_over_stream {
            context.set_cursor_icon(egui::CursorIcon::None);
        }
    }

    fn performance_overlay(&self, context: &egui::Context) {
        let Some(active) = &self.active_stream else {
            return;
        };
        let diagnostics = active.media.diagnostics();
        let text_color = if self
            .texture
            .as_ref()
            .is_some_and(StreamTexture::native_hdr_active)
        {
            // PQ code value for roughly 200 nit reference white. The whole fullscreen surface is
            // BT.2020/PQ while native HDR is active, so ordinary sRGB white would signal 10,000 nits.
            Color32::from_gray(148)
        } else {
            Color32::WHITE
        };
        let top_offset = if self.fullscreen { 12.0 } else { 56.0 };
        egui::Area::new(egui::Id::new("performance_diagnostics"))
            .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-12.0, top_offset))
            .order(egui::Order::Foreground)
            .interactable(false)
            .show(context, |ui| {
                egui::Frame::NONE
                    .fill(Color32::from_black_alpha(190))
                    .corner_radius(egui::CornerRadius::same(6))
                    .inner_margin(egui::Margin::same(12))
                    .show(ui, |ui| {
                        ui.set_min_width(360.0);
                        ui.label(
                            RichText::new("PERFORMANCE DIAGNOSTICS  ·  F10")
                                .monospace()
                                .strong()
                                .color(text_color),
                        );
                        ui.label(
                            RichText::new(overlay_text(
                                &active.profile_label,
                                active.connection_quality,
                                &diagnostics,
                            ))
                            .monospace()
                            .size(12.0)
                            .color(text_color),
                        );
                    });
            });
    }
}

fn overlay_text(
    profile_label: &str,
    connection_quality: ConnectionQuality,
    diagnostics: &StreamDiagnostics,
) -> String {
    let quality = match connection_quality {
        ConnectionQuality::Okay => "Good",
        ConnectionQuality::Poor => "Poor",
    };
    let video_drift = diagnostics
        .video_clock_drift_ms
        .map_or_else(|| "—".to_owned(), |value| format!("{value:+} ms"));
    let audio_drift = diagnostics
        .audio_clock_drift_ms
        .map_or_else(|| "—".to_owned(), |value| format!("{value:+} ms"));
    let hdr_source = if diagnostics.hdr_source_active {
        "HDR10"
    } else {
        "SDR"
    };
    let hdr_metadata = diagnostics.hdr_max_content_light_level.map_or_else(
        || {
            if diagnostics.hdr_metadata_available {
                "metadata present".to_owned()
            } else {
                "no metadata".to_owned()
            }
        },
        |value| format!("MaxCLL {value} nits"),
    );
    format!(
        "\nRequested {profile_label}\n\
         Delivered Connection {quality}\n\
         Decoder   {} · {} · {}\n\
         Color     {hdr_source} · {} · {hdr_metadata}\n\
         Output    {}\n\
         Audio out {} · {}\n\
         Video     In {:>5.1} · Decode {:>5.1} · Present {:>5.1} FPS delivered\n\
         Drops     Decode queue {} · Callback queue {} /s\n\
         Network   {:>6.1} Mbps delivered · {:>6.0} video packets/s\n\
         Issues    Video {} · recovered {} /s\n\
         Audio     {:>5.0} packets/s · {:>5.0} Kbps\n\
         Issues    Audio {} · recovered {} /s\n\
         Clocks    Video {video_drift} · Audio {audio_drift}
         Pacing    {}",
        diagnostics.decoder,
        diagnostics.memory_path,
        diagnostics.video_bit_depth,
        diagnostics.video_color_space,
        diagnostics.hdr_presentation,
        diagnostics.audio_layout,
        diagnostics.audio_output,
        diagnostics.video_ingress_fps,
        diagnostics.decoded_fps,
        diagnostics.presented_fps,
        diagnostics.decoder_queue_dropped,
        diagnostics.callback_queue_dropped,
        diagnostics.video_mbps,
        diagnostics.video_network_pps,
        diagnostics.video_packet_issues,
        diagnostics.video_fec_recovered,
        diagnostics.audio_ingress_pps,
        diagnostics.audio_kbps,
        diagnostics.audio_packet_issues,
        diagnostics.audio_fec_recovered,
        if diagnostics.frame_pacing_active {
            "Presentation timestamps"
        } else {
            "Low-latency latest frame"
        },
    )
}

fn trace_decoder_capabilities(capabilities: DecoderCapabilities) {
    tracing::info!(
        target: "artemis::media",
        h264 = capabilities.h264.available,
        h264_hardware = capabilities.h264.hardware,
        hevc = capabilities.hevc.available,
        hevc_hardware = capabilities.hevc.hardware,
        hevc_main10 = capabilities.hevc.main10,
        av1 = capabilities.av1.available,
        av1_hardware = capabilities.av1.hardware,
        av1_main10 = capabilities.av1.main10,
        presentation_bit_depth = capabilities.presentation_bit_depth,
        "video decoder capabilities"
    );
}

impl eframe::App for ArtemisApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        Color32::from_rgb(28, 30, 37).to_normalized_gamma_f32()
    }

    fn update(&mut self, context: &egui::Context, frame: &mut eframe::Frame) {
        self.drain_tasks(context);
        let suppress_escape = self.handle_stream_shortcuts(context);
        self.pump_stream(context, frame, suppress_escape);

        if self.active_stream.is_some() && !self.fullscreen {
            egui::TopBottomPanel::bottom("status")
                .exact_height(30.0)
                .show(context, |ui| {
                    ui.horizontal_centered(|ui| {
                        if self.busy {
                            ui.spinner();
                        }
                        ui.label(
                            RichText::new(&self.status)
                                .small()
                                .color(Color32::from_rgb(70, 70, 68)),
                        );
                    });
                });
        }

        if self.active_stream.is_some() {
            self.stream_ui(context);
            // The compositor supplies the presentation cadence. Continuous frame callbacks avoid
            // missing a vblank while the media runtime still holds early 30 FPS frames by PTS.
            context.request_repaint();
        } else {
            self.browser_ui(context);
            if self.busy {
                context.request_repaint_after(Duration::from_millis(50));
            }
        }
    }
}

impl Drop for ArtemisApp {
    fn drop(&mut self) {
        if let Some(active) = &mut self.active_stream {
            active.input.release_all(&mut active.session);
            active.controller.disconnect(&mut active.session);
            active.media.shutdown();
            active.session.stop();
        }
    }
}

fn application_for_apollo_launch(
    applications: &[Application],
    request: &ApolloLaunchRequest,
) -> Option<Application> {
    request
        .app_uuid
        .as_deref()
        .and_then(|uuid| {
            applications.iter().find(|application| {
                application
                    .uuid
                    .as_deref()
                    .is_some_and(|candidate| candidate.eq_ignore_ascii_case(uuid))
            })
        })
        .or_else(|| {
            request
                .app_id
                .and_then(|id| applications.iter().find(|application| application.id == id))
        })
        .or_else(|| {
            request.app_name.as_deref().and_then(|name| {
                applications
                    .iter()
                    .find(|application| application.title.eq_ignore_ascii_case(name))
            })
        })
        .cloned()
}

fn configure_style(context: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = Color32::from_rgb(28, 30, 37);
    visuals.window_fill = Color32::from_rgb(53, 70, 111);
    visuals.faint_bg_color = Color32::from_rgb(42, 46, 57);
    visuals.extreme_bg_color = Color32::from_rgb(22, 24, 30);
    visuals.widgets.inactive.bg_fill = Color32::from_rgb(63, 78, 116);
    visuals.widgets.inactive.bg_stroke.color = Color32::from_rgb(92, 112, 159);
    visuals.widgets.inactive.fg_stroke.color = Color32::from_rgb(224, 228, 239);
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(77, 102, 158);
    visuals.widgets.hovered.bg_stroke.color = Color32::from_rgb(126, 164, 230);
    visuals.widgets.hovered.fg_stroke.color = Color32::WHITE;
    visuals.widgets.active.bg_fill = Color32::from_rgb(67, 91, 149);
    visuals.widgets.active.fg_stroke.color = Color32::WHITE;
    visuals.selection.bg_fill = Color32::from_rgb(101, 151, 225);
    visuals.selection.stroke.color = Color32::WHITE;
    context.set_visuals(visuals);

    let mut style = (*context.style()).clone();
    style.spacing.item_spacing = egui::vec2(10.0, 10.0);
    style.spacing.button_padding = egui::vec2(16.0, 11.0);
    style.visuals.widgets.inactive.corner_radius = egui::CornerRadius::same(7);
    style.visuals.widgets.hovered.corner_radius = egui::CornerRadius::same(7);
    style.visuals.widgets.active.corner_radius = egui::CornerRadius::same(7);
    context.set_style(style);
}

fn parse_manual_host(value: &str) -> std::result::Result<HostAddress, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("Enter a host name or IP address.".to_owned());
    }
    if value.matches(':').count() == 1 {
        if let Some((host, port)) = value.rsplit_once(':') {
            let port = port
                .parse::<u16>()
                .map_err(|_| "The manual HTTP port is invalid.".to_owned())?;
            if host.is_empty() {
                return Err("The manual host is empty.".to_owned());
            }
            return Ok(HostAddress::new(host, port));
        }
    }
    Ok(HostAddress::new(value, DEFAULT_HTTP_PORT))
}

fn autostart_from_environment() -> std::result::Result<Option<AutostartRequest>, String> {
    let host = std::env::var(AUTOSTART_HOST_ENV).ok();
    let application = std::env::var(AUTOSTART_APP_ENV).ok();
    let preset = std::env::var(AUTOSTART_PRESET_ENV).ok();
    let frame_rate = std::env::var(AUTOSTART_FPS_ENV).ok();
    let bitrate_mbps = std::env::var(AUTOSTART_BITRATE_ENV).ok();
    let codec = std::env::var(AUTOSTART_CODEC_ENV).ok();
    let fullscreen = std::env::var(AUTOSTART_FULLSCREEN_ENV).ok();
    let autostop_after_seconds = std::env::var(AUTOSTOP_AFTER_ENV).ok();
    let autostop_cancel_host = std::env::var(AUTOSTOP_CANCEL_HOST_ENV).ok();
    autostart_from_values(AutostartValues {
        host: host.as_deref(),
        application: application.as_deref(),
        preset: preset.as_deref(),
        frame_rate: frame_rate.as_deref(),
        bitrate_mbps: bitrate_mbps.as_deref(),
        codec: codec.as_deref(),
        fullscreen: fullscreen.as_deref(),
        autostop_after_seconds: autostop_after_seconds.as_deref(),
        autostop_cancel_host: autostop_cancel_host.as_deref(),
    })
}

fn autostart_from_values(
    values: AutostartValues<'_>,
) -> std::result::Result<Option<AutostartRequest>, String> {
    let AutostartValues {
        host,
        application,
        preset,
        frame_rate,
        bitrate_mbps,
        codec,
        fullscreen,
        autostop_after_seconds,
        autostop_cancel_host,
    } = values;
    let (Some(host), Some(application)) = (host, application) else {
        if host.is_some() || application.is_some() {
            return Err(format!(
                "{AUTOSTART_HOST_ENV} and {AUTOSTART_APP_ENV} must be set together"
            ));
        }
        return Ok(None);
    };
    let address = parse_manual_host(host)?;
    let application_title = application.trim();
    if application_title.is_empty() {
        return Err(format!("{AUTOSTART_APP_ENV} cannot be empty"));
    }
    let preset_value = preset.unwrap_or("1080p");
    let preset = parse_autostart_preset(preset_value)?;
    let frame_rate = match frame_rate {
        Some(value) => parse_autostart_frame_rate(value)?,
        None => parse_frame_rate_from_preset(preset_value),
    };
    let codec = parse_autostart_codec(codec.unwrap_or("auto"))?;
    let bitrate_override = match bitrate_mbps {
        Some(value) => {
            let mbps = value
                .parse::<i32>()
                .map_err(|_| format!("{AUTOSTART_BITRATE_ENV} must be a whole number"))?;
            Some(StreamBitrate::from_mbps(mbps).ok_or_else(|| {
                format!(
                    "{AUTOSTART_BITRATE_ENV} must be between {} and {}",
                    StreamBitrate::MIN_MBPS,
                    StreamBitrate::MAX_MBPS
                )
            })?)
        }
        None => None,
    };
    let fullscreen = match fullscreen.unwrap_or("false").to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" => true,
        "0" | "false" | "no" => false,
        _ => {
            return Err(format!(
                "{AUTOSTART_FULLSCREEN_ENV} must be true, false, 1, 0, yes, or no"
            ));
        }
    };
    let autostop_after = autostop_after_seconds
        .map(|value| {
            let seconds = value
                .parse::<u64>()
                .map_err(|_| format!("{AUTOSTOP_AFTER_ENV} must be a whole number"))?;
            if !(5..=3_600).contains(&seconds) {
                return Err(format!(
                    "{AUTOSTOP_AFTER_ENV} must be between 5 and 3600 seconds"
                ));
            }
            Ok(Duration::from_secs(seconds))
        })
        .transpose()?;
    let autostop_cancel_host =
        parse_optional_boolean(autostop_cancel_host, AUTOSTOP_CANCEL_HOST_ENV)?;
    if autostop_cancel_host && autostop_after.is_none() {
        return Err(format!(
            "{AUTOSTOP_CANCEL_HOST_ENV} requires {AUTOSTOP_AFTER_ENV}"
        ));
    }
    Ok(Some(AutostartRequest {
        address,
        application_title: application_title.to_owned(),
        preset,
        frame_rate,
        bitrate_override,
        codec,
        fullscreen,
        autostop_after,
        autostop_cancel_host,
    }))
}

fn parse_optional_boolean(
    value: Option<&str>,
    variable: &str,
) -> std::result::Result<bool, String> {
    match value.unwrap_or("false").to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" => Ok(true),
        "0" | "false" | "no" => Ok(false),
        _ => Err(format!("{variable} must be true, false, 1, 0, yes, or no")),
    }
}

fn parse_autostart_frame_rate(value: &str) -> std::result::Result<StreamFrameRate, String> {
    match value
        .trim()
        .trim_end_matches(['f', 'p', 's', 'F', 'P', 'S'])
    {
        "30" => Ok(StreamFrameRate::Fps30),
        "60" => Ok(StreamFrameRate::Fps60),
        _ => Err(format!(
            "{AUTOSTART_FPS_ENV} must be 30 or 60; higher refresh rates are not enabled yet"
        )),
    }
}

fn parse_frame_rate_from_preset(value: &str) -> StreamFrameRate {
    let normalized = value.trim().to_ascii_lowercase();
    if normalized.ends_with("30") {
        StreamFrameRate::Fps30
    } else {
        StreamFrameRate::Fps60
    }
}

fn parse_autostart_codec(value: &str) -> std::result::Result<VideoCodecPreference, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "auto" | "automatic" => Ok(VideoCodecPreference::Automatic),
        "av1" => Ok(VideoCodecPreference::Av1),
        "hevc" | "h265" | "h.265" => Ok(VideoCodecPreference::Hevc),
        "h264" | "h.264" => Ok(VideoCodecPreference::H264),
        _ => Err(format!(
            "{AUTOSTART_CODEC_ENV} must be auto, AV1, HEVC, H265, or H264"
        )),
    }
}

fn parse_autostart_preset(value: &str) -> std::result::Result<StreamPreset, String> {
    let normalized = value.trim().to_ascii_lowercase();
    if normalized.ends_with("90") || normalized.ends_with("120") {
        return Err(format!(
            "{AUTOSTART_PRESET_ENV} supports only 30 or 60 FPS; higher refresh rates are not \
             enabled yet"
        ));
    }
    match normalized.as_str() {
        "720p" | "720p30" | "720p60" => Ok(StreamPreset::Hd60),
        "1080p" | "1080p30" | "1080p60" => Ok(StreamPreset::FullHd60),
        "1440p" | "1440p30" | "1440p60" => Ok(StreamPreset::QuadHd60),
        "4k" | "4k30" | "4k60" | "2160p" | "2160p30" | "2160p60" => Ok(StreamPreset::UltraHd60),
        _ => Err(format!(
            "{AUTOSTART_PRESET_ENV} must be 720p, 1080p, 1440p, 4K, or 2160p"
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use artemis_core::{Application, StreamFrameRate, StreamPreset};
    use artemis_moonlight::{
        ConnectionQuality, VIDEO_FORMAT_AV1, VIDEO_FORMAT_AV1_MAIN10, VIDEO_FORMAT_H264,
        VIDEO_FORMAT_HEVC, VIDEO_FORMAT_HEVC_MAIN10,
    };

    use super::{
        AUTOSTART_APP_ENV, AUTOSTOP_AFTER_ENV, AutostartValues, VideoBitDepthPreference,
        VideoCodecPreference, application_for_apollo_launch, apply_autostart_settings,
        autostart_from_values, overlay_text, parse_manual_host,
    };
    use crate::deep_link::ApolloLaunchRequest;
    use crate::media::{DecoderCapabilities, DecoderSupport, StreamDiagnostics};
    use crate::settings::{AppSettings, BitrateMode};

    #[test]
    fn parses_default_and_explicit_ports() {
        assert_eq!(
            parse_manual_host("sunshine.local").expect("host").http_port,
            47_989
        );
        assert_eq!(
            parse_manual_host("192.168.1.20:48000")
                .expect("host and port")
                .http_port,
            48_000
        );
    }

    #[test]
    fn parses_diagnostic_autostart_profile() {
        let request = autostart_from_values(AutostartValues {
            host: Some("192.168.100.128"),
            application: Some("Desktop"),
            preset: Some("4K"),
            frame_rate: Some("60"),
            bitrate_mbps: Some("40"),
            codec: Some("av1"),
            fullscreen: Some("true"),
            autostop_after_seconds: Some("30"),
            autostop_cancel_host: Some("true"),
        })
        .expect("valid configuration")
        .expect("autostart request");

        assert_eq!(request.address.host, "192.168.100.128");
        assert_eq!(request.application_title, "Desktop");
        assert_eq!(request.preset, StreamPreset::UltraHd60);
        assert_eq!(request.frame_rate, StreamFrameRate::Fps60);
        assert_eq!(
            request.bitrate_override.expect("explicit bitrate").mbps(),
            40
        );
        assert_eq!(request.codec, VideoCodecPreference::Av1);
        assert!(request.fullscreen);
        assert_eq!(request.autostop_after, Some(Duration::from_secs(30)));
        assert!(request.autostop_cancel_host);
    }

    #[test]
    fn parses_seven_twenty_p_sixty_with_a_codec_recommendation() {
        let request = autostart_from_values(AutostartValues {
            host: Some("192.168.100.128"),
            application: Some("Desktop"),
            preset: Some("720p60"),
            codec: Some("auto"),
            ..AutostartValues::default()
        })
        .expect("valid configuration")
        .expect("autostart request");

        assert_eq!(request.preset, StreamPreset::Hd60);
        assert_eq!(request.frame_rate, StreamFrameRate::Fps60);
        assert!(request.bitrate_override.is_none());

        let mut settings = AppSettings::default();
        apply_autostart_settings(
            &mut settings,
            Some(&request),
            DecoderCapabilities {
                h264: DecoderSupport {
                    available: true,
                    hardware: true,
                    main10: false,
                },
                hevc: DecoderSupport {
                    available: true,
                    hardware: true,
                    main10: true,
                },
                av1: DecoderSupport {
                    available: true,
                    hardware: false,
                    main10: false,
                },
                presentation_bit_depth: 10,
            },
        );
        assert_eq!(settings.bitrate_mode, BitrateMode::Balanced);
        assert_eq!(settings.bitrate_mbps, 4);
    }

    #[test]
    fn rejects_high_refresh_autostart_profiles_for_now() {
        let error = autostart_from_values(AutostartValues {
            host: Some("192.168.100.128"),
            application: Some("Desktop"),
            preset: Some("720p90"),
            codec: Some("auto"),
            ..AutostartValues::default()
        })
        .expect_err("90 FPS should not be enabled");

        assert!(error.contains("30 or 60"));

        let error = autostart_from_values(AutostartValues {
            host: Some("192.168.100.128"),
            application: Some("Desktop"),
            frame_rate: Some("120"),
            ..AutostartValues::default()
        })
        .expect_err("120 FPS should not be enabled");

        assert!(error.contains("30 or 60"));
    }

    #[test]
    fn host_cancellation_requires_an_autostop_deadline() {
        let error = autostart_from_values(AutostartValues {
            host: Some("192.168.100.128"),
            application: Some("Desktop"),
            autostop_cancel_host: Some("true"),
            ..AutostartValues::default()
        })
        .expect_err("cancellation without a deadline");

        assert!(error.contains(AUTOSTOP_AFTER_ENV));
    }

    #[test]
    fn requires_both_autostart_target_values() {
        let error = autostart_from_values(AutostartValues {
            host: Some("192.168.100.128"),
            preset: Some("4K60"),
            ..AutostartValues::default()
        })
        .expect_err("incomplete configuration");

        assert!(error.contains(AUTOSTART_APP_ENV));
    }

    #[test]
    fn automatic_codec_mode_advertises_only_hardware_advanced_codecs() {
        let capabilities = DecoderCapabilities {
            h264: DecoderSupport {
                available: true,
                hardware: false,
                main10: false,
            },
            hevc: DecoderSupport {
                available: true,
                hardware: true,
                main10: true,
            },
            av1: DecoderSupport {
                available: true,
                hardware: true,
                main10: true,
            },
            presentation_bit_depth: 10,
        };

        assert_eq!(
            VideoCodecPreference::Automatic
                .supported_video_formats(VideoBitDepthPreference::EightBit, capabilities),
            VIDEO_FORMAT_AV1 | VIDEO_FORMAT_HEVC | VIDEO_FORMAT_H264
        );
    }

    #[test]
    fn automatic_bitrate_recommendation_uses_the_best_hardware_decoder() {
        let capabilities = DecoderCapabilities {
            h264: DecoderSupport {
                available: true,
                hardware: false,
                main10: false,
            },
            hevc: DecoderSupport {
                available: true,
                hardware: true,
                main10: true,
            },
            av1: DecoderSupport {
                available: true,
                hardware: false,
                main10: false,
            },
            presentation_bit_depth: 10,
        };

        assert_eq!(
            VideoCodecPreference::Automatic.bitrate_preference(capabilities),
            VideoCodecPreference::Hevc
        );
    }

    #[test]
    fn hevc_preference_excludes_av1_and_retains_h264_fallback() {
        let available = DecoderSupport {
            available: true,
            hardware: true,
            main10: true,
        };
        let capabilities = DecoderCapabilities {
            h264: available,
            hevc: available,
            av1: available,
            presentation_bit_depth: 10,
        };

        assert_eq!(
            VideoCodecPreference::Hevc
                .supported_video_formats(VideoBitDepthPreference::EightBit, capabilities),
            VIDEO_FORMAT_HEVC | VIDEO_FORMAT_H264
        );
    }

    #[test]
    fn main10_advertising_requires_decoder_and_ten_bit_presentation() {
        let support = DecoderSupport {
            available: true,
            hardware: true,
            main10: true,
        };
        let mut capabilities = DecoderCapabilities {
            h264: DecoderSupport::default(),
            hevc: support,
            av1: support,
            presentation_bit_depth: 8,
        };

        assert_eq!(
            VideoCodecPreference::Automatic
                .supported_video_formats(VideoBitDepthPreference::TenBit, capabilities),
            0
        );
        capabilities.presentation_bit_depth = 10;
        assert_eq!(
            VideoCodecPreference::Automatic
                .supported_video_formats(VideoBitDepthPreference::TenBit, capabilities),
            VIDEO_FORMAT_AV1_MAIN10 | VIDEO_FORMAT_HEVC_MAIN10
        );
    }

    #[test]
    fn apollo_launch_prefers_the_stable_application_uuid() {
        let applications = [
            Application {
                id: 1,
                uuid: Some("desktop-uuid".to_owned()),
                title: "Desktop".to_owned(),
                hdr_supported: false,
            },
            Application {
                id: 2,
                uuid: Some("steam-uuid".to_owned()),
                title: "Steam".to_owned(),
                hdr_supported: false,
            },
        ];
        let request = ApolloLaunchRequest {
            host_uuid: "host".to_owned(),
            host_name: None,
            app_uuid: Some("STEAM-UUID".to_owned()),
            app_name: Some("Renamed Steam".to_owned()),
            app_id: Some(99),
        };

        let application =
            application_for_apollo_launch(&applications, &request).expect("matched application");
        assert_eq!(application.id, 2);
    }

    #[test]
    fn performance_overlay_labels_measured_linux_stream_metrics() {
        let text = overlay_text(
            "3840x2160 at 60 FPS · 100 Mbps",
            ConnectionQuality::Okay,
            &StreamDiagnostics {
                video_ingress_fps: 60.0,
                decoded_fps: 59.8,
                presented_fps: 58.9,
                video_mbps: 96.4,
                audio_ingress_pps: 200.0,
                decoder: "VA-API H.264 (vah264dec)",
                memory_path: "DMABUF to GL texture",
                ..StreamDiagnostics::default()
            },
        );

        assert!(text.contains("Requested 3840x2160 at 60 FPS · 100 Mbps"));
        assert!(text.contains("Delivered Connection Good"));
        assert!(text.contains("VA-API H.264 (vah264dec)"));
        assert!(text.contains("Present  58.9 FPS delivered"));
        assert!(text.contains("96.4 Mbps delivered"));
        assert!(text.contains("200 packets/s"));
    }
}
