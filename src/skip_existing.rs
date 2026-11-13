//! Drop the recipes whose exact builds are already published.
//!
//! A package is built when a build with the same name, version and build string
//! (variant hash + build number) is on one of the `skip_existing` channels. The
//! recipes are rendered with rattler-build's own crates against the variant
//! config to get those build strings, as built natively on the target platform.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use fs_err as fs;
use miette::Diagnostic;
use rattler_build_recipe::{RenderConfig, variant_render::RenderError};
use rattler_conda_types::{PackageRecord, ParseSubdirError, Subdir};
use rattler_repodata_gateway::GatewayError;
use rayon::prelude::*;
use thiserror::Error;

#[derive(Debug, Error, Diagnostic)]
pub enum Error {
    #[error(transparent)]
    Platform(#[from] ParseSubdirError),
    #[error("invalid channel url {url}")]
    Channel { url: String, source: url::ParseError },
    #[error("failed to start the async runtime")]
    Runtime(#[source] std::io::Error),
    #[error("failed to fetch the published packages")]
    Gateway(#[from] GatewayError),
    #[error("failed to render {}", path.display())]
    Render { path: PathBuf, source: RenderError },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

type Result<T, E = Error> = std::result::Result<T, E>;

pub type Build = (String, String, String);

/// ROS 2 packages share their names across distributions (ros2-*): only builds
/// for this distribution (its mutex, or the mutex's build string) count.
fn built_for_distro(r: &PackageRecord, distro: &str) -> bool {
    if r.name.as_normalized().ends_with("distro-mutex") {
        return r.build.to_string().starts_with(&format!("{distro}_"));
    }
    let mutexes: Vec<&String> = r
        .depends
        .iter()
        .filter(|d| d.split(' ').next().unwrap_or_default().ends_with("distro-mutex"))
        .collect();
    mutexes.is_empty()
        || mutexes.iter().any(|d| d.contains(&format!(" {distro}_")) || d.ends_with(&format!(" {distro}")))
}

/// (name, version, build string) of the builds of `names` for `distro` on the
/// channels. Fetched with pixi's repodata gateway (rattler): only the packages'
/// shards where the channel is sharded (prefix.dev), else its compressed
/// repodata, in parallel and cached (HTTP caching) in rattler's cache, which
/// pixi and rattler-build share.
pub fn published(
    channels: &[String],
    platform: &str,
    distro: &str,
    names: &[String],
) -> Result<HashSet<Build>> {
    use rattler_conda_types::{Channel, PackageName};
    use rattler_repodata_gateway::Gateway;

    let subdir = Subdir::from_str(platform)?;
    let channels: Vec<Channel> = channels
        .iter()
        .map(|c| {
            let url = url::Url::parse(c).map_err(|source| Error::Channel { url: c.clone(), source })?;
            Ok(Channel::from_url(url))
        })
        .collect::<Result<_>>()?;
    let names: Vec<PackageName> = names.iter().filter_map(|n| PackageName::from_str(n).ok()).collect();
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().map_err(Error::Runtime)?;
    let output = runtime.block_on(async {
        Gateway::new().query(channels, [subdir, Subdir::NoArch], names).recursive(false).await
    })?;
    Ok(output
        .repodata
        .iter()
        .flat_map(|repodata| repodata.iter())
        .map(|r| &r.package_record)
        .filter(|p| built_for_distro(p, distro))
        .map(|p| (p.name.as_normalized().to_string(), p.version.to_string(), p.build.to_string()))
        .collect())
}

/// Every output (variant) a recipe builds.
pub fn render(recipe: &Path, platform: &str, variant_config: Option<&Path>) -> Result<Vec<Build>> {
    let subdir = Subdir::from_str(platform)?;
    let config = RenderConfig {
        target_platform: subdir,
        build_platform: subdir,
        host_platform: subdir,
        ..RenderConfig::default()
    };
    let files: Vec<PathBuf> = variant_config.into_iter().map(Path::to_path_buf).collect();
    let variants = rattler_build_recipe::render_recipe_with_variants(recipe, &files, Some(config))
        .map_err(|source| Error::Render { path: recipe.into(), source })?;
    Ok(variants
        .into_iter()
        .map(|v| {
            let r = v.recipe;
            let build: Option<String> = r.build.string.into();
            (
                r.package.name.as_normalized().to_string(),
                r.package.version.to_string(),
                build.unwrap_or_default(),
            )
        })
        .collect())
}

/// Remove the recipe directories whose every output is published; their names.
pub fn remove_built(
    recipes: &Path,
    platform: &str,
    variant_config: Option<&Path>,
    published: &HashSet<Build>,
) -> Result<BTreeSet<String>> {
    let dirs: Vec<PathBuf> = fs::read_dir(recipes)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join("recipe.yaml").is_file())
        .collect();
    let built: Vec<Result<Option<PathBuf>>> = dirs
        .into_par_iter()
        .map(|dir| {
            let builds = render(&dir.join("recipe.yaml"), platform, variant_config)?;
            Ok((!builds.is_empty() && builds.iter().all(|b| published.contains(b))).then_some(dir))
        })
        .collect();
    let mut removed = BTreeSet::new();
    for dir in built {
        if let Some(dir) = dir? {
            fs::remove_dir_all(&dir)?;
            removed.insert(dir.file_name().unwrap().to_string_lossy().to_string());
        }
    }
    Ok(removed)
}
