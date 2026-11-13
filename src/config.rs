//! Layered `vinca.yaml` (`extends:`) and the files next to each layer.
//!
//! Port of vinca/configuration.py + vinca/schemas.py (RoboStack/vinca#200), typed
//! with serde. Only the keys the RoboStack monorepo uses are accepted: the
//! deprecated ones (`packages_skip_by_deps`, `build_all`, `variants_mode`,
//! string-form `mutex_package`, remote `conda_index`, ...) are errors, as are typos.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use indexmap::{IndexMap, IndexSet};
use serde::{Deserialize, Serialize};
use serde_yaml::{Mapping, Value};

use crate::selectors::evaluate_for_platform;

const PATCH_PLATFORMS: [&str; 4] = ["osx", "linux", "win", "emscripten"];
const PATH_KEYS: [&str; 3] = ["patch_dir", "rosdistro_snapshot", "rosdistro_additional_recipes"];
const LAYERED_LISTS: [&str; 3] = ["packages_select_by_deps", "packages_exclude", "packages_skip"];

// ----------------------------------------------------------------------------
// schemas (vinca/schemas.py)
// ----------------------------------------------------------------------------

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PackageNameMode {
    #[default]
    Legacy,
    Both,
    New,
}

#[derive(Debug, Deserialize, Clone, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MutexPackage {
    pub name: String,
    pub version: String,
    pub upper_bound: String,
    pub build_number: Option<u64>,
    #[serde(default)]
    pub run_constraints: Vec<String>,
}

#[derive(Debug, Deserialize, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct DepMods {
    #[serde(default)]
    pub add_build: Vec<Value>,
    #[serde(default)]
    pub add_host: Vec<Value>,
    #[serde(default)]
    pub add_run: Vec<Value>,
    #[serde(default)]
    pub remove_build: Vec<Value>,
    #[serde(default)]
    pub remove_host: Vec<Value>,
    #[serde(default)]
    pub remove_run: Vec<Value>,
}

#[derive(Debug, Deserialize, Clone, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DummyPackage {
    pub dep_name: String,
    pub upper_bound: Option<String>,
    pub max_pin: Option<String>,
    pub override_version: Option<String>,
}

#[derive(Debug, Deserialize, Clone, Default, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PkgAdditionalInfo {
    #[serde(default)]
    pub additional_cmake_args: Option<String>,
    pub build_number: Option<u64>,
    pub generate_dummy_package_with_run_deps: Option<DummyPackage>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct SnapshotEntry {
    pub url: String,
    pub version: String,
    pub tag: Option<String>,
    pub rev: Option<String>,
    /// None: a snapshot written before vinca-snapshot recorded dependencies
    pub dependencies: Option<Vec<String>>,
    pub repository: Option<String>,
    #[serde(default)]
    pub additional_folder: String,
    /// rosdistro_additional_recipes.yaml: the package.xml's file name, when the
    /// repository stores it under another (e.g. package_ROS2.xml)
    pub package_xml_name: Option<String>,
    /// rosdistro_additional_recipes.yaml: whether to clone git submodules
    /// (rattler-build's default: yes)
    pub submodules: Option<bool>,
    /// rosdistro_additional_recipes.yaml: why this entry replaces the
    /// snapshot's release of the same package (required when it does)
    #[serde(rename = "override")]
    pub override_reason: Option<String>,
}

/// `robostack:` of a conda_index entry: for every platform, or per platform
/// (`linux`, `osx`, `win64`, `emscripten` or a conda subdir). Other keys (rosdep
/// systems) are ignored.
#[derive(Debug, Deserialize, Clone)]
pub struct CondaIndexEntry {
    pub robostack: Option<PlatformList>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub enum PlatformList {
    All(Vec<String>),
    PerPlatform(BTreeMap<String, Option<Vec<String>>>),
}

pub type CondaIndex = HashMap<String, CondaIndexEntry>;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct VincaYaml {
    ros_distro: String,
    build_number: Option<u64>,
    #[serde(default)]
    package_name_mode: PackageNameMode,
    // validated here, read per layer (relative to each file)
    #[serde(default)]
    #[allow(dead_code)]
    conda_index: Vec<String>,
    #[allow(dead_code)]
    patch_dir: Option<String>,
    rosdistro_snapshot: Option<String>,
    rosdistro_additional_recipes: Option<String>,
    mutex_package: Option<MutexPackage>,
    #[serde(default)]
    packages_select_by_deps: Vec<String>,
    #[serde(default)]
    packages_exclude: Vec<String>,
    #[serde(default)]
    packages_skip: Vec<String>,
    #[serde(default)]
    skip_existing: Vec<String>,
    // read by the CI workflow generator, which moves to the monorepo
    #[allow(dead_code)]
    setup_pixi_version: Option<String>,
    #[allow(dead_code)]
    pixi_version: Option<String>,
}

// ----------------------------------------------------------------------------
// the configuration vinca-rs works with
// ----------------------------------------------------------------------------

pub type Patches = HashMap<String, HashMap<String, Vec<PathBuf>>>;

pub struct Config {
    pub platform: String,
    pub ros_distro: String,
    pub build_number: u64,
    pub name_mode: PackageNameMode,
    pub select_by_deps: Vec<String>,
    pub skip_by_deps: Vec<String>,
    pub remove_from_deps: Vec<String>,
    pub patches: Patches,
    pub tests: HashMap<String, PathBuf>,
    pub test_folders: HashMap<String, PathBuf>,
    pub depmods: HashMap<String, DepMods>,
    pub additional_info: HashMap<String, PkgAdditionalInfo>,
    /// conda_index files, the extending configuration's first
    pub conda_indexes: Vec<CondaIndex>,
    pub mutex: Option<MutexPackage>,
    /// rosdistro snapshot + additional recipes (which win)
    pub snapshot: IndexMap<String, SnapshotEntry>,
    pub additional: IndexMap<String, SnapshotEntry>,
    pub skip_existing: Vec<String>,
}

pub fn load_yaml(path: &Path) -> Result<Value> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_yaml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

fn typed<T: for<'de> Deserialize<'de>>(v: Value, what: &Path) -> Result<T> {
    serde_yaml::from_value(v).with_context(|| format!("invalid {}", what.display()))
}

fn names(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Sequence(s)) => s
            .iter()
            .filter_map(|x| match x {
                Value::String(s) => Some(s.clone()),
                Value::Null => None,
                other => Some(serde_yaml::to_string(other).unwrap().trim().to_string()),
            })
            .collect(),
        _ => vec![],
    }
}

fn unique(v: impl IntoIterator<Item = String>) -> Vec<String> {
    v.into_iter().collect::<IndexSet<_>>().into_iter().collect()
}

fn load_layers(path: &Path, platform: &str, seen: &mut Vec<PathBuf>) -> Result<Vec<(PathBuf, Mapping)>> {
    let path = path
        .canonicalize()
        .with_context(|| format!("{}", path.display()))?;
    if seen.contains(&path) {
        bail!("vinca.yaml extends itself: {}", path.display());
    }
    seen.push(path.clone());
    let dir = path.parent().unwrap().to_path_buf();
    let mut conf = match evaluate_for_platform(&load_yaml(&path)?, platform) {
        Value::Mapping(m) => m,
        _ => Mapping::new(),
    };
    for key in PATH_KEYS {
        if let Some(Value::String(p)) = conf.get(key) {
            let abs = dir.join(p);
            conf.insert(key.into(), Value::String(abs.to_string_lossy().into()));
        }
    }
    let mut layers = match conf.remove("extends") {
        Some(Value::String(base)) => load_layers(&dir.join(base), platform, seen)?,
        _ => vec![],
    };
    layers.push((dir, conf));
    Ok(layers)
}

fn constraint_name(spec: &str) -> String {
    spec.trim()
        .split(|c: char| c.is_whitespace() || "=<>!~".contains(c))
        .next()
        .unwrap_or_default()
        .to_string()
}

/// A layer on top of the one it extends: package lists are combined,
/// `mutex_package` merged, other keys replaced.
fn merge_configs(base: &mut Mapping, own: &Mapping) {
    for (k, v) in own {
        let key = k.as_str().unwrap_or_default();
        if LAYERED_LISTS.contains(&key) {
            let merged = unique(names(base.get(k)).into_iter().chain(names(Some(v))));
            base.insert(
                k.clone(),
                Value::Sequence(merged.into_iter().map(Value::String).collect()),
            );
        } else if key == "mutex_package" && base.get(k).is_some_and(Value::is_mapping) {
            let b = base[k].as_mapping().unwrap().clone();
            let o = v.as_mapping().cloned().unwrap_or_default();
            let mut merged = b.clone();
            for (kk, vv) in &o {
                merged.insert(kk.clone(), vv.clone());
            }
            let mine = names(o.get("run_constraints"));
            let mine_names: Vec<_> = mine.iter().map(|c| constraint_name(c)).collect();
            let rc: Vec<Value> = names(b.get("run_constraints"))
                .into_iter()
                .filter(|c| !mine_names.contains(&constraint_name(c)))
                .chain(mine)
                .map(Value::String)
                .collect();
            merged.insert("run_constraints".into(), Value::Sequence(rc));
            base.insert(k.clone(), Value::Mapping(merged));
        } else {
            base.insert(k.clone(), v.clone());
        }
    }
}

/// Per-package files: the extending layer's keys of a package's entry win.
fn merge_per_package(base: &mut Mapping, own: &Mapping) {
    for (pkg, entry) in own {
        match (base.get_mut(pkg), entry) {
            (Some(Value::Mapping(b)), Value::Mapping(e)) => {
                for (k, v) in e {
                    b.insert(k.clone(), v.clone());
                }
            }
            _ => {
                base.insert(pkg.clone(), entry.clone());
            }
        }
    }
}

/// `entries` also under ros-<distro>-<pkg>, ros-<pkg> and ros2-<pkg> (setdefault).
fn add_name_variants<T: Clone>(entries: &mut HashMap<String, T>, distro: &str) {
    let legacy = format!("ros-{distro}-");
    for (name, value) in entries.clone() {
        let short = name
            .strip_prefix(&legacy)
            .or_else(|| name.strip_prefix("ros2-"))
            .or_else(|| name.strip_prefix("ros-"))
            .unwrap_or(&name)
            .to_string();
        for v in [
            format!("{legacy}{short}"),
            format!("ros-{short}"),
            format!("ros2-{short}"),
        ] {
            entries.entry(v).or_insert_with(|| value.clone());
        }
    }
}

/// `<package>[.<platform>].patch`, grouped by package and target OS.
fn discover_patches(dir: &Path, distro: &str) -> Result<Patches> {
    let mut patches: Patches = HashMap::new();
    let mut files: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "patch"))
        .collect();
    files.sort();
    for path in files {
        let fname = path.file_name().unwrap().to_string_lossy().to_string();
        let parts: Vec<&str> = fname.split('.').collect();
        let entry = patches.entry(parts[0].to_string()).or_insert_with(|| {
            std::iter::once("any")
                .chain(PATCH_PLATFORMS)
                .map(|p| (p.to_string(), vec![]))
                .collect()
        });
        let dest: Vec<&str> = if parts.len() == 3 && PATCH_PLATFORMS.contains(&parts[1]) {
            vec![parts[1]]
        } else if parts.len() == 3 && parts[1] == "unix" {
            vec!["linux", "osx"]
        } else {
            vec!["any"]
        };
        for p in dest {
            entry.get_mut(p).unwrap().push(path.clone());
        }
    }
    add_name_variants(&mut patches, distro);
    Ok(patches)
}

fn discover_tests(dir: &Path, distro: &str) -> (HashMap<String, PathBuf>, HashMap<String, PathBuf>) {
    let mut tests = HashMap::new();
    let mut folders = HashMap::new();
    if let Ok(rd) = std::fs::read_dir(dir.join("tests")) {
        for p in rd.filter_map(|e| e.ok().map(|e| e.path())) {
            let name = p.file_name().unwrap().to_string_lossy().to_string();
            if p.is_dir() {
                folders.insert(name, p);
            } else if name.ends_with(".yaml") {
                tests.insert(name.split('.').next().unwrap().to_string(), p);
            }
        }
    }
    add_name_variants(&mut tests, distro);
    add_name_variants(&mut folders, distro);
    (tests, folders)
}

fn selected_mapping(path: &Path, platform: &str) -> Result<Mapping> {
    Ok(match evaluate_for_platform(&load_yaml(path)?, platform) {
        Value::Mapping(m) => m,
        _ => Mapping::new(),
    })
}

pub fn read(path: &Path, platform: &str) -> Result<Config> {
    let layers = load_layers(path, platform, &mut vec![])?;
    let mut merged = Mapping::new();
    for (_, layer) in &layers {
        merge_configs(&mut merged, layer);
    }
    let conf: VincaYaml = typed(Value::Mapping(merged), path)?;

    let exclude = unique(conf.packages_exclude.clone());
    let skip = unique(conf.packages_skip.clone());
    let dropped: Vec<String> = exclude.iter().chain(&skip).map(|n| n.replace('-', "_")).collect();
    let select_by_deps = conf
        .packages_select_by_deps
        .iter()
        .filter(|n| !dropped.contains(&n.replace('-', "_")))
        .cloned()
        .collect();

    let mut patches = Patches::new();
    let mut tests = HashMap::new();
    let mut test_folders = HashMap::new();
    let mut depmods = Mapping::new();
    let mut additional_info = Mapping::new();
    let mut conda_indexes: Vec<CondaIndex> = vec![];
    for (dir, layer) in &layers {
        if let Some(Value::String(pd)) = layer.get("patch_dir") {
            let pd = PathBuf::from(pd);
            patches.extend(discover_patches(&pd, &conf.ros_distro)?);
            let deps = pd.join("dependencies.yaml");
            if deps.exists() {
                merge_per_package(&mut depmods, &selected_mapping(&deps, platform)?);
            }
        }
        let (t, f) = discover_tests(dir, &conf.ros_distro);
        tests.extend(t);
        test_folders.extend(f);
        let info = dir.join("pkg_additional_info.yaml");
        if info.exists() {
            merge_per_package(&mut additional_info, &selected_mapping(&info, platform)?);
        }
        let mut layer_indexes = vec![];
        for i in names(layer.get("conda_index")) {
            let p = dir.join(&i);
            if !p.is_file() {
                bail!("conda_index {i} is not a file next to {}", dir.display());
            }
            layer_indexes.push(typed::<CondaIndex>(load_yaml(&p)?, &p)?);
        }
        layer_indexes.extend(conda_indexes);
        conda_indexes = layer_indexes;
    }

    let load_snapshot = |p: &Option<String>| -> Result<IndexMap<String, SnapshotEntry>> {
        Ok(match p {
            Some(p) => typed(load_yaml(Path::new(p))?, Path::new(p))?,
            None => IndexMap::new(),
        })
    };
    let mut snapshot = load_snapshot(&conf.rosdistro_snapshot)?;
    let additional = load_snapshot(&conf.rosdistro_additional_recipes)?;
    check_overrides(&snapshot, &additional)?;
    snapshot.extend(additional.clone());

    let mut mutex = conf.mutex_package.clone();
    if let Some(m) = &mut mutex {
        m.build_number.get_or_insert(conf.build_number.unwrap_or(1));
    }

    Ok(Config {
        platform: platform.to_string(),
        ros_distro: conf.ros_distro,
        build_number: conf.build_number.unwrap_or(0),
        name_mode: conf.package_name_mode,
        skip_by_deps: unique(exclude.iter().chain(&skip).cloned()),
        remove_from_deps: exclude,
        select_by_deps,
        patches,
        tests,
        test_folders,
        depmods: typed(Value::Mapping(depmods), Path::new("patch/dependencies.yaml"))?,
        additional_info: typed(
            Value::Mapping(additional_info),
            Path::new("pkg_additional_info.yaml"),
        )?,
        conda_indexes,
        mutex,
        snapshot,
        additional,
        skip_existing: conf.skip_existing,
    })
}

/// An additional recipe that replaces a released package must say why
/// (RoboStack/vinca#126): otherwise a stale fork silently shadows newer releases.
fn check_overrides(
    snapshot: &IndexMap<String, SnapshotEntry>,
    additional: &IndexMap<String, SnapshotEntry>,
) -> Result<()> {
    let mut unexplained = vec![];
    for (name, add) in additional {
        let Some(rel) = snapshot.get(name) else {
            continue;
        };
        let newer = version_lt(&add.version, &rel.version);
        match &add.override_reason {
            None => unexplained.push(format!(
                "  {name}: released {} ({}), additional recipe {} ({}){}",
                rel.version,
                rel.url,
                add.version,
                add.url,
                if newer { " -- the release is newer" } else { "" }
            )),
            Some(_) if newer => eprintln!(
                "warning: {name}: the release {} is newer than the additional recipe's {}",
                rel.version, add.version
            ),
            Some(_) => {}
        }
    }
    if !unexplained.is_empty() {
        bail!(
            "rosdistro_additional_recipes.yaml replaces released packages without `override: <reason>`:\n{}",
            unexplained.join("\n")
        );
    }
    Ok(())
}

fn version_lt(a: &str, b: &str) -> bool {
    let parse = |v: &str| v.parse::<rattler_conda_types::Version>().ok();
    matches!((parse(a), parse(b)), (Some(a), Some(b)) if a < b)
}

impl Config {
    /// ROS name without distro prefix, with underscores.
    pub fn short_name(&self, name: &str) -> String {
        let n = name.replace('-', "_");
        for p in [
            "ros2_".to_string(),
            format!("ros_{}_", self.ros_distro),
            "ros_".to_string(),
        ] {
            if let Some(s) = n.strip_prefix(&p) {
                return s.to_string();
            }
        }
        n
    }

    /// The pkg_additional_info entry, by ROS name or conda name.
    pub fn info(&self, name: &str) -> PkgAdditionalInfo {
        let n = name.replace('-', "_");
        self.additional_info
            .get(&n)
            .or_else(|| self.additional_info.get(&self.short_name(name)))
            .cloned()
            .unwrap_or_default()
    }

    pub fn build_number_for(&self, default: u64, name: &str) -> u64 {
        self.info(name).build_number.unwrap_or(default)
    }

    pub fn is_dummy(&self, name: &str) -> bool {
        self.info(name).generate_dummy_package_with_run_deps.is_some()
    }

    pub fn package_patches(&self, conda_name: &str) -> Vec<PathBuf> {
        let Some(p) = self.patches.get(conda_name) else {
            return vec![];
        };
        let os = self.platform.split('-').next().unwrap_or_default();
        p.get("any")
            .into_iter()
            .flatten()
            .chain(p.get(os).into_iter().flatten())
            .cloned()
            .collect()
    }
}
