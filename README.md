# vinca-rs

Generates the [rattler-build](https://rattler.build) recipes of a ROS 2
distribution, and the GitHub Actions workflow that builds them, for
[RoboStack](https://robostack.github.io). A Rust rewrite of
[vinca](https://github.com/RoboStack/vinca), reusing
[pixi-build-ros](https://github.com/prefix-dev/pixi/tree/main/crates/pixi_build_ros)'s
`package.xml` parsing and rattler-build's own crates for rendering.

Status: experimental. It generates the same recipes as vinca for RoboStack's humble
(see [Comparison with vinca](#comparison-with-vinca)).

## Usage

```bash
# recipes for one target platform, without those already published on the
# configuration's skip_existing channels (same name, version and build string)
vinca-rs generate --config vinca.yaml --platform linux-64 --out recipes

# every recipe, published or not
vinca-rs generate --platform linux-64 --all

# the workflow that builds ./recipes, from a template (see below)
vinca-rs workflow --platform linux-64 --recipes recipes \
  --template .github/build-template.yaml --out .github/workflows/build.yaml \
  --timings build_timings.json --var distro=humble --var branch=buildbranch_humble_linux_64
```

## Configuration

`vinca.yaml` can `extend` another one: package lists are combined, `mutex_package`
is merged, other keys are replaced. Next to each layer: `patch/` (patches and
`dependencies.yaml`), `tests/`, `pkg_additional_info.yaml`, and the `conda_index`
files (`robostack.yaml`). Selectors (`if`/`then`/`else`) are resolved for the target
platform. Unknown keys are errors.

| key | |
|---|---|
| `ros_distro` | the rosdistro distribution |
| `rosdistro_snapshot` | pinned releases with their dependencies (`vinca-snapshot`) |
| `rosdistro_additional_recipes` | packages from elsewhere: `url`, `tag` or `rev`, `version`, `additional_folder`, `submodules`, `override` |
| `packages_select_by_deps` | the packages to build, with their dependencies |
| `packages_exclude` | not built, and dropped from other packages' dependencies |
| `packages_skip` | not built; dependents keep the dependency |
| `conda_index` | rosdep key → conda packages (`robostack:` per platform) |
| `build_number`, `mutex_package`, `package_name_mode` | |
| `skip_existing` | channels whose builds aren't built again |

An additional recipe that replaces a release of the snapshot needs
`override: <reason>` (RoboStack/vinca#126); vinca-rs warns when the release is newer.

## The workflow

vinca-rs decides what each job builds and which jobs it waits for. How a job builds
belongs to the caller: the template's `jobs.build` is copied once per job, with
`@recipes@` replaced by the job's recipes (`@name@` for any `--var name=value`).
Keep the job small (e.g. a composite action) so that many jobs fit GitHub's 500 KiB
workflow limit.

- Each job **needs only the jobs that build its recipes' dependencies**, not a whole
  previous stage, so it starts once those are built, and a failure only skips the
  recipes that depend on it.
- Recipes are clustered into jobs by **earliest finish time**, taken in order of the
  work that depends on them. Several schedules, from many small jobs to few large
  ones, are simulated on the available runners (`runners` in the timings; 20, or 5
  on macOS), and the fastest one that fits the size limit is written.
- Legacy-name aliases (`ros-<distro>-<pkg>`) are built in their package's job.

## Comparison with vinca

These are vinca's recipes for RoboStack's humble (vinca 74d1e85 / RoboStack/vinca#200):

- Same recipes on linux-64, osx-arm64 and win-64 (2531 / 2181 / 1837). Build
  scripts, patches, sources, tests and metadata are identical. `tools/compare.py`
  compares by meaning.
- Platform selectors are resolved at generation, so each recipe carries the
  target's build script (`script: build_ament_cmake.sh`), not both.
- Packages that only wrap a conda-forge package (`generate_dummy_package_with_run_deps`)
  only depend on it and the mutex.
- Every ament package gets a `package_contents` test for its `package.xml` (#108).
- Licenses: an explicit table maps the common non-SPDX names, and everything else
  has to be valid SPDX.

Not ported: ROS 1, GitLab/Azure pipelines, `vinca-migrate`, `variants_mode: local`,
single-file recipes, archive and GitLab sources, remote `conda_index`.

## Development

```bash
cargo test
cargo clippy --all-targets -- -D warnings
```
