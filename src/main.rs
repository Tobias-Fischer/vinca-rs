//! vinca-rs: rattler-build recipes and the CI workflow building them, for a ROS
//! distribution (RoboStack).

mod config;
mod distro;
mod generate;
mod license;
mod recipe;
mod selectors;
mod skip_existing;
mod snapshot;
mod workflow;

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use pixi_build_ros::package_xml::PackageXml;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// What `generate` and `requirements` work on.
#[derive(Args)]
struct Target {
    /// the vinca.yaml to generate from
    #[arg(long, default_value = "vinca.yaml")]
    config: PathBuf,
    /// conda subdir to generate for
    #[arg(long)]
    platform: String,
}

#[derive(Subcommand)]
enum Command {
    /// Generate the recipes of a distribution for one target platform
    Generate {
        #[command(flatten)]
        target: Target,
        /// where the recipes go (one directory per recipe; emptied first)
        #[arg(long, default_value = "recipes")]
        out: PathBuf,
        /// variant config the recipes are built with (to compare build strings
        /// with the skip_existing channels)
        #[arg(long, default_value = "conda_build_config.yaml")]
        variant_config: PathBuf,
        /// generate every recipe, also those already on the skip_existing channels
        #[arg(long)]
        all: bool,
    },
    /// Generate the CI jobs that build a directory of recipes: a GitHub Actions
    /// workflow (from a template) and/or the jobs as JSON
    Workflow {
        #[arg(long, default_value = "recipes")]
        recipes: PathBuf,
        #[arg(long)]
        platform: String,
        /// workflow template: its jobs.build is copied for every job
        #[arg(long)]
        template: Option<PathBuf>,
        /// the GitHub Actions workflow to write
        #[arg(long)]
        out: Option<PathBuf>,
        /// the jobs as JSON: {"stages": [[{"id", "name", "recipes", "needs"}]]}
        #[arg(long)]
        json: Option<PathBuf>,
        /// at most this many recipes per job
        #[arg(long, default_value_t = 25)]
        max_batch: usize,
        /// measured build times: {"recipes": {name: seconds}, "default", "job_overhead", "runners"}
        #[arg(long)]
        timings: Option<PathBuf>,
        /// template variables, name=value: @name@ in the template
        #[arg(long = "var", value_parser = parse_var)]
        vars: Vec<(String, String)>,
        /// the CI runs each stage after the whole previous one (e.g. fixed matrix
        /// slots), not each job after its needs: schedule for that
        #[arg(long)]
        stage_barriers: bool,
    },
    /// Pin a distribution's releases and their dependencies (rosdistro_snapshot.yaml)
    Snapshot {
        #[arg(long, short)]
        distro: String,
        /// a git ref of ros/rosdistro, e.g. a sync tag such as `jazzy/2026-10-05`
        /// (default: the published cache, the latest state)
        #[arg(long)]
        rosdistro_ref: Option<String>,
        /// only this package and its dependencies
        #[arg(long, short)]
        package: Option<String>,
        #[arg(long, short, default_value = "rosdistro_snapshot.yaml")]
        out: PathBuf,
        /// don't snapshot again: add the dependencies to the entries of `out` that
        /// have none, from their pinned releases (versions stay as they are)
        #[arg(long)]
        fill_dependencies: bool,
    },
    /// The requirements of every selected package for a platform, as JSON
    Requirements {
        #[command(flatten)]
        target: Target,
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

fn parse_var(s: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .ok_or_else(|| format!("{s}: expected name=value"))
}

fn write_json(value: &serde_json::Value, out: Option<&Path>) -> Result<()> {
    let text = serde_json::to_string_pretty(value)?;
    match out {
        Some(p) => std::fs::write(p, text).with_context(|| p.display().to_string()),
        None => {
            println!("{text}");
            Ok(())
        }
    }
}

/// The configuration and the distribution it builds.
fn load(t: &Target) -> Result<(config::Config, distro::Distro)> {
    let conf = config::read(&t.config, &t.platform)?;
    let distro = distro::Distro::load(&conf)?;
    Ok((conf, distro))
}

/// The selected packages, with every package.xml they need fetched, and
/// ros_base's packages (for `<group_depend>`).
fn select(g: &generate::Generator) -> Result<(Vec<String>, Vec<PackageXml>)> {
    let packages = g.select()?;
    let ros_base_names = g.distro.depends("ros_base", &HashSet::new())?;
    let fetched = g.distro.prefetch(packages.iter().chain(ros_base_names.iter()))?;
    eprintln!("{} packages selected; {fetched} package.xml files fetched", packages.len());
    let ros_base = ros_base_names.iter().filter_map(|p| g.distro.package(p).ok()).collect();
    Ok((packages, ros_base))
}

fn generate_cmd(t: Target, out: PathBuf, variant_config: PathBuf, all: bool) -> Result<()> {
    let t0 = Instant::now();
    let (conf, distro) = load(&t)?;
    let mut g = generate::Generator::new(&conf, &distro);
    let (packages, ros_base) = select(&g)?;

    if out.exists() {
        std::fs::remove_dir_all(&out).with_context(|| out.display().to_string())?;
    }
    std::fs::create_dir_all(&out)?;
    let mut n = 0;
    for ros in &packages {
        let Some(dir) = g.package(ros, &ros_base)? else {
            continue;
        };
        dir.write(&out)?;
        n += 1;
        if let Some(alias) = g.alias(ros, &dir.recipe) {
            alias.write(&out)?;
            n += 1;
        }
    }
    if let Some(m) = g.mutex() {
        m.write(&out)?;
        n += 1;
    }
    eprintln!("{n} recipes generated in {:.1?}", t0.elapsed());
    if !g.unsatisfied.is_empty() {
        eprintln!("unsatisfied: {}", g.unsatisfied.iter().cloned().collect::<Vec<_>>().join(", "));
    }
    if !all && !conf.skip_existing.is_empty() {
        // the generated recipes' names (one directory per recipe)
        let names: Vec<String> = std::fs::read_dir(&out)?
            .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().to_string()))
            .collect();
        let published = skip_existing::published(&conf.skip_existing, &t.platform, &conf.ros_distro, &names)?;
        let variants = variant_config.is_file().then_some(variant_config.as_path());
        let removed = skip_existing::remove_built(&out, &t.platform, variants, &published)?;
        eprintln!(
            "{} recipes already published, {} left to build ({:.1?})",
            removed.len(),
            n - removed.len(),
            t0.elapsed()
        );
    }
    Ok(())
}

fn requirements_cmd(t: Target, out: Option<PathBuf>) -> Result<()> {
    let (conf, distro) = load(&t)?;
    let mut g = generate::Generator::new(&conf, &distro);
    let (selected, ros_base) = select(&g)?;
    let mut packages = BTreeMap::new();
    for ros in &selected {
        if let Some(dir) = g.package(ros, &ros_base)? {
            packages.insert(ros.clone(), serde_json::to_value(&dir.recipe.requirements)?);
        }
    }
    write_json(
        &serde_json::json!({
            "platform": t.platform,
            "prefix": g.prefix(),
            "packages": packages,
            "run_constraints": conf.mutex_package.as_ref().map(|m| m.run_constraints.clone()).unwrap_or_default(),
        }),
        out.as_deref(),
    )
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Generate { target, out, variant_config, all } => {
            generate_cmd(target, out, variant_config, all)
        }
        Command::Workflow {
            recipes,
            platform,
            template,
            out,
            json,
            max_batch,
            timings,
            vars,
            stage_barriers,
        } => {
            if out.is_none() && json.is_none() {
                bail!("nothing to write: give --out (with --template) and/or --json");
            }
            let timings = match timings {
                Some(p) => serde_json::from_str(&std::fs::read_to_string(&p)?)
                    .with_context(|| p.display().to_string())?,
                None => workflow::Timings::default(),
            };
            let opts =
                workflow::Options { platform, max_batch, timings, vars, stage_barriers, template, out, json };
            if !workflow::generate(&recipes, &opts)? {
                eprintln!("nothing to build");
                if let Some(j) = opts.json {
                    std::fs::write(j, r#"{"stages": []}"#)?;
                }
            }
            Ok(())
        }
        Command::Snapshot { distro, rosdistro_ref, package, out, fill_dependencies } => {
            if fill_dependencies {
                snapshot::fill_dependencies(&distro, &out)
            } else {
                snapshot::write(&distro, rosdistro_ref.as_deref(), package.as_deref(), &out)
            }
        }
        Command::Requirements { target, out } => requirements_cmd(target, out),
    }
}
