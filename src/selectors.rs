//! rattler-build v1 selectors (`- if: ... then: ... else: ...`) in vinca's own
//! configuration files, evaluated for one target platform (port of
//! vinca/v1_selectors.py).

use std::collections::BTreeMap;

use minijinja::{Environment, Value as JValue};
use serde_yaml::Value;

pub fn platform_flags(platform: &str) -> BTreeMap<String, JValue> {
    let (os, arch) = platform.split_once('-').unwrap_or((platform, ""));
    let flags = [
        ("linux", os == "linux"),
        ("osx", os == "osx"),
        ("win", os == "win"),
        ("emscripten", os == "emscripten"),
        ("wasi", os == "wasi"),
        ("freebsd", os == "freebsd"),
        ("zos", os == "zos"),
        ("unix", matches!(os, "linux" | "osx" | "emscripten" | "wasi" | "freebsd" | "zos")),
        ("x86", matches!(arch, "32" | "64")),
        ("x86_64", arch == "64"),
        ("aarch64", matches!(arch, "aarch64" | "arm64")),
        ("arm64", matches!(arch, "aarch64" | "arm64")),
        ("armV6l", arch == "armv6l"),
        ("armV7l", arch == "armv7l"),
        ("ppc64", arch == "ppc64"),
        ("ppc64le", arch == "ppc64le"),
        ("s390x", arch == "s390x"),
        ("riscv32", arch == "riscv32"),
        ("riscv64", arch == "riscv64"),
        ("wasm32", arch == "wasm32"),
    ];
    let mut f: BTreeMap<String, JValue> = flags.into_iter().map(|(k, v)| (k.to_string(), v.into())).collect();
    f.insert("target_platform".into(), platform.into());
    f
}

/// Evaluate a selector expression; unknown variables are falsy and an invalid
/// expression is false (as in vinca).
pub fn eval(expr: &str, ctx: &BTreeMap<String, JValue>) -> bool {
    let env = Environment::new();
    env.compile_expression(expr).ok().and_then(|e| e.eval(ctx).ok()).is_some_and(|v| v.is_true())
}

/// Resolve every selector for which `cond` returns `Some`; `None` keeps the node.
pub fn process(node: &Value, cond: &dyn Fn(&str) -> Option<bool>) -> Option<Value> {
    match node {
        Value::Mapping(m) => {
            if m.contains_key("if") && m.contains_key("then") {
                let expr = match &m["if"] {
                    Value::String(s) => s.clone(),
                    other => serde_yaml::to_string(other).unwrap_or_default(),
                };
                match cond(&expr) {
                    Some(true) => return process(&m["then"], cond),
                    Some(false) => return m.get("else").and_then(|e| process(e, cond)),
                    None => {}
                }
            }
            let entries = m.iter().filter_map(|(k, v)| Some((k.clone(), process(v, cond)?)));
            Some(Value::Mapping(entries.collect()))
        }
        Value::Sequence(seq) => {
            let mut out = Vec::new();
            for item in seq {
                match process(item, cond) {
                    None => {}
                    Some(Value::Sequence(inner)) => out.extend(inner),
                    Some(v) => out.push(v),
                }
            }
            Some(Value::Sequence(out))
        }
        other => Some(other.clone()),
    }
}

pub fn evaluate_for_platform(data: &Value, platform: &str) -> Value {
    let ctx = platform_flags(platform);
    process(data, &|e| Some(eval(e, &ctx))).unwrap_or(Value::Null)
}

/// Only the selectors that mention `ros_distro` (test files shared by distros).
pub fn evaluate_distro(data: &Value, ros_distro: &str) -> Value {
    let mut ctx = BTreeMap::new();
    ctx.insert("ros_distro".to_string(), JValue::from(ros_distro));
    process(data, &|e| e.contains("ros_distro").then(|| eval(e, &ctx))).unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selectors_resolve_and_splice() {
        let v: Value = serde_yaml::from_str(
            "list: [a, {if: linux, then: [b, c]}, {if: win, then: d, else: e}]\nkey: {if: osx, then: x}",
        )
        .unwrap();
        let linux = evaluate_for_platform(&v, "linux-aarch64");
        assert_eq!(linux["list"], serde_yaml::from_str::<Value>("[a, b, c, e]").unwrap());
        assert!(linux.get("key").is_none());
        let osx = evaluate_for_platform(&v, "osx-arm64");
        assert_eq!(osx["key"], Value::from("x"));
        assert!(eval("aarch64 and unix", &platform_flags("osx-arm64")));
    }

    #[test]
    fn distro_selectors_leave_platform_ones() {
        let v: Value =
            serde_yaml::from_str("[{if: 'ros_distro == \"humble\"', then: h}, {if: linux, then: l}]")
                .unwrap();
        let out = evaluate_distro(&v, "humble");
        assert_eq!(out[0], Value::from("h"));
        assert!(out[1].get("if").is_some());
    }
}
