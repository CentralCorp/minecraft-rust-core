pub mod forge;
pub mod meta_loader;
pub mod neoforge;
pub mod patcher;

use std::path::Path;

use crate::bundle::BundleManager;
use crate::config::{LaunchOptions, LoaderType};
use crate::downloader::Downloader;
use crate::error::{CoreError, CoreResult};
use crate::events::EventCallback;
use crate::minecraft::arguments::LoaderProfileData;

/// Installs or verifies the configured mod loader (`Forge`, `NeoForge`, `Fabric`, `LegacyFabric`,
/// or `Quilt`) inside the isolated `<path>/loader/<slug_or_build>` directory and returns
/// [`LoaderProfileData`] to merge into the launch classpath & arguments.
pub async fn install_loader(
    options: &LaunchOptions,
    mc_version_id: &str,
    java_bin: &Path,
    downloader: &Downloader,
    bundle_manager: &BundleManager,
    on_event: &EventCallback,
) -> CoreResult<Option<LoaderProfileData>> {
    if !options.loader.enable {
        return Ok(None);
    }

    let loader_type = options.loader.loader_type.ok_or_else(|| {
        CoreError::InvalidConfig(
            "options.loader.enable is true, but options.loader.loader_type is None".to_string(),
        )
    })?;

    let profile = match loader_type {
        LoaderType::Forge => {
            forge::install_forge(
                options,
                mc_version_id,
                java_bin,
                downloader,
                bundle_manager,
                on_event,
            )
            .await?
        }
        LoaderType::NeoForge => {
            neoforge::install_neoforge(
                options,
                mc_version_id,
                java_bin,
                downloader,
                bundle_manager,
                on_event,
            )
            .await?
        }
        LoaderType::Fabric | LoaderType::LegacyFabric | LoaderType::Quilt => {
            meta_loader::install_meta_loader(
                loader_type,
                options,
                mc_version_id,
                downloader,
                bundle_manager,
                on_event,
            )
            .await?
        }
    };

    Ok(Some(profile))
}
