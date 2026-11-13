//! From the selected ROS packages to recipes: name policy, rosdep -> conda
//! resolution, RoboStack's per-package configuration, and the auxiliary recipes
//! (mutex, legacy-name aliases, conda-forge wrappers). The requirement rules and
//! build scripts are pixi-build-ros's (shared with `pixi build`).

use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;

use fs_err as fs;
use miette::Diagnostic;
use pixi_build_ros::build_script::{
    ACTIVATION_SCRIPTS, BuildMode, BuildType, ScriptVars, render_build_script, script_name,
};
use pixi_build_ros::distro::RosVersion;
use pixi_build_ros::package_map::{MappedPackages, PackageMapError, lookup};
use pixi_build_ros::package_xml::PackageXml;
use pixi_build_ros::requirements::{CORE, DepKind, Rules};
use serde_yaml::Value;
use thiserror::Error;

use crate::config::{self, Config, PackageNameMode};
use crate::distro::{self, Distro};
use crate::recipe::*;
use crate::selectors::evaluate_distro;

#[derive(Debug, Error, Diagnostic)]
pub enum Error {
    #[error(transparent)]
    #[diagnostic(transparent)]
    PackageMap(#[from] PackageMapError),
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

pub struct Generator<'a> {
    pub conf: &'a Config,
    pub distro: &'a Distro,
    pub unsatisfied: BTreeSet<String>,
    /// ROS packages a conda_index maps to conda packages: never built
    shadowed: BTreeSet<String>,
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

impl<'a> Generator<'a> {
    pub fn new(conf: &'a Config, distro: &'a Distro) -> Self {
        let shadowed = conf.package_map.keys().filter(|n| distro.has(n)).cloned().collect();
        Self { conf, distro, unsatisfied: BTreeSet::new(), shadowed }
    }

    /// The distribution, as pixi-build-ros's rules know it.
    fn ros_distro(&self) -> pixi_build_ros::distro::Distro {
        let version = if self.distro.ros1 { RosVersion::Ros1 } else { RosVersion::Ros2 };
        pixi_build_ros::distro::Distro { name: self.distro.name.clone(), version }
    }

    // ---------------------------------------------------------------- naming

    pub fn prefix(&self) -> String {
        match self.conf.package_name_mode {
            PackageNameMode::Legacy => format!("ros-{}", self.distro.name),
            _ => self.ros_distro().package_prefix(),
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
        match lookup(&self.conf.package_map, dep, self.conf.platform)? {
            Some(MappedPackages::Conda(specs)) if kind == DepKind::Build => Ok(specs),
            Some(MappedPackages::Conda(specs)) => Ok(specs
                .into_iter()
                .map(|s| if s.contains("${{") { s } else { s.split(' ').next().unwrap().to_string() })
                .filter(|s| s != "python")
                .collect()),
            Some(MappedPackages::Ros(names)) => {
                Ok(names.iter().filter_map(|n| self.ros_dependency(n)).collect())
            }
            None => Ok(self.ros_dependency(dep).into_iter().collect()),
        }
    }

    /// A ROS package dependency's conda name, unless the distribution doesn't have
    /// it or excludes it (then it is reported as unsatisfied).
    fn ros_dependency(&mut self, dep: &str) -> Option<String> {
        let excluded = self.conf.packages_exclude.iter().any(|e| e == dep || *e == dep.replace('_', "-"));
        if !self.distro.has(dep) || excluded {
            self.unsatisfied.insert(dep.to_string());
            return None;
        }
        Some(self.conda_name(dep))
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
        for root in &self.conf.packages_select_by_deps {
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
            license: pixi_build_ros::license::to_spdx(&xml.licenses, ros),
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
        let build_type = match BuildType::of(&xml, &self.ros_distro()) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("{ros}: {e}");
                return Ok(None);
            }
        };
        let unix = !self.conf.platform.is_windows();
        let script = script_name(build_type, !unix);

        let rules = Rules {
            package_prefix: self.prefix(),
            mutex: self.mutex_spec(),
            c_stdlib: true,
            conditional_python: self.conf.conditional_python_dependency,
            ..Rules::new(self.ros_distro(), self.conf.platform)
        };
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
        let mut reqs =
            rules.requirements(&xml, build_type, &groups, |dep, kind| self.resolve(&dep.name, kind))?;

        // patch/dependencies.yaml
        if let Some(m) = self.conf.depmods(ros).cloned() {
            for (list, add, remove) in [
                (&mut reqs.build, &m.add_build, &m.remove_build),
                (&mut reqs.host, &m.add_host, &m.remove_host),
                (&mut reqs.run, &m.add_run, &m.remove_run),
            ] {
                for a in add.iter().filter_map(|v| v.as_str()) {
                    list.spec(self.normalize(a));
                }
                for r in remove.iter().filter_map(|v| v.as_str()) {
                    let spec = self.normalize(r);
                    list.remove(spec.split_whitespace().next().unwrap_or_default());
                }
            }
        }

        // after dependencies.yaml, which can give a package Python (e.g. one that
        // embeds the interpreter)
        let python_free = rules.finish(&mut reqs);

        let mut recipe = Recipe::new(&conda, &version, self.conf.build_number_for(&conda));
        recipe.build.script = Some(script.to_string());
        recipe.build.post_process = sysroot_post_process();
        recipe.requirements =
            Requirements { build: reqs.build, host: reqs.host, run: reqs.run, ..Default::default() };
        recipe.about = self.about(ros, &xml);

        let mut files = RecipeFiles::default();
        if let Some((src, copies)) = self.source(ros, &conda) {
            recipe.source = Some(src);
            files.copies.extend(copies);
        }
        // every ament package installs its package.xml: a cheap check that the
        // build installed anything at all (RoboStack/vinca#108)
        if !matches!(build_type, BuildType::Cmake | BuildType::Catkin) {
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
        let folder = self.distro.additional_folder(ros);
        let source_dir = if unix {
            format!("$SRC_DIR/$PKG_NAME/src/work/{folder}")
        } else {
            format!("%SRC_DIR%\\%PKG_NAME%\\src\\work\\{folder}")
        };
        let vars = ScriptVars {
            generator: "vinca-rs (https://github.com/Tobias-Fischer/vinca-rs)",
            source_dir: &source_dir,
            package_prefix: &prefix,
            distro: &self.distro.name,
            cmake_args: info.additional_cmake_args.as_deref().unwrap_or(""),
        };
        files
            .scripts
            .push((script.to_string(), render_build_script(build_type, !unix, BuildMode::Clean, &vars)));
        // ros_workspace installs the environment's activation scripts
        if ros == "ros_workspace" {
            files.scripts.extend(ACTIVATION_SCRIPTS.iter().map(|(n, s)| (n.to_string(), s.to_string())));
        }
        Ok(Some(RecipeDir { recipe, files }))
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
        // only requirements: one build for every platform
        r.build.noarch = Some(Noarch::Generic);
        Some(RecipeDir { recipe: r, files: RecipeFiles::default() })
    }
}
