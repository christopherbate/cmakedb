//! Finding renderers: human text, stable JSON, SARIF 2.1.0 (§2.3).

use crate::{Finding, Severity};
use serde_json::json;

pub fn render_text(findings: &[Finding], color: bool) -> String {
    let mut out = String::new();
    for f in findings {
        let (sev, reset) = if color {
            let c = match f.severity {
                Severity::Error => "\x1b[31m",
                Severity::Warning => "\x1b[33m",
                Severity::Note => "\x1b[36m",
            };
            (
                format!("{c}{}\x1b[0m", severity_word(f.severity)),
                "\x1b[2m",
            )
        } else {
            (severity_word(f.severity).to_string(), "")
        };
        let reset_end = if color { "\x1b[0m" } else { "" };
        out.push_str(&format!(
            "{sev} [{}] {}\n  --> {}\n",
            f.rule, f.message, f.primary
        ));
        for (span, note) in &f.related {
            if span.file.starts_with('<') {
                out.push_str(&format!("      {reset}{note}{reset_end}\n"));
            } else {
                out.push_str(&format!("      {reset}{span}: {note}{reset_end}\n"));
            }
        }
    }
    if findings.is_empty() {
        out.push_str("no findings\n");
    }
    out
}

fn severity_word(s: Severity) -> &'static str {
    match s {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Note => "note",
    }
}

/// Stable machine-readable schema, versioned (§2.3).
pub fn render_json(findings: &[Finding]) -> String {
    serde_json::to_string_pretty(&json!({
        "cmakedb_findings_version": 1,
        "findings": findings,
    }))
    .expect("findings serialize")
}

/// SARIF 2.1.0; `related` becomes relatedLocations so provenance chains
/// surface natively in code-scanning UIs (§3.4). Every result carries a
/// `partialFingerprints` entry so GitHub code scanning dedupes findings
/// across runs, and every rule links its user-guide entry via `helpUri`
/// (the guide's section anchors are the rule ids).
pub fn render_sarif(findings: &[Finding]) -> String {
    let mut rules: Vec<&str> = findings.iter().map(|f| f.rule.as_str()).collect();
    rules.sort();
    rules.dedup();
    let rule_objs: Vec<_> = rules
        .iter()
        .map(|r| {
            json!({
                "id": r,
                "name": r,
                "helpUri": format!(
                    "https://github.com/christopherbate/cmakedb/blob/main/docs/user-guide.md#{r}"
                ),
            })
        })
        .collect();
    let results: Vec<_> = findings
        .iter()
        .map(|f| {
            let related: Vec<_> = f
                .related
                .iter()
                .filter(|(s, _)| !s.file.starts_with('<'))
                .map(|(s, note)| {
                    json!({
                        "physicalLocation": physical_location(s),
                        "message": {"text": note},
                    })
                })
                .collect();
            json!({
                "ruleId": f.rule,
                "level": f.severity.sarif_level(),
                "message": {"text": f.message},
                "locations": [{"physicalLocation": physical_location(&f.primary)}],
                "relatedLocations": related,
                "partialFingerprints": {
                    "cmakedbFindingKey/v1": finding_fingerprint(f),
                },
            })
        })
        .collect();
    serde_json::to_string_pretty(&json!({
        "$schema": "https://raw.githubusercontent.com/oasis-tcs/sarif-spec/master/Schemata/sarif-schema-2.1.0.json",
        "version": "2.1.0",
        "runs": [{
            "tool": {"driver": {
                "name": "cmakedb",
                "informationUri": "https://github.com/christopherbate/cmakedb",
                "version": env!("CARGO_PKG_VERSION"),
                "rules": rule_objs,
            }},
            "results": results,
        }],
    }))
    .expect("sarif serialize")
}

/// Stable fingerprint for SARIF `partialFingerprints` deduplication.
///
/// Hashes (rule, primary file, primary line) only — the same identity the
/// CLI baseline and `--also` intersection use — and deliberately excludes
/// the message, whose embedded counts/wording change run to run and would
/// defeat deduplication. FNV-1a 64 is implemented inline because Rust's
/// std hasher is not guaranteed stable across releases, and fingerprint
/// stability across cmakedb versions is the entire point.
fn finding_fingerprint(f: &Finding) -> String {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = FNV_OFFSET;
    let line = f.primary.line.to_string();
    let bytes = f
        .rule
        .bytes()
        .chain([0u8])
        .chain(f.primary.file.bytes())
        .chain([0u8])
        .chain(line.bytes());
    for b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    format!("{h:016x}")
}

fn physical_location(span: &crate::SourceSpan) -> serde_json::Value {
    let mut region = json!({"startLine": span.line.max(1)});
    if let Some(c) = span.col {
        region["startColumn"] = json!(c);
    }
    json!({
        "artifactLocation": {"uri": span.file},
        "region": region,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SourceSpan;

    fn sample() -> Vec<Finding> {
        vec![Finding {
            rule: "dead-options".into(),
            severity: Severity::Warning,
            message: "UNREAD FOO".into(),
            primary: SourceSpan::new("cmake/options.cmake", 41),
            related: vec![(SourceSpan::new("a.cmake", 3), "evidence".into())],
            fix: None,
        }]
    }

    #[test]
    fn text_contains_location() {
        let t = render_text(&sample(), false);
        assert!(t.contains("cmake/options.cmake:41"));
        assert!(t.contains("warning [dead-options]"));
    }

    #[test]
    fn sarif_shape() {
        let s = render_sarif(&sample());
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["version"], "2.1.0");
        assert_eq!(v["runs"][0]["results"][0]["ruleId"], "dead-options");
        assert_eq!(
            v["runs"][0]["results"][0]["locations"][0]["physicalLocation"]["region"]["startLine"],
            41
        );
        assert_eq!(
            v["runs"][0]["results"][0]["relatedLocations"][0]["physicalLocation"]
                ["artifactLocation"]["uri"],
            "a.cmake"
        );
        // Rules link their user-guide entry (anchors are the rule ids).
        assert_eq!(
            v["runs"][0]["tool"]["driver"]["rules"][0]["helpUri"],
            "https://github.com/christopherbate/cmakedb/blob/main/docs/user-guide.md#dead-options"
        );
        // partialFingerprints: golden value locks the hash algorithm —
        // changing it silently would break code-scanning dedup and reopen
        // every previously-seen finding.
        assert_eq!(
            v["runs"][0]["results"][0]["partialFingerprints"]["cmakedbFindingKey/v1"],
            "24f4f9861014ae06"
        );
    }

    #[test]
    fn sarif_fingerprint_ignores_message_but_not_location() {
        let mut a = sample();
        a[0].message = "UNREAD FOO (4 reads elided)".into();
        let base = sample();
        let fp = |findings: &[Finding]| {
            let v: serde_json::Value = serde_json::from_str(&render_sarif(findings)).unwrap();
            v["runs"][0]["results"][0]["partialFingerprints"]["cmakedbFindingKey/v1"]
                .as_str()
                .unwrap()
                .to_string()
        };
        // Message changes (counts, wording) keep the fingerprint stable...
        assert_eq!(fp(&a), fp(&base));
        // ...but a moved declaration site is a different finding.
        let mut moved = sample();
        moved[0].primary.line = 42;
        assert_ne!(fp(&moved), fp(&base));
    }

    #[test]
    fn json_versioned() {
        let v: serde_json::Value = serde_json::from_str(&render_json(&sample())).unwrap();
        assert_eq!(v["cmakedb_findings_version"], 1);
    }
}
