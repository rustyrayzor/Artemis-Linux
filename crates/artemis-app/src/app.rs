use std::thread;
use std::time::Duration;

use artemis_core::{
    Application, ClientIdentity, HostAddress, HostRecord, HostStore, LaunchResult, NvClient,
    PairingOutcome, ServerInfo, StreamProfile, cancel_host_application, discover, generate_pin,
    launch_application, list_applications, pair, stereo_audio_configuration,
};
use artemis_moonlight::{EventReceiver, Session, StreamConfig, StreamEvent};
use crossbeam_channel::{Receiver, Sender, unbounded};
use eframe::egui::{self, Color32, RichText, TextureHandle, TextureOptions};

use crate::controller::ControllerManager;
use crate::input::InputRouter;
use crate::media::MediaRuntime;

const DEFAULT_HTTP_PORT: u16 = 47_989;

pub struct ArtemisApp {
    identity: ClientIdentity,
    store: HostStore,
    paired_hosts: Vec<HostRecord>,
    discovered_hosts: Vec<artemis_core::DiscoveredHost>,
    selected_address: Option<HostAddress>,
    selected_record: Option<HostRecord>,
    selected_info: Option<ServerInfo>,
    applications: Vec<Application>,
    manual_host: String,
    passphrase: String,
    pairing_pin: Option<String>,
    status: String,
    busy: bool,
    tasks: Sender<TaskMessage>,
    task_results: Receiver<TaskMessage>,
    active_stream: Option<ActiveStream>,
    texture: Option<TextureHandle>,
}

struct ActiveStream {
    session: Session,
    events: EventReceiver,
    media: MediaRuntime,
    controller: ControllerManager,
    input: InputRouter,
    record: HostRecord,
    app_title: String,
    connected: bool,
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
    Launched {
        record: HostRecord,
        title: String,
        result: std::result::Result<LaunchResult, String>,
    },
    NativeConnected {
        record: HostRecord,
        title: String,
        result: std::result::Result<(Session, EventReceiver), String>,
    },
    Cancelled(std::result::Result<(), String>),
}

impl ArtemisApp {
    pub fn new(
        context: &eframe::CreationContext<'_>,
        identity: ClientIdentity,
        store: HostStore,
    ) -> Self {
        configure_style(&context.egui_ctx);
        let paired_hosts = store.load().unwrap_or_default();
        let (tasks, task_results) = unbounded();
        let mut app = Self {
            identity,
            store,
            paired_hosts,
            discovered_hosts: Vec::new(),
            selected_address: None,
            selected_record: None,
            selected_info: None,
            applications: Vec::new(),
            manual_host: String::new(),
            passphrase: String::new(),
            pairing_pin: None,
            status: "Ready".to_owned(),
            busy: false,
            tasks,
            task_results,
            active_stream: None,
            texture: None,
        };
        app.start_discovery();
        app
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

    fn launch(&mut self, application: Application) {
        let Some(record) = self.selected_record.clone() else {
            return;
        };
        self.busy = true;
        self.status = format!("Launching {}…", application.title);
        let identity = self.identity.clone();
        let sender = self.tasks.clone();
        thread::spawn(move || {
            let mut client = NvClient::new(
                record.address.clone(),
                identity,
                Some(record.https_port),
                Some(record.certificate_der.clone()),
            );
            let title = application.title;
            let result = launch_application(&mut client, application.id, StreamProfile::default())
                .map_err(|error| error.to_string());
            let _ = sender.send(TaskMessage::Launched {
                record,
                title,
                result,
            });
        });
    }

    fn begin_native_connection(&mut self, record: HostRecord, title: String, launch: LaunchResult) {
        self.status = format!("Connecting stream for {title}…");
        let config = StreamConfig {
            address: record.address.host.clone(),
            app_version: launch.server_info.app_version,
            gfe_version: launch.server_info.gfe_version,
            rtsp_session_url: launch.rtsp_session_url,
            server_codec_mode_support: launch.server_info.codec_mode_support,
            width: launch.profile.width,
            height: launch.profile.height,
            fps: launch.profile.fps,
            bitrate_kbps: launch.profile.bitrate_kbps,
            packet_size: launch.profile.packet_size,
            audio_configuration: stereo_audio_configuration(),
            client_refresh_rate_x100: launch.profile.fps * 100,
            remote_input_key: *launch.remote_input.key(),
            remote_input_iv: *launch.remote_input.iv(),
        };
        let sender = self.tasks.clone();
        thread::spawn(move || {
            let result = Session::connect(config).map_err(|error| error.to_string());
            let _ = sender.send(TaskMessage::NativeConnected {
                record,
                title,
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
                                self.refresh_applications();
                            }
                        }
                        Err(error) => self.status = error,
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
                            self.selected_record = Some(record);
                            self.applications = applications;
                        }
                        Err(error) => self.status = error,
                    }
                }
                TaskMessage::Launched {
                    record,
                    title,
                    result,
                } => match result {
                    Ok(launch) => self.begin_native_connection(record, title, launch),
                    Err(error) => {
                        self.busy = false;
                        self.status = error;
                    }
                },
                TaskMessage::NativeConnected {
                    record,
                    title,
                    result,
                } => {
                    self.busy = false;
                    match result {
                        Ok((mut session, mut events)) => {
                            let audio_events = events.take_audio();
                            match MediaRuntime::new(audio_events) {
                                Ok(media) => {
                                    self.status = format!("Stream connected: {title}");
                                    self.active_stream = Some(ActiveStream {
                                        session,
                                        events,
                                        media,
                                        controller: ControllerManager::new(),
                                        input: InputRouter::new(),
                                        record,
                                        app_title: title,
                                        connected: false,
                                    });
                                    context.send_viewport_cmd(egui::ViewportCommand::CursorGrab(
                                        egui::CursorGrab::Locked,
                                    ));
                                    context.send_viewport_cmd(
                                        egui::ViewportCommand::CursorVisible(false),
                                    );
                                }
                                Err(error) => {
                                    session.stop();
                                    self.status = error;
                                }
                            }
                        }
                        Err(error) => self.status = error,
                    }
                }
                TaskMessage::Cancelled(result) => {
                    self.busy = false;
                    self.status = result.map_or_else(
                        |error| error,
                        |()| "The host application ended cleanly.".to_owned(),
                    );
                    if self.selected_record.is_some() {
                        self.refresh_applications();
                    }
                }
            }
        }
    }

    fn pump_stream(&mut self, context: &egui::Context) {
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
                    self.status = format!("Streaming {}.", active.app_title);
                }
                StreamEvent::Terminated(error) => terminated = Some(error),
                event @ (StreamEvent::VideoSetup { .. }
                | StreamEvent::VideoFrame { .. }
                | StreamEvent::AudioSetup { .. }
                | StreamEvent::AudioPacket(_)) => {
                    if let Err(error) = active.media.handle(event) {
                        self.status = error;
                        active.session.request_idr();
                    }
                }
            }
        }

        if let Some(active) = &mut self.active_stream {
            active.controller.poll(&mut active.session);
            if active.connected {
                active.input.forward(context, &mut active.session);
            }
            if let Some(frame) = active.media.try_frame() {
                let image = egui::ColorImage::from_rgba_unmultiplied(
                    [frame.width, frame.height],
                    &frame.rgba,
                );
                if let Some(texture) = &mut self.texture {
                    texture.set(image, TextureOptions::LINEAR);
                } else {
                    self.texture =
                        Some(context.load_texture("artemis-stream", image, TextureOptions::LINEAR));
                }
            }
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

    fn disconnect(&mut self, context: &egui::Context, cancel_host: bool) {
        let Some(mut active) = self.active_stream.take() else {
            return;
        };
        active.input.release_all(&mut active.session);
        active.controller.disconnect(&mut active.session);
        active.media.shutdown();
        active.session.stop();
        self.texture = None;
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

    #[allow(clippy::too_many_lines)]
    fn browser_ui(&mut self, context: &egui::Context) {
        egui::SidePanel::left("hosts")
            .resizable(false)
            .default_width(280.0)
            .show(context, |ui| {
                ui.add_space(20.0);
                ui.heading(RichText::new("Hosts").size(24.0));
                ui.add_space(12.0);
                if ui
                    .add_enabled(!self.busy, egui::Button::new("Discover again"))
                    .clicked()
                {
                    self.start_discovery();
                }
                ui.add_space(18.0);
                section_label(ui, "PAIRED");
                let paired = self.paired_hosts.clone();
                for host in paired {
                    if ui
                        .selectable_label(
                            self.selected_address.as_ref() == Some(&host.address),
                            &host.name,
                        )
                        .clicked()
                    {
                        self.inspect(host.address);
                    }
                }
                ui.add_space(18.0);
                section_label(ui, "LOCAL NETWORK");
                let discovered = self.discovered_hosts.clone();
                for host in discovered {
                    let label = format!("{}\n{}", host.name, host.address.host);
                    if ui
                        .selectable_label(
                            self.selected_address.as_ref() == Some(&host.address),
                            label,
                        )
                        .clicked()
                    {
                        self.inspect(host.address);
                    }
                }
                ui.add_space(22.0);
                section_label(ui, "MANUAL");
                ui.text_edit_singleline(&mut self.manual_host);
                ui.label(
                    RichText::new("Host or host:port")
                        .small()
                        .color(Color32::from_rgb(120, 119, 116)),
                );
                if ui
                    .add_enabled(!self.busy, egui::Button::new("Connect"))
                    .clicked()
                {
                    match parse_manual_host(&self.manual_host) {
                        Ok(address) => self.inspect(address),
                        Err(error) => self.status = error,
                    }
                }
            });

        egui::CentralPanel::default().show(context, |ui| {
            ui.add_space(28.0);
            ui.horizontal(|ui| {
                ui.heading(RichText::new("Artemis Linux").size(30.0));
                ui.add_space(10.0);
                ui.label(
                    RichText::new("H.264 · 1080p60 · SDR")
                        .small()
                        .color(Color32::from_rgb(52, 101, 56)),
                );
            });
            ui.add_space(28.0);
            let Some(info) = self.selected_info.clone() else {
                ui.label(
                    RichText::new("Choose a discovered host or enter one manually.")
                        .size(18.0)
                        .color(Color32::from_rgb(120, 119, 116)),
                );
                return;
            };
            ui.group(|ui| {
                ui.set_min_width(ui.available_width());
                ui.heading(RichText::new(&info.name).size(24.0));
                ui.label(format!("{} · GameStream {}", info.state, info.app_version));
                ui.label(format!(
                    "Codec capability 0x{:x} · HTTPS {}",
                    info.codec_mode_support, info.https_port
                ));
                ui.add_space(12.0);
                if self.selected_record.is_none() {
                    ui.label("This client is not paired with the host.");
                    ui.horizontal(|ui| {
                        ui.label("Apollo passphrase (optional)");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.passphrase)
                                .password(true)
                                .desired_width(180.0),
                        );
                    });
                    if ui
                        .add_enabled(!self.busy, egui::Button::new("Start pairing"))
                        .clicked()
                    {
                        self.start_pairing();
                    }
                    if let Some(pin) = &self.pairing_pin {
                        ui.add_space(10.0);
                        ui.label(RichText::new(format!("PIN  {pin}")).size(28.0).strong());
                        ui.label("Enter this PIN in the host pairing dialog.");
                    }
                } else if ui
                    .add_enabled(!self.busy, egui::Button::new("Refresh applications"))
                    .clicked()
                {
                    self.refresh_applications();
                }
            });

            if !self.applications.is_empty() {
                ui.add_space(24.0);
                section_label(ui, "APPLICATIONS");
                ui.add_space(8.0);
                let applications = self.applications.clone();
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for application in applications {
                        ui.group(|ui| {
                            ui.set_min_width(ui.available_width());
                            ui.horizontal(|ui| {
                                ui.vertical(|ui| {
                                    ui.label(RichText::new(&application.title).size(18.0).strong());
                                    ui.label(
                                        RichText::new(format!("Application {}", application.id))
                                            .small()
                                            .color(Color32::from_rgb(120, 119, 116)),
                                    );
                                });
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui
                                            .add_enabled(!self.busy, egui::Button::new("Stream"))
                                            .clicked()
                                        {
                                            self.launch(application.clone());
                                        }
                                    },
                                );
                            });
                        });
                        ui.add_space(8.0);
                    }
                });
            }
        });
    }

    fn stream_ui(&mut self, context: &egui::Context) {
        egui::TopBottomPanel::top("stream_controls").show(context, |ui| {
            ui.horizontal(|ui| {
                let title = self
                    .active_stream
                    .as_ref()
                    .map_or("Stream", |active| active.app_title.as_str());
                ui.label(RichText::new(title).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("End host app").clicked() {
                        self.disconnect(context, true);
                    }
                    if ui.button("Disconnect").clicked() {
                        self.disconnect(context, false);
                    }
                });
            });
        });
        egui::CentralPanel::default()
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
                        ui.image((texture.id(), size));
                    });
                } else {
                    ui.centered_and_justified(|ui| {
                        ui.spinner();
                    });
                }
            });
    }
}

impl eframe::App for ArtemisApp {
    fn update(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_tasks(context);
        self.pump_stream(context);

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

        if self.active_stream.is_some() {
            self.stream_ui(context);
            context.request_repaint_after(Duration::from_millis(8));
        } else {
            self.browser_ui(context);
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

fn configure_style(context: &egui::Context) {
    let mut visuals = egui::Visuals::light();
    visuals.panel_fill = Color32::from_rgb(247, 246, 243);
    visuals.window_fill = Color32::from_rgb(251, 251, 250);
    visuals.faint_bg_color = Color32::from_rgb(249, 249, 248);
    visuals.extreme_bg_color = Color32::WHITE;
    visuals.widgets.inactive.bg_stroke.color = Color32::from_rgb(234, 234, 234);
    visuals.widgets.hovered.bg_stroke.color = Color32::from_rgb(180, 180, 178);
    visuals.selection.bg_fill = Color32::from_rgb(225, 243, 254);
    visuals.selection.stroke.color = Color32::from_rgb(31, 108, 159);
    context.set_visuals(visuals);

    let mut style = (*context.style()).clone();
    style.spacing.item_spacing = egui::vec2(10.0, 10.0);
    style.spacing.button_padding = egui::vec2(14.0, 7.0);
    style.visuals.widgets.inactive.corner_radius = egui::CornerRadius::same(5);
    style.visuals.widgets.hovered.corner_radius = egui::CornerRadius::same(5);
    style.visuals.widgets.active.corner_radius = egui::CornerRadius::same(5);
    context.set_style(style);
}

fn section_label(ui: &mut egui::Ui, text: &str) {
    ui.label(
        RichText::new(text)
            .size(11.0)
            .strong()
            .color(Color32::from_rgb(120, 119, 116)),
    );
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

#[cfg(test)]
mod tests {
    use super::parse_manual_host;

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
}
