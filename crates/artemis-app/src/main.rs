mod app;
mod controller;
mod diagnostics;
mod input;
mod media;
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

    if std::env::args().any(|argument| argument == "--diagnostics") {
        diagnostics::print_report();
        return Ok(());
    }

    let identity =
        ClientIdentity::load_or_create_default().context("initialize client identity")?;
    let store = HostStore::new(identity.config_dir());
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("Artemis Linux")
            .with_inner_size([1280.0, 760.0])
            .with_min_inner_size([900.0, 600.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Artemis Linux",
        options,
        Box::new(move |context| Ok(Box::new(app::ArtemisApp::new(context, identity, store)))),
    )
    .map_err(|error| anyhow::anyhow!(error.to_string()))
}
