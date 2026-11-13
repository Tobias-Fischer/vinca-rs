//! `vinca-rs solve --serve`: solve the host environments of recipes, one request per
//! line, with the repodata loaded once and kept in memory.
//!
//! Each request names a recipe (and optionally a glibc for its virtual packages).
//! The recipe is rendered with rattler-build's crates against the variant config, a
//! bare host requirement gets its variant pin (as rattler-build's `apply_variant`), and
//! the host specs of every variant are solved with rattler's solver: the same answer
//! as `rattler-build build --render-only --with-solve`, without a process, a render
//! of the channels' repodata and a solver set-up per question. The response is
//! `{"ok": bool, "text": "<the solver's explanation when not ok>"}`.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use miette::Diagnostic;
use rattler_build_recipe::stage1::requirements::Dependency;
use rattler_build_recipe::{RenderConfig, variant_render::RenderError};
use rattler_conda_types::{
    Channel, MatchSpec, NamelessMatchSpec, ParseChannelError, ParseStrictness, ParseSubdirError, Subdir,
};
use rattler_repodata_gateway::{Gateway, GatewayError};
use rattler_solve::{ChannelPriority, SolverImpl, SolverTask, resolvo};
use rattler_virtual_packages::{
    DetectVirtualPackageError, Override, VirtualPackageOverrides, VirtualPackages,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error, Diagnostic)]
pub enum Error {
    #[error(transparent)]
    Platform(#[from] ParseSubdirError),
    #[error("invalid channel {name}")]
    Channel { name: String, source: ParseChannelError },
    #[error("failed to start the async runtime")]
    Runtime(#[source] std::io::Error),
    #[error("invalid request {line}")]
    Request { line: String, source: serde_json::Error },
    #[error("failed to render {}", path.display())]
    Render { path: PathBuf, source: RenderError },
    #[error("failed to detect the virtual packages")]
    VirtualPackages(#[from] DetectVirtualPackageError),
    #[error("failed to fetch the repodata")]
    Gateway(#[from] GatewayError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Deserialize)]
struct Request {
    recipe: PathBuf,
    glibc: Option<String>,
}

#[derive(Serialize)]
struct Response {
    ok: bool,
    text: String,
}

/// `name` alone gets the variant's value, as rattler-build does for host specs.
fn apply_variant(spec: &MatchSpec, variant: &[(String, String)]) -> MatchSpec {
    let (Some(name), None, None) = (spec.name.as_exact(), &spec.version, &spec.build) else {
        return spec.clone();
    };
    // variant keys are normalized: `-`, `_` and `.` are all `_` (llvm-openmp: llvm_openmp)
    let key: String =
        name.as_normalized().chars().map(|c| if matches!(c, '-' | '.') { '_' } else { c }).collect();
    let Some(value) = variant.iter().find(|(k, _)| *k == key).map(|(_, v)| v.clone()) else {
        return spec.clone();
    };
    let value =
        if value.chars().all(|c| c.is_alphanumeric() || c == '.') { format!("={value}") } else { value };
    NamelessMatchSpec::from_str(&value, ParseStrictness::Lenient)
        .map(|nameless| MatchSpec::from_nameless(nameless, spec.name.clone()))
        .unwrap_or_else(|_| spec.clone())
}

struct Server {
    platform: Subdir,
    channels: Vec<Channel>,
    variant_config: PathBuf,
    gateway: Gateway,
    runtime: tokio::runtime::Runtime,
}

impl Server {
    /// None when every variant's host environment solves, else the explanation.
    fn solve(&self, recipe: &Path, glibc: Option<String>) -> Result<Option<String>> {
        let config = RenderConfig {
            target_platform: self.platform,
            build_platform: self.platform,
            host_platform: self.platform,
            ..RenderConfig::default()
        };
        let variants =
            rattler_build_recipe::render_recipe_with_variants(recipe, &[&self.variant_config], Some(config))
                .map_err(|source| Error::Render { path: recipe.into(), source })?;
        let mut overrides = VirtualPackageOverrides::from_env();
        if let Some(glibc) = glibc {
            overrides.libc = Some(Override::String(glibc));
        }
        let virtual_packages: Vec<_> = VirtualPackages::detect_for_platform(self.platform, &overrides, None)?
            .into_generic_virtual_packages()
            .collect();
        for v in variants {
            let variant: Vec<(String, String)> =
                v.variant.iter().map(|(k, value)| (k.normalize(), value.to_string())).collect();
            let specs: Vec<MatchSpec> = v
                .recipe
                .requirements
                .host
                .iter()
                .filter_map(|d| match d {
                    Dependency::Spec(m) => Some(apply_variant(m, &variant)),
                    _ => None,
                })
                .collect();
            let repodata = self.runtime.block_on(async {
                self.gateway
                    .query(self.channels.clone(), [self.platform, Subdir::NoArch], specs.clone())
                    .recursive(true)
                    .await
            })?;
            let task = SolverTask {
                virtual_packages: virtual_packages.clone(),
                specs,
                channel_priority: ChannelPriority::Disabled,
                ..SolverTask::from_iter(&repodata.repodata)
            };
            if let Err(e) = resolvo::Solver.solve(task) {
                return Ok(Some(e.to_string()));
            }
        }
        Ok(None)
    }
}

pub fn serve(platform: &str, channels: &[String], variant_config: PathBuf) -> Result<()> {
    let server = Server {
        platform: Subdir::from_str(platform)?,
        // as rattler-build's -c: a name is a channel on conda.anaconda.org
        channels: {
            let config = rattler_conda_types::ChannelConfig::default_with_root_dir(std::env::current_dir()?);
            channels
                .iter()
                .map(|c| {
                    Channel::from_str(c, &config).map_err(|source| Error::Channel { name: c.clone(), source })
                })
                .collect::<Result<_>>()?
        },
        variant_config,
        gateway: Gateway::new(),
        runtime: tokio::runtime::Builder::new_multi_thread().enable_all().build().map_err(Error::Runtime)?,
    };
    let stdout = std::io::stdout();
    for line in std::io::stdin().lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let request: Request =
            serde_json::from_str(&line).map_err(|source| Error::Request { line: line.clone(), source })?;
        let response = match server.solve(&request.recipe, request.glibc) {
            Ok(None) => Response { ok: true, text: String::new() },
            Ok(Some(explanation)) => Response { ok: false, text: explanation },
            Err(e) => Response { ok: false, text: crate::chain(&e) },
        };
        let mut out = stdout.lock();
        writeln!(out, "{}", serde_json::to_string(&response)?)?;
        out.flush()?;
    }
    Ok(())
}
