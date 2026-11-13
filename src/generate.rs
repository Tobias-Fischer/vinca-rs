//! From the selected ROS packages to recipes: name policy, rosdep -> conda
//! resolution, the requirement rules, and the auxiliary recipes (mutex,
//! legacy-name aliases, conda-forge wrappers).

use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;

use fs_err as fs;
use miette::Diagnostic;
use pixi_build_ros::package_xml::{Dependency, PackageXml};
use serde_yaml::Value;
use thiserror::Error;

use crate::config::{self, Config, PackageNameMode, PlatformList};
use crate::distro::{self, Distro};
use crate::license::to_spdx;
use crate::recipe::*;
use crate::selectors::evaluate_distro;

#[derive(Debug, Error, Diagnostic)]
pub enum Error {
    #[error("conda_index entry {0} has no robostack mapping")]
    NoRobostackMapping(String),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Distro(#[from] distro::Error),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Config(#[from] config::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Yaml(#[from] serde_yaml::Error),
}

type Result<T, E = Error> = std::result::Result<T, E>;

/// conditional_python_dependency: dependencies that mean a package's own
/// artifact needs Python (rosidl_default_generators doesn't: rclcpp only uses it
/// for test messages; message packages are found by their group instead).
const PYTHON_MARKERS: [&str; 5] =
    ["rclpy", "pybind11", "python_cmake_module", "ament_cmake_python", "rosidl_pycommon"];

/// conditional_python_dependency: Python for a package built without it (an
/// interpreter for ament's scripts), pinned so that it isn't a variant.
const BUILD_TIME_PYTHON: &str = "python ${{ python_min | default('3.11') }}.*";

/// Whether a package's own artifact needs Python (host/run), i.e. is built per
/// Python version. Conservative: a false "yes" only costs builds. Conditions are
/// evaluated (RoboStack/vinca#189).
fn python_needed(xml: &PackageXml, build_type: BuildType) -> bool {
    if build_type == BuildType::AmentPython {
        return true;
    }
    if xml.member_of_groups.iter().any(|g| g.is_active() && g.name == "rosidl_interface_packages") {
        return true;
    }
    let d = &xml.dependencies;
    let deps = names(&[
        &d.build_depends,
        &d.build_export_depends,
        &d.exec_depends,
        &d.run_depends,
        &d.test_depends,
        &d.depend,
    ]);
    let tools = names(&[&d.buildtool_depends, &d.buildtool_export_depends]);
    // build tools only by the markers: a plain python3 buildtool_depend is an
    // interpreter for a code generator (BUILD_TIME_PYTHON)
    deps.iter().chain(&tools).any(|n| PYTHON_MARKERS.contains(&n.as_str()))
        || deps.iter().any(|n| n.contains("python") || n.starts_with("pybind"))
}

/// The ROS packages that every ROS 2 package builds on, and that themselves don't.
const CORE: [&str; 4] = ["ament_cmake_core", "ament_package", "ros_workspace", "ros_environment"];

#[derive(Clone, Copy, PartialEq)]
enum BuildType {
    AmentCmake,
    AmentPython,
    /// also catkin's
    Cmake,
}

impl BuildType {
    fn as_str(self) -> &'static str {
        match self {
            BuildType::AmentCmake => "ament_cmake",
            BuildType::AmentPython => "ament_python",
            BuildType::Cmake => "cmake",
        }
    }
}

/// Run dependencies carry no version (run_exports pin them) and never python.
#[derive(Clone, Copy, PartialEq)]
enum DepKind {
    Build,
    Run,
}

pub struct Generator<'a> {
    pub conf: &'a Config,
    pub distro: &'a Distro,
    pub unsatisfied: BTreeSet<String>,
    /// ROS packages a conda_index maps to conda packages: never built
    shadowed: BTreeSet<String>,
}

fn template(name: &str) -> Option<&'static str> {
    Some(match name {
        "build_ament_cmake.sh" => include_str!("../templates/build_ament_cmake.sh.in"),
        "bld_ament_cmake.bat" => include_str!("../templates/bld_ament_cmake.bat.in"),
        "build_ament_python.sh" => include_str!("../templates/build_ament_python.sh.in"),
        "bld_ament_python.bat" => include_str!("../templates/bld_ament_python.bat.in"),
        "build_cmake.sh" => include_str!("../templates/build_cmake.sh.in"),
        "bld_cmake.bat" => include_str!("../templates/bld_cmake.bat.in"),
        "activate.sh" => include_str!("../templates/activate.sh.in"),
        "activate.bat" => include_str!("../templates/activate.bat.in"),
        "activate.ps1" => include_str!("../templates/activate.ps1.in"),
        "deactivate.sh" => include_str!("../templates/deactivate.sh.in"),
        "deactivate.bat" => include_str!("../templates/deactivate.bat.in"),
        "deactivate.ps1" => include_str!("../templates/deactivate.ps1.in"),
        _ => return None,
    })
}

fn render(tpl: &str, vars: &[(&str, &str)]) -> String {
    let mut s = tpl.to_string();
    for (k, v) in vars {
        s = s.replace(&format!("@({k})"), v);
    }
    s.replace("@@", "@")
}

/// catkin_pkg's description: the element's inner XML, stripped.
fn description(xml: &str) -> Option<String> {
    let doc = roxmltree::Document::parse(xml).ok()?;
    let node = doc.root_element().children().find(|n| n.has_tag_name("description"))?;
    let outer = &xml[node.range()];
    let inner = &outer[outer.find('>')? + 1..outer.rfind("</").unwrap_or(outer.len())];
    // as minidom's toxml() writes it: quotes unescaped
    let s = inner.trim_matches(|c| " \n\r\t".contains(c)).replace("&quot;", "\"").replace("&apos;", "'");
    (!s.is_empty()).then_some(s)
}

fn names(deps: &[&Vec<Dependency>]) -> Vec<String> {
    deps.iter().flat_map(|d| d.iter()).filter(|d| d.is_active()).map(|d| d.name.clone()).collect()
}

impl<'a> Generator<'a> {
    pub fn new(conf: &'a Config, distro: &'a Distro) -> Self {
        let shadowed =
            conf.conda_indexes.iter().flat_map(|i| i.keys()).filter(|n| distro.has(n)).cloned().collect();
        Self { conf, distro, unsatisfied: BTreeSet::new(), shadowed }
    }

    fn emscripten(&self) -> bool {
        self.conf.platform.only_platform() == Some("emscripten")
    }

    // ---------------------------------------------------------------- naming

    pub fn prefix(&self) -> String {
        match self.conf.package_name_mode {
            PackageNameMode::Legacy => format!("ros-{}", self.distro.name),
            _ if self.distro.ros1 => "ros".into(),
            _ => "ros2".into(),
        }
    }

    pub fn conda_name(&self, ros: &str) -> String {
        format!("{}-{}", self.prefix(), ros.replace('_', "-"))
    }

    fn legacy_name(&self, ros: &str) -> String {
        format!("ros-{}-{}", self.distro.name, ros.replace('_', "-"))
    }

    /// dependencies.yaml entries may use legacy names
    fn normalize(&self, spec: &str) -> String {
        let legacy = format!("ros-{}-", self.distro.name);
        match spec.strip_prefix(&legacy) {
            Some(rest) => format!("{}-{rest}", self.prefix()),
            None => spec.to_string(),
        }
    }

    fn mutex_spec(&self) -> Option<String> {
        let m = self.conf.mutex_package.as_ref()?;
        let depth = m.upper_bound.split('.').count();
        let pin: Vec<&str> = m.version.split('.').take(depth).collect();
        Some(format!("{} {}.* {}_*", m.name, pin.join("."), self.distro.name))
    }

    // ------------------------------------------------------------ resolution

    /// A rosdep key or ROS package -> conda specs.
    fn resolve(&mut self, dep: &str, kind: DepKind) -> Result<Vec<String>> {
        for index in &self.conf.conda_indexes {
            let Some(entry) = index.get(dep) else {
                continue;
            };
            let specs = match &entry.robostack {
                None => return Err(Error::NoRobostackMapping(dep.into())),
                Some(PlatformList::All(l)) => l.clone(),
                Some(PlatformList::PerPlatform(m)) => m
                    .get(self.conf.platform.as_str())
                    .or_else(|| match self.conf.platform.only_platform() {
                        Some("win") => m.get("win64"),
                        Some(os @ ("linux" | "osx" | "emscripten")) => m.get(os),
                        _ => None,
                    })
                    .cloned()
                    .flatten()
                    .unwrap_or_default(),
            };
            if kind == DepKind::Build {
                return Ok(specs);
            }
            return Ok(specs
                .into_iter()
                .map(|s| if s.contains("${{") { s } else { s.split(' ').next().unwrap().to_string() })
                .filter(|s| s != "python")
                .collect());
        }
        let excluded = self.conf.packages_exclude.iter().any(|e| e == dep || *e == dep.replace('_', "-"));
        if !self.distro.has(dep) || excluded {
            self.unsatisfied.insert(dep.to_string());
            return Ok(vec![]);
        }
        Ok(vec![self.conda_name(dep)])
    }

    fn resolve_all(&mut self, deps: impl IntoIterator<Item = String>, kind: DepKind) -> Result<Vec<String>> {
        let mut out = vec![];
        for d in deps {
            out.extend(self.resolve(&d, kind)?);
        }
        Ok(out)
    }

    // ------------------------------------------------------------- selection

    pub fn select(&self) -> Result<Vec<String>, distro::Error> {
        let mut skip: HashSet<String> = HashSet::new();
        for s in self.conf.packages_exclude.iter().chain(&self.conf.packages_skip) {
            skip.insert(s.clone());
            skip.insert(s.replace('-', "_"));
        }
        skip.extend(self.shadowed.iter().cloned());
        let mut selected = BTreeSet::new();
        let mut unknown = vec![];
        for root in &self.conf.packages_select_by_deps {
            let root = root.replace('-', "_");
            if self.shadowed.contains(&root) {
                continue;
            }
            if !self.distro.has(&root) {
                unknown.push(root.clone());
            }
            selected.insert(root.clone());
            if skip.contains(&root) {
                continue;
            }
            for dep in self.distro.depends(&root, &skip)? {
                selected.insert(dep);
            }
        }
        if !unknown.is_empty() {
            eprintln!("warning: packages_select_by_deps not in the distribution: {}", unknown.join(", "));
        }
        if !self.distro.ros1 && selected.iter().any(|p| !CORE.contains(&p.as_str())) {
            for p in ["ros_workspace", "ros_environment"] {
                if self.distro.has(p) {
                    selected.insert(p.into());
                }
            }
        }
        Ok(selected.into_iter().collect())
    }

    // ------------------------------------------------------------- recipes

    fn source(&self, ros: &str, conda: &str) -> Option<(Source, Vec<(PathBuf, PathBuf)>)> {
        let e = self.distro.entry(ros).filter(|e| e.tag.is_some() || e.rev.is_some())?;
        let patches = self.conf.package_patches(conda);
        let rel: Vec<String> =
            patches.iter().map(|p| format!("patch/{}", p.file_name().unwrap().to_string_lossy())).collect();
        let copies = rel.iter().cloned().map(PathBuf::from).zip(patches).collect();
        Some((
            Source {
                git: e.url.clone(),
                tag: e.tag.clone(),
                rev: if e.tag.is_some() { None } else { e.rev.clone() },
                target_directory: format!("{conda}/src/work"),
                submodules: e.submodules,
                patches: rel,
            },
            copies,
        ))
    }

    fn tests(&self, conda: &str, files: &mut RecipeFiles) -> Result<Vec<serde_yaml::Value>> {
        let (test_file, test_folder) = self.conf.package_tests(conda);
        if let Some(dir) = test_folder {
            for e in fs::read_dir(dir)? {
                let e = e?;
                files.copies.push((PathBuf::from(e.file_name()), e.path()));
            }
        }
        let Some(path) = test_file else {
            return Ok(vec![]);
        };
        let t = evaluate_distro(&config::load_yaml(path)?, &self.distro.name, self.conf.platform.as_str())
            .map_err(|source| config::Error::Selector { path: path.to_path_buf(), source })?;
        Ok(t.get("tests").and_then(Value::as_sequence).cloned().unwrap_or_default())
    }

    /// A ROS package that conda-forge provides: only a dependency on it.
    fn wrapper(&mut self, ros: &str, conda: &str, version: &str, xml: &PackageXml) -> Result<RecipeDir> {
        let d = self.conf.info(ros).generate_dummy_package_with_run_deps.unwrap();
        let version = d.override_version.clone().unwrap_or(version.to_string());
        let pin = d.upper_bound.as_deref().or(d.max_pin.as_deref()).unwrap_or("x");
        let mut parts: Vec<u64> = version.split('.').map(|p| p.parse().unwrap_or(0)).collect();
        let depth = pin.split('.').count();
        let n = parts.len();
        parts.truncate(depth);
        *parts.last_mut().unwrap() += 1;
        parts.resize(n, 0);
        let upper: Vec<String> = parts.iter().map(u64::to_string).collect();
        let mut r = Recipe::new(conda, &version, self.conf.build_number_for(ros));
        r.requirements.run.spec(format!("{} >={version}, <{}a0", d.dep_name, upper.join(".")));
        if let Some(m) = self.mutex_spec() {
            r.requirements.run.spec(m);
        }
        r.about = self.about(ros, xml);
        let mut files = RecipeFiles::default();
        r.tests = self.tests(conda, &mut files)?;
        Ok(RecipeDir { recipe: r, files })
    }

    fn about(&self, ros: &str, xml: &PackageXml) -> About {
        let urls: Vec<(String, Option<String>)> =
            xml.urls.iter().map(|u| (u.url.clone(), u.url_type.clone())).collect();
        About {
            homepage: urls
                .iter()
                .rfind(|(_, t)| t.as_deref().unwrap_or("website") == "website")
                .map(|(u, _)| u.clone()),
            repository: self.distro.repository_url(ros, &urls),
            license: to_spdx(&xml.licenses, ros),
            summary: description(&self.distro.package_xml(ros).unwrap_or_default()),
        }
    }

    pub fn package(&mut self, ros: &str, ros_base: &[PackageXml]) -> Result<Option<RecipeDir>> {
        let Some(version) = self.distro.version(ros).map(str::to_string) else {
            return Ok(None);
        };
        let Some(conda) = self.resolve(ros, DepKind::Build)?.into_iter().next() else {
            return Ok(None);
        };
        let xml = self.distro.package(ros)?;
        if self.conf.is_dummy(ros) {
            return self.wrapper(ros, &conda, &version, &xml).map(Some);
        }
        // No <build_type> means catkin (REP 140). In ROS 2 that is a CMake package,
        // an ament one when it builds with ament_cmake (e.g. async_web_server_cpp,
        // mrt_cmake_modules, which only declare build types for ROS 1).
        let undeclared_ament = xml.export.build_types.is_empty()
            && names(&[&xml.dependencies.buildtool_depends]).iter().any(|d| d.starts_with("ament_cmake"));
        let build_type = if undeclared_ament { "ament_cmake".to_string() } else { xml.build_type() };
        let build_type = match build_type.as_str() {
            "ament_cmake" => BuildType::AmentCmake,
            "ament_python" => BuildType::AmentPython,
            "cmake" | "catkin" => BuildType::Cmake,
            other => {
                eprintln!("{ros}: unsupported build type {other}");
                return Ok(None);
            }
        };
        let unix = !self.conf.platform.is_windows();
        let base = build_type.as_str();
        let script = if unix { format!("build_{base}.sh") } else { format!("bld_{base}.bat") };

        let conditional = self.conf.conditional_python_dependency;
        let needs_python = !conditional || python_needed(&xml, build_type);

        let mut build = Reqs::default();
        build.extend(["${{ compiler('cxx') }}", "${{ compiler('c') }}"]);
        if !self.emscripten() {
            build.spec("${{ stdlib('c') }}");
        }
        build.extend(["ninja", "python", "setuptools", "git", "git-lfs"]);
        if unix {
            build.extend(["patch", "make", "coreutils"]);
        } else {
            build.spec("m2-patch");
        }
        if self.conf.platform.is_osx() {
            build.spec("tapi");
        }
        build.extend(["cmake", "cython"]);
        for s in ["pkg-config", "python", "cross-python_${{ target_platform }}", "numpy"] {
            build.push(Req::cross(s));
        }
        if self.conf.platform.is_osx() && build_type != BuildType::AmentPython {
            build.spec("clang-tools ${{ (cxx_compiler_version ~ '.*') if cxx_compiler_version is defined else '*' }}");
        }
        let mut host = Reqs::default();
        host.push(Req::native("pkg-config"));
        let mut run = Reqs::default();
        if needs_python {
            host.extend(["python", "numpy", "pip"]);
            let python = self.resolve("python", DepKind::Build)?;
            host.extend(python.clone());
            run.extend(python);
        }
        if build_type == BuildType::AmentPython {
            host.extend(self.resolve("python-setuptools", DepKind::Build)?);
        }
        if let Some(m) = self.mutex_spec() {
            host.spec(m.clone());
            run.spec(m);
        }
        if !self.distro.ros1 && !CORE.contains(&ros) {
            host.extend([self.conda_name("ros_environment"), self.conda_name("ros_workspace")]);
            run.spec(self.conda_name("ros_workspace"));
        }

        // <group_depend>: the members among ros_base's packages
        let groups: Vec<String> = xml
            .group_depends
            .iter()
            .flat_map(|g| {
                ros_base
                    .iter()
                    .filter(|p| p.member_of_groups.iter().any(|m| m.name == g.name && m.is_active()))
                    .map(|p| p.name.clone())
            })
            .collect();
        let d = &xml.dependencies;
        let mut build_deps = names(&[&d.build_depends, &d.depend, &d.build_export_depends, &d.test_depends]);
        if xml.format == 1 {
            build_deps.extend(names(&[&d.run_depends]));
        }
        build_deps.extend(groups.clone());
        // build tools go to host, except git, which has to run on the build machine
        for tool in names(&[&d.buildtool_depends, &d.buildtool_export_depends]) {
            let resolved = self.resolve(&tool, DepKind::Build)?;
            if resolved.iter().any(|r| r == "git") {
                build.extend(resolved);
            } else if conditional && resolved == ["python"] {
                // an interpreter for a code generator: BUILD_TIME_PYTHON
            } else if !resolved.is_empty() && tool != "cmake" {
                build_deps.push(tool);
            }
        }
        // cyclonedds generates code during the build
        if ros == "cyclonedds" || build_deps.iter().any(|d| d == "cyclonedds") {
            build.push(Req::cross(self.conda_name("cyclonedds")));
        }
        host.extend(self.resolve_all(build_deps, DepKind::Build)?);
        let mut run_deps = names(&[
            &d.run_depends,
            &d.exec_depends,
            &d.depend,
            &d.build_export_depends,
            &d.buildtool_export_depends,
        ]);
        run_deps.extend(groups);
        run.extend(self.resolve_all(run_deps, DepKind::Run)?);

        // patch/dependencies.yaml
        if let Some(m) = self.conf.depmods(ros).cloned() {
            for (list, add, remove) in [
                (&mut build, &m.add_build, &m.remove_build),
                (&mut host, &m.add_host, &m.remove_host),
                (&mut run, &m.add_run, &m.remove_run),
            ] {
                for a in add.iter().filter_map(|v| v.as_str()) {
                    list.spec(self.normalize(a));
                }
                for r in remove.iter().filter_map(|v| v.as_str()) {
                    list.remove(&self.normalize(r));
                }
            }
        }

        // after dependencies.yaml, which can give a package Python (e.g. one that
        // embeds the interpreter)
        let python_free = conditional && !host.contains("python");
        if python_free {
            build.replace("python", BUILD_TIME_PYTHON);
        }
        self.apply_rules(&mut build, &mut host, &mut run);

        let mut recipe = Recipe::new(&conda, &version, self.conf.build_number_for(&conda));
        recipe.build.script = Some(script.clone());
        recipe.build.post_process = sysroot_post_process();
        recipe.requirements =
            Requirements { build, host: host.sorted(), run: run.sorted(), ..Default::default() };
        recipe.about = self.about(ros, &xml);

        let mut files = RecipeFiles::default();
        if let Some((src, copies)) = self.source(ros, &conda) {
            recipe.source = Some(src);
            files.copies.extend(copies);
        }
        // every ament package installs its package.xml: a cheap check that the
        // build installed anything at all (RoboStack/vinca#108)
        if build_type != BuildType::Cmake {
            let share = if unix { "share" } else { "Library/share" };
            recipe.tests.push(serde_yaml::to_value(serde_json::json!({
                "package_contents": { "files": [format!("{share}/{}/package.xml", xml.name)] }
            }))?);
        }
        // built once, for no particular Python: Python modules in it would only work
        // with the interpreter it happened to be built with
        if python_free {
            recipe.tests.push(serde_yaml::to_value(serde_json::json!({
                "package_contents": {
                    "strict": false,
                    "files": { "not_exists": ["**/site-packages/**", "**/*.cpython-*.so", "**/*.pyd"] }
                }
            }))?);
        }
        recipe.tests.extend(self.tests(&conda, &mut files)?);
        let info = self.conf.info(&conda);
        let prefix = self.prefix();
        let vars = [
            ("ros_package_prefix", prefix.as_str()),
            ("additional_cmake_args", info.additional_cmake_args.as_deref().unwrap_or("")),
            ("additional_folder", self.distro.additional_folder(ros)),
            ("ros_distro", self.distro.name.as_str()),
        ];
        files.scripts.push((script.clone(), render(template(&script).unwrap(), &vars)));
        // ros_workspace installs the environment's activation scripts
        if ros == "ros_workspace" {
            for s in ["activate", "deactivate"] {
                for ext in ["sh", "bat", "ps1"] {
                    let n = format!("{s}.{ext}");
                    files.scripts.push((n.clone(), render(template(&n).unwrap(), &vars)));
                }
            }
        }
        Ok(Some(RecipeDir { recipe, files }))
    }

    /// Requirement rules that encode which dependencies break when building for
    /// Emscripten or cross-compiling (vinca's `_adjust_requirements`).
    fn apply_rules(&self, build: &mut Reqs, host: &mut Reqs, run: &mut Reqs) {
        let emscripten = self.emscripten();
        // cmake is a build tool, never a host or (on Emscripten) run dependency
        if host.remove("cmake") {
            build.spec("cmake");
        }
        if emscripten {
            run.remove("cmake");
        }
        // mimick_vendor runs on the build machine; it doesn't build for Emscripten
        let mimick = self.conda_name("mimick_vendor");
        let had = host.remove(&mimick);
        if emscripten {
            build.remove(&mimick);
        } else if had {
            build.spec(mimick);
        }
        // Emscripten resolves the generators out of `build`
        let gens = self.conda_name("rosidl_default_generators");
        if emscripten && host.contains(&gens) {
            build.spec(gens);
        }
        if host.contains(&self.conda_name("pybind11_vendor")) {
            host.spec("pybind11");
        }
        for d in ["pybind11", "qt-main"] {
            if host.contains(d) {
                build.push(Req::cross(d));
            }
        }
        // build-machine tools: host when native, build when cross-compiling
        for d in ["pyqt-builder", "git", "doxygen", "git-lfs"] {
            if host.remove(d) {
                host.push(Req::native(d));
                build.push(Req::cross(d));
            }
        }
    }

    /// The distribution mutex.
    pub fn mutex(&self) -> Option<RecipeDir> {
        let m = self.conf.mutex_package.as_ref()?;
        let bn = m.build_number.or(self.conf.build_number).unwrap_or(1);
        let mut r = Recipe::new(&m.name, &m.version, bn);
        r.build.string = Some(format!("{}_{bn}", self.distro.name));
        r.requirements.run_constraints = m.run_constraints.clone();
        r.requirements.run_exports = Some(RunExports {
            weak: vec![format!("${{{{ pin_subpackage('{}', upper_bound='{}') }}}}", m.name, m.upper_bound)],
        });
        let d = &self.distro.name;
        r.about = About {
            homepage: Some(format!("https://github.com/robostack/ros-{d}")),
            license: Some("BSD-3-Clause".into()),
            summary: Some(format!(
                "The ROS2 distro mutex. To switch between ROS2 versions, you need to change the mutex.\n\
                 E.g. mamba install {}=*={d} to switch to {d}.",
                m.name
            )),
            ..Default::default()
        };
        Some(RecipeDir { recipe: r, files: RecipeFiles::default() })
    }

    /// The old (ros-<distro>-<pkg>) name, depending on the new one.
    pub fn alias(&self, ros: &str, canonical: &Recipe) -> Option<RecipeDir> {
        if self.conf.package_name_mode != PackageNameMode::Both
            || canonical.package.name != self.conda_name(ros)
        {
            return None;
        }
        let legacy = self.legacy_name(ros);
        let version = &canonical.package.version;
        let mut r = Recipe::new(&legacy, version, self.conf.build_number_for(&legacy));
        r.requirements.run.spec(format!("{} =={version}", canonical.package.name));
        if let Some(m) = self.mutex_spec() {
            r.requirements.run.spec(m);
        }
        r.about.summary = Some(format!("Compatibility package for {}", canonical.package.name));
        Some(RecipeDir { recipe: r, files: RecipeFiles::default() })
    }
}
