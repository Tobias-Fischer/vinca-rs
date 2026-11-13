"""Compare vinca-rs recipes with Python vinca's, by meaning.

Both sides' selectors are evaluated for the target platform, natively and when
cross-compiling, and requirement lists are compared as sets. Usage:
    python compare.py <baseline recipes> <vinca-rs recipes> <platform>
"""

import collections
import filecmp
import json
import sys
from pathlib import Path

import jinja2
import yaml

base_dir, new_dir, platform = Path(sys.argv[1]), Path(sys.argv[2]), sys.argv[3]
os_, arch = platform.split("-", 1)
FLAGS = {
    "target_platform": platform,
    "linux": os_ == "linux", "osx": os_ == "osx", "win": os_ == "win",
    "emscripten": os_ == "emscripten", "wasm32": arch == "wasm32",
    "unix": os_ != "win", "aarch64": arch in ("aarch64", "arm64"),
}
ENV = jinja2.Environment()


def evaluate(node, ctx):
    if isinstance(node, dict) and "if" in node and "then" in node:
        try:
            ok = bool(ENV.compile_expression(str(node["if"]))(ctx))
        except Exception:
            ok = False
        branch = node.get("then" if ok else "else")
        return evaluate(branch, ctx) if branch is not None else None
    if isinstance(node, dict):
        return {k: v2 for k, v in node.items() if (v2 := evaluate(v, ctx)) is not None}
    if isinstance(node, list):
        out = []
        for i in node:
            v = evaluate(i, ctx)
            if v is None:
                continue
            out.extend(v if isinstance(v, list) else [v])
        return out
    return node


def reqs(recipe, cross):
    ctx = dict(FLAGS, build_platform="other" if cross else platform)
    r = evaluate(recipe.get("requirements") or {}, ctx)
    return {k: sorted(set(map(str, r.get(k) or []))) for k in ("build", "host", "run", "run_constraints")} | {
        "run_exports": r.get("run_exports")
    }


def script_file(recipe):
    s = str((recipe.get("build") or {}).get("script") or "")
    if "${{" in s:  # vinca's '$RECIPE_DIR/x.sh' if unix else '%RECIPE_DIR%\\x.bat'
        s = s.split("'")[1 if FLAGS["unix"] else 3].replace("$RECIPE_DIR/", "").replace("%RECIPE_DIR%\\\\", "")
    return s


base = {p.name for p in base_dir.iterdir()}
new = {p.name for p in new_dir.iterdir()}
diffs = collections.defaultdict(list)
for name in sorted(base & new):
    b = yaml.safe_load((base_dir / name / "recipe.yaml").read_text())
    n = yaml.safe_load((new_dir / name / "recipe.yaml").read_text())
    for key in ("package", "source", "about", "tests"):
        bv, nv = b.get(key), n.get(key)
        if key == "about":  # template defaults vinca left in for recipes without metadata
            bv = {k: v for k, v in (bv or {}).items()}
        if bv != nv:
            diffs[key].append((name, bv, nv))
    if b["build"].get("number") != n["build"].get("number"):
        diffs["build.number"].append((name, b["build"].get("number"), n["build"].get("number")))
    if b["build"].get("string") != n["build"].get("string"):
        diffs["build.string"].append((name, b["build"].get("string"), n["build"].get("string")))
    if script_file(b) != script_file(n):
        diffs["build.script"].append((name, script_file(b), script_file(n)))
    for cross in (False, True):
        rb, rn = reqs(b, cross), reqs(n, cross)
        for k in rb:
            if rb[k] != rn[k]:
                if isinstance(rb[k], list):
                    delta = {"only_vinca": sorted(set(rb[k]) - set(rn[k])), "only_rs": sorted(set(rn[k]) - set(rb[k]))}
                else:
                    delta = {"vinca": rb[k], "rs": rn[k]}
                diffs[f"requirements.{k}{' (cross)' if cross else ''}"].append((name, delta))
    # files next to the recipe: the platform's build script, activation scripts, patches, tests
    for f in sorted(p.relative_to(new_dir / name) for p in (new_dir / name).rglob("*") if p.is_file()):
        if str(f) == "recipe.yaml":
            continue
        bf = base_dir / name / f
        if not bf.exists():
            diffs["files.missing_in_vinca"].append((name, str(f)))
        elif not filecmp.cmp(bf, new_dir / name / f, shallow=False):
            diffs["files.content"].append((name, str(f)))
    for f in sorted(p.relative_to(base_dir / name) for p in (base_dir / name).rglob("*") if p.is_file()):
        if str(f) != "recipe.yaml" and not (new_dir / name / f).exists():
            diffs["files.only_in_vinca"].append((name, str(f)))

print(f"{platform}: {len(base)} vinca recipes, {len(new)} vinca-rs recipes, {len(base & new)} common")
print(f"  only vinca: {sorted(base - new)[:20]}")
print(f"  only vinca-rs: {sorted(new - base)[:20]}")
for k, v in sorted(diffs.items()):
    print(f"  {k}: {len(v)} recipes, e.g. {json.dumps(v[0], default=str)[:400]}")
json.dump(diffs, open(f"diff-{platform}.json", "w"), indent=1, default=str)
