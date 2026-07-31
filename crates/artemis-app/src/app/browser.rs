use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Instant;

use artemis_core::{
    Application, HostAddress, HostRecord, NvClient, StreamBitrate, StreamFrameRate, StreamPreset,
};
use eframe::egui::{self, Align2, Color32, FontId, RichText, Stroke, StrokeKind, Vec2};
use serde::{Deserialize, Serialize};

use super::{ArtemisApp, TaskMessage};
use crate::media::{DecoderCapabilities, HdrDisplayCapabilities};
use crate::settings::{
    AVAILABLE_FRAME_RATES, BitrateMode, VideoBitDepthPreference, VideoCodecPreference,
    high_quality_bitrate_mbps_for_range, recommended_bitrate_mbps,
    recommended_bitrate_mbps_for_range,
};

const BACKGROUND: Color32 = Color32::from_rgb(28, 30, 37);
const SURFACE: Color32 = Color32::from_rgb(48, 53, 66);
const SURFACE_HOVER: Color32 = Color32::from_rgb(58, 73, 103);
const HEADER: Color32 = Color32::from_rgb(66, 86, 184);
const ACCENT: Color32 = Color32::from_rgb(101, 151, 225);
const TEXT: Color32 = Color32::from_rgb(238, 241, 248);
const MUTED_TEXT: Color32 = Color32::from_rgb(177, 185, 203);
const SUCCESS: Color32 = Color32::from_rgb(112, 205, 157);
const WARNING: Color32 = Color32::from_rgb(242, 190, 88);
const DANGER: Color32 = Color32::from_rgb(238, 111, 111);
const DEFAULT_HTTP_PORT: u16 = 47_989;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum BrowserPage {
    Computers,
    Applications,
    Settings,
}

#[derive(Clone)]
pub(super) enum BrowserDialog {
    AddComputer,
    Help,
    PairHost,
    HostActions { address: HostAddress, name: String },
    HostDetails { address: HostAddress, name: String },
    ConfirmDelete { record: HostRecord },
    AppActions { application: Application },
    AppDetails { application: Application },
    NetworkResult { title: String, summary: String },
}

#[derive(Default, Deserialize, Serialize)]
struct SavedBrowserPreferences {
    hidden_apps: BTreeMap<String, BTreeSet<i32>>,
}

pub(super) struct BrowserState {
    pub page: BrowserPage,
    pub dialog: Option<BrowserDialog>,
    pub open_apps_after_inspect: bool,
    pub show_hidden_apps: bool,
    settings_return_page: BrowserPage,
    preferences: SavedBrowserPreferences,
    preferences_path: PathBuf,
}

impl BrowserState {
    pub fn load(config_dir: &Path) -> Self {
        let preferences_path = config_dir.join("ui-preferences.json");
        let preferences = fs::read(&preferences_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self {
            page: BrowserPage::Computers,
            dialog: None,
            open_apps_after_inspect: false,
            show_hidden_apps: false,
            settings_return_page: BrowserPage::Computers,
            preferences,
            preferences_path,
        }
    }

    pub fn is_hidden(&self, host_id: &str, application_id: i32) -> bool {
        self.preferences
            .hidden_apps
            .get(host_id)
            .is_some_and(|applications| applications.contains(&application_id))
    }

    fn toggle_hidden(&mut self, host_id: &str, application_id: i32) -> Result<bool, String> {
        let applications = self
            .preferences
            .hidden_apps
            .entry(host_id.to_owned())
            .or_default();
        let hidden = if applications.remove(&application_id) {
            false
        } else {
            applications.insert(application_id);
            true
        };
        if applications.is_empty() {
            self.preferences.hidden_apps.remove(host_id);
        }
        self.save()?;
        Ok(hidden)
    }

    fn remove_host(&mut self, host_id: &str) -> Result<(), String> {
        self.preferences.hidden_apps.remove(host_id);
        self.save()
    }

    fn save(&self) -> Result<(), String> {
        let temporary_path = self.preferences_path.with_extension("json.tmp");
        let contents =
            serde_json::to_vec_pretty(&self.preferences).map_err(|error| error.to_string())?;
        fs::write(&temporary_path, contents).map_err(|error| error.to_string())?;
        fs::rename(temporary_path, &self.preferences_path).map_err(|error| error.to_string())
    }
}

#[derive(Clone)]
struct HostTile {
    address: HostAddress,
    name: String,
    record: Option<HostRecord>,
    online: bool,
}

enum BrowserAction {
    None,
    CloseDialog,
    BackToComputers,
    BackFromSettings,
    Discover,
    OpenAddComputer,
    OpenHelp,
    OpenSettings,
    InspectHost {
        address: HostAddress,
        show_hidden_apps: bool,
    },
    OpenHostActions {
        address: HostAddress,
        name: String,
    },
    OpenHostDetails {
        address: HostAddress,
        name: String,
    },
    OpenAppActions(Application),
    OpenAppDetails(Application),
    StartPairing,
    OpenServerConfig(HostAddress),
    TestNetwork {
        address: HostAddress,
        record: Option<HostRecord>,
        name: String,
    },
    ConfirmDelete(HostRecord),
    DeleteHost(HostRecord),
    Launch(Application),
    ToggleHidden(Application),
    CreateShortcut(Application),
    ExportLauncher(Application),
}

impl ArtemisApp {
    fn browser_header(&mut self, context: &egui::Context, controls_enabled: bool) -> BrowserAction {
        let mut action = BrowserAction::None;
        egui::TopBottomPanel::top("browser_header")
            .exact_height(52.0)
            .frame(
                egui::Frame::NONE
                    .fill(HEADER)
                    .inner_margin(egui::Margin::symmetric(16, 4)),
            )
            .show(context, |ui| {
                if !controls_enabled {
                    ui.disable();
                }
                let header_rect = ui.max_rect();
                let title = match self.browser.page {
                    BrowserPage::Computers => "Computers".to_owned(),
                    BrowserPage::Applications => self
                        .selected_record
                        .as_ref()
                        .map_or_else(|| "Applications".to_owned(), |host| host.name.clone()),
                    BrowserPage::Settings => "Settings".to_owned(),
                };
                ui.horizontal(|ui| {
                    ui.set_min_height(header_rect.height());
                    if self.browser.page == BrowserPage::Applications {
                        if header_icon_button(ui, "‹", "Back to computers") {
                            action = BrowserAction::BackToComputers;
                        }
                    } else if self.browser.page == BrowserPage::Settings
                        && header_icon_button(ui, "‹", "Back")
                    {
                        action = BrowserAction::BackFromSettings;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if self.browser.page != BrowserPage::Settings
                            && header_icon_button(ui, "⚙", "Stream settings")
                        {
                            action = BrowserAction::OpenSettings;
                        }
                        if header_icon_button(ui, "?", "Help and controls") {
                            action = BrowserAction::OpenHelp;
                        }
                        if header_icon_button(ui, "+", "Add a computer") {
                            action = BrowserAction::OpenAddComputer;
                        }
                    });
                });
                ui.painter().text(
                    header_rect.center(),
                    Align2::CENTER_CENTER,
                    title,
                    FontId::proportional(20.0),
                    TEXT,
                );
            });
        action
    }

    pub(super) fn browser_ui(&mut self, context: &egui::Context) {
        if self.browser.dialog.is_some()
            && context
                .input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
        {
            self.browser.dialog = None;
        }

        let controls_enabled = self.browser.dialog.is_none();
        let mut action = self.browser_header(context, controls_enabled);

        egui::TopBottomPanel::bottom("browser_status")
            .exact_height(42.0)
            .frame(
                egui::Frame::NONE
                    .fill(Color32::from_rgb(35, 38, 47))
                    .inner_margin(egui::Margin::symmetric(18, 8)),
            )
            .show(context, |ui| {
                ui.horizontal_centered(|ui| {
                    if self.busy {
                        ui.spinner();
                    }
                    ui.label(RichText::new(&self.status).color(MUTED_TEXT).size(13.0));
                });
            });

        egui::CentralPanel::default()
            .frame(
                egui::Frame::NONE
                    .fill(BACKGROUND)
                    .inner_margin(egui::Margin::same(24)),
            )
            .show(context, |ui| {
                if !controls_enabled {
                    ui.disable();
                }
                let page_action = match self.browser.page {
                    BrowserPage::Computers => self.computer_grid(ui),
                    BrowserPage::Applications => self.application_grid(ui),
                    BrowserPage::Settings => self.settings_page(ui),
                };
                if !matches!(page_action, BrowserAction::None) {
                    action = page_action;
                }
            });

        if let Some(dialog) = self.browser.dialog.clone() {
            let dialog_action = self.browser_dialog(context, dialog);
            if !matches!(dialog_action, BrowserAction::None) {
                action = dialog_action;
            }
        }
        self.apply_browser_action(action);
    }

    pub(super) fn show_network_result(&mut self, title: String, result: Result<String, String>) {
        self.busy = false;
        let summary = result.unwrap_or_else(|error| format!("The network test failed.\n\n{error}"));
        summary
            .lines()
            .next()
            .unwrap_or_default()
            .clone_into(&mut self.status);
        self.browser.dialog = Some(BrowserDialog::NetworkResult { title, summary });
    }

    fn computer_grid(&self, ui: &mut egui::Ui) -> BrowserAction {
        let hosts = self.host_tiles();
        ui.horizontal(|ui| {
            ui.heading(RichText::new("Your computers").size(28.0).color(TEXT));
            ui.label(
                RichText::new("Select a computer to view its applications")
                    .size(14.0)
                    .color(MUTED_TEXT),
            );
        });
        ui.add_space(24.0);
        if hosts.is_empty() {
            return empty_computers(ui, self.busy);
        }

        let mut action = BrowserAction::None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = Vec2::splat(18.0);
                    for host in hosts {
                        let interaction = host_tile(ui, &host, !self.busy);
                        if interaction.open {
                            action = BrowserAction::InspectHost {
                                address: host.address.clone(),
                                show_hidden_apps: false,
                            };
                        } else if interaction.options {
                            action = BrowserAction::OpenHostActions {
                                address: host.address,
                                name: host.name,
                            };
                        }
                    }
                });
            });
        action
    }

    fn application_grid(&self, ui: &mut egui::Ui) -> BrowserAction {
        let Some(record) = &self.selected_record else {
            return BrowserAction::BackToComputers;
        };
        let mut applications = self
            .applications
            .iter()
            .filter_map(|application| {
                let hidden = self
                    .browser
                    .is_hidden(&record.server_unique_id, application.id);
                (self.browser.show_hidden_apps || !hidden).then(|| (application.clone(), hidden))
            })
            .collect::<Vec<_>>();
        applications.sort_by_key(|(application, _)| application.title.to_lowercase());

        ui.horizontal(|ui| {
            ui.heading(
                RichText::new(if self.browser.show_hidden_apps {
                    "All applications"
                } else {
                    "Applications"
                })
                .size(28.0)
                .color(TEXT),
            );
            ui.label(
                RichText::new(format!(
                    "{} · {} at {} · {} Mbps",
                    record.name,
                    self.settings.resolution.resolution_label(),
                    self.settings.frame_rate.label(),
                    self.settings.bitrate_mbps
                ))
                .size(14.0)
                .color(MUTED_TEXT),
            );
        });
        ui.add_space(24.0);
        if applications.is_empty() {
            ui.vertical_centered(|ui| {
                ui.add_space(80.0);
                ui.label(
                    RichText::new(if self.busy {
                        "Loading applications…"
                    } else {
                        "No visible applications"
                    })
                    .size(22.0)
                    .color(TEXT),
                );
                ui.label(
                    RichText::new(
                        "Use the computer menu and choose View All Apps to show hidden apps.",
                    )
                    .color(MUTED_TEXT),
                );
            });
            return BrowserAction::None;
        }

        let mut action = BrowserAction::None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = Vec2::splat(18.0);
                    for (application, hidden) in applications {
                        let artwork = self
                            .artwork
                            .texture(&record.server_unique_id, application.id);
                        let interaction =
                            application_tile(ui, &application, artwork, hidden, !self.busy);
                        if interaction.open {
                            action = BrowserAction::Launch(application);
                        } else if interaction.options {
                            action = BrowserAction::OpenAppActions(application);
                        }
                    }
                });
            });
        action
    }

    fn browser_dialog(&mut self, context: &egui::Context, dialog: BrowserDialog) -> BrowserAction {
        let title = match &dialog {
            BrowserDialog::AddComputer => "Add computer".to_owned(),
            BrowserDialog::Help => "Help".to_owned(),
            BrowserDialog::PairHost => self
                .selected_info
                .as_ref()
                .map_or_else(|| "Pair computer".to_owned(), |info| info.name.clone()),
            BrowserDialog::HostActions { address, name } => {
                let online =
                    self.discovered_hosts.iter().any(|host| {
                        host.address == *address || host.name.eq_ignore_ascii_case(name)
                    }) || (self.selected_address.as_ref() == Some(address)
                        && self.selected_info.is_some());
                format!("{name} - {}", if online { "Online" } else { "Offline" })
            }
            BrowserDialog::HostDetails { name, .. } => name.clone(),
            BrowserDialog::ConfirmDelete { record } => format!("Delete {}?", record.name),
            BrowserDialog::AppActions { application }
            | BrowserDialog::AppDetails { application } => application.title.clone(),
            BrowserDialog::NetworkResult { title, .. } => title.clone(),
        };
        let mut action = BrowserAction::None;
        egui::Window::new(title)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .collapsible(false)
            .resizable(false)
            .title_bar(true)
            .default_width(600.0)
            .frame(
                egui::Frame::window(&context.style())
                    .fill(Color32::from_rgb(53, 70, 111))
                    .stroke(Stroke::new(1.0, Color32::from_rgb(104, 137, 205)))
                    .corner_radius(egui::CornerRadius::same(10))
                    .inner_margin(egui::Margin::same(18)),
            )
            .show(context, |ui| {
                ui.set_min_width(560.0);
                action = match dialog {
                    BrowserDialog::AddComputer => self.add_computer_dialog(ui),
                    BrowserDialog::Help => help_dialog(ui),
                    BrowserDialog::PairHost => self.pairing_dialog(ui),
                    BrowserDialog::HostActions { address, name } => {
                        self.host_actions_dialog(ui, address, name)
                    }
                    BrowserDialog::HostDetails { address, name } => {
                        self.host_details_dialog(ui, &address, &name)
                    }
                    BrowserDialog::ConfirmDelete { record } => confirm_delete_dialog(ui, record),
                    BrowserDialog::AppActions { application } => {
                        self.app_actions_dialog(ui, application)
                    }
                    BrowserDialog::AppDetails { application } => {
                        self.app_details_dialog(ui, &application)
                    }
                    BrowserDialog::NetworkResult { summary, .. } => {
                        network_result_dialog(ui, &summary)
                    }
                };
            });
        action
    }

    fn add_computer_dialog(&mut self, ui: &mut egui::Ui) -> BrowserAction {
        ui.label(
            RichText::new("Enter an Apollo or Sunshine host name or IP address.").color(MUTED_TEXT),
        );
        ui.add_space(10.0);
        ui.add(
            egui::TextEdit::singleline(&mut self.manual_host)
                .hint_text("192.168.1.20 or host:port")
                .desired_width(f32::INFINITY),
        );
        ui.add_space(16.0);
        let mut action = BrowserAction::None;
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    !self.busy,
                    egui::Button::new("Connect").min_size(Vec2::new(120.0, 44.0)),
                )
                .clicked()
            {
                match super::parse_manual_host(&self.manual_host) {
                    Ok(address) => {
                        action = BrowserAction::InspectHost {
                            address,
                            show_hidden_apps: false,
                        };
                    }
                    Err(error) => self.status = error,
                }
            }
            if ui
                .add(egui::Button::new("Cancel").min_size(Vec2::new(100.0, 44.0)))
                .clicked()
            {
                action = BrowserAction::CloseDialog;
            }
        });
        action
    }

    fn pairing_dialog(&mut self, ui: &mut egui::Ui) -> BrowserAction {
        let Some(info) = &self.selected_info else {
            ui.label("Computer details are unavailable.");
            return close_button(ui);
        };
        ui.label(
            RichText::new(format!(
                "{} is online but is not paired with this Artemis client.",
                info.name
            ))
            .color(MUTED_TEXT),
        );
        ui.add_space(12.0);
        ui.label("Apollo passphrase (optional)");
        ui.add(
            egui::TextEdit::singleline(&mut self.passphrase)
                .password(true)
                .desired_width(f32::INFINITY),
        );
        if let Some(pin) = &self.pairing_pin {
            ui.add_space(16.0);
            ui.label(
                RichText::new(format!("PIN  {pin}"))
                    .size(32.0)
                    .strong()
                    .color(TEXT),
            );
            ui.label(RichText::new("Enter this PIN in the host pairing dialog.").color(MUTED_TEXT));
        }
        ui.add_space(18.0);
        let mut action = BrowserAction::None;
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    !self.busy,
                    egui::Button::new("Start pairing").min_size(Vec2::new(140.0, 44.0)),
                )
                .clicked()
            {
                action = BrowserAction::StartPairing;
            }
            if ui
                .add(egui::Button::new("Cancel").min_size(Vec2::new(100.0, 44.0)))
                .clicked()
            {
                action = BrowserAction::CloseDialog;
            }
        });
        action
    }

    fn settings_page(&mut self, ui: &mut egui::Ui) -> BrowserAction {
        let previous_settings = self.settings.clone();
        ui.vertical_centered(|ui| {
            ui.heading(
                RichText::new("Streaming preferences")
                    .size(28.0)
                    .color(TEXT),
            );
            ui.label(
                RichText::new("Changes apply to the next session unless noted.")
                    .size(14.0)
                    .color(MUTED_TEXT),
            );
        });
        ui.add_space(18.0);
        ui.spacing_mut().scroll = egui::style::ScrollStyle::solid();
        egui::ScrollArea::vertical()
            .id_salt("settings_scroll")
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysVisible)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if ui.available_width() >= 980.0 {
                    ui.columns(2, |columns| {
                        columns[0].spacing_mut().item_spacing.y = 10.0;
                        columns[1].spacing_mut().item_spacing.y = 10.0;
                        self.basic_and_audio_settings(&mut columns[0]);
                        self.input_and_gamepad_settings(&mut columns[1]);
                    });
                } else {
                    self.basic_and_audio_settings(ui);
                    ui.add_space(16.0);
                    self.input_and_gamepad_settings(ui);
                }
                ui.add_space(24.0);
            });
        if self.settings != previous_settings {
            self.save_settings();
        }
        BrowserAction::None
    }

    fn basic_and_audio_settings(&mut self, ui: &mut egui::Ui) {
        self.basic_stream_settings(ui);
        ui.add_space(16.0);
        self.codec_settings(ui);
        ui.add_space(16.0);
        self.audio_settings(ui);
    }

    fn basic_stream_settings(&mut self, ui: &mut egui::Ui) {
        settings_group(ui, "Basic settings", |ui| {
            ui.label(RichText::new("Resolution and FPS").color(TEXT));
            ui.label(
                RichText::new("30 and 60 FPS are enabled for the current Linux beta.")
                    .size(12.0)
                    .color(MUTED_TEXT),
            );
            let previous_profile = (self.settings.resolution, self.settings.frame_rate);
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("stream_resolution")
                    .selected_text(self.settings.resolution.resolution_label())
                    .width(132.0)
                    .show_ui(ui, |ui| {
                        for preset in StreamPreset::ALL {
                            ui.selectable_value(
                                &mut self.settings.resolution,
                                preset,
                                preset.resolution_label(),
                            );
                        }
                    });
                egui::ComboBox::from_id_salt("stream_frame_rate")
                    .selected_text(self.settings.frame_rate.label())
                    .width(132.0)
                    .show_ui(ui, |ui| {
                        for frame_rate in AVAILABLE_FRAME_RATES {
                            ui.selectable_value(
                                &mut self.settings.frame_rate,
                                frame_rate,
                                frame_rate.label(),
                            );
                        }
                    });
            });
            if (self.settings.resolution, self.settings.frame_rate) != previous_profile {
                self.apply_bitrate_mode();
            }
            self.bitrate_settings(ui);
            ui.add_space(6.0);
            ui.label(RichText::new("Display mode").color(TEXT));
            egui::ComboBox::from_id_salt("stream_display_mode")
                .selected_text(self.settings.display_mode.label())
                .width(280.0)
                .show_ui(ui, |ui| {
                    for mode in crate::settings::StreamDisplayMode::ALL {
                        ui.selectable_value(&mut self.settings.display_mode, mode, mode.label());
                    }
                });
            settings_checkbox(ui, &mut self.settings.vsync, "V-Sync")
                .on_hover_text("V-Sync takes effect after restarting Artemis.");
            settings_checkbox(ui, &mut self.settings.frame_pacing, "Frame pacing").on_hover_text(
                "Schedules decoded frames from host presentation timestamps for smoother motion.",
            );
        });
    }

    fn bitrate_settings(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        let (balanced_bitrate, high_quality_bitrate, recommendation_codec) =
            self.bitrate_profile_settings();
        ui.label(RichText::new("Bitrate profile").color(TEXT));
        let previous_bitrate_mode = self.settings.bitrate_mode;
        egui::ComboBox::from_id_salt("bitrate_profile")
            .selected_text(self.settings.bitrate_mode.label())
            .width(280.0)
            .show_ui(ui, |ui| {
                ui.selectable_value(
                    &mut self.settings.bitrate_mode,
                    BitrateMode::Balanced,
                    format!("Balanced — {balanced_bitrate} Mbps"),
                );
                ui.selectable_value(
                    &mut self.settings.bitrate_mode,
                    BitrateMode::HighQualityLan,
                    format!("High Quality LAN — {high_quality_bitrate} Mbps"),
                );
                ui.selectable_value(
                    &mut self.settings.bitrate_mode,
                    BitrateMode::Custom,
                    "Custom",
                );
            });
        if self.settings.bitrate_mode != previous_bitrate_mode {
            self.apply_bitrate_mode();
        }
        ui.add_space(6.0);
        ui.label(
            RichText::new(format!(
                "Video bitrate: {} Mbps",
                self.settings.bitrate_mbps
            ))
            .color(TEXT),
        );
        let bitrate_slider = ui.add(
            egui::Slider::new(
                &mut self.settings.bitrate_mbps,
                StreamBitrate::MIN_MBPS..=StreamBitrate::MAX_MBPS,
            )
            .step_by(1.0)
            .show_value(false),
        );
        if bitrate_slider.changed() {
            self.settings.bitrate_mode = BitrateMode::Custom;
        }
        let bitrate_help = match self.settings.bitrate_mode {
            BitrateMode::Balanced => format!(
                "Balanced follows the {} table for {} at {} and {}.",
                if self.settings.video_bit_depth == VideoBitDepthPreference::TenBit {
                    "HDR"
                } else {
                    "SDR"
                },
                recommendation_codec.bitrate_label(),
                self.settings.resolution.resolution_label(),
                self.settings.frame_rate.label(),
            ),
            BitrateMode::HighQualityLan => {
                "High Quality LAN adds 25% headroom for a reliable wired or fast Wi-Fi LAN."
                    .to_owned()
            }
            BitrateMode::Custom => {
                "Custom preserves this bitrate when resolution, FPS, or codec changes.".to_owned()
            }
        };
        ui.label(RichText::new(bitrate_help).size(12.0).color(MUTED_TEXT));
    }

    fn codec_settings(&mut self, ui: &mut egui::Ui) {
        settings_group(ui, "Video codec", |ui| {
            let previous_codec = self.settings.video_codec;
            egui::ComboBox::from_id_salt("video_codec")
                .selected_text(self.settings.video_codec.label())
                .width(280.0)
                .show_ui(ui, |ui| {
                    for preference in VideoCodecPreference::ALL {
                        ui.selectable_value(
                            &mut self.settings.video_codec,
                            preference,
                            preference.label(),
                        );
                    }
                });
            if self.settings.video_codec != previous_codec {
                if !main10_available(self.settings.video_codec, self.decoder_capabilities) {
                    self.settings.video_bit_depth = VideoBitDepthPreference::EightBit;
                }
                self.apply_bitrate_mode();
            }
            ui.label(
                RichText::new(codec_support_text(
                    self.settings.video_codec,
                    self.decoder_capabilities,
                ))
                .size(12.0)
                .color(MUTED_TEXT),
            );
            ui.add_space(8.0);
            ui.label(RichText::new("Dynamic range").color(TEXT));
            let main10_available =
                main10_available(self.settings.video_codec, self.decoder_capabilities);
            let previous_dynamic_range = self.settings.video_bit_depth;
            egui::ComboBox::from_id_salt("video_bit_depth")
                .selected_text(self.settings.video_bit_depth.label())
                .width(280.0)
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut self.settings.video_bit_depth,
                        VideoBitDepthPreference::EightBit,
                        VideoBitDepthPreference::EightBit.label(),
                    );
                    ui.add_enabled_ui(main10_available, |ui| {
                        ui.selectable_value(
                            &mut self.settings.video_bit_depth,
                            VideoBitDepthPreference::TenBit,
                            VideoBitDepthPreference::TenBit.label(),
                        );
                    });
                });
            if self.settings.video_bit_depth != previous_dynamic_range {
                self.apply_bitrate_mode();
            }
            ui.label(
                RichText::new(main10_support_text(
                    self.settings.video_codec,
                    self.decoder_capabilities,
                    &self.hdr_display_capabilities,
                ))
                .size(12.0)
                .color(MUTED_TEXT),
            );
        });
    }

    fn bitrate_profile_settings(&self) -> (i32, i32, VideoCodecPreference) {
        let codec = self
            .settings
            .video_codec
            .bitrate_preference(self.decoder_capabilities);
        (
            recommended_bitrate_mbps_for_range(
                self.settings.resolution,
                self.settings.frame_rate,
                codec,
                self.settings.video_bit_depth,
            )
            .unwrap_or_else(|| {
                recommended_bitrate_mbps(self.settings.resolution, self.settings.frame_rate, codec)
            }),
            high_quality_bitrate_mbps_for_range(
                self.settings.resolution,
                self.settings.frame_rate,
                codec,
                self.settings.video_bit_depth,
            )
            .unwrap_or_else(|| {
                crate::settings::high_quality_bitrate_mbps(
                    self.settings.resolution,
                    self.settings.frame_rate,
                    codec,
                )
            }),
            codec,
        )
    }

    fn apply_bitrate_mode(&mut self) {
        let codec = self
            .settings
            .video_codec
            .bitrate_preference(self.decoder_capabilities);
        if let Some(bitrate_mbps) = self.settings.bitrate_mode.bitrate_mbps_for_range(
            self.settings.resolution,
            self.settings.frame_rate,
            codec,
            self.settings.video_bit_depth,
        ) {
            self.settings.bitrate_mbps = bitrate_mbps;
        }
    }

    fn audio_settings(&mut self, ui: &mut egui::Ui) {
        settings_group(ui, "Audio settings", |ui| {
            ui.label(RichText::new("Audio configuration").color(TEXT));
            egui::ComboBox::from_id_salt("audio_configuration")
                .selected_text(self.settings.audio_configuration.label())
                .width(230.0)
                .show_ui(ui, |ui| {
                    for configuration in artemis_core::StreamAudioConfiguration::ALL {
                        ui.selectable_value(
                            &mut self.settings.audio_configuration,
                            configuration,
                            configuration.label(),
                        );
                    }
                });
            ui.label(
                RichText::new(
                    "5.1 requires a surround-capable HDMI output and host capture device.",
                )
                .size(12.0)
                .color(MUTED_TEXT),
            );
            settings_checkbox(
                ui,
                &mut self.settings.mute_host_audio,
                "Mute host PC speakers while streaming",
            );
            settings_checkbox(
                ui,
                &mut self.settings.mute_audio_when_inactive,
                "Mute the audio stream while Artemis is inactive",
            );
        });
    }

    fn input_and_gamepad_settings(&mut self, ui: &mut egui::Ui) {
        settings_group(ui, "Input settings", |ui| {
            settings_checkbox(
                ui,
                &mut self.settings.optimize_mouse_for_desktop,
                "Optimize mouse for remote desktop instead of games",
            );
            let mut capture_shortcuts = false;
            ui.horizontal(|ui| {
                settings_checkbox_enabled(
                    ui,
                    false,
                    &mut capture_shortcuts,
                    "Capture system keyboard shortcuts",
                )
                .on_disabled_hover_text(
                    "Reserved shortcuts are controlled by the Linux desktop compositor.",
                );
                ui.add_enabled_ui(false, |ui| {
                    egui::ComboBox::from_id_salt("keyboard_capture")
                        .selected_text(self.settings.keyboard_capture.label())
                        .width(130.0)
                        .show_ui(ui, |ui| {
                            for mode in crate::settings::KeyboardCaptureMode::ALL {
                                ui.selectable_value(
                                    &mut self.settings.keyboard_capture,
                                    mode,
                                    mode.label(),
                                );
                            }
                        });
                });
            });
            settings_checkbox(
                ui,
                &mut self.settings.swap_mouse_buttons,
                "Swap left and right mouse buttons",
            );
            settings_checkbox(
                ui,
                &mut self.settings.reverse_scrolling,
                "Reverse mouse scrolling direction",
            );
            ui.label(
                RichText::new("Touchscreen trackpad mode is omitted on desktop Linux.")
                    .size(12.0)
                    .color(MUTED_TEXT),
            );
        });
        ui.add_space(16.0);
        settings_group(ui, "Gamepad settings", |ui| {
            settings_checkbox(
                ui,
                &mut self.settings.swap_gamepad_buttons,
                "Swap A/B and X/Y gamepad buttons",
            );
            settings_checkbox(
                ui,
                &mut self.settings.force_gamepad_one,
                "Force gamepad #1 always connected",
            );
            let mut gamepad_mouse_control = false;
            settings_checkbox_enabled(
                ui,
                false,
                &mut gamepad_mouse_control,
                "Enable mouse control by holding the Start button",
            )
            .on_disabled_hover_text("Gamepad mouse emulation is not implemented yet.");
            settings_checkbox(
                ui,
                &mut self.settings.gamepad_background_input,
                "Process gamepad input while Artemis is in the background",
            );
        });
        ui.add_space(16.0);
        settings_group(ui, "Diagnostics", |ui| {
            let mut diagnostics = self.settings.show_performance_diagnostics;
            if settings_checkbox(
                ui,
                &mut diagnostics,
                "Show performance diagnostics while streaming",
            )
            .changed()
            {
                self.settings.show_performance_diagnostics = diagnostics;
                self.diagnostics_overlay = if diagnostics {
                    super::DiagnosticsOverlay::Visible
                } else {
                    super::DiagnosticsOverlay::Hidden
                };
            }
            ui.label(
                RichText::new("Toggle the TV overlay during a stream with F10.")
                    .size(12.0)
                    .color(MUTED_TEXT),
            );
        });
    }

    fn save_settings(&mut self) {
        self.status = match self.settings.save(self.identity.config_dir()) {
            Ok(()) => "Settings saved.".to_owned(),
            Err(error) => format!("Could not save settings: {error}"),
        };
    }

    fn host_actions_dialog(
        &self,
        ui: &mut egui::Ui,
        address: HostAddress,
        name: String,
    ) -> BrowserAction {
        let record = self
            .paired_hosts
            .iter()
            .find(|record| record.address == address)
            .cloned();
        if menu_row(ui, "Go to Server Config", false) {
            return BrowserAction::OpenServerConfig(address);
        }
        if record.is_some() && menu_row(ui, "View All Apps", false) {
            return BrowserAction::InspectHost {
                address,
                show_hidden_apps: true,
            };
        }
        if menu_row(ui, "Test Network Connection", false) {
            return BrowserAction::TestNetwork {
                address,
                record,
                name,
            };
        }
        if let Some(record) = record {
            if menu_row(ui, "Delete PC", true) {
                return BrowserAction::ConfirmDelete(record);
            }
        }
        if menu_row(ui, "View Details", false) {
            return BrowserAction::OpenHostDetails { address, name };
        }
        if menu_row(ui, "Close", false) {
            return BrowserAction::CloseDialog;
        }
        BrowserAction::None
    }

    fn host_details_dialog(
        &self,
        ui: &mut egui::Ui,
        address: &HostAddress,
        name: &str,
    ) -> BrowserAction {
        let record = self
            .paired_hosts
            .iter()
            .find(|record| &record.address == address);
        let info = self
            .selected_address
            .as_ref()
            .filter(|selected| *selected == address)
            .and(self.selected_info.as_ref());
        detail_row(ui, "Computer", name);
        detail_row(ui, "Address", &display_address(address));
        detail_row(
            ui,
            "Status",
            if info.is_some() { "Online" } else { "Saved" },
        );
        detail_row(
            ui,
            "Pairing",
            if record.is_some() {
                "Paired"
            } else {
                "Not paired"
            },
        );
        if let Some(info) = info {
            detail_row(ui, "Host state", &info.state);
            detail_row(ui, "GameStream", &info.app_version);
            detail_row(ui, "HTTPS port", &info.https_port.to_string());
            detail_row(
                ui,
                "Codec capability",
                &format!("0x{:x}", info.codec_mode_support),
            );
            detail_row(ui, "Running application", &info.current_game.to_string());
        }
        ui.add_space(16.0);
        close_button(ui)
    }

    fn app_actions_dialog(&self, ui: &mut egui::Ui, application: Application) -> BrowserAction {
        let hidden = self.selected_record.as_ref().is_some_and(|record| {
            self.browser
                .is_hidden(&record.server_unique_id, application.id)
        });
        if menu_row(ui, "Start in Primary Display", false) {
            return BrowserAction::Launch(application);
        }
        if menu_row(
            ui,
            if hidden {
                "☑  Hide App"
            } else {
                "☐  Hide App"
            },
            false,
        ) {
            return BrowserAction::ToggleHidden(application);
        }
        if menu_row(ui, "View Details", false) {
            return BrowserAction::OpenAppDetails(application);
        }
        if menu_row(ui, "Create Shortcut", false) {
            return BrowserAction::CreateShortcut(application);
        }
        if menu_row(ui, "Export Launcher File", false) {
            return BrowserAction::ExportLauncher(application);
        }
        if menu_row(ui, "Close", false) {
            return BrowserAction::CloseDialog;
        }
        BrowserAction::None
    }

    fn app_details_dialog(&self, ui: &mut egui::Ui, application: &Application) -> BrowserAction {
        detail_row(ui, "Application", &application.title);
        detail_row(ui, "Application ID", &application.id.to_string());
        detail_row(
            ui,
            "HDR advertised",
            if application.hdr_supported {
                "Yes"
            } else {
                "No"
            },
        );
        if let Some(record) = &self.selected_record {
            detail_row(ui, "Computer", &record.name);
        }
        detail_row(
            ui,
            "Video profile",
            &format!(
                "{} at {}",
                self.settings.resolution.resolution_label(),
                self.settings.frame_rate.label()
            ),
        );
        detail_row(ui, "Video codec", self.settings.video_codec.label());
        detail_row(
            ui,
            "Configured bitrate",
            &format!("{} Mbps", self.settings.bitrate_mbps),
        );
        detail_row(ui, "Display target", "Primary display");
        ui.add_space(16.0);
        close_button(ui)
    }

    fn apply_browser_action(&mut self, action: BrowserAction) {
        match action {
            BrowserAction::None => {}
            BrowserAction::CloseDialog => self.browser.dialog = None,
            BrowserAction::BackToComputers => {
                self.browser.page = BrowserPage::Computers;
                self.browser.dialog = None;
            }
            BrowserAction::BackFromSettings => {
                self.browser.page = self.browser.settings_return_page;
                self.browser.dialog = None;
            }
            BrowserAction::Discover => self.start_discovery(),
            BrowserAction::OpenAddComputer => {
                self.browser.dialog = Some(BrowserDialog::AddComputer);
            }
            BrowserAction::OpenHelp => self.browser.dialog = Some(BrowserDialog::Help),
            BrowserAction::OpenSettings => {
                self.browser.settings_return_page = self.browser.page;
                self.browser.page = BrowserPage::Settings;
                self.browser.dialog = None;
            }
            BrowserAction::InspectHost {
                address,
                show_hidden_apps,
            } => {
                self.browser.dialog = None;
                self.browser.open_apps_after_inspect = true;
                self.browser.show_hidden_apps = show_hidden_apps;
                self.inspect(address);
            }
            BrowserAction::OpenHostActions { address, name } => {
                self.browser.dialog = Some(BrowserDialog::HostActions { address, name });
            }
            BrowserAction::OpenHostDetails { address, name } => {
                self.browser.dialog = Some(BrowserDialog::HostDetails { address, name });
            }
            BrowserAction::OpenAppActions(application) => {
                self.browser.dialog = Some(BrowserDialog::AppActions { application });
            }
            BrowserAction::OpenAppDetails(application) => {
                self.browser.dialog = Some(BrowserDialog::AppDetails { application });
            }
            BrowserAction::StartPairing => self.start_pairing(),
            BrowserAction::OpenServerConfig(address) => {
                self.browser.dialog = None;
                let url = server_config_url(&address);
                self.status = open_url(&url).map_or_else(
                    |error| format!("Could not open server config: {error}"),
                    |()| format!("Opened {url}"),
                );
            }
            BrowserAction::TestNetwork {
                address,
                record,
                name,
            } => self.start_network_test(address, record, name),
            BrowserAction::ConfirmDelete(record) => {
                self.browser.dialog = Some(BrowserDialog::ConfirmDelete { record });
            }
            BrowserAction::DeleteHost(record) => self.delete_host(&record),
            BrowserAction::Launch(application) => {
                self.browser.dialog = None;
                self.launch(application);
            }
            BrowserAction::ToggleHidden(application) => {
                self.toggle_hidden_application(&application);
            }
            BrowserAction::CreateShortcut(application) => {
                self.browser.dialog = None;
                self.write_application_launcher(&application, LauncherDestination::Applications);
            }
            BrowserAction::ExportLauncher(application) => {
                self.browser.dialog = None;
                self.write_application_launcher(&application, LauncherDestination::Downloads);
            }
        }
    }

    fn host_tiles(&self) -> Vec<HostTile> {
        let mut hosts = self
            .paired_hosts
            .iter()
            .map(|record| {
                let online = self
                    .discovered_hosts
                    .iter()
                    .any(|host| host.address == record.address)
                    || (self.selected_address.as_ref() == Some(&record.address)
                        && self.selected_info.is_some());
                HostTile {
                    address: record.address.clone(),
                    name: record.name.clone(),
                    record: Some(record.clone()),
                    online,
                }
            })
            .collect::<Vec<_>>();
        for discovered in &self.discovered_hosts {
            if let Some(existing) = hosts
                .iter_mut()
                .find(|host| host.name.eq_ignore_ascii_case(&discovered.name))
            {
                existing.online = true;
                continue;
            }
            if hosts.iter().any(|host| host.address == discovered.address) {
                continue;
            }
            hosts.push(HostTile {
                address: discovered.address.clone(),
                name: discovered.name.clone(),
                record: None,
                online: true,
            });
        }
        hosts.sort_by_key(|host| host.name.to_lowercase());
        hosts
    }

    fn start_network_test(
        &mut self,
        address: HostAddress,
        record: Option<HostRecord>,
        name: String,
    ) {
        self.browser.dialog = None;
        self.busy = true;
        self.status = format!("Testing the control connection to {name}…");
        let identity = self.identity.clone();
        let sender = self.tasks.clone();
        thread::spawn(move || {
            let authenticated = record.is_some();
            let mut client = NvClient::new(
                address,
                identity,
                record.as_ref().map(|record| record.https_port),
                record.map(|record| record.certificate_der),
            );
            let result = (|| {
                let mut samples = Vec::with_capacity(3);
                for _ in 0..3 {
                    let started = Instant::now();
                    client.server_info().map_err(|error| error.to_string())?;
                    samples.push(started.elapsed().as_secs_f64() * 1_000.0);
                }
                let minimum = samples.iter().copied().fold(f64::INFINITY, f64::min);
                let maximum = samples.iter().copied().fold(0.0_f64, f64::max);
                let average = samples.iter().sum::<f64>() / 3.0;
                let request_kind = if authenticated {
                    "Authenticated control requests"
                } else {
                    "Control requests"
                };
                Ok(format!(
                    "Connection to {name} is working.\n\n{request_kind}: 3\nMinimum: {minimum:.1} \
                     ms\nAverage: {average:.1} ms\nMaximum: {maximum:.1} ms"
                ))
            })();
            let _ = sender.send(TaskMessage::NetworkTested {
                title: format!("{name} network test"),
                result,
            });
        });
    }

    fn delete_host(&mut self, record: &HostRecord) {
        self.browser.dialog = None;
        if let Err(error) = self.store.remove(&record.server_unique_id) {
            self.status = format!("Could not delete {}: {error}", record.name);
            return;
        }
        if let Err(error) = self.browser.remove_host(&record.server_unique_id) {
            self.status = format!(
                "{} was deleted, but its UI preferences could not be removed: {error}",
                record.name
            );
        } else {
            self.status = format!("{} was deleted from this Artemis client.", record.name);
        }
        self.paired_hosts = self.store.load().unwrap_or_default();
        if self
            .selected_record
            .as_ref()
            .is_some_and(|selected| selected.server_unique_id == record.server_unique_id)
        {
            self.selected_record = None;
            self.selected_info = None;
            self.selected_address = None;
            self.applications.clear();
        }
        self.browser.page = BrowserPage::Computers;
    }

    fn toggle_hidden_application(&mut self, application: &Application) {
        let Some(record) = &self.selected_record else {
            return;
        };
        self.browser.dialog = None;
        self.status = match self
            .browser
            .toggle_hidden(&record.server_unique_id, application.id)
        {
            Ok(true) => format!("{} is hidden.", application.title),
            Ok(false) => format!("{} is visible.", application.title),
            Err(error) => format!("Could not save the hidden-app preference: {error}"),
        };
    }

    fn write_application_launcher(
        &mut self,
        application: &Application,
        destination: LauncherDestination,
    ) {
        let Some(record) = &self.selected_record else {
            return;
        };
        let stream = LauncherStreamSettings {
            preset: self.settings.resolution,
            frame_rate: self.settings.frame_rate,
            bitrate: self.settings.bitrate(),
            codec: self.settings.video_codec,
            fullscreen: self.settings.display_mode.fullscreen(),
        };
        self.status = match write_launcher(record, application, stream, destination) {
            Ok(path) => format!("Launcher written to {}", path.display()),
            Err(error) => format!("Could not write launcher: {error}"),
        };
    }
}

#[derive(Default)]
struct TileInteraction {
    open: bool,
    options: bool,
}

fn host_tile(ui: &mut egui::Ui, host: &HostTile, enabled: bool) -> TileInteraction {
    let size = Vec2::new(236.0, 205.0);
    let response = ui.add_enabled(
        enabled,
        egui::Button::new("")
            .min_size(size)
            .fill(SURFACE)
            .stroke(Stroke::new(1.0, Color32::from_rgb(72, 79, 96)))
            .corner_radius(egui::CornerRadius::same(10)),
    );
    let rect = response.rect;
    if response.hovered() || response.has_focus() {
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::same(10), SURFACE_HOVER);
        ui.painter().rect_stroke(
            rect.shrink(1.0),
            egui::CornerRadius::same(10),
            Stroke::new(3.0, ACCENT),
            StrokeKind::Inside,
        );
    }
    draw_monitor(
        ui.painter(),
        egui::pos2(rect.center().x, rect.top() + 78.0),
        host.online,
    );
    ui.painter().text(
        egui::pos2(rect.center().x, rect.bottom() - 50.0),
        Align2::CENTER_CENTER,
        truncate_label(&host.name, 22),
        FontId::proportional(20.0),
        TEXT,
    );
    ui.painter().text(
        egui::pos2(rect.center().x, rect.bottom() - 24.0),
        Align2::CENTER_CENTER,
        if host.online {
            if host.record.is_some() {
                "ONLINE · PAIRED"
            } else {
                "ONLINE · NOT PAIRED"
            }
        } else {
            "OFFLINE"
        },
        FontId::proportional(11.0),
        if host.online { SUCCESS } else { WARNING },
    );
    ui.painter().text(
        egui::pos2(rect.right() - 18.0, rect.top() + 18.0),
        Align2::CENTER_CENTER,
        "...",
        FontId::proportional(22.0),
        MUTED_TEXT,
    );
    response.clone().on_hover_text(format!(
        "{}\n{}\nRight-click or press Shift+F10 for options",
        host.name,
        display_address(&host.address)
    ));
    TileInteraction {
        open: response.clicked(),
        options: response.secondary_clicked()
            || (response.has_focus()
                && ui.input_mut(|input| input.consume_key(egui::Modifiers::SHIFT, egui::Key::F10))),
    }
}

fn application_tile(
    ui: &mut egui::Ui,
    application: &Application,
    artwork: Option<&egui::TextureHandle>,
    hidden: bool,
    enabled: bool,
) -> TileInteraction {
    let size = Vec2::new(214.0, 230.0);
    let response = ui.add_enabled(
        enabled,
        egui::Button::new("")
            .min_size(size)
            .fill(if hidden {
                Color32::from_rgb(40, 43, 52)
            } else {
                SURFACE
            })
            .stroke(Stroke::new(1.0, Color32::from_rgb(72, 79, 96)))
            .corner_radius(egui::CornerRadius::same(10)),
    );
    let rect = response.rect;
    if response.hovered() || response.has_focus() {
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::same(10), SURFACE_HOVER);
        ui.painter().rect_stroke(
            rect.shrink(1.0),
            egui::CornerRadius::same(10),
            Stroke::new(3.0, ACCENT),
            StrokeKind::Inside,
        );
    }
    let icon_rect = egui::Rect::from_center_size(
        egui::pos2(rect.center().x, rect.top() + 88.0),
        Vec2::new(132.0, 118.0),
    );
    if let Some(artwork) = artwork {
        ui.painter()
            .rect_filled(icon_rect, egui::CornerRadius::same(12), Color32::BLACK);
        let artwork_size = artwork.size_vec2();
        let scale = (icon_rect.width() / artwork_size.x).min(icon_rect.height() / artwork_size.y);
        let displayed_size = artwork_size * scale;
        let displayed_rect = egui::Rect::from_center_size(icon_rect.center(), displayed_size);
        ui.painter().image(
            artwork.id(),
            displayed_rect,
            egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
            Color32::WHITE,
        );
    } else {
        let hue = application_color(application.id);
        ui.painter()
            .rect_filled(icon_rect, egui::CornerRadius::same(12), hue);
        ui.painter().text(
            icon_rect.center(),
            Align2::CENTER_CENTER,
            application_mark(&application.title),
            FontId::proportional(44.0),
            TEXT,
        );
    }
    ui.painter().text(
        egui::pos2(rect.center().x, rect.bottom() - 49.0),
        Align2::CENTER_CENTER,
        truncate_label(&application.title, 20),
        FontId::proportional(18.0),
        TEXT,
    );
    if hidden {
        ui.painter().text(
            egui::pos2(rect.center().x, rect.bottom() - 24.0),
            Align2::CENTER_CENTER,
            "HIDDEN",
            FontId::proportional(11.0),
            WARNING,
        );
    }
    ui.painter().text(
        egui::pos2(rect.right() - 18.0, rect.top() + 18.0),
        Align2::CENTER_CENTER,
        "⋯",
        FontId::proportional(22.0),
        MUTED_TEXT,
    );
    response.clone().on_hover_text(format!(
        "{}\nRight-click or press Shift+F10 for options",
        application.title
    ));
    TileInteraction {
        open: response.clicked(),
        options: response.secondary_clicked()
            || (response.has_focus()
                && ui.input_mut(|input| input.consume_key(egui::Modifiers::SHIFT, egui::Key::F10))),
    }
}

fn draw_monitor(painter: &egui::Painter, center: egui::Pos2, online: bool) {
    let monitor = egui::Rect::from_center_size(center, Vec2::new(104.0, 66.0));
    let color = if online { TEXT } else { MUTED_TEXT };
    painter.rect_stroke(
        monitor,
        egui::CornerRadius::same(6),
        Stroke::new(7.0, color),
        StrokeKind::Inside,
    );
    painter.line_segment(
        [
            egui::pos2(center.x, monitor.bottom()),
            egui::pos2(center.x, monitor.bottom() + 20.0),
        ],
        Stroke::new(7.0, color),
    );
    painter.line_segment(
        [
            egui::pos2(center.x - 27.0, monitor.bottom() + 20.0),
            egui::pos2(center.x + 27.0, monitor.bottom() + 20.0),
        ],
        Stroke::new(7.0, color),
    );
    if !online {
        painter.text(
            center,
            Align2::CENTER_CENTER,
            "!",
            FontId::proportional(30.0),
            WARNING,
        );
    }
}

fn header_icon_button(ui: &mut egui::Ui, text: &str, tooltip: &str) -> bool {
    ui.add(
        egui::Button::new(RichText::new(text).size(25.0).color(TEXT))
            .frame(false)
            .min_size(Vec2::splat(44.0)),
    )
    .on_hover_text(tooltip)
    .clicked()
}

fn empty_computers(ui: &mut egui::Ui, busy: bool) -> BrowserAction {
    let mut action = BrowserAction::None;
    ui.vertical_centered(|ui| {
        ui.add_space(70.0);
        draw_monitor(
            ui.painter(),
            egui::pos2(ui.max_rect().center().x, 190.0),
            false,
        );
        ui.add_space(125.0);
        ui.label(
            RichText::new(if busy {
                "Looking for computers…"
            } else {
                "No computers found"
            })
            .size(24.0)
            .color(TEXT),
        );
        ui.label(
            RichText::new("Add a computer manually or search the local network again.")
                .color(MUTED_TEXT),
        );
        ui.add_space(18.0);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    !busy,
                    egui::Button::new("Add computer").min_size(Vec2::new(140.0, 44.0)),
                )
                .clicked()
            {
                action = BrowserAction::OpenAddComputer;
            }
            if ui
                .add_enabled(
                    !busy,
                    egui::Button::new("Search again").min_size(Vec2::new(140.0, 44.0)),
                )
                .clicked()
            {
                action = BrowserAction::Discover;
            }
        });
    });
    action
}

fn help_dialog(ui: &mut egui::Ui) -> BrowserAction {
    ui.label(
        RichText::new("Computer and application browser")
            .size(18.0)
            .strong()
            .color(TEXT),
    );
    ui.label(
        RichText::new(
            "Select a tile to open it. Right-click a tile or focus it and press Shift+F10 \
             for the same action menus shown in Artemis.",
        )
        .color(MUTED_TEXT),
    );
    ui.add_space(16.0);
    dialog_section(ui, "STREAM CONTROLS");
    detail_row(ui, "F10", "Toggle performance diagnostics");
    detail_row(ui, "F11", "Toggle fullscreen");
    detail_row(ui, "Escape", "Leave fullscreen or close a dialog");
    ui.add_space(16.0);
    dialog_section(ui, "INPUT");
    ui.label(
        RichText::new(
            "Keyboard, mouse, wheel, and one controller are forwarded after a stream connects.",
        )
        .color(MUTED_TEXT),
    );
    ui.add_space(18.0);
    close_button(ui)
}

fn confirm_delete_dialog(ui: &mut egui::Ui, record: HostRecord) -> BrowserAction {
    ui.label(
        RichText::new(format!(
            "This removes {} and its hidden-app preferences from this Artemis client.",
            record.name
        ))
        .color(TEXT),
    );
    ui.label(
        RichText::new("It does not delete or reconfigure Apollo/Sunshine on the host.")
            .color(MUTED_TEXT),
    );
    ui.add_space(18.0);
    let mut action = BrowserAction::None;
    ui.horizontal(|ui| {
        if ui
            .add(
                egui::Button::new(RichText::new("Delete PC").color(DANGER))
                    .min_size(Vec2::new(120.0, 44.0)),
            )
            .clicked()
        {
            action = BrowserAction::DeleteHost(record);
        }
        if ui
            .add(egui::Button::new("Cancel").min_size(Vec2::new(100.0, 44.0)))
            .clicked()
        {
            action = BrowserAction::CloseDialog;
        }
    });
    action
}

fn network_result_dialog(ui: &mut egui::Ui, summary: &str) -> BrowserAction {
    ui.label(RichText::new(summary).color(TEXT).size(15.0));
    ui.add_space(18.0);
    close_button(ui)
}

fn close_button(ui: &mut egui::Ui) -> BrowserAction {
    if ui
        .add(egui::Button::new("Close").min_size(Vec2::new(100.0, 44.0)))
        .clicked()
    {
        BrowserAction::CloseDialog
    } else {
        BrowserAction::None
    }
}

fn menu_row(ui: &mut egui::Ui, label: &str, danger: bool) -> bool {
    ui.add_sized(
        [ui.available_width(), 52.0],
        egui::Button::new(RichText::new(label).size(17.0).color(if danger {
            DANGER
        } else {
            TEXT
        }))
        .fill(Color32::from_rgb(60, 81, 128))
        .stroke(Stroke::NONE),
    )
    .clicked()
}

fn detail_row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.set_min_height(30.0);
        ui.label(RichText::new(label).strong().color(MUTED_TEXT).size(14.0));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(RichText::new(value).color(TEXT).size(14.0));
        });
    });
}

fn dialog_section(ui: &mut egui::Ui, label: &str) {
    ui.label(
        RichText::new(label)
            .size(12.0)
            .strong()
            .color(Color32::from_rgb(176, 199, 242)),
    );
    ui.add_space(5.0);
}

fn settings_group(ui: &mut egui::Ui, title: &str, add_contents: impl FnOnce(&mut egui::Ui)) {
    ui.label(
        RichText::new(title)
            .size(15.0)
            .strong()
            .color(Color32::from_rgb(146, 190, 229)),
    );
    let content_width = (ui.available_width() - 28.0).max(280.0);
    egui::Frame::NONE
        .fill(Color32::from_rgb(43, 46, 54))
        .stroke(Stroke::new(1.0, Color32::from_rgb(76, 81, 94)))
        .corner_radius(egui::CornerRadius::same(4))
        .inner_margin(egui::Margin::same(14))
        .show(ui, |ui| {
            ui.set_min_width(content_width);
            ui.spacing_mut().item_spacing.y = 10.0;
            add_contents(ui);
        });
}

fn settings_checkbox(
    ui: &mut egui::Ui,
    checked: &mut bool,
    text: impl Into<egui::WidgetText>,
) -> egui::Response {
    settings_checkbox_enabled(ui, true, checked, text)
}

fn settings_checkbox_enabled(
    ui: &mut egui::Ui,
    enabled: bool,
    checked: &mut bool,
    text: impl Into<egui::WidgetText>,
) -> egui::Response {
    ui.scope(|ui| {
        let corner_radius = egui::CornerRadius::same(1);
        let widgets = &mut ui.visuals_mut().widgets;
        widgets.noninteractive.corner_radius = corner_radius;
        widgets.inactive.corner_radius = corner_radius;
        widgets.hovered.corner_radius = corner_radius;
        widgets.active.corner_radius = corner_radius;
        widgets.open.corner_radius = corner_radius;
        ui.add_enabled(enabled, egui::Checkbox::new(checked, text))
    })
    .inner
}

fn application_color(application_id: i32) -> Color32 {
    let value = application_id.unsigned_abs();
    let red = 54 + u8::try_from(value % 42).unwrap_or_default();
    let green = 91 + u8::try_from((value / 7) % 58).unwrap_or_default();
    let blue = 142 + u8::try_from((value / 13) % 72).unwrap_or_default();
    Color32::from_rgb(red, green, blue)
}

fn application_mark(title: &str) -> String {
    if title.to_ascii_lowercase().contains("desktop") {
        "▣".to_owned()
    } else if title.to_ascii_lowercase().contains("steam") {
        "●".to_owned()
    } else {
        title
            .split_whitespace()
            .filter_map(|word| word.chars().next())
            .take(2)
            .collect::<String>()
            .to_uppercase()
    }
}

fn truncate_label(value: &str, maximum_characters: usize) -> String {
    let mut characters = value.chars();
    let truncated = characters
        .by_ref()
        .take(maximum_characters)
        .collect::<String>();
    if characters.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

fn display_address(address: &HostAddress) -> String {
    if address.http_port == DEFAULT_HTTP_PORT {
        address.host.clone()
    } else {
        format!("{}:{}", address.host, address.http_port)
    }
}

fn server_config_url(address: &HostAddress) -> String {
    let host = if address.host.contains(':') {
        format!("[{}]", address.host)
    } else {
        address.host.clone()
    };
    let port = address.http_port.saturating_add(1);
    format!("https://{host}:{port}")
}

fn open_url(url: &str) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    let mut command = {
        let mut command = Command::new("xdg-open");
        command.arg(url);
        command
    };
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = Command::new("cmd");
        command.args(["/C", "start", "", url]);
        command
    };
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = Command::new("open");
        command.arg(url);
        command
    };
    command
        .spawn()
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn codec_support_text(
    preference: VideoCodecPreference,
    capabilities: DecoderCapabilities,
) -> String {
    match preference {
        VideoCodecPreference::Automatic => {
            let mut order = Vec::new();
            if capabilities.av1.hardware {
                order.push("hardware AV1");
            }
            if capabilities.hevc.hardware {
                order.push("hardware HEVC");
            }
            if capabilities.h264.available {
                order.push("H.264");
            }
            if order.is_empty() {
                "No compatible GStreamer video decoder was detected.".to_owned()
            } else {
                format!(
                    "Apollo negotiation order: {}. Advanced codecs are automatic only with \
                     hardware decoding.",
                    order.join(" → ")
                )
            }
        }
        VideoCodecPreference::Av1 => codec_preference_support_text(
            "AV1",
            capabilities.av1.available,
            capabilities.av1.hardware,
            "HEVC, then H.264",
        ),
        VideoCodecPreference::Hevc => codec_preference_support_text(
            "HEVC",
            capabilities.hevc.available,
            capabilities.hevc.hardware,
            "H.264",
        ),
        VideoCodecPreference::H264 => codec_preference_support_text(
            "H.264",
            capabilities.h264.available,
            capabilities.h264.hardware,
            "no other codec",
        ),
    }
}

fn codec_preference_support_text(
    codec: &str,
    available: bool,
    hardware: bool,
    fallback: &str,
) -> String {
    if hardware {
        format!("Hardware {codec} decoding is available; fallback is {fallback}.")
    } else if available {
        format!(
            "Only software {codec} decoding is available; 4K60 may be slow. Fallback is \
             {fallback}."
        )
    } else {
        format!("{codec} decoding is unavailable; Artemis will fall back to {fallback}.")
    }
}

fn main10_available(preference: VideoCodecPreference, capabilities: DecoderCapabilities) -> bool {
    match preference {
        VideoCodecPreference::Automatic | VideoCodecPreference::Av1 => {
            capabilities.main10_ready(artemis_moonlight::VideoCodec::Av1)
                || capabilities.main10_ready(artemis_moonlight::VideoCodec::Hevc)
        }
        VideoCodecPreference::Hevc => {
            capabilities.main10_ready(artemis_moonlight::VideoCodec::Hevc)
        }
        VideoCodecPreference::H264 => false,
    }
}

fn main10_support_text(
    preference: VideoCodecPreference,
    capabilities: DecoderCapabilities,
    display: &HdrDisplayCapabilities,
) -> String {
    if capabilities.presentation_bit_depth < 10 {
        return format!(
            "HDR10 is unavailable because the GPU path reports a {}-bit surface. Select SDR or \
             use a display path with 10-bit EGL support.",
            capabilities.presentation_bit_depth
        );
    }
    if main10_available(preference, capabilities) {
        if display.native_hdr_presentation {
            "HDR10 Main10 decode and native HDR display presentation are available.".to_owned()
        } else if display.display_hdr10 {
            format!(
                "The connected display supports HDR10. This GNOME/OpenGL path cannot signal \
                 native HDR, so Artemis preserves Main10 and tone-maps the HDR source to SDR. {}",
                display.presentation_reason
            )
        } else {
            "HDR10 Main10 decode is available, but no connected HDR10 display was detected. \
             Artemis will use its explicit SDR tone-map fallback."
                .to_owned()
        }
    } else if preference == VideoCodecPreference::H264 {
        "H.264 10-bit is outside the Apollo/Moonlight streaming profile; select HEVC or AV1."
            .to_owned()
    } else {
        "The selected hardware decoder does not expose P010 Main10 output.".to_owned()
    }
}

#[derive(Clone, Copy)]
enum LauncherDestination {
    Applications,
    Downloads,
}

#[derive(Clone, Copy)]
struct LauncherStreamSettings {
    preset: StreamPreset,
    frame_rate: StreamFrameRate,
    bitrate: StreamBitrate,
    codec: VideoCodecPreference,
    fullscreen: bool,
}

fn write_launcher(
    record: &HostRecord,
    application: &Application,
    stream: LauncherStreamSettings,
    destination: LauncherDestination,
) -> Result<PathBuf, String> {
    let directory = launcher_directory(destination)?;
    fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    let filename = format!(
        "artemis-{}-{}.desktop",
        slug(&application.title),
        application.id
    );
    let path = directory.join(filename);
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let contents = launcher_contents(record, application, stream, &executable);
    fs::write(&path, contents).map_err(|error| error.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = fs::metadata(&path)
            .map_err(|error| error.to_string())?
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).map_err(|error| error.to_string())?;
    }
    Ok(path)
}

fn launcher_directory(destination: LauncherDestination) -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is not available".to_owned())?;
    Ok(match destination {
        LauncherDestination::Applications => std::env::var_os("XDG_DATA_HOME")
            .map_or_else(|| home.join(".local/share"), PathBuf::from)
            .join("applications"),
        LauncherDestination::Downloads => home.join("Downloads"),
    })
}

fn launcher_contents(
    record: &HostRecord,
    application: &Application,
    stream: LauncherStreamSettings,
    executable: &Path,
) -> String {
    let name = desktop_text(&format!("Artemis - {}", application.title));
    let host = desktop_exec_quote(&display_address(&record.address));
    let application_title = desktop_exec_quote(&application.title);
    let executable = desktop_exec_quote(&executable.to_string_lossy());
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Version=1.0\n\
         Name={name}\n\
         Comment=Stream {} from {}\n\
         Exec=env ARTEMIS_AUTOSTART_HOST={host} ARTEMIS_AUTOSTART_APP={application_title} \
         ARTEMIS_AUTOSTART_PRESET={} ARTEMIS_AUTOSTART_FPS={} \
         ARTEMIS_AUTOSTART_BITRATE_MBPS={} ARTEMIS_AUTOSTART_CODEC={} \
         ARTEMIS_AUTOSTART_FULLSCREEN={} {executable}\n\
         Icon=applications-games\n\
         Terminal=false\n\
         Categories=Game;\n\
         StartupNotify=true\n",
        desktop_text(&application.title),
        desktop_text(&record.name),
        stream.preset.resolution_label(),
        stream.frame_rate.fps(),
        stream.bitrate.mbps(),
        stream.codec.environment_value(),
        stream.fullscreen,
    )
}

fn desktop_text(value: &str) -> String {
    value
        .chars()
        .map(|character| match character {
            '\n' | '\r' => ' ',
            _ => character,
        })
        .collect()
}

fn desktop_exec_quote(value: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('`', "\\`")
        .replace('$', "\\$");
    format!("\"{escaped}\"")
}

fn slug(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut last_was_separator = false;
    for character in value.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            output.push(character);
            last_was_separator = false;
        } else if !last_was_separator && !output.is_empty() {
            output.push('-');
            last_was_separator = true;
        }
    }
    output.trim_end_matches('-').to_owned()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};

    use artemis_core::{
        Application, HostAddress, HostRecord, StreamBitrate, StreamFrameRate, StreamPreset,
    };

    use super::{
        BrowserState, LauncherStreamSettings, VideoCodecPreference, launcher_contents,
        server_config_url, slug,
    };

    #[test]
    fn management_url_uses_the_port_after_gamestream_http() {
        assert_eq!(
            server_config_url(&HostAddress::new("192.168.1.20", 47_989)),
            "https://192.168.1.20:47990"
        );
        assert_eq!(
            server_config_url(&HostAddress::new("2001:db8::1", 47_989)),
            "https://[2001:db8::1]:47990"
        );
    }

    #[test]
    fn launcher_slug_is_safe_and_stable() {
        assert_eq!(slug("Steam Big Picture"), "steam-big-picture");
        assert_eq!(slug("  Desktop / HDR  "), "desktop-hdr");
    }

    #[test]
    fn launcher_preserves_the_selected_stream_profile() {
        let record = HostRecord {
            address: HostAddress::new("192.168.1.20", 47_989),
            name: "Living Room".to_owned(),
            server_unique_id: "host".to_owned(),
            https_port: 47_984,
            certificate_der: Vec::new(),
        };
        let application = Application {
            id: 1,
            uuid: Some("steam-app".to_owned()),
            title: "Steam Big Picture".to_owned(),
            hdr_supported: false,
        };
        let launcher = launcher_contents(
            &record,
            &application,
            LauncherStreamSettings {
                preset: StreamPreset::UltraHd60,
                frame_rate: StreamFrameRate::Fps60,
                bitrate: StreamBitrate::from_mbps(100).expect("bitrate"),
                codec: VideoCodecPreference::Av1,
                fullscreen: true,
            },
            Path::new("/opt/Artemis Linux/artemis-linux"),
        );

        assert!(launcher.contains("ARTEMIS_AUTOSTART_APP=\"Steam Big Picture\""));
        assert!(launcher.contains("ARTEMIS_AUTOSTART_PRESET=4K"));
        assert!(launcher.contains("ARTEMIS_AUTOSTART_FPS=60"));
        assert!(launcher.contains("ARTEMIS_AUTOSTART_BITRATE_MBPS=100"));
        assert!(launcher.contains("ARTEMIS_AUTOSTART_CODEC=av1"));
        assert!(launcher.contains("\"/opt/Artemis Linux/artemis-linux\""));
    }

    #[test]
    fn hidden_app_preferences_survive_a_restart() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("artemis-browser-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&directory).expect("temporary directory");

        let mut state = BrowserState::load(&directory);
        assert!(state.toggle_hidden("host-one", 7).expect("hide app"));

        let mut restored = BrowserState::load(&directory);
        assert!(restored.is_hidden("host-one", 7));
        assert!(!restored.toggle_hidden("host-one", 7).expect("show app"));
        assert!(!BrowserState::load(&directory).is_hidden("host-one", 7));

        fs::remove_dir_all(directory).expect("remove temporary directory");
    }
}
