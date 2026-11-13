//! From the selected ROS packages to recipes: name policy, rosdep -> conda
//! resolution, the requirement rules, and the auxiliary recipes (mutex,
//! legacy-name aliases, conda-forge wrappers).

use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;

use anyhow::{Result, bail};
use pixi_build_ros::package_xml::{Dependency, PackageXml};

use crate::config::{Config, PackageNameMode, PlatformList};
use crate::distro::Distro;
use crate::license::to_spdx;
use crate::recipe::*;
use crate::selectors::evaluate_distro;

/// The ROS packages that every ROS 2 package builds on, and that themselves don't.
const CORE: [&str; 4] = [
    "ament_cmake_core",
    "ament_package",
    "ros_workspace",
    "ros_environment",
];

pub struct Generator<'a> {
    pub conf: &'a Config,
    pub distro: &'a Distro,
    pub unsatisfied: BTreeSet<String>,
    os: &'static str,
    /// ROS packages a conda_index maps to conda packages: never built
    shadowed: BTreeSet<String>,
}

pub struct Selection {
    pub packages: Vec<String>,
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
    let node = doc
        .root_element()
        .children()
        .find(|n| n.has_tag_name("description"))?;
    let outer = &xml[node.range()];
    let inner = &outer[outer.find('>')? + 1..outer.rfind("</").unwrap_or(outer.len())];
    // as minidom's toxml() writes it: quotes unescaped
    let s = inner
        .trim_matches(|c| " \n\r\t".contains(c))
        .replace("&quot;", "\"")
        .replace("&apos;", "'");
    (!s.is_empty()).then_some(s)
}

fn names(deps: &[&Vec<Dependency>]) -> Vec<String> {
    deps.iter()
        .flat_map(|d| d.iter())
        .filter(|d| d.is_active())
        .map(|d| d.name.clone())
        .collect()
}

impl<'a> Generator<'a> {
    pub fn new(conf: &'a Config, distro: &'a Distro) -> Self {
        let os = match conf.platform.split('-').next().unwrap_or_default() {
            "linux" => "linux",
            "osx" => "osx",
            "win" => "win",
            "emscripten" => "emscripten",
            _ => "other",
        };
        let shadowed = conf
            .conda_indexes
            .iter()
            .flat_map(|i| i.keys())
            .filter(|n| distro.has(n))
            .cloned()
            .collect();
        Self {
            conf,
            distro,
            unsatisfied: BTreeSet::new(),
            os,
            shadowed,
        }
    }

    fn unix(&self) -> bool {
        self.os != "win"
    }

    // ---------------------------------------------------------------- naming

    pub fn prefix(&self) -> String {
        match self.conf.name_mode {
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
        let m = self.conf.mutex.as_ref()?;
        let depth = m.upper_bound.split('.').count();
        let pin: Vec<&str> = m.version.split('.').take(depth).collect();
        Some(format!("{} {}.* {}_*", m.name, pin.join("."), self.distro.name))
    }

    // ------------------------------------------------------------ resolution

    /// A rosdep key or ROS package -> conda specs. Run dependencies carry no
    /// version (run_exports pin them) and never python.
    fn resolve(&mut self, dep: &str, run: bool) -> Result<Vec<String>> {
        for index in &self.conf.conda_indexes {
            let Some(entry) = index.get(dep) else {
                continue;
            };
            let specs = match &entry.robostack {
                None => bail!("conda_index entry {dep} has no robostack mapping"),
                Some(PlatformList::All(l)) => l.clone(),
                Some(PlatformList::PerPlatform(m)) => m
                    .get(&self.conf.platform)
                    .or_else(|| m.get(if self.os == "win" { "win64" } else { self.os }))
                    .cloned()
                    .flatten()
                    .unwrap_or_default(),
            };
            if !run {
                return Ok(specs);
            }
            return Ok(specs
                .into_iter()
                .map(|s| {
                    if s.contains("${{") {
                        s
                    } else {
                        s.split(' ').next().unwrap().to_string()
                    }
                })
                .filter(|s| s != "python")
                .collect());
        }
        let excluded = self
            .conf
            .remove_from_deps
            .iter()
            .any(|e| e == dep || *e == dep.replace('_', "-"));
        if !self.distro.has(dep) || excluded {
            self.unsatisfied.insert(dep.to_string());
            return Ok(vec![]);
        }
        Ok(vec![self.conda_name(dep)])
    }

    fn resolve_all(&mut self, deps: impl IntoIterator<Item = String>, run: bool) -> Result<Vec<String>> {
        let mut out = vec![];
        for d in deps {
            out.extend(self.resolve(&d, run)?);
        }
        Ok(out)
    }

    // ------------------------------------------------------------- selection

    pub fn select(&self) -> Result<Selection> {
        let mut skip: HashSet<String> = HashSet::new();
        for s in &self.conf.skip_by_deps {
            skip.insert(s.clone());
            skip.insert(s.replace('-', "_"));
        }
        skip.extend(self.shadowed.iter().cloned());
        let mut selected = BTreeSet::new();
        for root in &self.conf.select_by_deps {
            let root = root.replace('-', "_");
            if self.shadowed.contains(&root) {
                continue;
            }
            selected.insert(root.clone());
            if skip.contains(&root) {
                continue;
            }
            for dep in self.distro.depends(&root, &skip)? {
                selected.insert(dep);
            }
        }
        if !self.distro.ros1 && selected.iter().any(|p| !CORE.contains(&p.as_str())) {
            for p in ["ros_workspace", "ros_environment"] {
                if self.distro.has(p) {
                    selected.insert(p.into());
                }
            }
        }
        Ok(Selection {
            packages: selected.into_iter().collect(),
        })
    }

    // ------------------------------------------------------------- recipes

    fn base(&self, name: &str, version: &str, number: u64) -> Recipe {
        Recipe {
            package: Package {
                name: name.into(),
                version: version.into(),
            },
            source: None,
            build: Build {
                number,
                string: None,
                script: Some(String::new()),
                post_process: vec![],
            },
            requirements: Requirements::default(),
            tests: vec![],
            about: About::default(),
            extra: Extra {
                recipe_maintainers: vec!["ros-forge".into()],
            },
        }
    }

    fn source(&self, ros: &str, conda: &str) -> Option<(Source, Vec<(PathBuf, PathBuf)>)> {
        let (url, reference, kind) = self.distro.source(ros)?;
        let patches = self.conf.package_patches(conda);
        let rel: Vec<String> = patches
            .iter()
            .map(|p| format!("patch/{}", p.file_name().unwrap().to_string_lossy()))
            .collect();
        let copies = rel.iter().cloned().map(PathBuf::from).zip(patches).collect();
        Some((
            Source {
                git: url,
                tag: (kind == "tag").then(|| reference.clone()),
                rev: (kind == "rev").then_some(reference),
                target_directory: format!("{conda}/src/work"),
                submodules: self.distro.submodules(ros),
                patches: rel,
            },
            copies,
        ))
    }

    fn tests(&self, conda: &str, files: &mut RecipeFiles) -> Result<Vec<serde_yaml::Value>> {
        if let Some(dir) = self.conf.test_folders.get(conda) {
            for e in std::fs::read_dir(dir)? {
                let e = e?;
                files.copies.push((PathBuf::from(e.file_name()), e.path()));
            }
        }
        let Some(path) = self.conf.tests.get(conda) else {
            return Ok(vec![]);
        };
        let t = evaluate_distro(&crate::config::load_yaml(path)?, &self.distro.name);
        Ok(t.get("tests")
            .and_then(|t| t.as_sequence())
            .cloned()
            .unwrap_or_default())
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
        let mut r = self.base(
            conda,
            &version,
            self.conf.build_number_for(self.conf.build_number, ros),
        );
        r.requirements
            .run
            .spec(format!("{} >={version}, <{}a0", d.dep_name, upper.join(".")));
        if let Some(m) = self.mutex_spec() {
            r.requirements.run.spec(m);
        }
        r.about = self.about(ros, xml);
        let mut files = RecipeFiles::default();
        r.tests = self.tests(conda, &mut files)?;
        Ok(RecipeDir { recipe: r, files })
    }

    fn about(&self, ros: &str, xml: &PackageXml) -> About {
        let urls: Vec<(String, Option<String>)> = xml
            .urls
            .iter()
            .map(|u| (u.url.clone(), u.url_type.clone()))
            .collect();
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
        let Some(conda) = self.resolve(ros, false)?.into_iter().next() else {
            return Ok(None);
        };
        let xml = self.distro.package(ros)?;
        if self.conf.is_dummy(ros) {
            return self.wrapper(ros, &conda, &version, &xml).map(Some);
        }
        let build_type = xml.build_type();
        let script_base = match build_type.as_str() {
            "ament_cmake" => "ament_cmake",
            "ament_python" => "ament_python",
            // no <build_type> means catkin (REP 140): in ROS 2, a plain CMake package
            "cmake" | "catkin" => "cmake",
            other => {
                eprintln!("{ros}: unsupported build type {other}");
                return Ok(None);
            }
        };
        let script = if self.unix() {
            format!("build_{script_base}.sh")
        } else {
            format!("bld_{script_base}.bat")
        };

        let cross = |s: &str| Req::cross(s);
        let mut build = Reqs::default();
        build.extend(["${{ compiler('cxx') }}", "${{ compiler('c') }}"].map(String::from));
        if self.os != "emscripten" {
            build.spec("${{ stdlib('c') }}");
        }
        build.extend(["ninja", "python", "setuptools", "git", "git-lfs"].map(String::from));
        match self.os {
            "win" => build.spec("m2-patch"),
            _ => build.extend(["patch", "make", "coreutils"].map(String::from)),
        }
        if self.os == "osx" {
            build.spec("tapi");
        }
        build.extend(["cmake", "cython"].map(String::from));
        for s in [
            "pkg-config",
            "python",
            "cross-python_${{ target_platform }}",
            "numpy",
        ] {
            build.push(cross(s));
        }
        if self.os == "osx" && matches!(script_base, "ament_cmake" | "cmake") {
            build.spec("clang-tools ${{ (cxx_compiler_version ~ '.*') if cxx_compiler_version is defined else '*' }}");
        }
        let mut host = Reqs::default();
        host.push(Req::native("pkg-config"));
        host.extend(["python", "numpy", "pip"].map(String::from));
        let mut run = Reqs::default();

        let python = self.resolve("python", false)?;
        host.extend(python.clone());
        run.extend(python);
        if build_type == "ament_python" {
            host.extend(self.resolve("python-setuptools", false)?);
        }
        if let Some(m) = self.mutex_spec() {
            host.spec(m.clone());
            run.spec(m);
        }
        if !self.distro.ros1 && !CORE.contains(&ros) {
            host.extend([
                self.conda_name("ros_environment"),
                self.conda_name("ros_workspace"),
            ]);
            run.spec(self.conda_name("ros_workspace"));
        }

        // <group_depend>: the members among ros_base's packages
        let groups: Vec<String> = xml
            .group_depends
            .iter()
            .flat_map(|g| {
                ros_base
                    .iter()
                    .filter(|p| {
                        p.member_of_groups
                            .iter()
                            .any(|m| m.name == g.name && m.is_active())
                    })
                    .map(|p| p.name.clone())
            })
            .collect();
        let d = &xml.dependencies;
        let mut build_deps = names(&[
            &d.build_depends,
            &d.depend,
            &d.build_export_depends,
            &d.test_depends,
        ]);
        if xml.format == 1 {
            build_deps.extend(names(&[&d.run_depends]));
        }
        build_deps.extend(groups.clone());
        // build tools go to host, except git, which has to run on the build machine
        for tool in names(&[&d.buildtool_depends, &d.buildtool_export_depends]) {
            let resolved = self.resolve(&tool, false)?;
            if resolved.iter().any(|r| r == "git") {
                build.extend(resolved);
            } else if !resolved.is_empty() && tool != "cmake" {
                build_deps.push(tool);
            }
        }
        // cyclonedds generates code during the build
        if ros == "cyclonedds" || build_deps.iter().any(|d| d == "cyclonedds") {
            build.push(cross(&self.conda_name("cyclonedds")));
        }
        host.extend(self.resolve_all(build_deps, false)?);
        let mut run_deps = names(&[
            &d.run_depends,
            &d.exec_depends,
            &d.depend,
            &d.build_export_depends,
            &d.buildtool_export_depends,
        ]);
        run_deps.extend(groups);
        run.extend(self.resolve_all(run_deps, true)?);

        // patch/dependencies.yaml
        if let Some(m) = self.conf.depmods.get(&xml.name).cloned() {
            for (list, add, remove) in [
                (&mut build, &m.add_build, &m.remove_build),
                (&mut host, &m.add_host, &m.remove_host),
                (&mut run, &m.add_run, &m.remove_run),
            ] {
                for a in add.iter().filter_map(|v| v.as_str()) {
                    list.spec(self.normalize(a));
                }
                for r in remove.iter().filter_map(|v| v.as_str()) {
                    list.remove(&Req::Spec(self.normalize(r)));
                }
            }
        }

        self.apply_rules(&mut build, &mut host, &mut run);

        let mut recipe = self.base(
            &conda,
            &version,
            self.conf.build_number_for(self.conf.build_number, &conda),
        );
        recipe.build.script = Some(script.clone());
        recipe.build.post_process = sysroot_post_process();
        recipe.requirements = Requirements {
            build,
            host: host.sorted(),
            run: run.sorted(),
            ..Default::default()
        };
        recipe.about = self.about(ros, &xml);

        let mut files = RecipeFiles::default();
        if let Some((src, copies)) = self.source(ros, &conda) {
            recipe.source = Some(src);
            files.copies.extend(copies);
        }
        // every ament package installs its package.xml: a cheap check that the
        // build installed anything at all (RoboStack/vinca#108)
        if script_base.starts_with("ament") {
            let share = if self.unix() { "share" } else { "Library/share" };
            recipe.tests.push(serde_yaml::to_value(serde_json::json!({
                "package_contents": { "files": [format!("{share}/{}/package.xml", xml.name)] }
            }))?);
        }
        recipe.tests.extend(self.tests(&conda, &mut files)?);
        let info = self.conf.info(&self.conf.short_name(&conda));
        let prefix = self.prefix();
        let vars = [
            ("ros_package_prefix", prefix.as_str()),
            (
                "additional_cmake_args",
                info.additional_cmake_args.as_deref().unwrap_or(""),
            ),
            ("additional_folder", self.distro.additional_folder(ros)),
            ("ros_distro", self.distro.name.as_str()),
        ];
        files
            .scripts
            .push((script.clone(), render(template(&script).unwrap(), &vars)));
        // ros_workspace installs the environment's activation scripts
        if ros == "ros_workspace" {
            for s in ["activate", "deactivate"] {
                for ext in ["sh", "bat", "ps1"] {
                    let n = format!("{s}.{ext}");
                    files
                        .scripts
                        .push((n.clone(), render(template(&n).unwrap(), &vars)));
                }
            }
        }
        Ok(Some(RecipeDir { recipe, files }))
    }

    /// Requirement rules that encode which dependencies break when building for
    /// Emscripten or cross-compiling (vinca's `_adjust_requirements`).
    fn apply_rules(&self, build: &mut Reqs, host: &mut Reqs, run: &mut Reqs) {
        let emscripten = self.os == "emscripten";
        // cmake is a build tool, never a host or (on Emscripten) run dependency
        if host.remove(&Req::Spec("cmake".into())) {
            build.spec("cmake");
        }
        if emscripten {
            run.remove(&Req::Spec("cmake".into()));
        }
        // mimick_vendor runs on the build machine; it doesn't build for Emscripten
        let mimick = self.conda_name("mimick_vendor");
        let had = host.remove(&Req::Spec(mimick.clone()));
        if emscripten {
            build.remove(&Req::Spec(mimick));
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
            if host.remove(&Req::Spec(d.into())) {
                host.push(Req::native(d));
                build.push(Req::cross(d));
            }
        }
    }

    /// The distribution mutex.
    pub fn mutex(&self) -> Option<RecipeDir> {
        let m = self.conf.mutex.as_ref()?;
        let bn = m.build_number.unwrap_or(self.conf.build_number);
        let mut r = self.base(&m.name, &m.version, bn);
        r.build.string = Some(format!("{}_{bn}", self.distro.name));
        r.requirements.run_constraints = m.run_constraints.clone();
        r.requirements.run_exports = Some(RunExports {
            weak: vec![format!(
                "${{{{ pin_subpackage('{}', upper_bound='{}') }}}}",
                m.name, m.upper_bound
            )],
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
        Some(RecipeDir {
            recipe: r,
            files: RecipeFiles::default(),
        })
    }

    /// The old (ros-<distro>-<pkg>) name, depending on the new one.
    pub fn alias(&self, ros: &str, canonical: &Recipe) -> Option<RecipeDir> {
        if self.conf.name_mode != PackageNameMode::Both || canonical.package.name != self.conda_name(ros) {
            return None;
        }
        let legacy = self.legacy_name(ros);
        let version = &canonical.package.version;
        let mut r = self.base(
            &legacy,
            version,
            self.conf.build_number_for(self.conf.build_number, &legacy),
        );
        r.requirements
            .run
            .spec(format!("{} =={version}", canonical.package.name));
        if let Some(m) = self.mutex_spec() {
            r.requirements.run.spec(m);
        }
        r.about.summary = Some(format!("Compatibility package for {}", canonical.package.name));
        Some(RecipeDir {
            recipe: r,
            files: RecipeFiles::default(),
        })
    }
}
