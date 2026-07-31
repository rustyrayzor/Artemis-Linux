use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use artemis_core::{Application, NvClient, application_asset};
use eframe::egui::{self, TextureHandle};

const MAX_ARTWORK_BYTES: usize = 8 * 1024 * 1024;
const MAX_ARTWORK_DIMENSION: u32 = 4096;
const MAX_ARTWORK_PIXELS: u64 = 16_777_216;
const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct ArtworkKey {
    host_id: String,
    application_id: i32,
}

impl ArtworkKey {
    pub(super) fn new(host_id: &str, application_id: i32) -> Self {
        Self {
            host_id: host_id.to_owned(),
            application_id,
        }
    }
}

pub(super) struct DecodedArtwork {
    pub key: ArtworkKey,
    pub image: egui::ColorImage,
}

enum ArtworkState {
    Loading,
    Ready(TextureHandle),
    Failed,
}

#[derive(Default)]
pub(super) struct ArtworkStore {
    states: HashMap<ArtworkKey, ArtworkState>,
}

impl ArtworkStore {
    pub(super) fn begin_host_load(
        &mut self,
        host_id: &str,
        applications: &[Application],
    ) -> Vec<Application> {
        self.states.retain(|key, _| key.host_id == host_id);
        applications
            .iter()
            .filter_map(|application| {
                let key = ArtworkKey::new(host_id, application.id);
                if let std::collections::hash_map::Entry::Vacant(entry) = self.states.entry(key) {
                    entry.insert(ArtworkState::Loading);
                    Some(application.clone())
                } else {
                    None
                }
            })
            .collect()
    }

    pub(super) fn finish(&mut self, context: &egui::Context, result: DecodedArtwork) {
        let name = format!(
            "app-artwork-{}-{}",
            result.key.host_id, result.key.application_id
        );
        let texture = context.load_texture(name, result.image, egui::TextureOptions::LINEAR);
        self.states.insert(result.key, ArtworkState::Ready(texture));
    }

    pub(super) fn fail(&mut self, key: ArtworkKey) {
        self.states.insert(key, ArtworkState::Failed);
    }

    pub(super) fn texture(&self, host_id: &str, application_id: i32) -> Option<&TextureHandle> {
        match self.states.get(&ArtworkKey::new(host_id, application_id)) {
            Some(ArtworkState::Ready(texture)) => Some(texture),
            Some(ArtworkState::Loading | ArtworkState::Failed) | None => None,
        }
    }
}

pub(super) fn load(
    config_dir: &Path,
    host_id: &str,
    client: &NvClient,
    application: &Application,
) -> Result<DecodedArtwork, String> {
    let key = ArtworkKey::new(host_id, application.id);
    let cache_path = cache_path(config_dir, &key);
    if let Ok(bytes) = fs::read(&cache_path) {
        match decode(&bytes) {
            Ok(image) => return Ok(DecodedArtwork { key, image }),
            Err(error) => {
                tracing::warn!(
                    path = %cache_path.display(),
                    %error,
                    "discarding invalid cached application artwork"
                );
                if let Err(remove_error) = fs::remove_file(&cache_path) {
                    tracing::debug!(
                        path = %cache_path.display(),
                        %remove_error,
                        "could not remove invalid artwork cache entry"
                    );
                }
            }
        }
    }

    let bytes = application_asset(client, application).map_err(|error| error.to_string())?;
    let image = decode(&bytes)?;
    persist(&cache_path, &bytes)?;
    Ok(DecodedArtwork { key, image })
}

fn decode(bytes: &[u8]) -> Result<egui::ColorImage, String> {
    if bytes.len() > MAX_ARTWORK_BYTES {
        return Err(format!(
            "application artwork exceeded the {} MiB safety limit",
            MAX_ARTWORK_BYTES / (1024 * 1024)
        ));
    }
    if !bytes.starts_with(PNG_SIGNATURE) {
        return Err("application artwork was not a PNG image".to_owned());
    }
    let icon = eframe::icon_data::from_png_bytes(bytes)
        .map_err(|error| format!("could not decode application artwork: {error}"))?;
    let pixels = u64::from(icon.width) * u64::from(icon.height);
    if icon.width > MAX_ARTWORK_DIMENSION
        || icon.height > MAX_ARTWORK_DIMENSION
        || pixels > MAX_ARTWORK_PIXELS
    {
        return Err(format!(
            "application artwork dimensions {}x{} exceeded the safety limit",
            icon.width, icon.height
        ));
    }
    let width =
        usize::try_from(icon.width).map_err(|error| format!("invalid artwork width: {error}"))?;
    let height =
        usize::try_from(icon.height).map_err(|error| format!("invalid artwork height: {error}"))?;
    Ok(egui::ColorImage::from_rgba_unmultiplied(
        [width, height],
        &icon.rgba,
    ))
}

fn cache_path(config_dir: &Path, key: &ArtworkKey) -> PathBuf {
    config_dir
        .join("app-artwork")
        .join(safe_path_component(&key.host_id))
        .join(format!("{}.png", key.application_id))
}

fn safe_path_component(value: &str) -> String {
    let sanitized = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .take(128)
        .collect::<String>();
    if sanitized.is_empty() {
        "unknown-host".to_owned()
    } else {
        sanitized
    }
}

fn persist(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "artwork cache path has no parent".to_owned())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("could not create artwork cache directory: {error}"))?;
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    fs::write(&temporary, bytes)
        .map_err(|error| format!("could not write artwork cache: {error}"))?;
    fs::rename(&temporary, path).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        format!("could not publish artwork cache entry: {error}")
    })
}

#[cfg(test)]
mod tests {
    use super::{ArtworkKey, MAX_ARTWORK_BYTES, cache_path, decode, safe_path_component};
    use std::path::Path;

    #[test]
    fn cache_paths_cannot_escape_the_cache_root() {
        let key = ArtworkKey::new("../../Apollo:Living Room", 12);
        assert_eq!(
            cache_path(Path::new("config"), &key),
            Path::new("config")
                .join("app-artwork")
                .join("______Apollo_Living_Room")
                .join("12.png")
        );
    }

    #[test]
    fn empty_host_identifier_has_stable_fallback() {
        assert_eq!(safe_path_component(""), "unknown-host");
    }

    #[test]
    fn rejects_non_png_and_oversized_payloads() {
        assert!(decode(b"not an image").is_err());
        assert!(decode(&vec![0; MAX_ARTWORK_BYTES + 1]).is_err());
    }
}
