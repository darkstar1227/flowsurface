//! Review item C3: the pieces that decide whether an order may go out each have exactly one
//! production implementation or construction site. The types cannot enforce this (a trait
//! can be implemented by any crate, a public enum variant built by any caller), so these
//! tests read the production source and fail when a second one appears. Test modules are
//! skipped; a new legitimate site must be added here with a reason, after review.

use std::fs;
use std::path::{Path, PathBuf};

/// The source of `path` without its trailing `#[cfg(test)] mod ... {` block, if any.
fn production_part(src: &str) -> String {
    let lines: Vec<&str> = src.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let is_test_mod = lines[i].trim() == "#[cfg(test)]"
            && lines
                .get(i + 1)
                .is_some_and(|l| l.trim_start().starts_with("mod ") || l.contains(" mod "));
        if is_test_mod {
            // Test modules sit at the end of a file, or are a single `mod x;` line: skip the
            // block by brace depth.
            let mut depth = 0i32;
            let mut seen = false;
            i += 1;
            while i < lines.len() {
                // Braces inside string literals (and `'{'` char literals) do not count.
                let mut in_str = false;
                let mut prev = ' ';
                for c in lines[i].chars() {
                    if in_str {
                        if c == '"' && prev != '\\' {
                            in_str = false;
                        }
                    } else {
                        match c {
                            '"' => in_str = true,
                            '{' if prev != '\'' => {
                                depth += 1;
                                seen = true;
                            }
                            '}' if prev != '\'' => depth -= 1,
                            _ => {}
                        }
                    }
                    prev = c;
                }
                i += 1;
                if (seen && depth <= 0) || (!seen && lines[i - 1].trim_end().ends_with(';')) {
                    break;
                }
            }
            continue;
        }
        out.push(lines[i]);
        i += 1;
    }
    out.join("\n")
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            rust_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// Lines of production code (not test modules, not test-only files) containing `needle`,
/// as `file:line`.
fn hits(root: &Path, needle: &str) -> Vec<String> {
    let mut files = Vec::new();
    rust_files(root, &mut files);
    // A missing directory (a packaged crate, a moved source tree) must fail the scan, not
    // pass it with nothing to look at.
    assert!(
        files.len() > 10,
        "no sources found under {}",
        root.display()
    );
    let mut found = Vec::new();
    for f in files {
        let name = f.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name == "send_tests.rs" || name == "matrix_tests.rs" || name == "source_rules_tests.rs" {
            continue;
        }
        let Ok(src) = fs::read_to_string(&f) else {
            continue;
        };
        for (n, line) in production_part(&src).lines().enumerate() {
            let t = line.trim_start();
            if t.starts_with("//") {
                continue;
            }
            if line.contains(needle) {
                found.push(format!("{}:{}", f.display(), n + 1));
            }
        }
    }
    found
}

fn crate_src() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

#[test]
fn only_the_risk_snapshot_implements_the_risk_hook() {
    // The trait is sealed (only this crate can implement it); this scan is a second alarm.
    let h = hits(&crate_src(), "RiskHook for");
    assert_eq!(h.len(), 1, "{h:?}");
    let src = fs::read_to_string(crate_src().join("trade/risk.rs")).unwrap();
    assert!(production_part(&src).contains("impl RiskHook for RiskSnapshot"));
    let s = hits(&crate_src(), "Sealed for");
    assert_eq!(s.len(), 1, "{s:?}");
    assert!(s[0].contains("risk.rs"), "{s:?}");
}

#[test]
fn no_production_send_gate_implementation_lives_in_the_exchange_crate() {
    // The one allowed implementation is `data::config::trade_gate::SessionGate`; the data
    // crate checks that itself.
    let h = hits(&crate_src(), "SendGate for");
    assert!(h.is_empty(), "{h:?}");
}

#[test]
fn only_bulk_builds_the_risk_close_path() {
    // `AutoIntent::new` refuses the path; only `risk_close` builds it, and only bulk.rs calls it.
    let h = hits(&crate_src(), "AutoIntent::risk_close(");
    assert_eq!(h.len(), 1, "{h:?}");
    assert!(h[0].contains("bulk.rs"), "{h:?}");
    let p = hits(&crate_src(), "AutoPath::RiskClose");
    assert!(p.iter().all(|x| x.contains("gate.rs")), "{p:?}");
}

#[test]
fn the_scan_skips_test_modules_and_keeps_production_code() {
    let src = "fn a() {}\n#[cfg(test)]\nfn only_for_tests() {}\nfn b() {}\n#[cfg(test)]\nmod tests {\n    fn x() { RiskHook for }\n}\n";
    let p = production_part(src);
    assert!(p.contains("fn b()"));
    assert!(p.contains("only_for_tests"));
    assert!(!p.contains("RiskHook for"));
    // A brace inside a string must not end the test module early.
    let src = "#[cfg(test)]\nmod tests {\n    const S: &str = \"}\";\n    fn x() { RiskHook for }\n}\nfn after() {}\n";
    let p = production_part(src);
    assert!(!p.contains("RiskHook for"), "{p}");
    assert!(p.contains("fn after()"));
}

#[test]
fn no_production_journal_implementation_lives_in_the_exchange_crate() {
    // The one allowed implementation is `data::config::unknown_store::UnknownStore` (review N3):
    // a no-op journal would switch the write-ahead record off. The data crate checks its own
    // side.
    let h = hits(&crate_src(), "UnknownJournal for");
    assert!(h.is_empty(), "{h:?}");
}
