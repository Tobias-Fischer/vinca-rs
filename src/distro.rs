//! Where ROS package metadata comes from: the rosdistro snapshot (versions,
//! sources, dependency graph), the rosdistro release cache (package.xml files)
//! and, for packages whose snapshot entry differs from the live cache, the
//! package.xml at the pinned tag/rev on GitHub.
//!
//! Replaces vinca/distro.py + the `rosdistro` Python package: with a snapshot
//! that records dependencies, the cache is only a package.xml store.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::Read;
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow, bail};
use indexmap::IndexMap;
use pixi_build_ros::package_xml::PackageXml;
use rayon::prelude::*;
use serde::Deserialize;

use crate::config::{Config, SnapshotEntry};

const INDEX_URL: &str = "https://raw.githubusercontent.com/ros/rosdistro/master/index-v4.yaml";

#[derive(Deserialize)]
struct Index {
    distributions: HashMap<String, IndexDistro>,
}

#[derive(Deserialize)]
struct IndexDistro {
    distribution_cache: String,
    distribution_type: String,
    python_version: u32,
}

#[derive(Deserialize)]
struct Cache {
    release_package_xmls: HashMap<String, String>,
    distribution_file: Vec<DistributionFile>,
}

#[derive(Deserialize)]
struct DistributionFile {
    repositories: HashMap<String, Repository>,
}

#[derive(Deserialize)]
struct Repository {
    release: Option<ReleaseRepo>,
    source: Option<SourceRepo>,
}

#[derive(Deserialize)]
struct ReleaseRepo {
    url: Option<String>,
    version: Option<String>,
    packages: Option<Vec<String>>,
    #[serde(default)]
    tags: HashMap<String, String>,
}

#[derive(Deserialize)]
struct SourceRepo {
    url: Option<String>,
}

/// What rosdistro says about one released package.
#[derive(Clone, PartialEq)]
pub struct LiveRelease {
    pub url: String,
    pub tag: String,
    /// upstream version, without the release increment
    pub version: String,
    pub source_url: Option<String>,
}

/// The rosdistro index entry, distribution file(s) and release cache of a distribution.
pub struct Rosdistro {
    pub ros1: bool,
    pub python_version: u32,
    pub releases: HashMap<String, LiveRelease>,
    pub xmls: HashMap<String, String>,
}

fn releases_of(files: &[DistributionFile]) -> HashMap<String, LiveRelease> {
    let mut live = HashMap::new();
    for file in files {
        for (repo_name, repo) in &file.repositories {
            let Some(rel) = &repo.release else { continue };
            let (Some(url), Some(version)) = (&rel.url, &rel.version) else {
                continue;
            };
            let pkgs = rel.packages.clone().unwrap_or_else(|| vec![repo_name.clone()]);
            for pkg in pkgs {
                let tag = rel
                    .tags
                    .get("release")
                    .cloned()
                    .unwrap_or_else(|| "release/{package}/{version}".into())
                    .replace("{package}", &pkg)
                    .replace("{version}", version)
                    .replace("{upstream_version}", version.split('-').next().unwrap());
                live.insert(
                    pkg,
                    LiveRelease {
                        url: url.clone(),
                        tag,
                        version: version.split('-').next().unwrap().to_string(),
                        source_url: repo.source.as_ref().and_then(|s| s.url.clone()),
                    },
                );
            }
        }
    }
    live
}

pub fn client() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .user_agent("vinca-rs")
        .timeout(std::time::Duration::from_secs(120))
        .build()?)
}

const ROSDISTRO_RAW: &str = "https://raw.githubusercontent.com/ros/rosdistro";

impl Rosdistro {
    /// The published cache; with `rosdistro_ref` (e.g. a sync tag such as
    /// `jazzy/2026-10-05`), the releases of that ref instead, keeping only the
    /// cached package.xml files of releases that didn't change.
    pub fn load(client: &reqwest::blocking::Client, name: &str, rosdistro_ref: Option<&str>) -> Result<Self> {
        let index: Index = serde_yaml::from_str(&get(client, INDEX_URL)?.text()?)?;
        let info = index
            .distributions
            .get(name)
            .with_context(|| format!("{name} is not in the rosdistro index"))?;
        let mut gz = Vec::new();
        get(client, &info.distribution_cache)?.read_to_end(&mut gz)?;
        let mut text = String::new();
        flate2::read::GzDecoder::new(&gz[..]).read_to_string(&mut text)?;
        let cache: Cache = serde_yaml::from_str(&text).context("parsing the rosdistro cache")?;
        let mut releases = releases_of(&cache.distribution_file);
        let mut xmls = cache.release_package_xmls;
        if let Some(r) = rosdistro_ref {
            let index: serde_yaml::Value =
                serde_yaml::from_str(&get(client, &format!("{ROSDISTRO_RAW}/{r}/index-v4.yaml"))?.text()?)?;
            let urls: Vec<String> = match &index["distributions"][name]["distribution"] {
                serde_yaml::Value::String(u) => vec![u.clone()],
                serde_yaml::Value::Sequence(s) => {
                    s.iter().filter_map(|u| u.as_str().map(String::from)).collect()
                }
                _ => bail!("{name} has no distribution file at rosdistro {r}"),
            };
            let mut files = vec![];
            for u in urls {
                // relative to the index, i.e. the files of the same ref
                let u = if u.contains("://") {
                    u
                } else {
                    format!("{ROSDISTRO_RAW}/{r}/{u}")
                };
                files.push(serde_yaml::from_str::<DistributionFile>(
                    &get(client, &u)?.text()?,
                )?);
            }
            let at_ref = releases_of(&files);
            xmls.retain(|pkg, _| at_ref.get(pkg).is_some_and(|rel| releases.get(pkg) == Some(rel)));
            releases = at_ref;
        }
        Ok(Self {
            ros1: info.distribution_type == "ros1",
            python_version: info.python_version,
            releases,
            xmls,
        })
    }

    /// The package.xml of a release: cached, or from its release tag.
    pub fn package_xml(&self, client: &reqwest::blocking::Client, pkg: &str) -> Result<String> {
        if let Some(x) = self.xmls.get(pkg) {
            return Ok(x.clone());
        }
        let rel = self
            .releases
            .get(pkg)
            .with_context(|| format!("{pkg} is not released"))?;
        let entry = SnapshotEntry {
            url: rel.url.clone(),
            version: rel.version.clone(),
            tag: Some(rel.tag.clone()),
            rev: None,
            dependencies: None,
            repository: None,
            additional_folder: String::new(),
            package_xml_name: None,
            submodules: None,
            override_reason: None,
        };
        Ok(get(client, &Distro::raw_url(&entry)?)?.text()?)
    }

    pub fn condition_env(&self, name: &str) -> HashMap<String, String> {
        let mut env: HashMap<String, String> = std::env::vars().collect();
        env.insert("ROS_DISTRO".into(), name.into());
        env.insert("ROS_VERSION".into(), if self.ros1 { "1" } else { "2" }.into());
        env.insert("ROS_PYTHON_VERSION".into(), self.python_version.to_string());
        env
    }
}

pub struct Distro {
    pub name: String,
    pub ros1: bool,
    /// environment for package.xml conditions (REP 149)
    pub condition_env: HashMap<String, String>,
    snapshot: IndexMap<String, SnapshotEntry>,
    additional: IndexMap<String, SnapshotEntry>,
    cache_xmls: HashMap<String, String>,
    live: HashMap<String, LiveRelease>,
    fetched: Mutex<HashMap<String, String>>,
    client: reqwest::blocking::Client,
}

fn get(client: &reqwest::blocking::Client, url: &str) -> Result<reqwest::blocking::Response> {
    let mut last = None;
    for attempt in 0..5 {
        let mut req = client.get(url);
        if url.contains("github")
            && let Ok(token) = std::env::var("GITHUB_TOKEN")
        {
            req = req.header("Authorization", format!("token {token}"));
        }
        match req.send() {
            Ok(r) if r.status().is_success() => return Ok(r),
            Ok(r) if ![429, 500, 502, 503, 504].contains(&r.status().as_u16()) => {
                bail!("{} for {url}", r.status())
            }
            Ok(r) => last = Some(anyhow!("{} for {url}", r.status())),
            Err(e) => last = Some(e.into()),
        }
        std::thread::sleep(std::time::Duration::from_secs(1 << attempt));
    }
    Err(last.unwrap())
}

impl Distro {
    pub fn load(conf: &Config) -> Result<Self> {
        let client = client()?;
        let rd = Rosdistro::load(&client, &conf.ros_distro, None)?;
        Ok(Self {
            name: conf.ros_distro.clone(),
            ros1: rd.ros1,
            condition_env: rd.condition_env(&conf.ros_distro),
            snapshot: conf.snapshot.clone(),
            additional: conf.additional.clone(),
            cache_xmls: rd.xmls,
            live: rd.releases,
            fetched: Mutex::new(HashMap::new()),
            client,
        })
    }

    fn entry(&self, pkg: &str) -> Option<&SnapshotEntry> {
        self.snapshot
            .get(pkg)
            .or_else(|| self.snapshot.get(&pkg.replace('_', "-")))
    }

    pub fn has(&self, pkg: &str) -> bool {
        self.additional.contains_key(pkg) || self.entry(pkg).is_some()
    }

    pub fn version(&self, pkg: &str) -> Option<&str> {
        self.entry(pkg).map(|e| e.version.as_str())
    }

    pub fn source(&self, pkg: &str) -> Option<(String, String, &'static str)> {
        let e = self.entry(pkg)?;
        Some(match (&e.tag, &e.rev) {
            (Some(tag), _) => (e.url.clone(), tag.clone(), "tag"),
            (None, Some(rev)) => (e.url.clone(), rev.clone(), "rev"),
            _ => return None,
        })
    }

    pub fn submodules(&self, pkg: &str) -> Option<bool> {
        self.entry(pkg).and_then(|e| e.submodules)
    }

    pub fn additional_folder(&self, pkg: &str) -> &str {
        self.additional
            .get(pkg)
            .map(|e| e.additional_folder.as_str())
            .unwrap_or("")
    }

    /// The upstream repository: snapshot, then rosdistro's source entry, then
    /// the package.xml's `<url type="repository">` (never a bloom release repo).
    pub fn repository_url(&self, pkg: &str, xml_urls: &[(String, Option<String>)]) -> Option<String> {
        let strip = |u: &str| u.strip_suffix(".git").unwrap_or(u).to_string();
        let is_release = |u: &str| {
            let base = u
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or_default()
                .to_lowercase();
            let base = base.strip_suffix(".git").unwrap_or(&base);
            base.ends_with("-release") || base.ends_with("_release")
        };
        if let Some(r) = self.entry(pkg).and_then(|e| e.repository.as_deref()) {
            return Some(strip(r));
        }
        if !self.additional.contains_key(pkg)
            && let Some(src) = self.live.get(pkg).and_then(|l| l.source_url.as_deref())
            && !is_release(src)
        {
            return Some(strip(src));
        }
        xml_urls
            .iter()
            .find(|(u, t)| t.as_deref() == Some("repository") && !is_release(u))
            .map(|(u, _)| strip(u))
    }

    /// Direct ROS dependencies, from the snapshot.
    fn direct_depends(&self, pkg: &str) -> Result<BTreeSet<String>> {
        if self.additional.contains_key(pkg) {
            // all dependency tags of the package.xml, unconditionally (as vinca)
            let xml = self.package_xml(pkg)?;
            let doc = roxmltree::Document::parse(&xml)?;
            const TAGS: [&str; 8] = [
                "depend",
                "build_depend",
                "buildtool_depend",
                "buildtool_export_depend",
                "exec_depend",
                "run_depend",
                "test_depend",
                "build_export_depend",
            ];
            return Ok(doc
                .descendants()
                .filter(|n| n.is_element() && TAGS.contains(&n.tag_name().name()))
                .filter_map(|n| n.text().map(|t| t.trim().to_string()))
                .collect());
        }
        match self.entry(pkg).and_then(|e| e.dependencies.clone()) {
            Some(d) => Ok(d.into_iter().collect()),
            None => bail!(
                "the snapshot entry of {pkg} has no dependencies: regenerate the snapshot \
                 (`vinca-rs snapshot --distro <distro>`)"
            ),
        }
    }

    /// Transitive ROS dependencies, not walking through `ignore`.
    pub fn depends(&self, pkg: &str, ignore: &HashSet<String>) -> Result<BTreeSet<String>> {
        let mut deps = BTreeSet::new();
        if !self.has(pkg) {
            return Ok(deps);
        }
        let mut todo = vec![pkg.to_string()];
        let mut seen: HashSet<String> = HashSet::from([pkg.to_string()]);
        while let Some(cur) = todo.pop() {
            if ignore.contains(&cur) {
                continue;
            }
            for d in self.direct_depends(&cur)? {
                if ignore.contains(&d) || !seen.insert(d.clone()) {
                    continue;
                }
                deps.insert(d.clone());
                if self.has(&d) {
                    todo.push(d);
                }
            }
        }
        Ok(deps)
    }

    pub fn raw_url(e: &SnapshotEntry) -> Result<String> {
        let base = e.url.strip_suffix(".git").unwrap_or(&e.url);
        let Some(owner_repo) = base.split("github.com/").nth(1) else {
            bail!("only GitHub sources are supported: {}", e.url)
        };
        let reference = match (&e.rev, &e.tag) {
            (Some(rev), _) => rev.clone(),
            (None, Some(tag)) => format!("refs/tags/{tag}"),
            _ => bail!("{} has neither tag nor rev", e.url),
        };
        let folder = if e.additional_folder.is_empty() {
            String::new()
        } else {
            format!("{}/", e.additional_folder)
        };
        Ok(format!(
            "https://raw.githubusercontent.com/{owner_repo}/{reference}/{folder}{}",
            e.package_xml_name.as_deref().unwrap_or("package.xml")
        ))
    }

    /// Where the package.xml of `pkg` comes from: the cache when its release
    /// matches the snapshot, else the pinned tag/rev.
    fn xml_source(&self, pkg: &str) -> Result<Result<&String, String>> {
        let entry = self.entry(pkg).or_else(|| self.additional.get(pkg));
        let Some(e) = entry else {
            return self
                .cache_xmls
                .get(pkg)
                .map(Ok)
                .with_context(|| format!("no package.xml for {pkg}"));
        };
        let matches = [pkg.to_string(), pkg.replace('_', "-")].iter().any(|n| {
            self.live.get(n).is_some_and(|l| {
                Some(&l.url) == Some(&e.url) && e.tag.as_ref() == Some(&l.tag) && l.version == e.version
            }) && self.cache_xmls.contains_key(n)
        });
        if matches && !self.additional.contains_key(pkg) {
            let n = if self.cache_xmls.contains_key(pkg) {
                pkg.to_string()
            } else {
                pkg.replace('_', "-")
            };
            return Ok(Ok(&self.cache_xmls[&n]));
        }
        Ok(Err(Self::raw_url(e)?))
    }

    /// Download, in parallel, every package.xml of `pkgs` the cache doesn't have.
    pub fn prefetch<'a>(&self, pkgs: impl IntoIterator<Item = &'a String>) -> Result<usize> {
        let urls: BTreeSet<String> = pkgs
            .into_iter()
            .filter_map(|p| self.xml_source(p).ok()?.err())
            .collect();
        let n = urls.len();
        let results: Vec<Result<(String, String)>> = urls
            .into_par_iter()
            .map(|u| Ok((u.clone(), get(&self.client, &u)?.text()?)))
            .collect();
        let mut fetched = self.fetched.lock().unwrap();
        for r in results {
            let (u, t) = r?;
            fetched.insert(u, t);
        }
        Ok(n)
    }

    pub fn package_xml(&self, pkg: &str) -> Result<String> {
        match self.xml_source(pkg)? {
            Ok(xml) => Ok(xml.clone()),
            Err(url) => {
                if let Some(x) = self.fetched.lock().unwrap().get(&url) {
                    return Ok(x.clone());
                }
                let x = get(&self.client, &url)?.text()?;
                self.fetched.lock().unwrap().insert(url, x.clone());
                Ok(x)
            }
        }
    }

    /// The package.xml, parsed (pixi-build-ros) with its conditions evaluated.
    pub fn package(&self, pkg: &str) -> Result<PackageXml> {
        let xml = self.package_xml(pkg)?;
        Ok(PackageXml::parse(&xml)
            .map_err(|e| anyhow!("{pkg}: {e}"))?
            .evaluate_conditions(&self.condition_env))
    }
}
