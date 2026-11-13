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
use std::path::{Path, PathBuf};

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
    /// the jobs run in stages that each wait for the whole previous one (e.g.
    /// fixed matrix slots), not only for their needs: schedule for that
    pub stage_barriers: bool,
    /// workflow template: its jobs.build is copied for every job
    pub template: Option<PathBuf>,
    /// the GitHub Actions workflow to write (from the template)
    pub out: Option<PathBuf>,
    /// the jobs as JSON (`{"stages": [[{"id", "name", "recipes", "needs"}]]}`, a job's
    /// stage being one more than its latest need's; RoboStack/vinca#201's format)
    pub json: Option<PathBuf>,
}

/// Each job a list of recipes, built in order.
type Jobs = Vec<Vec<String>>;

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
        out.insert(name, Recipe { deps, trivial: script.is_empty() });
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

/// The recipes to schedule: legacy-name aliases (trivial recipes with one
/// non-trivial dependency) ride along with that dependency, and each recipe knows
/// how much work depends on it.
struct Graph<'a> {
    followers: BTreeMap<String, Vec<String>>,
    sched: Vec<&'a String>,
    deps: HashMap<&'a String, BTreeSet<String>>,
    dependents: HashMap<String, Vec<&'a String>>,
    /// longest chain of work from the recipe to the end
    prio: HashMap<&'a String, f64>,
}

impl<'a> Graph<'a> {
    fn new(recipes: &'a BTreeMap<String, Recipe>, clock: &Clock) -> Result<Self> {
        levels(recipes)?; // no cycles
        let mut followers: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut leader: HashMap<&String, &String> = HashMap::new();
        for (n, r) in recipes {
            let real: Vec<&String> = r.deps.iter().filter(|d| !recipes[*d].trivial).collect();
            if r.trivial && real.len() == 1 {
                followers.entry(real[0].clone()).or_default().push(n.clone());
                leader.insert(n, real[0]);
            }
        }
        let lead =
            |n: &String| -> String { leader.get(n).map(|l| (*l).clone()).unwrap_or_else(|| n.clone()) };
        let sched: Vec<&String> = recipes.keys().filter(|n| !leader.contains_key(n)).collect();
        let deps: HashMap<&String, BTreeSet<String>> = sched
            .iter()
            .map(|n| {
                let mut d: BTreeSet<String> = recipes[*n].deps.iter().map(lead).collect();
                d.extend(
                    followers.get(*n).into_iter().flatten().flat_map(|f| recipes[f].deps.iter().map(lead)),
                );
                d.remove(*n);
                (*n, d)
            })
            .collect();
        let mut dependents: HashMap<String, Vec<&String>> = HashMap::new();
        for (n, ds) in &deps {
            for d in ds {
                dependents.entry(d.clone()).or_default().push(n);
            }
        }
        fn rest(
            n: &str,
            g: &HashMap<String, Vec<&String>>,
            clock: &Clock,
            memo: &mut HashMap<String, f64>,
        ) -> f64 {
            if let Some(v) = memo.get(n) {
                return *v;
            }
            let tail = g.get(n).into_iter().flatten().map(|m| rest(m, g, clock, memo)).fold(0.0, f64::max);
            let v = clock.secs(n) + tail;
            memo.insert(n.to_string(), v);
            v
        }
        let mut memo = HashMap::new();
        let prio = sched.iter().map(|n| (*n, rest(n, &dependents, clock, &mut memo))).collect();
        Ok(Self { followers, sched, deps, dependents, prio })
    }

    /// The recipe's own time, its followers included.
    fn own(&self, p: &str, clock: &Clock) -> f64 {
        clock.secs(p) + self.followers.get(p).into_iter().flatten().map(|f| clock.secs(f)).sum::<f64>()
    }

    /// Jobs of scheduled recipes, with their followers.
    fn expand(&self, jobs: Vec<Vec<String>>) -> Vec<Vec<String>> {
        jobs.into_iter()
            .map(|job| {
                job.into_iter()
                    .flat_map(|p| {
                        std::iter::once(p.clone()).chain(self.followers.get(&p).cloned().unwrap_or_default())
                    })
                    .collect()
            })
            .collect()
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
    let g = Graph::new(recipes, clock)?;
    let (deps, dependents, prio) = (&g.deps, &g.dependents, &g.prio);
    struct Job {
        recipes: Vec<String>,
        start: f64,
        end: f64,
    }
    let mut jobs: Vec<Job> = vec![];
    let mut placed: HashMap<String, (usize, f64)> = HashMap::new();
    let mut waiting: HashMap<&String, usize> = deps.iter().map(|(n, d)| (*n, d.len())).collect();
    let mut ready: Vec<&String> = g.sched.iter().copied().filter(|n| deps[*n].is_empty()).collect();
    while !ready.is_empty() {
        // the ready recipe with the most work after it
        let i = (0..ready.len())
            .max_by(|&a, &b| prio[ready[a]].total_cmp(&prio[ready[b]]).then(ready[b].cmp(ready[a])))
            .unwrap();
        let p = ready.swap_remove(i);
        let own = g.own(p, clock);
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
                jobs.push(Job { recipes: vec![], start: at, end: at + clock.overhead });
                (jobs.len() - 1, fresh)
            }
        };
        jobs[j].recipes.push(p.clone());
        jobs[j].end = end;
        placed.insert(p.clone(), (j, end));
        for m in dependents.get(p).into_iter().flatten() {
            let w = waiting.get_mut(m).unwrap();
            *w -= 1;
            if *w == 0 {
                ready.push(m);
            }
        }
    }
    Ok(g.expand(jobs.into_iter().map(|j| j.recipes).collect()))
}

/// Jobs for a CI that runs stage after stage (each waits for the whole previous
/// one), with each job's stage. A stage takes as long as its longest job, so each
/// stage gets one length: the longest ready recipe, or the stage's work spread
/// over the runners if that is longer. The ready recipes are packed into jobs of
/// at most that length (longest first), and a recipe whose dependencies are all
/// built, or in one job of the stage, follows them into that job while it fits.
fn batch_staged(
    recipes: &BTreeMap<String, Recipe>,
    clock: &Clock,
    max_batch: usize,
) -> Result<(Vec<Vec<String>>, Vec<usize>)> {
    let g = Graph::new(recipes, clock)?;
    let mut built: BTreeSet<String> = BTreeSet::new();
    let mut left: BTreeSet<&String> = g.sched.iter().copied().collect();
    let (mut jobs, mut stages): (Vec<Vec<String>>, Vec<usize>) = (vec![], vec![]);
    let mut stage = 0;
    while !left.is_empty() {
        let mut ready: Vec<&String> = left.iter().copied().filter(|n| g.deps[*n].is_subset(&built)).collect();
        ready.sort_by(|a, b| g.prio[*b].total_cmp(&g.prio[*a]).then(a.cmp(b)));
        let work: f64 = ready.iter().map(|p| g.own(p, clock)).sum();
        let longest = ready.iter().map(|p| g.own(p, clock)).fold(0.0, f64::max);
        let length = clock.overhead + longest.max(work / clock.runners as f64);
        // (recipes, load)
        let mut stage_jobs: Vec<(Vec<String>, f64)> = vec![];
        let mut in_job: HashMap<String, usize> = HashMap::new();
        let place = |p: &String,
                     job: Option<usize>,
                     stage_jobs: &mut Vec<(Vec<String>, f64)>,
                     in_job: &mut HashMap<String, usize>| {
            let t = g.own(p, clock);
            let j = job
                .or_else(|| {
                    (0..stage_jobs.len())
                        .filter(|&j| {
                            stage_jobs[j].0.len() < max_batch && stage_jobs[j].1 + t <= length + 1e-6
                        })
                        .min_by(|&a, &b| stage_jobs[a].1.total_cmp(&stage_jobs[b].1))
                })
                .unwrap_or_else(|| {
                    stage_jobs.push((vec![], clock.overhead));
                    stage_jobs.len() - 1
                });
            stage_jobs[j].0.push(p.clone());
            stage_jobs[j].1 += t;
            in_job.insert(p.clone(), j);
        };
        for p in &ready {
            place(p, None, &mut stage_jobs, &mut in_job);
        }
        // follow the dependencies into their job
        loop {
            let mut moved = false;
            let mut candidates: Vec<&String> = left
                .iter()
                .copied()
                .filter(|n| !in_job.contains_key(*n) && !g.deps[*n].is_subset(&built))
                .collect();
            candidates.sort_by(|a, b| g.prio[*b].total_cmp(&g.prio[*a]).then(a.cmp(b)));
            for c in candidates {
                let mut host: Option<usize> = None;
                let fits = g.deps[c].iter().all(|d| {
                    if built.contains(d) {
                        return true;
                    }
                    match in_job.get(d) {
                        Some(&j) if host.is_none_or(|h| h == j) => {
                            host = Some(j);
                            true
                        }
                        _ => false,
                    }
                });
                if let (true, Some(j)) = (fits, host)
                    && stage_jobs[j].0.len() < max_batch
                    && stage_jobs[j].1 + g.own(c, clock) <= length + 1e-6
                {
                    place(c, Some(j), &mut stage_jobs, &mut in_job);
                    moved = true;
                }
            }
            if !moved {
                break;
            }
        }
        for (job, _) in stage_jobs {
            for p in &job {
                built.insert(p.clone());
                left.remove(p);
            }
            jobs.push(job);
            stages.push(stage);
        }
        stage += 1;
    }
    Ok((g.expand(jobs), stages))
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
            .or_else(|| p.strip_prefix("ros-").and_then(|r| r.split_once('-').map(|(_, s)| s)))
            .unwrap_or(p);
        if !seen.contains(&short) {
            seen.push(short);
        }
    }
    let name = seen.join(" ");
    if name.len() > 100 { format!("{}…", &name[..name.floor_char_boundary(99)]) } else { name }
}

fn render(template: &Value, jobs: &[Vec<String>], needs: &[BTreeSet<usize>]) -> Result<String> {
    let mut wf = template.clone();
    let job_tpl = wf["jobs"]["build"].clone();
    if !job_tpl.is_mapping() {
        bail!("the workflow template has no jobs.build");
    }
    let mut out = Mapping::new();
    for (i, job) in jobs.iter().enumerate() {
        let mut j = job_tpl.clone();
        substitute(&mut j, "@recipes@", &job.join(" "));
        // name and needs first, then the template's keys
        let mut m = Mapping::new();
        m.insert("name".into(), job_name(job).into());
        if !needs[i].is_empty() {
            let needs: Vec<Value> = needs[i].iter().map(|n| format!("job_{n}").into()).collect();
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

/// Write the outputs for the recipes; false when there is nothing to build.
pub fn generate(recipes_dir: &Path, opts: &Options) -> Result<bool> {
    let recipes = read_recipes(recipes_dir)?;
    if recipes.is_empty() {
        return Ok(false);
    }
    let template = match (&opts.template, &opts.out) {
        (Some(t), Some(_)) => {
            let mut text = std::fs::read_to_string(t).with_context(|| t.display().to_string())?;
            text = text.replace("@platform@", &opts.platform);
            for (k, v) in &opts.vars {
                text = text.replace(&format!("@{k}@"), v);
            }
            Some(evaluate_for_platform(&serde_yaml::from_str(&text)?, &opts.platform))
        }
        (None, Some(_)) => bail!("a workflow needs --template"),
        _ => None,
    };

    let mut known: Vec<f64> = opts.timings.recipes.values().copied().collect();
    known.sort_by(f64::total_cmp);
    let clock = Clock {
        t: &opts.timings,
        default: opts.timings.default.or(known.get(known.len() / 2).copied()).unwrap_or(20.0),
        overhead: opts.timings.job_overhead.unwrap_or(30.0),
        runners: opts.timings.runners.unwrap_or(if opts.platform.starts_with("osx") { 5 } else { 20 }),
    };
    // the schedule that finishes first on the runners (and fits GitHub's size
    // limit): more slack, fewer and fuller jobs
    // (simulated duration, how it was planned, jobs, their needs and stages, workflow)
    type Schedule = (f64, String, Jobs, Vec<BTreeSet<usize>>, Vec<usize>, Option<String>);
    // (how it was planned, jobs, their stages if not by their needs)
    let mut candidates: Vec<(String, Jobs, Option<Vec<usize>>)> = vec![];
    for slack in [0.0, 30.0, 60.0, 120.0, 180.0, 300.0, 600.0, 1200.0, 2400.0, 3600.0, f64::INFINITY] {
        let jobs = batch(&recipes, &clock, opts.max_batch, slack)?;
        candidates.push((format!("clustered, slack {slack} s"), jobs, None));
    }
    if opts.stage_barriers {
        let (jobs, stages) = batch_staged(&recipes, &clock, opts.max_batch)?;
        candidates.push(("stage by stage".into(), jobs, Some(stages)));
    }
    let mut best: Option<Schedule> = None;
    for (how, jobs, stages) in candidates {
        let needs = job_needs(&jobs, &recipes);
        let stages = stages.unwrap_or_else(|| job_stages(&needs));
        let yaml = template.as_ref().map(|t| render(t, &jobs, &needs)).transpose()?;
        let span = simulate(&jobs, &stages, &needs, &clock, opts.stage_barriers);
        let size = yaml.as_ref().map_or(0, String::len);
        if std::env::var_os("VINCA_RS_DEBUG").is_some() {
            eprintln!("{how}: {} jobs, {:.0} min, {} KiB", jobs.len(), span / 60.0, size / 1024);
        }
        if size <= MAX_WORKFLOW_BYTES && best.as_ref().is_none_or(|b| span < b.0) {
            best = Some((span, how, jobs, needs, stages, yaml));
        }
    }
    let Some((span, how, jobs, needs, stages, yaml)) = best else {
        bail!("no schedule fits GitHub's {} KiB workflow limit: raise --max-batch", MAX_WORKFLOW_BYTES / 1024)
    };
    if let (Some(out), Some(yaml)) = (&opts.out, &yaml) {
        std::fs::write(out, yaml).with_context(|| out.display().to_string())?;
    }
    if let Some(out) = &opts.json {
        std::fs::write(out, serde_json::to_string_pretty(&stages_json(&jobs, &stages, &needs))?)
            .with_context(|| out.display().to_string())?;
    }
    eprintln!(
        "{} recipes in {} jobs ({how}): {:.0} min on {} runners (estimate){}",
        recipes.len(),
        jobs.len(),
        span / 60.0,
        clock.runners,
        yaml.map(|y| format!("; {} KiB", y.len() / 1024)).unwrap_or_default()
    );
    Ok(true)
}

fn job_needs(jobs: &[Vec<String>], recipes: &BTreeMap<String, Recipe>) -> Vec<BTreeSet<usize>> {
    let owner: HashMap<&String, usize> =
        jobs.iter().enumerate().flat_map(|(i, j)| j.iter().map(move |p| (p, i))).collect();
    jobs.iter()
        .enumerate()
        .map(|(i, j)| {
            j.iter().flat_map(|p| recipes[p].deps.iter().map(|d| owner[d])).filter(|&o| o != i).collect()
        })
        .collect()
}

/// Each job's stage: one more than its latest need's.
fn job_stages(needs: &[BTreeSet<usize>]) -> Vec<usize> {
    fn stage_of(i: usize, needs: &[BTreeSet<usize>], memo: &mut [Option<usize>]) -> usize {
        if let Some(s) = memo[i] {
            return s;
        }
        let s = needs[i].iter().map(|&n| stage_of(n, needs, memo) + 1).max().unwrap_or(0);
        memo[i] = Some(s);
        s
    }
    let mut memo = vec![None; needs.len()];
    (0..needs.len()).map(|i| stage_of(i, needs, &mut memo)).collect()
}

fn stages_json(jobs: &[Vec<String>], stage: &[usize], needs: &[BTreeSet<usize>]) -> serde_json::Value {
    // ids as RoboStack/vinca#201 writes them: stage_<stage>_job_<index in the stage>
    let mut id = vec![String::new(); jobs.len()];
    let mut count = vec![0usize; stage.iter().max().map_or(0, |m| m + 1)];
    for i in 0..jobs.len() {
        id[i] = format!("stage_{}_job_{}", stage[i], count[stage[i]]);
        count[stage[i]] += 1;
    }
    let mut stages: Vec<Vec<serde_json::Value>> = vec![vec![]; count.len()];
    for (i, job) in jobs.iter().enumerate() {
        stages[stage[i]].push(serde_json::json!({
            "id": id[i],
            "name": job_name(job),
            "recipes": job.join(" "),
            "needs": needs[i].iter().map(|&n| id[n].clone()).collect::<Vec<_>>(),
        }));
    }
    serde_json::json!({ "stages": stages })
}

/// When the last job ends: a job starts once its needs are done and a runner is
/// free (earliest-created first), and takes the overhead plus its recipes' times.
fn simulate(
    jobs: &[Vec<String>],
    stage: &[usize],
    needs: &[BTreeSet<usize>],
    clock: &Clock,
    barriers: bool,
) -> f64 {
    let mut open_in_stage = vec![0usize; stage.iter().max().map_or(0, |m| m + 1)];
    for &s in stage {
        open_in_stage[s] += 1;
    }
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
            let ready = if barriers {
                open_in_stage[..stage[i]].iter().all(|&n| n == 0)
            } else {
                needs[i].iter().all(|&n| done[n])
            };
            if !started[i] && ready {
                started[i] = true;
                running.push((now + clock.job(&jobs[i]), i));
            }
        }
        let k = (0..running.len()).min_by(|&a, &b| running[a].0.total_cmp(&running[b].0)).unwrap();
        let (t, i) = running.swap_remove(k);
        now = t;
        done[i] = true;
        open_in_stage[stage[i]] -= 1;
        left -= 1;
    }
    now
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (name, dependencies, trivial)
    fn recipes(list: &[(&str, &[&str], bool)]) -> BTreeMap<String, Recipe> {
        let recipe =
            |deps: &[&str], trivial| Recipe { deps: deps.iter().map(|s| s.to_string()).collect(), trivial };
        list.iter().map(|(n, deps, trivial)| (n.to_string(), recipe(deps, *trivial))).collect()
    }

    fn timings(secs: &[(&str, f64)]) -> Timings {
        Timings { recipes: secs.iter().map(|(n, s)| (n.to_string(), *s)).collect(), ..Default::default() }
    }

    #[test]
    fn cycle_is_an_error() {
        let recipes = recipes(&[("a", &["b"], false), ("b", &["a"], false)]);
        assert!(levels(&recipes).unwrap_err().to_string().contains("cycle"));
    }

    #[test]
    fn needs_only_the_dependencies_jobs() {
        // a <- b, c independent; a and c are slow, so they get their own jobs
        let recipes = recipes(&[("a", &[], false), ("b", &["a"], false), ("c", &[], false)]);
        let t = timings(&[("a", 600.0), ("b", 600.0), ("c", 600.0)]);
        let clock = Clock { t: &t, default: 600.0, overhead: 30.0, runners: 20 };
        let jobs = batch(&recipes, &clock, 5, 0.0).unwrap();
        // b after a in a's job (no new job overhead); c in parallel
        assert_eq!(jobs, [vec!["a", "b"], vec!["c"]]);
        let tpl: Value =
            serde_yaml::from_str("on: push\njobs:\n  build:\n    steps: [{run: 'build @recipes@'}]").unwrap();
        let yaml = render(&tpl, &jobs, &job_needs(&jobs, &recipes)).unwrap();
        let wf: Value = serde_yaml::from_str(&yaml).unwrap();
        assert!(wf["jobs"]["job_0"].get("needs").is_none());
        assert!(wf["jobs"]["job_1"].get("needs").is_none());
        assert_eq!(wf["jobs"]["job_0"]["steps"][0]["run"], "build a b");
        assert!(yaml.starts_with("on:"));
    }

    #[test]
    fn dependent_on_two_jobs_waits_for_both() {
        // d needs a and c, which run in parallel: d goes after the later one
        let recipes = recipes(&[("a", &[], false), ("c", &[], false), ("d", &["a", "c"], false)]);
        let t = timings(&[("a", 600.0), ("c", 300.0), ("d", 60.0)]);
        let clock = Clock { t: &t, default: 60.0, overhead: 30.0, runners: 20 };
        assert_eq!(batch(&recipes, &clock, 5, 0.0).unwrap(), [vec!["a"], vec!["c"], vec!["d"]]);
    }

    #[test]
    fn json_stages_use_vinca_ids() {
        let recipes = recipes(&[("a", &[], false), ("b", &["a"], false)]);
        let jobs = vec![vec!["a".to_string()], vec!["b".to_string()]];
        let j = stages_json(&jobs, &[0, 1], &job_needs(&jobs, &recipes));
        assert_eq!(j["stages"][0][0]["id"], "stage_0_job_0");
        assert_eq!(j["stages"][1][0]["id"], "stage_1_job_0");
        assert_eq!(j["stages"][1][0]["needs"], serde_json::json!(["stage_0_job_0"]));
        assert_eq!(j["stages"][1][0]["recipes"], "b");
    }

    #[test]
    fn aliases_follow_their_package() {
        let recipes = recipes(&[("ros2-x", &[], false), ("ros-humble-x", &["ros2-x"], true)]);
        let t = Timings::default();
        let clock = Clock { t: &t, default: 20.0, overhead: 30.0, runners: 20 };
        assert_eq!(batch(&recipes, &clock, 5, 0.0).unwrap(), [vec!["ros2-x", "ros-humble-x"]]);
    }
}
