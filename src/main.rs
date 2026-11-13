//! vinca-rs: rattler-build recipes and the CI workflow building them, for a ROS
//! distribution (RoboStack).

mod config;
mod distro;
mod generate;
#[cfg(test)]
mod golden;
mod license;
mod recipe;
mod selectors;
mod skip_existing;
mod snapshot;
mod solve;
mod workflow;

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use clap::{Args, Parser, Subcommand};
use fs_err as fs;
use miette::Diagnostic;
use pixi_build_ros::package_xml::PackageXml;
use rattler_conda_types::Subdir;
use thiserror::Error;

#[derive(Debug, Error, Diagnostic)]
enum Error {
    #[error("nothing to write")]
    #[diagnostic(help("give --out (with --template) and/or --json"))]
    NothingToWrite,
    #[error("invalid timings {}", path.display())]
    Timings { path: PathBuf, source: serde_json::Error },
    #[error("{} is not a recipes directory: it has {}", dir.display(), entry.display())]
    #[diagnostic(help(
        "--out is emptied first, so it must be empty or hold only recipe directories \
         (each with a recipe.yaml); give another --out or remove it yourself"
    ))]
    NotRecipesDir { dir: PathBuf, entry: PathBuf },
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Config(#[from] config::Error),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Distro(#[from] distro::Error),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Generate(#[from] generate::Error),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Recipe(#[from] recipe::Error),
    #[error(transparent)]
    #[diagnostic(transparent)]
    SkipExisting(#[from] skip_existing::Error),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Snapshot(#[from] snapshot::Error),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Solve(#[from] solve::Error),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Workflow(#[from] workflow::Error),
}

type Result<T, E = Error> = std::result::Result<T, E>;

/// An error and its causes, as one line (a cause that its error's message already
/// ends with only once).
fn chain(e: &dyn std::error::Error) -> String {
    let mut s = e.to_string();
    for cause in std::iter::successors(e.source(), |c| c.source()).map(ToString::to_string) {
        if !s.ends_with(&cause) {
            s = format!("{s}: {cause}");
        }
    }
    s
}

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
    platform: Subdir,
}

#[derive(Subcommand)]
enum Command {
    /// Generate the recipes of a distribution for one target platform
    Generate {
        #[command(flatten)]
        target: Target,
        /// where the recipes go (one directory per recipe; emptied first, which only
        /// a directory of recipe directories may be)
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
        platform: Subdir,
        /// workflow template: its jobs.build is copied for every job
        #[arg(long)]
        template: Option<PathBuf>,
        /// the GitHub Actions workflow to write
        #[arg(long, requires = "template")]
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
    /// Solve recipes' host environments, one JSON request per stdin line
    /// (`{"recipe": path, "glibc": optional}`), with the repodata kept in memory:
    /// `{"ok", "text"}` per line, as `rattler-build --render-only --with-solve` decides,
    /// with `"error"` when a request couldn't be answered (e.g. invalid, or no repodata)
    Solve {
        #[arg(long)]
        platform: Subdir,
        /// channels, as rattler-build's -c
        #[arg(long = "channel", short = 'c', required = true)]
        channels: Vec<String>,
        #[arg(long, default_value = "conda_build_config.yaml")]
        variant_config: PathBuf,
    },
}

fn parse_var(s: &str) -> std::result::Result<(String, String), String> {
    s.split_once('=')
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .ok_or_else(|| format!("{s}: expected name=value"))
}

fn write_json(value: &serde_json::Value, out: Option<&Path>) -> Result<()> {
    let text = serde_json::to_string_pretty(value)?;
    match out {
        Some(p) => fs::write(p, text)?,
        None => println!("{text}"),
    }
    Ok(())
}

/// The configuration and the distribution it builds.
fn load(t: &Target) -> Result<(config::Config, distro::Distro)> {
    let conf = config::read(&t.config, t.platform)?;
    let distro = distro::Distro::load(&conf)?;
    Ok((conf, distro))
}

/// The selected packages, with every package.xml they need fetched, and
/// ros_base's packages (for `<group_depend>`).
fn select(g: &generate::Generator) -> Result<(Vec<String>, Vec<PackageXml>)> {
    let packages = g.select()?;
    // the distribution's packages, whether selected for this platform or not
    let unmatched = g.conf.unmatched(|p| g.distro.has(p));
    if !unmatched.is_empty() {
        eprintln!("warning: entries for packages not in the distribution: {}", unmatched.join(", "));
    }
    let ros_base_names = g.distro.depends("ros_base", &HashSet::new())?;
    let fetched = g.distro.prefetch(packages.iter().chain(ros_base_names.iter()))?;
    eprintln!("{} packages selected; {fetched} package.xml files fetched", packages.len());
    let ros_base = ros_base_names.iter().filter_map(|p| g.distro.package(p).ok()).collect();
    Ok((packages, ros_base))
}

/// Empty `out` for the new recipes, if it is a recipes directory (only
/// subdirectories with a recipe.yaml): never anything else, e.g. `--out .`.
fn clear_recipes_dir(out: &Path) -> Result<()> {
    if !out.exists() {
        return Ok(());
    }
    for e in fs::read_dir(out)? {
        let path = e?.path();
        if !path.join("recipe.yaml").is_file() {
            return Err(Error::NotRecipesDir { dir: out.into(), entry: path });
        }
    }
    Ok(fs::remove_dir_all(out)?)
}

/// Every recipe of the distribution, in `out` (emptied first); how many.
fn write_recipes(g: &mut generate::Generator, out: &Path) -> Result<usize> {
    let (packages, ros_base) = select(g)?;
    clear_recipes_dir(out)?;
    fs::create_dir_all(out)?;
    let mut n = 0;
    for ros in &packages {
        let Some(dir) = g.package(ros, &ros_base)? else {
            continue;
        };
        dir.write(out)?;
        n += 1;
        if let Some(alias) = g.alias(ros, &dir.recipe) {
            alias.write(out)?;
            n += 1;
        }
    }
    if let Some(m) = g.mutex() {
        m.write(out)?;
        n += 1;
    }
    Ok(n)
}

fn generate_cmd(t: Target, out: PathBuf, variant_config: PathBuf, all: bool) -> Result<()> {
    let t0 = Instant::now();
    let (conf, distro) = load(&t)?;
    let mut g = generate::Generator::new(&conf, &distro);
    let n = write_recipes(&mut g, &out)?;
    eprintln!("{n} recipes generated in {:.1?}", t0.elapsed());
    if !g.unsatisfied.is_empty() {
        eprintln!("unsatisfied: {}", g.unsatisfied.iter().cloned().collect::<Vec<_>>().join(", "));
    }
    if !all && !conf.skip_existing.is_empty() {
        // the generated recipes' names (one directory per recipe)
        let names: Vec<String> = fs::read_dir(&out)?
            .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().to_string()))
            .collect();
        let mutex = conf.mutex_package.as_ref().map(|m| m.name.as_str());
        let published =
            skip_existing::published(&conf.skip_existing, t.platform, &conf.ros_distro, mutex, &names)?;
        let variants = variant_config.is_file().then_some(variant_config.as_path());
        let removed = skip_existing::remove_built(&out, t.platform, variants, &published)?;
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

fn main() -> miette::Result<()> {
    Ok(run(Cli::parse().command)?)
}

fn run(command: Command) -> Result<()> {
    match command {
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
                return Err(Error::NothingToWrite);
            }
            let timings = match timings {
                Some(path) => {
                    let text = fs::read_to_string(&path)?;
                    serde_json::from_str(&text).map_err(|source| Error::Timings { path, source })?
                }
                None => workflow::Timings::default(),
            };
            let template = template.zip(out);
            let opts =
                workflow::Options { platform, max_batch, timings, vars, stage_barriers, template, json };
            if !workflow::generate(&recipes, &opts)? {
                eprintln!("nothing to build");
                if let Some(j) = opts.json {
                    fs::write(j, r#"{"stages": []}"#)?;
                }
            }
            Ok(())
        }
        Command::Snapshot { distro, rosdistro_ref, package, out, fill_dependencies } => {
            if fill_dependencies {
                Ok(snapshot::fill_dependencies(&distro, &out)?)
            } else {
                Ok(snapshot::write(&distro, rosdistro_ref.as_deref(), package.as_deref(), &out)?)
            }
        }
        Command::Requirements { target, out } => requirements_cmd(target, out),
        Command::Solve { platform, channels, variant_config } => {
            Ok(solve::serve(platform, &channels, variant_config)?)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_recipes_directory_is_emptied() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("recipes");
        clear_recipes_dir(&out).unwrap(); // missing: nothing to do
        fs::create_dir_all(out.join("ros-x")).unwrap();
        fs::write(out.join("ros-x/recipe.yaml"), "").unwrap();
        clear_recipes_dir(&out).unwrap();
        assert!(!out.exists());

        fs::create_dir_all(out.join("ros-x")).unwrap();
        fs::write(out.join("ros-x/recipe.yaml"), "").unwrap();
        fs::write(out.join("README.md"), "").unwrap();
        assert!(matches!(clear_recipes_dir(&out), Err(Error::NotRecipesDir { .. })));
        assert!(out.join("ros-x/recipe.yaml").exists());
    }
}
