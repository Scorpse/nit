//! Recorded (or docs-derived) forge fixtures must never carry a credential or a real
//! person's address, and every GitHub fixture must be accounted for in the verification log.

use std::path::{Path, PathBuf};

/// Prefixes of GitHub and GitLab tokens, and header names that would only appear in a
/// fixture copied together with its request (as a JSON key or a raw header line; the
/// word alone appears in GitHub's SSO `authorization_request` URL).
const TOKEN_MARKERS: [&str; 12] = [
    "ghp_",
    "github_pat_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "glpat-",
    "\"authorization\"",
    "authorization:",
    "bearer ",
    "\"private-token\"",
    "private-token:",
];

fn fixture_dirs() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    ["github", "gitlab"]
        .iter()
        .map(|d| root.join(d))
        .filter(|d| d.is_dir())
        .collect()
}

fn fixture_files() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for dir in fixture_dirs() {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.extension().is_some_and(|x| x == "json") {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// Email addresses in `text` whose domain is not `example.com`.
fn foreign_emails(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let local = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'-');
    let domain = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-');
    let mut out = Vec::new();
    for (i, _) in text.match_indices('@') {
        let has_local = i > 0 && local(bytes[i - 1]);
        let end = bytes[i + 1..]
            .iter()
            .position(|&b| !domain(b))
            .map_or(bytes.len(), |n| i + 1 + n);
        let dom = text[i + 1..end].trim_end_matches('.').to_ascii_lowercase();
        if has_local && dom.contains('.') && dom != "example.com" {
            out.push(dom);
        }
    }
    out
}

#[test]
fn fixtures_contain_no_token_like_strings() {
    let files = fixture_files();
    assert!(!files.is_empty(), "no forge fixtures found");
    for f in files {
        let text = std::fs::read_to_string(&f).unwrap();
        let lower = text.to_ascii_lowercase();
        for m in TOKEN_MARKERS {
            assert!(!lower.contains(m), "{}: contains {m:?}", f.display());
        }
        let emails = foreign_emails(&text);
        assert!(emails.is_empty(), "{}: emails at {emails:?}", f.display());
    }
}

#[test]
fn the_email_check_catches_real_domains_only() {
    assert_eq!(
        foreign_emails("a octocat@example.com b"),
        Vec::<String>::new()
    );
    assert_eq!(
        foreign_emails("x me@corp.example.org"),
        vec!["corp.example.org"]
    );
    // a scope like `repo, read:org` or a bare `@` is not an address
    assert!(foreign_emails("@octocat mentioned, @ alone").is_empty());
}

#[test]
fn fixtures_are_valid_json() {
    for f in fixture_files() {
        let text = std::fs::read_to_string(&f).unwrap();
        // placeholders such as {base} live inside strings, so the file still parses
        serde_json::from_str::<serde_json::Value>(&text)
            .unwrap_or_else(|e| panic!("{}: {e}", f.display()));
    }
}

/// Every fixture of `forge` has a row in that forge's verification log.
fn assert_listed(forge: &str) {
    let log =
        Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/{forge}/SOURCES.md"));
    let log = std::fs::read_to_string(log).unwrap();
    let mut seen = 0;
    for f in fixture_files() {
        if f.parent()
            .and_then(Path::file_name)
            .is_some_and(|d| d == forge)
        {
            seen += 1;
            let name = f.file_name().unwrap().to_string_lossy().into_owned();
            assert!(log.contains(&format!("`{name}`")), "{name} not in the log");
        }
    }
    assert!(seen > 0, "no {forge} fixtures");
}

/// Each GitHub fixture has a row in the verification log saying where it came from.
#[test]
fn every_github_fixture_is_listed_in_the_verification_log() {
    assert_listed("github");
}

/// Each GitLab fixture has a row in its verification log (all from docs, not live-verified).
#[test]
fn every_gitlab_fixture_is_listed_in_the_verification_log() {
    assert_listed("gitlab");
}
