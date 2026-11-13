//! rattler-build v1 selectors (`- if: ... then: ... else: ...`) in vinca's own
//! configuration files, evaluated for one target platform. Strict, as in
//! rattler-build: an unknown name or an invalid expression is an error, not false.

use std::collections::BTreeMap;

use miette::Diagnostic;
use minijinja::{Environment, Value as JValue};
use serde_yaml::Value;
use thiserror::Error;

#[derive(Debug, Error, Diagnostic)]
pub enum Error {
    #[error("selector `if: {expr}`: {source}")]
    #[diagnostic(help(
        "selectors can use the platform names (linux, osx, win, unix, x86_64, aarch64, ...), target_platform and, in test files, ros_distro"
    ))]
    Eval { expr: String, source: minijinja::Error },
    #[error("selector `if: {expr}`: unknown name `{name}`")]
    #[diagnostic(help(
        "selectors can use the platform names (linux, osx, win, unix, x86_64, aarch64, ...), target_platform and, in test files, ros_distro"
    ))]
    Unknown { expr: String, name: String },
    #[error("selector `if:` is not a string: {0:?}")]
    NotAString(Value),
}

type Result<T, E = Error> = std::result::Result<T, E>;

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

/// Evaluate a selector expression.
pub fn eval(expr: &str, ctx: &BTreeMap<String, JValue>) -> Result<bool> {
    let env = Environment::new();
    let err = |source| Error::Eval { expr: expr.to_string(), source };
    let compiled = env.compile_expression(expr).map_err(err)?;
    if let Some(name) = compiled.undeclared_variables(false).into_iter().find(|n| !ctx.contains_key(n)) {
        return Err(Error::Unknown { expr: expr.to_string(), name });
    }
    Ok(compiled.eval(ctx).map_err(err)?.is_true())
}

/// Resolve every selector for which `cond` returns `Some`; `None` keeps the node.
pub fn process(node: &Value, cond: &dyn Fn(&str) -> Option<Result<bool>>) -> Result<Option<Value>> {
    Ok(match node {
        Value::Mapping(m) => {
            if m.contains_key("if") && m.contains_key("then") {
                let Value::String(expr) = &m["if"] else {
                    return Err(Error::NotAString(m["if"].clone()));
                };
                match cond(expr).transpose()? {
                    Some(true) => return process(&m["then"], cond),
                    Some(false) => return m.get("else").map_or(Ok(None), |e| process(e, cond)),
                    None => {}
                }
            }
            let mut out = serde_yaml::Mapping::new();
            for (k, v) in m {
                if let Some(v) = process(v, cond)? {
                    out.insert(k.clone(), v);
                }
            }
            Some(Value::Mapping(out))
        }
        Value::Sequence(seq) => {
            let mut out = Vec::new();
            for item in seq {
                match process(item, cond)? {
                    None => {}
                    Some(Value::Sequence(inner)) => out.extend(inner),
                    Some(v) => out.push(v),
                }
            }
            Some(Value::Sequence(out))
        }
        other => Some(other.clone()),
    })
}

pub fn evaluate_for_platform(data: &Value, platform: &str) -> Result<Value> {
    let ctx = platform_flags(platform);
    Ok(process(data, &|e| Some(eval(e, &ctx)))?.unwrap_or(Value::Null))
}

/// Only the selectors that mention `ros_distro` (test files shared by distros),
/// with the platform's names too (`linux and ros_distro == "humble"`): the
/// recipes are generated per platform. The others are left for rattler-build.
pub fn evaluate_distro(data: &Value, ros_distro: &str, platform: &str) -> Result<Value> {
    let mut ctx = platform_flags(platform);
    ctx.insert("ros_distro".to_string(), JValue::from(ros_distro));
    Ok(process(data, &|e| e.contains("ros_distro").then(|| eval(e, &ctx)))?.unwrap_or(Value::Null))
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
        let linux = evaluate_for_platform(&v, "linux-aarch64").unwrap();
        assert_eq!(linux["list"], serde_yaml::from_str::<Value>("[a, b, c, e]").unwrap());
        assert!(linux.get("key").is_none());
        let osx = evaluate_for_platform(&v, "osx-arm64").unwrap();
        assert_eq!(osx["key"], Value::from("x"));
        assert!(eval("aarch64 and unix", &platform_flags("osx-arm64")).unwrap());
    }

    #[test]
    fn unknown_names_and_invalid_expressions_are_errors() {
        let flags = platform_flags("linux-64");
        assert!(eval("lnux", &flags).is_err());
        assert!(eval("linux and", &flags).is_err());
        assert!(
            evaluate_for_platform(&serde_yaml::from_str("[{if: true, then: a}]").unwrap(), "linux-64")
                .is_err()
        );
    }

    #[test]
    fn distro_selectors_leave_platform_ones() {
        let v: Value = serde_yaml::from_str(
            "[{if: 'ros_distro == \"humble\"', then: h}, {if: linux, then: l}, \
             {if: 'osx and ros_distro == \"humble\"', then: o, else: n}]",
        )
        .unwrap();
        let out = evaluate_distro(&v, "humble", "linux-64").unwrap();
        assert_eq!(out[0], Value::from("h"));
        assert!(out[1].get("if").is_some());
        assert_eq!(out[2], Value::from("n"));
        assert_eq!(evaluate_distro(&v, "humble", "osx-arm64").unwrap()[2], Value::from("o"));
    }
}
