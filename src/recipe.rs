//! The generated rattler-build recipe, typed, and how it is written to disk
//! (recipe.yaml, build scripts, patches, test and recipe files).

use std::path::{Path, PathBuf};

use fs_err as fs;
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

/// The requirement lists, pixi-build-ros's (its rules make them).
pub use pixi_build_ros::requirements::Reqs;

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
    pub script: Option<Script>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub noarch: Option<Noarch>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub post_process: Vec<PostProcess>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum Script {
    /// the script's file, or its content (empty: nothing to build)
    Plain(String),
    /// a script run by an interpreter other than the platform's default
    File { file: String, interpreter: String },
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Noarch {
    Generic,
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
            build: Build {
                number,
                string: None,
                script: Some(Script::Plain(String::new())),
                noarch: None,
                post_process: vec![],
            },
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
