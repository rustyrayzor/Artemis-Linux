#![allow(unsafe_code)]

use std::collections::HashSet;

use artemis_moonlight::HdrMetadata;
use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use wayland_client::backend::{Backend, ObjectId};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_registry, wl_surface};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols::wp::color_management::v1::client::{
    wp_color_management_surface_v1::WpColorManagementSurfaceV1,
    wp_color_manager_v1::{
        self, Feature, Primaries, RenderIntent, TransferFunction, WpColorManagerV1,
    },
    wp_image_description_creator_params_v1::WpImageDescriptionCreatorParamsV1,
    wp_image_description_v1::{self, WpImageDescriptionV1},
};

const BT2020_TO_WAYLAND_SCALE: i32 = 20;

#[derive(Debug, Default)]
struct ProtocolState {
    supported: HashSet<Capability>,
    image_result: ImageResult,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Capability {
    Parametric,
    MasteringMetadata,
    Perceptual,
    Bt2020,
    Pq,
}

#[derive(Debug, Default)]
enum ImageResult {
    #[default]
    Pending,
    Ready,
    Failed(String),
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for ProtocolState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WpColorManagerV1, ()> for ProtocolState {
    fn event(
        state: &mut Self,
        _proxy: &WpColorManagerV1,
        event: wp_color_manager_v1::Event,
        _data: &(),
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
        match event {
            wp_color_manager_v1::Event::SupportedIntent {
                render_intent: WEnum::Value(RenderIntent::Perceptual),
            } => {
                state.supported.insert(Capability::Perceptual);
            }
            wp_color_manager_v1::Event::SupportedFeature {
                feature: WEnum::Value(Feature::Parametric),
            } => {
                state.supported.insert(Capability::Parametric);
            }
            wp_color_manager_v1::Event::SupportedFeature {
                feature: WEnum::Value(Feature::SetMasteringDisplayPrimaries),
            } => {
                state.supported.insert(Capability::MasteringMetadata);
            }
            wp_color_manager_v1::Event::SupportedPrimariesNamed {
                primaries: WEnum::Value(Primaries::Bt2020),
            } => {
                state.supported.insert(Capability::Bt2020);
            }
            wp_color_manager_v1::Event::SupportedTfNamed {
                tf: WEnum::Value(TransferFunction::St2084Pq),
            } => {
                state.supported.insert(Capability::Pq);
            }
            _ => {}
        }
    }
}

impl Dispatch<WpImageDescriptionV1, ()> for ProtocolState {
    fn event(
        state: &mut Self,
        _proxy: &WpImageDescriptionV1,
        event: wp_image_description_v1::Event,
        _data: &(),
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
        match event {
            wp_image_description_v1::Event::Ready { .. }
            | wp_image_description_v1::Event::Ready2 { .. } => {
                state.image_result = ImageResult::Ready;
            }
            wp_image_description_v1::Event::Failed { cause, msg } => {
                state.image_result = ImageResult::Failed(format!("{cause:?}: {msg}"));
            }
            _ => {}
        }
    }
}

wayland_client::delegate_noop!(ProtocolState: ignore WpColorManagementSurfaceV1);
wayland_client::delegate_noop!(ProtocolState: ignore WpImageDescriptionCreatorParamsV1);

/// Owns the color-management extension for eframe's existing Wayland surface.
///
/// The `wl_display` and `wl_surface` remain owned by eframe/winit. This wrapper uses a guest
/// connection so it never closes or destroys either foreign object.
pub struct NativeHdrSurface {
    connection: Connection,
    queue: EventQueue<ProtocolState>,
    state: ProtocolState,
    manager: WpColorManagerV1,
    color_surface: WpColorManagementSurfaceV1,
    image: Option<WpImageDescriptionV1>,
    metadata: Option<HdrMetadata>,
    active: bool,
}

impl NativeHdrSurface {
    pub fn new(frame: &eframe::Frame) -> Result<Self, String> {
        let display_handle = frame.display_handle().map_err(|error| error.to_string())?;
        let window_handle = frame.window_handle().map_err(|error| error.to_string())?;
        let RawDisplayHandle::Wayland(display) = display_handle.as_raw() else {
            return Err("the window is not using Wayland".to_owned());
        };
        let RawWindowHandle::Wayland(window) = window_handle.as_raw() else {
            return Err("the window is not a Wayland surface".to_owned());
        };

        // SAFETY: eframe owns both pointers and guarantees that they remain valid for the Frame's
        // lifetime. NativeHdrSurface is owned by the app and dropped before eframe destroys them.
        let backend = unsafe { Backend::from_foreign_display(display.display.as_ptr().cast()) };
        let connection = Connection::from_backend(backend);
        // SAFETY: raw-window-handle supplies the live wl_surface associated with this wl_display.
        // The wrapper is borrowed for protocol arguments only and never destroys the surface.
        let surface_id = unsafe {
            ObjectId::from_ptr(
                wl_surface::WlSurface::interface(),
                window.surface.as_ptr().cast(),
            )
        }
        .map_err(|error| format!("could not wrap the Wayland surface: {error}"))?;
        let surface = wl_surface::WlSurface::from_id(&connection, surface_id)
            .map_err(|error| format!("could not access the Wayland surface: {error}"))?;

        let (globals, mut queue) = registry_queue_init::<ProtocolState>(&connection)
            .map_err(|error| format!("could not read Wayland globals: {error}"))?;
        let manager: WpColorManagerV1 = globals
            .bind(&queue.handle(), 1..=2, ())
            .map_err(|error| format!("Wayland color management is unavailable: {error}"))?;
        let mut state = ProtocolState::default();
        queue
            .roundtrip(&mut state)
            .map_err(|error| format!("could not negotiate Wayland color management: {error}"))?;
        validate_support(&state)?;
        let color_surface = manager.get_surface(&surface, &queue.handle(), ());
        connection
            .flush()
            .map_err(|error| format!("could not create the HDR surface: {error}"))?;

        Ok(Self {
            connection,
            queue,
            state,
            manager,
            color_surface,
            image: None,
            metadata: None,
            active: false,
        })
    }

    pub fn activate(&mut self, metadata: Option<HdrMetadata>) -> Result<(), String> {
        if self.active && self.metadata == metadata {
            return Ok(());
        }
        self.deactivate()?;
        self.state.image_result = ImageResult::Pending;
        let creator = self
            .manager
            .create_parametric_creator(&self.queue.handle(), ());
        creator.set_tf_named(TransferFunction::St2084Pq);
        creator.set_primaries_named(Primaries::Bt2020);
        if self
            .state
            .supported
            .contains(&Capability::MasteringMetadata)
        {
            if let Some(metadata) = metadata {
                add_mastering_metadata(&creator, metadata, self.manager.version());
            }
        }
        let image = creator.create(&self.queue.handle(), ());
        self.queue
            .roundtrip(&mut self.state)
            .map_err(|error| format!("could not create the HDR image description: {error}"))?;
        match &self.state.image_result {
            ImageResult::Ready => {}
            ImageResult::Failed(error) => {
                image.destroy();
                return Err(format!("the compositor rejected BT.2020/PQ: {error}"));
            }
            ImageResult::Pending => {
                image.destroy();
                return Err("the compositor did not complete the HDR image description".to_owned());
            }
        }
        self.color_surface
            .set_image_description(&image, RenderIntent::Perceptual);
        self.connection
            .flush()
            .map_err(|error| format!("could not activate native HDR: {error}"))?;
        self.image = Some(image);
        self.metadata = metadata;
        self.active = true;
        Ok(())
    }

    pub const fn is_active(&self) -> bool {
        self.active
    }

    pub fn deactivate(&mut self) -> Result<(), String> {
        if self.active {
            self.color_surface.unset_image_description();
            self.connection
                .flush()
                .map_err(|error| format!("could not leave native HDR: {error}"))?;
        }
        if let Some(image) = self.image.take() {
            image.destroy();
        }
        self.metadata = None;
        self.active = false;
        Ok(())
    }
}

pub fn probe_compositor_support() -> Result<(), String> {
    let connection = Connection::connect_to_env()
        .map_err(|error| format!("could not connect to the Wayland compositor: {error}"))?;
    let (globals, mut queue) = registry_queue_init::<ProtocolState>(&connection)
        .map_err(|error| format!("could not read Wayland globals: {error}"))?;
    let manager: WpColorManagerV1 = globals
        .bind(&queue.handle(), 1..=2, ())
        .map_err(|error| format!("Wayland color management is unavailable: {error}"))?;
    let mut state = ProtocolState::default();
    queue
        .roundtrip(&mut state)
        .map_err(|error| format!("could not negotiate Wayland color management: {error}"))?;
    let result = validate_support(&state);
    manager.destroy();
    let _ = connection.flush();
    result
}

impl Drop for NativeHdrSurface {
    fn drop(&mut self) {
        if self.active {
            self.color_surface.unset_image_description();
        }
        if let Some(image) = self.image.take() {
            image.destroy();
        }
        self.color_surface.destroy();
        self.manager.destroy();
        let _ = self.connection.flush();
    }
}

fn validate_support(state: &ProtocolState) -> Result<(), String> {
    let mut missing = Vec::new();
    if !state.supported.contains(&Capability::Parametric) {
        missing.push("parametric image descriptions");
    }
    if !state.supported.contains(&Capability::Perceptual) {
        missing.push("perceptual rendering intent");
    }
    if !state.supported.contains(&Capability::Bt2020) {
        missing.push("BT.2020 primaries");
    }
    if !state.supported.contains(&Capability::Pq) {
        missing.push("ST 2084 PQ transfer");
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "the Wayland compositor lacks {}",
            missing.join(", ")
        ))
    }
}

fn add_mastering_metadata(
    creator: &WpImageDescriptionCreatorParamsV1,
    metadata: HdrMetadata,
    protocol_version: u32,
) {
    if valid_chromaticities(metadata) {
        creator.set_mastering_display_primaries(
            coordinate(metadata.display_primaries_x[0]),
            coordinate(metadata.display_primaries_y[0]),
            coordinate(metadata.display_primaries_x[1]),
            coordinate(metadata.display_primaries_y[1]),
            coordinate(metadata.display_primaries_x[2]),
            coordinate(metadata.display_primaries_y[2]),
            coordinate(metadata.white_point_x),
            coordinate(metadata.white_point_y),
        );
    }
    if metadata.max_display_luminance > 0
        && u32::from(metadata.max_display_luminance)
            > u32::from(metadata.min_display_luminance) / 10_000
    {
        creator.set_mastering_luminance(
            u32::from(metadata.min_display_luminance),
            u32::from(metadata.max_display_luminance),
        );
    }
    let max_cll = metadata.max_content_light_level;
    let max_fall = metadata.max_frame_average_light_level;
    let compatible_with_v1 = protocol_version >= 2
        || metadata.max_display_luminance == 0
        || max_cll <= metadata.max_display_luminance;
    if max_cll > 0 && compatible_with_v1 {
        creator.set_max_cll(u32::from(max_cll));
        if max_fall > 0 && max_fall <= max_cll {
            creator.set_max_fall(u32::from(max_fall));
        }
    }
}

fn valid_chromaticities(metadata: HdrMetadata) -> bool {
    metadata
        .display_primaries_x
        .into_iter()
        .chain(metadata.display_primaries_y)
        .chain([metadata.white_point_x, metadata.white_point_y])
        .all(|coordinate| coordinate > 0 && coordinate <= 50_000)
}

fn coordinate(value: u16) -> i32 {
    i32::from(value) * BT2020_TO_WAYLAND_SCALE
}

#[cfg(test)]
mod tests {
    use super::{coordinate, valid_chromaticities};
    use artemis_moonlight::HdrMetadata;

    fn bt2020_metadata() -> HdrMetadata {
        HdrMetadata {
            display_primaries_x: [35_400, 8_500, 6_550],
            display_primaries_y: [14_600, 39_850, 2_300],
            white_point_x: 15_635,
            white_point_y: 16_450,
            max_display_luminance: 1_000,
            min_display_luminance: 50,
            max_content_light_level: 1_000,
            max_frame_average_light_level: 400,
            max_full_frame_luminance: 600,
        }
    }

    #[test]
    fn moonlight_chromaticity_converts_to_wayland_scale() {
        assert_eq!(coordinate(35_400), 708_000);
        assert_eq!(coordinate(16_450), 329_000);
    }

    #[test]
    fn valid_mastering_metadata_requires_all_chromaticities() {
        assert!(valid_chromaticities(bt2020_metadata()));
        let mut missing = bt2020_metadata();
        missing.white_point_x = 0;
        assert!(!valid_chromaticities(missing));
    }
}
