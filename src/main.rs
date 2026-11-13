//! vinca-rs: rattler-build recipes and the CI workflow building them, for a ROS
//! distribution (RoboStack).

mod config;
mod distro;
mod generate;
mod license;
mod recipe;
mod selectors;
mod skip_existing;
mod workflow;

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Generate the recipes of a distribution for one target platform
    Generate {
        /// the vinca.yaml to generate from
        #[arg(long, default_value = "vinca.yaml")]
        config: PathBuf,
        /// conda subdir to generate for
        #[arg(long)]
        platform: String,
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
    /// Generate the GitHub Actions workflow that builds a directory of recipes
    Workflow {
        #[arg(long, default_value = "recipes")]
        recipes: PathBuf,
        #[arg(long)]
        platform: String,
        /// workflow template: its jobs.build is copied for every job
        #[arg(long)]
        template: PathBuf,
        #[arg(long)]
        out: PathBuf,
        /// at most this many recipes per job
        #[arg(long, default_value_t = 25)]
        max_batch: usize,
        /// measured build times: {"recipes": {name: seconds}, "default", "job_overhead", "runners"}
        #[arg(long)]
        timings: Option<PathBuf>,
        /// template variables, name=value: @name@ in the template
        #[arg(long = "var", value_parser = parse_var)]
        vars: Vec<(String, String)>,
    },
}

fn parse_var(s: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .ok_or_else(|| format!("{s}: expected name=value"))
}

fn generate(
    config: PathBuf,
    platform: String,
    out: PathBuf,
    variant_config: PathBuf,
    all: bool,
) -> Result<()> {
    let t0 = Instant::now();
    let conf = config::read(&config, &platform)?;
    let distro = distro::Distro::load(&conf)?;
    eprintln!("config + rosdistro cache: {:.1?}", t0.elapsed());

    let mut g = generate::Generator::new(&conf, &distro);
    let selection = g.select()?;
    let ros_base_names = distro.depends("ros_base", &HashSet::new())?;
    let fetched = distro.prefetch(selection.packages.iter().chain(ros_base_names.iter()))?;
    eprintln!(
        "{} packages selected; {fetched} package.xml files fetched",
        selection.packages.len()
    );
    let ros_base: Vec<_> = ros_base_names
        .iter()
        .filter_map(|p| distro.package(p).ok())
        .collect();

    if out.exists() {
        std::fs::remove_dir_all(&out).with_context(|| out.display().to_string())?;
    }
    std::fs::create_dir_all(&out)?;
    let mut n = 0;
    for ros in &selection.packages {
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
        eprintln!(
            "unsatisfied: {}",
            g.unsatisfied.iter().cloned().collect::<Vec<_>>().join(", ")
        );
    }

    if !all && !conf.skip_existing.is_empty() {
        let published = skip_existing::published(&conf.skip_existing, &platform, &conf.ros_distro)?;
        let variants = variant_config.is_file().then_some(variant_config.as_path());
        let removed = skip_existing::remove_built(&out, &platform, variants, &published)?;
        eprintln!(
            "{} recipes already published, {} left to build ({:.1?})",
            removed.len(),
            n - removed.len(),
            t0.elapsed()
        );
    }
    Ok(())
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Generate {
            config,
            platform,
            out,
            variant_config,
            all,
        } => generate(config, platform, out, variant_config, all),
        Command::Workflow {
            recipes,
            platform,
            template,
            out,
            max_batch,
            timings,
            vars,
        } => {
            let timings = match timings {
                Some(p) => serde_json::from_str(&std::fs::read_to_string(&p)?)
                    .with_context(|| p.display().to_string())?,
                None => workflow::Timings::default(),
            };
            let opts = workflow::Options {
                platform,
                max_batch,
                timings,
                vars,
            };
            if !workflow::generate(&recipes, &template, &out, &opts)? {
                eprintln!("nothing to build");
            }
            Ok(())
        }
    }
}
