//! Drop the recipes whose exact builds are already published.
//!
//! A package is built when a build with the same name, version and build string
//! (variant hash + build number) is on one of the `skip_existing` channels. The
//! recipes are rendered with rattler-build's own crates against the variant
//! config to get those build strings, as built natively on the target platform.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result, anyhow};
use rattler_build_recipe::RenderConfig;
use rattler_conda_types::Subdir;
use rayon::prelude::*;
use serde::Deserialize;

pub type Build = (String, String, String);

#[derive(Deserialize)]
struct Record {
    name: String,
    version: String,
    build: String,
    #[serde(default)]
    depends: Vec<String>,
}

#[derive(Deserialize, Default)]
struct Repodata {
    #[serde(default)]
    packages: HashMap<String, Record>,
    #[serde(default, rename = "packages.conda")]
    packages_conda: HashMap<String, Record>,
}

/// ROS 2 packages share their names across distributions (ros2-*): only builds
/// for this distribution (its mutex, or the mutex's build string) count.
fn built_for_distro(r: &Record, distro: &str) -> bool {
    if r.name.ends_with("distro-mutex") {
        return r.build.starts_with(&format!("{distro}_"));
    }
    let mutexes: Vec<&String> = r
        .depends
        .iter()
        .filter(|d| d.split(' ').next().unwrap_or_default().ends_with("distro-mutex"))
        .collect();
    mutexes.is_empty()
        || mutexes
            .iter()
            .any(|d| d.contains(&format!(" {distro}_")) || d.ends_with(&format!(" {distro}")))
}

/// (name, version, build string) of every build of `distro` on the channels.
pub fn published(channels: &[String], platform: &str, distro: &str) -> Result<HashSet<Build>> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("vinca-rs")
        .build()?;
    let mut out = HashSet::new();
    for channel in channels {
        for subdir in [platform, "noarch"] {
            let url = format!("{}/{subdir}/repodata.json", channel.trim_end_matches('/'));
            let resp = client.get(&url).send().with_context(|| url.clone())?;
            if resp.status() == reqwest::StatusCode::NOT_FOUND {
                continue;
            }
            let repodata: Repodata = resp.error_for_status()?.json().with_context(|| url.clone())?;
            out.extend(
                repodata
                    .packages
                    .into_values()
                    .chain(repodata.packages_conda.into_values())
                    .filter(|r| built_for_distro(r, distro))
                    .map(|r| (r.name, r.version, r.build)),
            );
        }
    }
    Ok(out)
}

/// Every output (variant) a recipe builds.
pub fn render(recipe: &Path, platform: &str, variant_config: Option<&Path>) -> Result<Vec<Build>> {
    let subdir = Subdir::from_str(platform).map_err(|e| anyhow!("{platform}: {e}"))?;
    let config = RenderConfig {
        target_platform: subdir,
        build_platform: subdir,
        host_platform: subdir,
        ..RenderConfig::default()
    };
    let files: Vec<PathBuf> = variant_config.into_iter().map(Path::to_path_buf).collect();
    let variants = rattler_build_recipe::render_recipe_with_variants(recipe, &files, Some(config))
        .map_err(|e| anyhow!("rendering {}: {e}", recipe.display()))?;
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
    let dirs: Vec<PathBuf> = std::fs::read_dir(recipes)?
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
            std::fs::remove_dir_all(&dir)?;
            removed.insert(dir.file_name().unwrap().to_string_lossy().to_string());
        }
    }
    Ok(removed)
}
