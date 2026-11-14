//! Layered `vinca.yaml` (`extends:`) and the files next to each layer.
//!
//! Port of vinca/configuration.py + vinca/schemas.py (RoboStack/vinca#200), typed
//! with serde. Only the keys the RoboStack monorepo uses are accepted: the
//! deprecated ones (`packages_skip_by_deps`, `build_all`, `variants_mode`,
//! string-form `mutex_package`, remote `conda_index`, ...) are errors, as are typos.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use fs_err as fs;
use indexmap::{IndexMap, IndexSet};
use miette::Diagnostic;
use rattler_conda_types::Subdir;
use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};
use serde_yaml::{Mapping, Value};
use thiserror::Error;

use pixi_build_ros::config::PackageMappingSource;
use pixi_build_ros::package_map::{PackageMapEntry, PackageMapError, load_package_map_data};

use crate::selectors::evaluate_for_platform;

#[derive(Debug, Error, Diagnostic)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("failed to parse {}", path.display())]
    Parse { path: PathBuf, source: serde_yaml::Error },
    #[error("invalid {}", path.display())]
    Invalid { path: PathBuf, source: serde_yaml::Error },
    #[error("vinca.yaml extends itself: {}", .0.display())]
    ExtendsItself(PathBuf),
    #[error("conda_index {name} is not a file next to {}", dir.display())]
    CondaIndexNotAFile { name: String, dir: PathBuf },
    #[error(
        "rosdistro_additional_recipes.yaml replaces released packages without `override: <reason>`:\n{0}"
    )]
    UnexplainedOverrides(String),
    #[error(transparent)]
    #[diagnostic(transparent)]
    PackageMap(#[from] PackageMapError),
    #[error("patch {} is not named <package>.patch or <package>.<platform>.patch", .0.display())]
    #[diagnostic(help("the platform is one of osx, linux, win, emscripten or unix (linux and osx)"))]
    PatchName(PathBuf),
    #[error("in {}", path.display())]
    Selector {
        path: PathBuf,
        #[source]
        #[diagnostic_source]
        source: crate::selectors::Error,
    },
}

type Result<T, E = Error> = std::result::Result<T, E>;

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
#[serde(default, deny_unknown_fields)]
pub struct DepMods {
    pub add_build: Vec<Value>,
    pub add_host: Vec<Value>,
    pub add_run: Vec<Value>,
    pub remove_build: Vec<Value>,
    pub remove_host: Vec<Value>,
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
    pub additional_cmake_args: Option<String>,
    pub build_number: Option<u64>,
    pub generate_dummy_package_with_run_deps: Option<DummyPackage>,
}

#[derive(Debug, Deserialize, Clone, Default)]
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

/// The rosdep mappings (conda_index files), by key; pixi-build-ros resolves them.
pub type PackageMap = HashMap<String, PackageMapEntry>;

/// Which target platforms a patch applies to (`<package>[.<platform>].patch`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PatchTarget {
    Any,
    Linux,
    Osx,
    Win,
    Emscripten,
}

impl PatchTarget {
    /// The file name's platform: `unix` is linux and osx.
    fn parse(s: &str) -> Option<&'static [Self]> {
        Some(match s {
            "linux" => &[Self::Linux],
            "osx" => &[Self::Osx],
            "win" => &[Self::Win],
            "emscripten" => &[Self::Emscripten],
            "unix" => &[Self::Linux, Self::Osx],
            _ => return None,
        })
    }

    fn of(platform: Subdir) -> Option<Self> {
        match platform.only_platform()? {
            "linux" => Some(Self::Linux),
            "osx" => Some(Self::Osx),
            "win" => Some(Self::Win),
            "emscripten" => Some(Self::Emscripten),
            _ => None,
        }
    }
}

/// Per package (`package_key`), the patches of each target.
pub type Patches = HashMap<String, HashMap<PatchTarget, Vec<PathBuf>>>;

/// The one name per-package files (patches, tests, pkg_additional_info.yaml) are
/// keyed and looked up by: the ROS package name, with underscores. A conda name (`ros-<distro>-<pkg>`, `ros2-<pkg>`,
/// `ros-<pkg>`) loses its prefix.
pub fn package_key(name: &str, distro: &str) -> String {
    let legacy = format!("ros-{distro}-");
    name.strip_prefix(&legacy).or_else(|| name.strip_prefix("ros2-")).unwrap_or(name).replace('-', "_")
}

/// The merged `vinca.yaml` layers, and what is read next to them.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub ros_distro: String,
    pub build_number: Option<u64>,
    #[serde(default)]
    pub package_name_mode: PackageNameMode,
    // validated here, read per layer (relative to each file)
    #[serde(default)]
    #[allow(dead_code)]
    conda_index: Vec<String>,
    #[allow(dead_code)]
    patch_dir: Option<String>,
    rosdistro_snapshot: Option<String>,
    rosdistro_additional_recipes: Option<String>,
    pub mutex_package: Option<MutexPackage>,
    /// without the excluded and skipped packages (`read` drops them)
    #[serde(default)]
    pub packages_select_by_deps: Vec<String>,
    /// not built, and dropped from the other packages' dependencies
    #[serde(default)]
    pub packages_exclude: Vec<String>,
    /// not built (nor walked through for dependencies)
    #[serde(default)]
    pub packages_skip: Vec<String>,
    #[serde(default)]
    pub skip_existing: Vec<String>,
    /// only packages with Python content get Python in host/run (and with it the
    /// Python variants); the others are built once
    #[serde(default)]
    pub conditional_python_dependency: bool,
    // accepted for RoboStack's own CI tooling (its shared vinca.yaml sets them)
    #[serde(default, rename = "setup_pixi_version")]
    _setup_pixi_version: IgnoredAny,
    #[serde(default, rename = "pixi_version")]
    _pixi_version: IgnoredAny,

    // not keys of vinca.yaml: filled in by `read`
    #[serde(skip, default = "noarch")]
    pub platform: Subdir,
    // per package, by `package_key`
    #[serde(skip)]
    patches: Patches,
    #[serde(skip)]
    tests: HashMap<String, PathBuf>,
    #[serde(skip)]
    test_folders: HashMap<String, PathBuf>,
    #[serde(skip)]
    recipe_files: HashMap<String, PathBuf>,
    #[serde(skip)]
    depmods: HashMap<String, DepMods>,
    #[serde(skip)]
    additional_info: HashMap<String, PkgAdditionalInfo>,
    /// the per-package entries of the distribution's own layers (not of the base one)
    #[serde(skip)]
    own_entries: BTreeSet<(&'static str, String)>,
    /// the conda_index files' mappings, the extending configuration's over the others
    #[serde(skip)]
    pub package_map: PackageMap,
    /// rosdistro snapshot + additional recipes (which win)
    #[serde(skip)]
    pub snapshot: IndexMap<String, SnapshotEntry>,
    #[serde(skip)]
    pub additional: IndexMap<String, SnapshotEntry>,
}

fn noarch() -> Subdir {
    Subdir::NoArch
}

pub fn load_yaml(path: &Path) -> Result<Value> {
    let text = fs::read_to_string(path)?;
    serde_yaml::from_str(&text).map_err(|source| Error::Parse { path: path.into(), source })
}

fn typed<T: for<'de> Deserialize<'de>>(v: Value, what: &Path) -> Result<T> {
    serde_yaml::from_value(v).map_err(|source| Error::Invalid { path: what.into(), source })
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

fn load_layers(path: &Path, platform: Subdir, seen: &mut Vec<PathBuf>) -> Result<Vec<(PathBuf, Mapping)>> {
    let path = fs::canonicalize(path)?;
    if seen.contains(&path) {
        return Err(Error::ExtendsItself(path));
    }
    seen.push(path.clone());
    let dir = path.parent().unwrap().to_path_buf();
    let mut conf = match selected(&path, platform)? {
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
            base.insert(k.clone(), Value::Sequence(merged.into_iter().map(Value::String).collect()));
        } else if key == "mutex_package" && base.get(k).is_some_and(Value::is_mapping) {
            let b = base[k].as_mapping().unwrap().clone();
            let o = v.as_mapping().cloned().unwrap_or_default();
            let mut merged = b.clone();
            merged.extend(o.clone());
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
        if let (Some(Value::Mapping(b)), Value::Mapping(e)) = (base.get_mut(pkg), entry) {
            b.extend(e.clone());
        } else {
            base.insert(pkg.clone(), entry.clone());
        }
    }
}

/// `<package>[.<platform>].patch`, grouped by package and target platform.
fn discover_patches(dir: &Path, distro: &str) -> Result<Patches> {
    let mut patches: Patches = HashMap::new();
    let mut files: Vec<_> = fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "patch"))
        .collect();
    files.sort();
    for path in files {
        let fname = path.file_name().unwrap().to_string_lossy().to_string();
        let parts: Vec<&str> = fname.split('.').collect();
        let targets = match parts[..] {
            [_, _] => &[PatchTarget::Any][..],
            [_, platform, _] => PatchTarget::parse(platform).ok_or_else(|| Error::PatchName(path.clone()))?,
            _ => return Err(Error::PatchName(path)),
        };
        let entry = patches.entry(package_key(parts[0], distro)).or_default();
        for t in targets {
            entry.entry(*t).or_default().push(path.clone());
        }
    }
    Ok(patches)
}

fn discover_tests(dir: &Path, distro: &str) -> (HashMap<String, PathBuf>, HashMap<String, PathBuf>) {
    let mut tests = HashMap::new();
    let mut folders = HashMap::new();
    if let Ok(rd) = fs::read_dir(dir.join("tests")) {
        for p in rd.filter_map(|e| e.ok().map(|e| e.path())) {
            let name = p.file_name().unwrap().to_string_lossy().to_string();
            if p.is_dir() {
                folders.insert(package_key(&name, distro), p);
            } else if name.ends_with(".yaml") {
                tests.insert(package_key(name.split('.').next().unwrap(), distro), p);
            }
        }
    }
    (tests, folders)
}

/// `files/<package>/`: copied into that package's recipe directory.
fn discover_recipe_files(dir: &Path, distro: &str) -> HashMap<String, PathBuf> {
    let Ok(rd) = fs::read_dir(dir.join("files")) else {
        return HashMap::new();
    };
    rd.filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .map(|p| (package_key(&p.file_name().unwrap().to_string_lossy(), distro), p))
        .collect()
}

/// A per-package file's entries, by `package_key` (merged as `merge_per_package`).
fn by_package_key(m: Mapping, distro: &str) -> Mapping {
    let mut out = Mapping::new();
    for (k, v) in m {
        let key = k.as_str().map_or(k.clone(), |n| Value::String(package_key(n, distro)));
        merge_per_package(&mut out, &Mapping::from_iter([(key, v)]));
    }
    out
}

fn selected(path: &Path, platform: Subdir) -> Result<Value> {
    evaluate_for_platform(&load_yaml(path)?, platform.as_str())
        .map_err(|source| Error::Selector { path: path.to_path_buf(), source })
}

fn selected_mapping(path: &Path, platform: Subdir) -> Result<Mapping> {
    Ok(match selected(path, platform)? {
        Value::Mapping(m) => m,
        _ => Mapping::new(),
    })
}

pub fn read(path: &Path, platform: Subdir) -> Result<Config> {
    let layers = load_layers(path, platform, &mut vec![])?;
    let mut merged = Mapping::new();
    for (_, layer) in &layers {
        merge_configs(&mut merged, layer);
    }
    let mut conf: Config = typed(Value::Mapping(merged), path)?;
    conf.platform = platform;
    let dropped: Vec<String> =
        conf.packages_exclude.iter().chain(&conf.packages_skip).map(|n| n.replace('-', "_")).collect();
    conf.packages_select_by_deps.retain(|n| !dropped.contains(&n.replace('-', "_")));

    let mut additional_info = Mapping::new();
    let mut conda_indexes = vec![];
    for (i, (dir, layer)) in layers.iter().enumerate() {
        // the distribution's own entries, checked against its packages: those of every
        // layer but the one the others extend (shared by every distribution)
        let mut own_entries = vec![];
        let mut note = |what: &'static str, keys: Vec<String>| {
            if i > 0 || layers.len() == 1 {
                own_entries.extend(keys.into_iter().map(|k| (what, k)));
            }
        };
        let roots = names(layer.get("packages_select_by_deps"));
        note("packages_select_by_deps", roots.iter().map(|n| n.replace('-', "_")).collect());
        if let Some(Value::String(pd)) = layer.get("patch_dir") {
            let pd = PathBuf::from(pd);
            let patches = discover_patches(&pd, &conf.ros_distro)?;
            note("patches", patches.keys().cloned().collect());
            conf.patches.extend(patches);
        }
        let (t, f) = discover_tests(dir, &conf.ros_distro);
        note("tests", t.keys().chain(f.keys()).cloned().collect());
        conf.tests.extend(t);
        conf.test_folders.extend(f);
        let files = discover_recipe_files(dir, &conf.ros_distro);
        note("files", files.keys().cloned().collect());
        conf.recipe_files.extend(files);
        let info = dir.join("pkg_additional_info.yaml");
        if info.exists() {
            let own = by_package_key(selected_mapping(&info, platform)?, &conf.ros_distro);
            note(
                "pkg_additional_info.yaml",
                own.keys().filter_map(|k| k.as_str().map(String::from)).collect(),
            );
            merge_per_package(&mut additional_info, &own);
        }
        conf.own_entries.extend(own_entries);
        let mut layer_indexes = vec![];
        for i in names(layer.get("conda_index")) {
            let p = dir.join(&i);
            if !p.is_file() {
                return Err(Error::CondaIndexNotAFile { name: i, dir: dir.clone() });
            }
            layer_indexes.push(PackageMappingSource::File { path: p });
        }
        layer_indexes.append(&mut conda_indexes);
        conda_indexes = layer_indexes;
    }
    conf.package_map = load_package_map_data(&conda_indexes)?;
    // a package's dependency fixes (add_*/remove_*) sit next to its other entries
    let mut depmods = Mapping::new();
    for (pkg, entry) in additional_info.iter_mut() {
        let Value::Mapping(entry) = entry else { continue };
        let fixes: Mapping = entry
            .iter()
            .filter(|(k, _)| k.as_str().is_some_and(|k| k.starts_with("add_") || k.starts_with("remove_")))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if !fixes.is_empty() {
            entry.retain(|k, _| !fixes.contains_key(k));
            depmods.insert(pkg.clone(), Value::Mapping(fixes));
        }
    }
    conf.depmods = typed(Value::Mapping(depmods), Path::new("pkg_additional_info.yaml"))?;
    conf.additional_info = typed(Value::Mapping(additional_info), Path::new("pkg_additional_info.yaml"))?;

    let load_snapshot = |p: &Option<String>| -> Result<IndexMap<String, SnapshotEntry>> {
        Ok(match p {
            Some(p) => typed(load_yaml(Path::new(p))?, Path::new(p))?,
            None => IndexMap::new(),
        })
    };
    conf.snapshot = load_snapshot(&conf.rosdistro_snapshot)?;
    conf.additional = load_snapshot(&conf.rosdistro_additional_recipes)?;
    check_overrides(&conf.snapshot, &conf.additional)?;
    conf.snapshot.extend(conf.additional.clone());
    Ok(conf)
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
        return Err(Error::UnexplainedOverrides(unexplained.join("\n")));
    }
    Ok(())
}

fn version_lt(a: &str, b: &str) -> bool {
    let parse = |v: &str| v.parse::<rattler_conda_types::Version>().ok();
    matches!((parse(a), parse(b)), (Some(a), Some(b)) if a < b)
}

impl Config {
    fn key(&self, name: &str) -> String {
        package_key(name, &self.ros_distro)
    }

    /// The pkg_additional_info entry, by ROS name or conda name.
    pub fn info(&self, name: &str) -> PkgAdditionalInfo {
        self.additional_info.get(&self.key(name)).cloned().unwrap_or_default()
    }

    pub fn build_number_for(&self, name: &str) -> u64 {
        self.info(name).build_number.or(self.build_number).unwrap_or(0)
    }

    pub fn is_dummy(&self, name: &str) -> bool {
        self.info(name).generate_dummy_package_with_run_deps.is_some()
    }

    /// The patches for the target platform: those for any, then its own.
    pub fn package_patches(&self, name: &str) -> Vec<PathBuf> {
        let Some(p) = self.patches.get(&self.key(name)) else {
            return vec![];
        };
        let own = PatchTarget::of(self.platform).and_then(|t| p.get(&t));
        p.get(&PatchTarget::Any).into_iter().flatten().chain(own.into_iter().flatten()).cloned().collect()
    }

    /// The test file (`tests/<package>.yaml`) and folder of a package.
    pub fn package_tests(&self, name: &str) -> (Option<&PathBuf>, Option<&PathBuf>) {
        let key = self.key(name);
        (self.tests.get(&key), self.test_folders.get(&key))
    }

    /// The folder whose files are copied into a package's recipe directory.
    pub fn package_files(&self, name: &str) -> Option<&PathBuf> {
        self.recipe_files.get(&self.key(name))
    }

    /// The dependency fixes (`add_*`/`remove_*` in pkg_additional_info.yaml) of a package.
    pub fn depmods(&self, name: &str) -> Option<&DepMods> {
        self.depmods.get(&self.key(name))
    }

    /// The distribution's own entries (its packages_select_by_deps, patches, tests,
    /// pkg_additional_info.yaml) for which `known` is false, e.g.
    /// typos or stale entries: "<where>: <package>".
    pub fn unmatched(&self, known: impl Fn(&str) -> bool) -> Vec<String> {
        self.own_entries.iter().filter(|(_, k)| !known(k)).map(|(w, k)| format!("{w}: {k}")).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(version: &str, reason: Option<&str>) -> SnapshotEntry {
        SnapshotEntry {
            url: "https://github.com/x/y.git".into(),
            version: version.into(),
            tag: Some("t".into()),
            dependencies: Some(vec![]),
            override_reason: reason.map(String::from),
            ..Default::default()
        }
    }

    #[test]
    fn package_keys() {
        for n in ["ros2-foo-bar", "ros-humble-foo-bar", "foo_bar", "foo-bar"] {
            assert_eq!(package_key(n, "humble"), "foo_bar", "{n}");
        }
        assert_eq!(package_key("ros2-ros-workspace", "humble"), "ros_workspace");
        assert_eq!(package_key("ros_workspace", "humble"), "ros_workspace");
        assert_eq!(package_key("ros2_control", "humble"), "ros2_control");
        // a ROS name written with dashes, not a conda prefix
        assert_eq!(package_key("ros-gz-bridge", "humble"), "ros_gz_bridge");
    }

    #[test]
    fn patch_platforms() {
        let tmp = tempfile::tempdir().unwrap();
        for f in ["ros2-a.patch", "ros2-a.unix.patch", "ros2-a.win.patch", "ros-humble-b.osx.patch"] {
            fs::write(tmp.path().join(f), "").unwrap();
        }
        let p = discover_patches(tmp.path(), "humble").unwrap();
        assert_eq!(p["a"].len(), 4);
        assert_eq!(p["a"][&PatchTarget::Linux], [tmp.path().join("ros2-a.unix.patch")]);
        assert_eq!(p["b"][&PatchTarget::Osx].len(), 1);
        for bad in ["ros2-c.win64.patch", "ros2-c.osx-arm64.patch", "ros2-c.win.x.patch"] {
            let dir = tempfile::tempdir().unwrap();
            fs::write(dir.path().join(bad), "").unwrap();
            assert!(matches!(discover_patches(dir.path(), "humble"), Err(Error::PatchName(_))), "{bad}");
        }
    }

    #[test]
    fn replacing_a_release_needs_a_reason() {
        let snapshot = IndexMap::from([("pkg".to_string(), entry("1.7.2", None))]);
        let silent = IndexMap::from([("pkg".to_string(), entry("1.4.2", None))]);
        insta::assert_snapshot!(check_overrides(&snapshot, &silent).unwrap_err(), @"
        rosdistro_additional_recipes.yaml replaces released packages without `override: <reason>`:
          pkg: released 1.7.2 (https://github.com/x/y.git), additional recipe 1.4.2 (https://github.com/x/y.git) -- the release is newer
        ");
        let explained = IndexMap::from([("pkg".to_string(), entry("1.4.2", Some("fork with a fix")))]);
        check_overrides(&snapshot, &explained).unwrap();
        let other = IndexMap::from([("new_pkg".to_string(), entry("1.0.0", None))]);
        check_overrides(&snapshot, &other).unwrap();
    }
}
