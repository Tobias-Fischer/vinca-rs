//! The recipes of a small offline distribution (tests/fixture), against the ones
//! checked in under tests/fixture/expected/<platform>. The package.xml files come
//! from tests/fixture/package_xml as the rosdistro cache, so nothing is fetched.
//! After an intended change: `VINCA_RS_UPDATE_GOLDEN=1 cargo test golden`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use fs_err as fs;
use rattler_conda_types::Subdir;

use super::*;
use crate::distro::{Distro, LiveRelease, Rosdistro};

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixture");

/// Every file under `dir` but the build/activation scripts (rendered templates),
/// by relative path.
fn files(dir: &Path) -> BTreeMap<PathBuf, String> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, String>) {
        for e in fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                walk(root, &p, out);
            } else if !p.extension().is_some_and(|e| e == "sh" || e == "bat" || e == "ps1") {
                out.insert(p.strip_prefix(root).unwrap().into(), fs::read_to_string(&p).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    if dir.is_dir() {
        walk(dir, dir, &mut out);
    }
    out
}

fn generate(platform: Subdir, out: &Path) {
    let conf = config::read(&Path::new(FIXTURE).join("humble/vinca.yaml"), platform).unwrap();
    // the snapshot's releases, with their package.xml as the cache's
    let mut rd =
        Rosdistro { ros1: false, python_version: 3, releases: Default::default(), xmls: Default::default() };
    for (name, e) in &conf.snapshot {
        let release = LiveRelease {
            url: e.url.clone(),
            tag: e.tag.clone().unwrap(),
            version: e.version.clone(),
            source_url: None,
        };
        rd.releases.insert(name.clone(), release);
        let xml = fs::read_to_string(Path::new(FIXTURE).join(format!("package_xml/{name}.xml"))).unwrap();
        rd.xmls.insert(name.clone(), xml);
    }
    let distro = Distro::new(&conf, rd, distro::client().unwrap());
    let mut g = generate::Generator::new(&conf, &distro);
    write_recipes(&mut g, out).unwrap();
    assert!(g.unsatisfied.is_empty(), "unsatisfied: {:?}", g.unsatisfied);
}

#[test]
fn golden_recipes() {
    for platform in [Subdir::Linux64, Subdir::Win64] {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("recipes");
        generate(platform, &out);
        let expected = Path::new(FIXTURE).join("expected").join(platform.as_str());
        if std::env::var_os("VINCA_RS_UPDATE_GOLDEN").is_some() {
            let _ = fs::remove_dir_all(&expected);
            for (p, text) in files(&out) {
                fs::create_dir_all(expected.join(&p).parent().unwrap()).unwrap();
                fs::write(expected.join(&p), text).unwrap();
            }
        }
        let (got, want) = (files(&out), files(&expected));
        assert_eq!(got.keys().collect::<Vec<_>>(), want.keys().collect::<Vec<_>>(), "{platform}: files");
        for (p, text) in &got {
            assert_eq!(text, &want[p], "{platform}: {}", p.display());
        }
    }
}
