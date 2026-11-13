//! The generated rattler-build recipe, typed, and how it is written to disk
//! (recipe.yaml, build/activation scripts, patches, test files).

use std::path::{Path, PathBuf};

use fs_err as fs;
use indexmap::IndexSet;
use miette::Diagnostic;
use serde::Serialize;
use serde_yaml::Value;
use thiserror::Error;

#[derive(Debug, Error, Diagnostic)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("failed to serialize the recipe of {0}")]
    Serialize(String, #[source] serde_yaml::Error),
}

/// A requirement: a match spec, or one only used when cross-compiling (or only
/// when not). Recipes are generated per target platform, so platform selectors
/// are resolved at generation time; only the build-vs-target platform remains.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(untagged)]
pub enum Req {
    Spec(String),
    If {
        #[serde(rename = "if")]
        cond: BuildPlatform,
        then: Vec<String>,
    },
}

/// Build vs target platform; `Ord` as the conditions' text sorts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub enum BuildPlatform {
    #[serde(rename = "build_platform != target_platform")]
    Cross,
    #[serde(rename = "build_platform == target_platform")]
    Native,
}

impl Req {
    pub fn cross(s: impl Into<String>) -> Self {
        Req::If { cond: BuildPlatform::Cross, then: vec![s.into()] }
    }
    pub fn native(s: impl Into<String>) -> Self {
        Req::If { cond: BuildPlatform::Native, then: vec![s.into()] }
    }
    fn sort_key(&self) -> (Option<BuildPlatform>, String) {
        match self {
            Req::Spec(s) => (None, s.clone()),
            Req::If { cond, then } => (Some(*cond), then.join(",")),
        }
    }
}

/// An ordered, duplicate-free requirement list.
#[derive(Debug, Clone, Default)]
pub struct Reqs(IndexSet<Req>);

impl Reqs {
    pub fn push(&mut self, r: Req) {
        self.0.insert(r);
    }
    pub fn spec(&mut self, s: impl Into<String>) {
        self.push(Req::Spec(s.into()));
    }
    pub fn extend(&mut self, specs: impl IntoIterator<Item = impl Into<String>>) {
        for s in specs {
            self.spec(s);
        }
    }
    pub fn contains(&self, s: &str) -> bool {
        self.0.contains(&Req::Spec(s.to_string()))
    }
    /// Remove the requirements on package `name` (a spec's first word), also
    /// inside conditional requirements; the removed specs.
    pub fn remove(&mut self, name: &str) -> Vec<String> {
        let is = |s: &String| s.split_whitespace().next() == Some(name);
        let mut removed = vec![];
        self.0 = std::mem::take(&mut self.0)
            .into_iter()
            .filter_map(|r| match r {
                Req::Spec(s) if is(&s) => {
                    removed.push(s);
                    None
                }
                Req::If { cond, then } => {
                    let (gone, keep): (Vec<String>, Vec<String>) = then.into_iter().partition(is);
                    removed.extend(gone);
                    (!keep.is_empty()).then_some(Req::If { cond, then: keep })
                }
                r => Some(r),
            })
            .collect();
        removed
    }
    pub fn sorted(mut self) -> Self {
        self.0.sort_by_key(Req::sort_key);
        self
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    /// Replace a spec, also inside conditional requirements.
    pub fn replace(&mut self, from: &str, to: &str) {
        let swap = |s: &String| if s == from { to.to_string() } else { s.clone() };
        self.0 = std::mem::take(&mut self.0)
            .into_iter()
            .map(|r| match r {
                Req::Spec(s) => Req::Spec(swap(&s)),
                Req::If { cond, then } => Req::If { cond, then: then.iter().map(swap).collect() },
            })
            .collect();
    }
}

impl Serialize for Reqs {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq(self.0.iter())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Package {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Source {
    pub git: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    pub target_directory: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub submodules: Option<bool>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub patches: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PostProcess {
    pub files: Vec<&'static str>,
    pub regex: &'static str,
    pub replacement: &'static str,
}

/// Replace the build machine's sysroot / macOS SDK paths in .pc and CMake files
/// with the environment's (CONDA_BUILD_SYSROOT, which conda's compiler activation
/// sets): e.g. rtabmap's exported targets carry the SDK's usr/include, which
/// doesn't exist on users' machines. rattler-build only relocates the prefixes.
pub fn sysroot_post_process() -> Vec<PostProcess> {
    vec![
        PostProcess {
            files: vec!["*.pc"],
            regex: r#"(?:-L|-I)?"?([^;\s]+/sysroot/)"#,
            replacement: "$$(CONDA_BUILD_SYSROOT_S)",
        },
        PostProcess {
            files: vec!["*.cmake"],
            regex: r#"([^;\s"]+/sysroot)"#,
            replacement: "$$ENV{CONDA_BUILD_SYSROOT}",
        },
        PostProcess {
            files: vec!["*.cmake"],
            regex: r#"([^;\s"]+/MacOSX\d*\.?\d*\.sdk)"#,
            replacement: "$$ENV{CONDA_BUILD_SYSROOT}",
        },
    ]
}

#[derive(Debug, Clone, Serialize)]
pub struct Build {
    pub number: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub string: Option<String>,
    /// a build script next to the recipe; none: a package without content
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub post_process: Vec<PostProcess>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RunExports {
    pub weak: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Requirements {
    #[serde(skip_serializing_if = "Reqs::is_empty")]
    pub build: Reqs,
    #[serde(skip_serializing_if = "Reqs::is_empty")]
    pub host: Reqs,
    #[serde(skip_serializing_if = "Reqs::is_empty")]
    pub run: Reqs,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub run_constraints: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_exports: Option<RunExports>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct About {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Extra {
    #[serde(rename = "recipe-maintainers")]
    pub recipe_maintainers: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Recipe {
    pub package: Package,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    pub build: Build,
    pub requirements: Requirements,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tests: Vec<Value>,
    pub about: About,
    pub extra: Extra,
}

impl Recipe {
    /// A recipe with an empty build script, i.e. of a package without content.
    pub fn new(name: &str, version: &str, number: u64) -> Self {
        Recipe {
            package: Package { name: name.into(), version: version.into() },
            source: None,
            build: Build { number, string: None, script: Some(String::new()), post_process: vec![] },
            requirements: Requirements::default(),
            tests: vec![],
            about: About::default(),
            extra: Extra { recipe_maintainers: vec!["ros-forge".into()] },
        }
    }
}

/// Files that go next to a recipe.yaml.
#[derive(Debug, Default)]
pub struct RecipeFiles {
    /// generated: name -> content (executable)
    pub scripts: Vec<(String, String)>,
    /// copied: destination (relative) -> source
    pub copies: Vec<(PathBuf, PathBuf)>,
}

pub struct RecipeDir {
    pub recipe: Recipe,
    pub files: RecipeFiles,
}

const SCHEMA: &str = "# yaml-language-server: $schema=https://raw.githubusercontent.com/prefix-dev/recipe-format/main/schema.json\n\n";

fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    if src.is_dir() {
        fs::create_dir_all(dst)?;
        for e in fs::read_dir(src)? {
            let e = e?;
            copy_tree(&e.path(), &dst.join(e.file_name()))?;
        }
    } else {
        fs::copy(src, dst)?;
    }
    Ok(())
}

impl RecipeDir {
    pub fn write(&self, recipes: &Path) -> Result<(), Error> {
        let name = &self.recipe.package.name;
        let dir = recipes.join(name);
        fs::create_dir_all(&dir)?;
        let yaml = serde_yaml::to_string(&self.recipe).map_err(|e| Error::Serialize(name.clone(), e))?;
        fs::write(dir.join("recipe.yaml"), format!("{SCHEMA}{yaml}"))?;
        for (dest, src) in &self.files.copies {
            let d = dir.join(dest);
            if let Some(p) = d.parent() {
                fs::create_dir_all(p)?;
            }
            copy_tree(src, &d)?;
        }
        for (name, content) in &self.files.scripts {
            let p = dir.join(name);
            fs::write(&p, content)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755))?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reqs(r: Vec<Req>) -> Reqs {
        let mut out = Reqs::default();
        r.into_iter().for_each(|r| out.push(r));
        out
    }

    #[test]
    fn remove_by_package_name() {
        let mut r = reqs(vec![
            Req::Spec("foo >=1".into()),
            Req::Spec("foo-bar".into()),
            Req::native("foo"),
            Req::If { cond: BuildPlatform::Cross, then: vec!["foo 2.*".into(), "baz".into()] },
        ]);
        assert_eq!(r.remove("foo"), ["foo >=1", "foo", "foo 2.*"]);
        assert_eq!(r.0.into_iter().collect::<Vec<_>>(), [Req::Spec("foo-bar".into()), Req::cross("baz")]);
    }
}
