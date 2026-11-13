//! package.xml `<license>` -> SPDX expression (port of vinca/license_utils.py,
//! with the `spdx` crate in place of Python's license-expression).

const LOOKUP: &[(&str, &str)] = &[
    ("bsd", "BSD-3-Clause"),
    ("bsd-3", "BSD-3-Clause"),
    ("bsd 3-clause", "BSD-3-Clause"),
    ("3-clause bsd", "BSD-3-Clause"),
    ("bsd clause 3", "BSD-3-Clause"),
    ("bsd 3 clause", "BSD-3-Clause"),
    ("bsd 3-clause license", "BSD-3-Clause"),
    ("bds-3", "BSD-3-Clause"),
    ("bsd 2-clause", "BSD-2-Clause"),
    ("bsd-2", "BSD-2-Clause"),
    ("gpl", "GPL-1.0-or-later"),
    ("bsd license 2.0", "BSD-2-Clause"),
    ("apache", "Apache-2.0"),
    ("apache 2", "Apache-2.0"),
    ("apache-2", "Apache-2.0"),
    ("apache2", "Apache-2.0"),
    ("apache2.0", "Apache-2.0"),
    ("apache 2.0", "Apache-2.0"),
    ("apache 2.0 license", "Apache-2.0"),
    ("apache license 2.0", "Apache-2.0"),
    ("apache license, version 2.0", "Apache-2.0"),
    ("apache-2.0 license", "Apache-2.0"),
    ("alv2", "Apache-2.0"),
    ("gplv2", "GPL-2.0-only"),
    ("gpl-2", "GPL-2.0-only"),
    ("gpl-2.0", "GPL-2.0-only"),
    ("gnu general public license v2.0", "GPL-2.0-only"),
    ("gplv3", "GPL-3.0-only"),
    ("gpl-3", "GPL-3.0-only"),
    ("gpl-3.0", "GPL-3.0-only"),
    ("agplv3", "AGPL-3.0-only"),
    ("agpl-3", "AGPL-3.0-only"),
    ("agpl-3.0", "AGPL-3.0-only"),
    ("lgpl", "LGPL-2.1-or-later"),
    ("lgplv2", "LGPL-2.1-or-later"),
    ("lgplv2.1", "LGPL-2.1-or-later"),
    ("lgpl-2", "LGPL-2.1-or-later"),
    ("lgpl-2.1", "LGPL-2.1-or-later"),
    ("lgpl v2.1", "LGPL-2.1-or-later"),
    ("lgplv3", "LGPL-3.0-only"),
    ("lgpl-3", "LGPL-3.0-only"),
    ("lgpl-3.0", "LGPL-3.0-only"),
    ("lgpl-v3", "LGPL-3.0-only"),
    ("lgpl (amcl)", "LGPL-2.1-or-later"),
    ("gnu lesser public license 2.1", "LGPL-2.1-only"),
    ("mozilla", "MPL-2.0"),
    ("mpl", "MPL-2.0"),
    ("mpl-2.0 license", "MPL-2.0"),
    ("mozilla public license 2.0", "MPL-2.0"),
    ("mozilla public license version 1.1", "MPL-1.1"),
    ("eclipse public license 2.0", "EPL-2.0"),
    ("eclipse distribution license 1.0", "BSD-3-Clause"),
    ("boost", "BSL-1.0"),
    ("boost software license", "BSL-1.0"),
    ("bsl", "BSL-1.0"),
    ("mit license", "MIT"),
    ("zlib license", "Zlib"),
    ("cc by-nc-sa 4.0", "CC-BY-NC-SA-4.0"),
    ("cc0", "CC0-1.0"),
    ("creative commons zero v1.0 universal", "CC0-1.0"),
    (
        "creative commons attribution-noncommercial-noderivatives 4.0 international public license",
        "CC-BY-NC-ND-4.0",
    ),
    ("creative commons", "CC0-1.0"),
    ("public domain", "Unlicense"),
];

fn lookup(s: &str) -> Option<&'static str> {
    let l = s.to_lowercase();
    LOOKUP.iter().find(|(k, _)| *k == l).map(|(_, v)| *v)
}

/// Valid SPDX identifiers only: the guessing is the lookup table's job.
const MODE: spdx::ParseMode = spdx::ParseMode {
    allow_lower_case_operators: true,
    allow_slash_as_or_operator: true,
    allow_imprecise_license_names: false,
    allow_postfix_plus_on_gpl: true,
};

/// The canonical identifier: GNU licenses as -only / -or-later.
fn canonical(id: spdx::LicenseId, or_later: bool) -> String {
    if !id.is_gnu() {
        return format!("{}{}", id.name, if or_later { "+" } else { "" });
    }
    let base = id
        .name
        .trim_end_matches('+')
        .trim_end_matches("-only")
        .trim_end_matches("-or-later");
    let later = or_later || id.name.ends_with('+') || id.name.ends_with("-or-later");
    format!("{base}{}", if later { "-or-later" } else { "-only" })
}

/// A valid SPDX expression, with canonical identifiers.
fn normalize(s: &str) -> Option<String> {
    let expr = spdx::Expression::parse_mode(s, MODE).ok()?;
    let mut out = s.to_string();
    for req in expr.requirements() {
        if let spdx::LicenseItem::Spdx { id, or_later } = req.req.license {
            let canonical = canonical(id, or_later);
            // the span leaves out a trailing `+` (GPL-3.0+), which `canonical` covers
            let mut end = req.span.end as usize;
            if or_later && s[end..].starts_with('+') {
                end += 1;
            }
            out = out.replacen(&s[req.span.start as usize..end], &canonical, 1);
        } else {
            return None;
        }
    }
    Some(out)
}

fn single(s: &str, pkg: &str) -> String {
    lookup(s)
        .map(str::to_string)
        .or_else(|| normalize(s))
        .unwrap_or_else(|| {
            let clean: String = s
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                        c
                    } else {
                        '-'
                    }
                })
                .collect();
            eprintln!("warning: license '{s}' of {pkg} is not SPDX: using LicenseRef-{clean}");
            format!("LicenseRef-{clean}")
        })
}

pub fn to_spdx(licenses: &[String], pkg: &str) -> Option<String> {
    let mut out: Vec<String> = vec![];
    for lic in licenses {
        let lic = lic.trim();
        if lic.is_empty() || lic.eq_ignore_ascii_case("todo") {
            continue;
        }
        let item = if let Some(l) = lookup(lic) {
            l.to_string()
        } else if let Some(n) = normalize(lic) {
            n
        } else if lic.contains(',') {
            let mut parts: Vec<String> = vec![];
            for p in lic.split(',').map(str::trim).filter(|p| !p.is_empty()) {
                let s = single(p, pkg);
                if !parts.contains(&s) {
                    parts.push(s);
                }
            }
            parts.join(" OR ")
        } else {
            single(lic, pkg)
        };
        if !out.contains(&item) {
            out.push(item);
        }
    }
    (!out.is_empty()).then(|| out.join(" OR "))
}

#[cfg(test)]
mod tests {
    use super::to_spdx;

    fn spdx(l: &[&str]) -> Option<String> {
        to_spdx(&l.iter().map(|s| s.to_string()).collect::<Vec<_>>(), "pkg")
    }

    #[test]
    fn lookup_names_win_over_imprecise_spdx_guesses() {
        assert_eq!(spdx(&["BSD"]).unwrap(), "BSD-3-Clause");
        assert_eq!(spdx(&["Apache License, Version 2.0"]).unwrap(), "Apache-2.0");
        assert_eq!(spdx(&["GPL"]).unwrap(), "GPL-1.0-or-later");
    }

    #[test]
    fn gnu_licenses_are_only_or_or_later() {
        assert_eq!(spdx(&["GPL-3.0+"]).unwrap(), "GPL-3.0-or-later");
        assert_eq!(spdx(&["LGPL-2.1+"]).unwrap(), "LGPL-2.1-or-later");
        assert_eq!(spdx(&["GPL-2.0"]).unwrap(), "GPL-2.0-only");
    }

    #[test]
    fn several_licenses() {
        assert_eq!(
            spdx(&["BSD", "Apache 2.0"]).unwrap(),
            "BSD-3-Clause OR Apache-2.0"
        );
        assert_eq!(spdx(&["BSD, Apache 2.0"]).unwrap(), "BSD-3-Clause OR Apache-2.0");
        assert_eq!(spdx(&["TODO"]), None);
        assert_eq!(spdx(&["My Own License"]).unwrap(), "LicenseRef-My-Own-License");
    }
}
