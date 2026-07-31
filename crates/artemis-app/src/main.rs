mod app;
mod controller;
mod deep_link;
mod diagnostics;
#[cfg(target_os = "linux")]
mod hdr_surface;
mod input;
mod media;
mod settings;
mod video_texture;

use anyhow::Context;
use artemis_core::{ClientIdentity, HostStore};
use tracing_subscriber::EnvFilter;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            EnvFilter::new("artemis=info,artemis_core=info,artemis_moonlight=info")
        }))
        .init();

    let arguments = std::env::args().collect::<Vec<_>>();
    if arguments.iter().any(|argument| argument == "--diagnostics") {
        diagnostics::print_report();
        return Ok(());
    }
    let start_in_settings = arguments.iter().any(|argument| argument == "--settings");
    let apollo_launch = deep_link::apollo_launch_from_arguments(&arguments);

    let identity =
        ClientIdentity::load_or_create_default().context("initialize client identity")?;
    let store = HostStore::new(identity.config_dir());
    let startup_settings =
        settings::AppSettings::load(identity.config_dir()).unwrap_or_else(|error| {
            tracing::warn!(%error, "using default startup settings");
            settings::AppSettings::default()
        });
    let compositor_controls_vsync = matches!(
        std::env::var("ARTEMIS_COMPOSITOR_VSYNC").as_deref(),
        Ok("1" | "true" | "yes")
    );
    if compositor_controls_vsync {
        tracing::info!(
            requested_vsync = startup_settings.vsync,
            "the compositor owns vblank pacing; disabling the client swap interval"
        );
    }
    let options = eframe::NativeOptions {
        vsync: startup_settings.vsync && !compositor_controls_vsync,
        color_buffer_bits: Some(10),
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("Artemis Linux")
            .with_inner_size([1280.0, 760.0])
            .with_min_inner_size([900.0, 600.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Artemis Linux",
        options,
        Box::new(move |context| {
            Ok(Box::new(app::ArtemisApp::new(
                context,
                identity,
                store,
                start_in_settings,
                apollo_launch,
            )))
        }),
    )
    .map_err(|error| anyhow::anyhow!(error.to_string()))
}
