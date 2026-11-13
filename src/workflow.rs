//! The GitHub Actions workflow that builds a directory of recipes.
//!
//! vinca-rs only decides *what* each job builds and which jobs it waits for; *how* a
//! job builds is the caller's: a workflow template whose `jobs.build` is copied once
//! per job. In each copy `@recipes@` becomes the job's recipes, and vinca-rs sets the
//! job's `name` and `needs`. Every other `@name@` in the template is a `--var`, and
//! `if`/`then`/`else` selectors are resolved for the target platform.
//!
//! Each job needs exactly the jobs that build its recipes' dependencies, not a
//! whole previous stage: a job starts as soon as its own dependencies are built, and
//! a failure only skips the recipes that depend on it.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_yaml::{Mapping, Value};

use crate::selectors::evaluate_for_platform;

/// GitHub refuses larger workflow files; keep a margin
const MAX_WORKFLOW_BYTES: usize = 500 * 1024 - 10 * 1024;

#[derive(Deserialize, Default)]
pub struct Timings {
    #[serde(default)]
    pub recipes: HashMap<String, f64>,
    pub default: Option<f64>,
    pub job_overhead: Option<f64>,
    pub runners: Option<usize>,
}

pub struct Options {
    pub platform: String,
    pub max_batch: usize,
    pub timings: Timings,
    pub vars: Vec<(String, String)>,
}

struct Recipe {
    deps: BTreeSet<String>,
    /// no build script: repackages only (e.g. a legacy-name alias)
    trivial: bool,
}

/// Every package name a requirement list mentions, both branches of selectors.
fn names(v: &Value, out: &mut BTreeSet<String>) {
    match v {
        Value::String(s) => {
            if let Some(n) = s.split_whitespace().next() {
                out.insert(n.to_string());
            }
        }
        Value::Sequence(s) => s.iter().for_each(|x| names(x, out)),
        Value::Mapping(m) => {
            for k in ["then", "else"] {
                if let Some(x) = m.get(k) {
                    names(x, out);
                }
            }
        }
        _ => {}
    }
}

fn read_recipes(dir: &Path) -> Result<BTreeMap<String, Recipe>> {
    let mut out = BTreeMap::new();
    for e in std::fs::read_dir(dir)? {
        let path = e?.path().join("recipe.yaml");
        if !path.is_file() {
            continue;
        }
        let r: Value = serde_yaml::from_str(&std::fs::read_to_string(&path)?)
            .with_context(|| path.display().to_string())?;
        let name = r["package"]["name"].as_str().context("package.name")?.to_string();
        let mut deps = BTreeSet::new();
        for section in ["build", "host", "run"] {
            names(&r["requirements"][section], &mut deps);
        }
        // a test environment needs its requirements built first, too
        if let Some(tests) = r.get("tests").and_then(Value::as_sequence) {
            for t in tests {
                names(&t["requirements"]["run"], &mut deps);
                names(&t["requirements"]["build"], &mut deps);
            }
        }
        let script = r["build"].get("script").and_then(Value::as_str).unwrap_or("");
        out.insert(
            name,
            Recipe {
                deps,
                trivial: script.is_empty(),
            },
        );
    }
    // only the recipes being built order the jobs
    let built: BTreeSet<String> = out.keys().cloned().collect();
    for (name, r) in out.iter_mut() {
        r.deps.retain(|d| built.contains(d) && d != name);
    }
    Ok(out)
}

/// Longest path from a recipe without dependencies; errors on a cycle.
fn levels(recipes: &BTreeMap<String, Recipe>) -> Result<BTreeMap<String, usize>> {
    fn visit(
        n: &str,
        recipes: &BTreeMap<String, Recipe>,
        level: &mut BTreeMap<String, usize>,
        path: &mut Vec<String>,
    ) -> Result<usize> {
        if let Some(l) = level.get(n) {
            return Ok(*l);
        }
        if let Some(i) = path.iter().position(|p| p == n) {
            bail!("dependency cycle: {} -> {n}", path[i..].join(" -> "));
        }
        path.push(n.to_string());
        let mut l = 0;
        for d in &recipes[n].deps {
            l = l.max(visit(d, recipes, level, path)? + 1);
        }
        path.pop();
        level.insert(n.to_string(), l);
        Ok(l)
    }
    let mut level = BTreeMap::new();
    for n in recipes.keys() {
        visit(n, recipes, &mut level, &mut vec![])?;
    }
    Ok(level)
}

struct Clock<'a> {
    t: &'a Timings,
    default: f64,
    overhead: f64,
    /// jobs that can run at once (GitHub's hosted-runner limit)
    runners: usize,
}

impl Clock<'_> {
    fn secs(&self, p: &str) -> f64 {
        self.t.recipes.get(p).copied().unwrap_or(self.default)
    }
    fn job(&self, j: &[String]) -> f64 {
        self.overhead + j.iter().map(|p| self.secs(p)).sum::<f64>()
    }
}

/// The jobs: each a list of recipes, built in order.
///
/// Earliest-finish-time clustering: recipes are taken in order of how much work
/// depends on them (longest remaining chain first); each goes at the end of a job
/// whose start doesn't have to move for it (the job of its latest-finishing
/// dependency, typically) or into a new job that starts as soon as its
/// dependencies are built, whichever builds it first. A new job costs the job
/// overhead plus `slack` seconds: more slack, fewer and fuller jobs.
fn batch(
    recipes: &BTreeMap<String, Recipe>,
    clock: &Clock,
    max_batch: usize,
    slack: f64,
) -> Result<Vec<Vec<String>>> {
    levels(recipes)?; // no cycles
    // a trivial recipe (e.g. a legacy-name alias) with a single non-trivial
    // dependency is built right after it, in its job
    let mut followers: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut leader: HashMap<&String, &String> = HashMap::new();
    for (n, r) in recipes {
        let real: Vec<&String> = r.deps.iter().filter(|d| !recipes[*d].trivial).collect();
        if r.trivial && real.len() == 1 {
            followers.entry(real[0].clone()).or_default().push(n.clone());
            leader.insert(n, real[0]);
        }
    }
    let lead = |n: &'_ String| -> String { leader.get(n).map(|l| (*l).clone()).unwrap_or_else(|| n.clone()) };
    let sched: Vec<&String> = recipes.keys().filter(|n| !leader.contains_key(n)).collect();
    let deps: HashMap<&String, BTreeSet<String>> = sched
        .iter()
        .map(|n| {
            let mut d: BTreeSet<String> = recipes[*n].deps.iter().map(&lead).collect();
            d.extend(
                followers
                    .get(*n)
                    .into_iter()
                    .flatten()
                    .flat_map(|f| recipes[f].deps.iter().map(&lead)),
            );
            d.remove(*n);
            (*n, d)
        })
        .collect();
    let mut dependents: HashMap<&str, Vec<&String>> = HashMap::new();
    for (n, ds) in &deps {
        for d in ds {
            dependents.entry(d.as_str()).or_default().push(n);
        }
    }
    // longest chain of work from a recipe to the end
    fn rest<'a>(
        n: &'a str,
        dependents: &HashMap<&str, Vec<&'a String>>,
        clock: &Clock,
        memo: &mut HashMap<&'a str, f64>,
    ) -> f64 {
        if let Some(v) = memo.get(n) {
            return *v;
        }
        let tail = dependents
            .get(n)
            .into_iter()
            .flatten()
            .map(|m| rest(m.as_str(), dependents, clock, memo))
            .fold(0.0, f64::max);
        let v = clock.secs(n) + tail;
        memo.insert(n, v);
        v
    }
    let mut memo = HashMap::new();
    let prio: HashMap<&String, f64> = sched
        .iter()
        .map(|n| (*n, rest(n.as_str(), &dependents, clock, &mut memo)))
        .collect();

    struct Job {
        recipes: Vec<String>,
        start: f64,
        end: f64,
    }
    let mut jobs: Vec<Job> = vec![];
    let mut placed: HashMap<String, (usize, f64)> = HashMap::new();
    let mut waiting: HashMap<&String, usize> = deps.iter().map(|(n, d)| (*n, d.len())).collect();
    let mut ready: Vec<&String> = sched.iter().copied().filter(|n| deps[*n].is_empty()).collect();
    while !ready.is_empty() {
        // the ready recipe with the most work after it
        let i = (0..ready.len())
            .max_by(|&a, &b| {
                prio[ready[a]]
                    .total_cmp(&prio[ready[b]])
                    .then(ready[b].cmp(ready[a]))
            })
            .unwrap();
        let p = ready.swap_remove(i);
        let own = clock.secs(p)
            + followers
                .get(p)
                .into_iter()
                .flatten()
                .map(|f| clock.secs(f))
                .sum::<f64>();
        let dep_jobs: Vec<(usize, f64)> = deps[p].iter().map(|d| placed[d]).collect();
        let at = dep_jobs.iter().map(|(_, f)| *f).fold(0.0, f64::max);
        let fresh = at + clock.overhead + own;
        let best = jobs
            .iter()
            .enumerate()
            .filter(|(j, job)| {
                job.recipes.len() < max_batch
                    && dep_jobs.iter().all(|(k, f)| k == j || *f <= job.start + 1e-6)
            })
            .map(|(j, job)| (j, job.end + own))
            .min_by(|a, b| a.1.total_cmp(&b.1));
        let (j, end) = match best {
            Some((j, end)) if end <= fresh + slack => (j, end),
            _ => {
                jobs.push(Job {
                    recipes: vec![],
                    start: at,
                    end: at + clock.overhead,
                });
                (jobs.len() - 1, fresh)
            }
        };
        jobs[j].recipes.push(p.clone());
        jobs[j].end = end;
        placed.insert(p.clone(), (j, end));
        for m in dependents.get(p.as_str()).into_iter().flatten() {
            let w = waiting.get_mut(m).unwrap();
            *w -= 1;
            if *w == 0 {
                ready.push(m);
            }
        }
    }
    Ok(jobs
        .into_iter()
        .map(|job| {
            job.recipes
                .into_iter()
                .flat_map(|p| {
                    std::iter::once(p.clone()).chain(followers.get(&p).cloned().unwrap_or_default())
                })
                .collect()
        })
        .collect())
}

fn substitute(v: &mut Value, from: &str, to: &str) {
    match v {
        Value::String(s) => *s = s.replace(from, to),
        Value::Sequence(seq) => seq.iter_mut().for_each(|x| substitute(x, from, to)),
        Value::Mapping(m) => m.iter_mut().for_each(|(_, x)| substitute(x, from, to)),
        _ => {}
    }
}

/// A readable job name: the recipes without their ros2- / ros-<distro>- prefix.
fn job_name(job: &[String]) -> String {
    let mut seen = vec![];
    for p in job {
        let short = p
            .strip_prefix("ros2-")
            .or_else(|| {
                p.strip_prefix("ros-")
                    .and_then(|r| r.split_once('-').map(|(_, s)| s))
            })
            .unwrap_or(p);
        if !seen.contains(&short) {
            seen.push(short);
        }
    }
    let name = seen.join(" ");
    if name.len() > 100 {
        format!("{}…", &name[..name.floor_char_boundary(99)])
    } else {
        name
    }
}

fn render(template: &Value, jobs: &[Vec<String>], recipes: &BTreeMap<String, Recipe>) -> Result<String> {
    let mut wf = template.clone();
    let job_tpl = wf["jobs"]["build"].clone();
    if !job_tpl.is_mapping() {
        bail!("the workflow template has no jobs.build");
    }
    let owner: HashMap<&String, usize> = jobs
        .iter()
        .enumerate()
        .flat_map(|(i, j)| j.iter().map(move |p| (p, i)))
        .collect();
    let mut out = Mapping::new();
    for (i, job) in jobs.iter().enumerate() {
        let mut j = job_tpl.clone();
        substitute(&mut j, "@recipes@", &job.join(" "));
        let needs: BTreeSet<usize> = job
            .iter()
            .flat_map(|p| recipes[p].deps.iter().map(|d| owner[d]))
            .filter(|&o| o != i)
            .collect();
        // name and needs first, then the template's keys
        let mut m = Mapping::new();
        m.insert("name".into(), job_name(job).into());
        if !needs.is_empty() {
            let needs: Vec<Value> = needs.into_iter().map(|n| format!("job_{n}").into()).collect();
            m.insert("needs".into(), Value::Sequence(needs));
        }
        for (k, v) in j.as_mapping().unwrap() {
            if !m.contains_key(k) {
                m.insert(k.clone(), v.clone());
            }
        }
        out.insert(format!("job_{i}").into(), Value::Mapping(m));
    }
    wf["jobs"] = Value::Mapping(out);
    // YAML 1.1 readers (GitHub's included) take a plain `on` for `true` and back
    Ok(serde_yaml::to_string(&wf)?.replace("'on':", "on:"))
}

/// Write the workflow for `recipes` to `out`; false when there is nothing to build.
pub fn generate(recipes_dir: &Path, template: &Path, out: &Path, opts: &Options) -> Result<bool> {
    let recipes = read_recipes(recipes_dir)?;
    if recipes.is_empty() {
        return Ok(false);
    }
    let mut text = std::fs::read_to_string(template).with_context(|| template.display().to_string())?;
    text = text.replace("@platform@", &opts.platform);
    for (k, v) in &opts.vars {
        text = text.replace(&format!("@{k}@"), v);
    }
    let template = evaluate_for_platform(&serde_yaml::from_str(&text)?, &opts.platform);

    let mut known: Vec<f64> = opts.timings.recipes.values().copied().collect();
    known.sort_by(f64::total_cmp);
    let clock = Clock {
        t: &opts.timings,
        default: opts
            .timings
            .default
            .or(known.get(known.len() / 2).copied())
            .unwrap_or(20.0),
        overhead: opts.timings.job_overhead.unwrap_or(30.0),
        runners: opts
            .timings
            .runners
            .unwrap_or(if opts.platform.starts_with("osx") { 5 } else { 20 }),
    };
    // the schedule that finishes first on the runners and fits GitHub's size
    // limit: more slack, fewer and fuller jobs
    let mut best: Option<(f64, f64, Vec<Vec<String>>, String)> = None;
    for slack in [
        0.0,
        30.0,
        60.0,
        120.0,
        180.0,
        300.0,
        600.0,
        1200.0,
        2400.0,
        3600.0,
        f64::INFINITY,
    ] {
        let jobs = batch(&recipes, &clock, opts.max_batch, slack)?;
        let yaml = render(&template, &jobs, &recipes)?;
        let span = simulate(&jobs, &recipes, &clock);
        if std::env::var_os("VINCA_RS_DEBUG").is_some() {
            eprintln!(
                "slack {slack}: {} jobs, {:.0} min, {} KiB",
                jobs.len(),
                span / 60.0,
                yaml.len() / 1024
            );
        }
        if yaml.len() <= MAX_WORKFLOW_BYTES && best.as_ref().is_none_or(|b| span < b.0) {
            best = Some((span, slack, jobs, yaml));
        }
    }
    let Some((span, slack, jobs, yaml)) = best else {
        bail!(
            "no schedule fits GitHub's {} KiB workflow limit: raise --max-batch",
            MAX_WORKFLOW_BYTES / 1024
        )
    };
    std::fs::write(out, &yaml)?;
    eprintln!(
        "{} recipes in {} jobs (slack {slack} s): {:.0} min on {} runners (estimate); {} KiB",
        recipes.len(),
        jobs.len(),
        span / 60.0,
        clock.runners,
        yaml.len() / 1024
    );
    Ok(true)
}

/// When the last job ends: a job starts once its needs are done and a runner is
/// free (earliest-created first), and takes the overhead plus its recipes' times.
fn simulate(jobs: &[Vec<String>], recipes: &BTreeMap<String, Recipe>, clock: &Clock) -> f64 {
    let owner: HashMap<&String, usize> = jobs
        .iter()
        .enumerate()
        .flat_map(|(i, j)| j.iter().map(move |p| (p, i)))
        .collect();
    let needs: Vec<BTreeSet<usize>> = jobs
        .iter()
        .enumerate()
        .map(|(i, j)| {
            j.iter()
                .flat_map(|p| recipes[p].deps.iter().map(|d| owner[d]))
                .filter(|&o| o != i)
                .collect()
        })
        .collect();
    let mut done = vec![false; jobs.len()];
    let mut started = vec![false; jobs.len()];
    let mut running: Vec<(f64, usize)> = vec![];
    let mut now = 0.0f64;
    let mut left = jobs.len();
    while left > 0 {
        for i in 0..jobs.len() {
            if running.len() >= clock.runners {
                break;
            }
            if !started[i] && needs[i].iter().all(|&n| done[n]) {
                started[i] = true;
                running.push((now + clock.job(&jobs[i]), i));
            }
        }
        let k = (0..running.len())
            .min_by(|&a, &b| running[a].0.total_cmp(&running[b].0))
            .unwrap();
        let (t, i) = running.swap_remove(k);
        now = t;
        done[i] = true;
        left -= 1;
    }
    now
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(deps: &[&str], trivial: bool) -> Recipe {
        Recipe {
            deps: deps.iter().map(|s| s.to_string()).collect(),
            trivial,
        }
    }

    #[test]
    fn cycle_is_an_error() {
        let recipes = BTreeMap::from([
            ("a".to_string(), r(&["b"], false)),
            ("b".to_string(), r(&["a"], false)),
        ]);
        assert!(levels(&recipes).unwrap_err().to_string().contains("cycle"));
    }

    #[test]
    fn needs_only_the_dependencies_jobs() {
        // a <- b, c independent; a and c are slow, so they get their own jobs
        let recipes = BTreeMap::from([
            ("a".to_string(), r(&[], false)),
            ("b".to_string(), r(&["a"], false)),
            ("c".to_string(), r(&[], false)),
        ]);
        let t = Timings {
            recipes: HashMap::from([("a".into(), 600.0), ("b".into(), 600.0), ("c".into(), 600.0)]),
            ..Default::default()
        };
        let clock = Clock {
            t: &t,
            default: 600.0,
            overhead: 30.0,
            runners: 20,
        };
        let jobs = batch(&recipes, &clock, 5, 0.0).unwrap();
        // b after a in a's job (no new job overhead); c in parallel
        assert_eq!(
            jobs,
            vec![vec!["a".to_string(), "b".to_string()], vec!["c".to_string()]]
        );
        let tpl: Value =
            serde_yaml::from_str("on: push\njobs:\n  build:\n    steps: [{run: 'build @recipes@'}]").unwrap();
        let yaml = render(&tpl, &jobs, &recipes).unwrap();
        let wf: Value = serde_yaml::from_str(&yaml).unwrap();
        assert!(wf["jobs"]["job_0"].get("needs").is_none());
        assert!(wf["jobs"]["job_1"].get("needs").is_none());
        assert_eq!(wf["jobs"]["job_0"]["steps"][0]["run"], "build a b");
        assert!(yaml.starts_with("on:"));
    }

    #[test]
    fn dependent_on_two_jobs_waits_for_both() {
        // d needs a and c, which run in parallel: d goes after the later one
        let recipes = BTreeMap::from([
            ("a".to_string(), r(&[], false)),
            ("c".to_string(), r(&[], false)),
            ("d".to_string(), r(&["a", "c"], false)),
        ]);
        let t = Timings {
            recipes: HashMap::from([("a".into(), 600.0), ("c".into(), 300.0), ("d".into(), 60.0)]),
            ..Default::default()
        };
        let clock = Clock {
            t: &t,
            default: 60.0,
            overhead: 30.0,
            runners: 20,
        };
        let jobs = batch(&recipes, &clock, 5, 0.0).unwrap();
        assert_eq!(
            jobs,
            vec![
                vec!["a".to_string()],
                vec!["c".to_string()],
                vec!["d".to_string()]
            ]
        );
    }

    #[test]
    fn aliases_follow_their_package() {
        let recipes = BTreeMap::from([
            ("ros2-x".to_string(), r(&[], false)),
            ("ros-humble-x".to_string(), r(&["ros2-x"], true)),
        ]);
        let t = Timings::default();
        let clock = Clock {
            t: &t,
            default: 20.0,
            overhead: 30.0,
            runners: 20,
        };
        assert_eq!(
            batch(&recipes, &clock, 5, 0.0).unwrap(),
            vec![vec!["ros2-x".to_string(), "ros-humble-x".to_string()]]
        );
    }
}
