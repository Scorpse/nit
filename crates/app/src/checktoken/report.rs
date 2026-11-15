use super::*;

// ---------------------------------------------------------------------------------------
// Report

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Pass,
    Fail,
    Skip,
}

impl Status {
    pub(super) fn tag(self) -> &'static str {
        match self {
            Status::Pass => "PASS",
            Status::Fail => "FAIL",
            Status::Skip => "SKIP",
        }
    }
}

#[derive(Clone)]
pub struct Check {
    pub id: String,
    pub title: &'static str,
    pub status: Status,
    pub detail: String,
    pub hint: Option<String>,
}

#[derive(Default)]
pub struct Report {
    /// Which account was tested (forge, host, id; no secret).
    pub account: Option<String>,
    pub source: Option<Source>,
    /// The numbered checks 1 to 9.
    pub checks: BTreeMap<u8, Check>,
    /// `--write-test` steps, in order; empty without it.
    pub write: Vec<Check>,
    pub write_target: Option<String>,
}

pub(super) fn title(n: u8) -> &'static str {
    match n {
        1 => "Credentials present",
        2 => "REST sign-in (GET /user)",
        3 => "List workspaces and repositories",
        4 => "List open pull requests",
        5 => "Read a pull request (diffstat, diff, comments)",
        6 => "Read approvals and permissions",
        7 => "Git sign-in (git ls-remote)",
        8 => "System git",
        9 => "HTTPS / TLS / proxy to the API",
        _ => "",
    }
}

impl Report {
    pub(super) fn set(
        &mut self,
        n: u8,
        status: Status,
        detail: impl Into<String>,
        hint: Option<String>,
    ) {
        let c = Check {
            id: n.to_string(),
            title: title(n),
            status,
            detail: detail.into(),
            hint,
        };
        self.checks.insert(n, c);
    }

    pub(super) fn pass(&mut self, n: u8, detail: impl Into<String>) {
        self.set(n, Status::Pass, detail, None);
    }

    pub(super) fn fail(&mut self, n: u8, detail: impl Into<String>, hint: Option<String>) {
        self.set(n, Status::Fail, detail, hint);
    }

    pub(super) fn skip(&mut self, n: u8, detail: impl Into<String>) {
        self.set(n, Status::Skip, detail, None);
    }

    pub(super) fn step(
        &mut self,
        n: usize,
        title: &'static str,
        status: Status,
        detail: String,
        hint: Option<String>,
    ) {
        self.write.push(Check {
            id: format!("W{n}"),
            title,
            status,
            detail,
            hint,
        });
    }

    #[cfg(test)]
    pub(super) fn status(&self, n: u8) -> Option<Status> {
        self.checks.get(&n).map(|c| c.status)
    }

    pub fn problems(&self) -> usize {
        self.checks
            .values()
            .chain(&self.write)
            .filter(|c| c.status == Status::Fail)
            .count()
    }

    /// The report as text, ending with `ALL OK` or `N problems`; every occurrence of a
    /// `secret` is replaced by `***`.
    pub fn render(&self, secrets: &[&str]) -> String {
        let mut out = String::from("nit token check\n");
        if let Some(a) = &self.account {
            out.push_str(&format!("Account: {a}\n"));
        }
        if let Some(s) = self.source {
            out.push_str(&format!("Credentials from: {}\n", s.label()));
        }
        out.push('\n');
        let line = |out: &mut String, c: &Check| {
            out.push_str(&format!(
                "[{}] {}. {}: {}\n",
                c.status.tag(),
                c.id,
                c.title,
                c.detail
            ));
            if let Some(h) = &c.hint {
                out.push_str(&format!("       Fix: {h}\n"));
            }
        };
        for c in self.checks.values() {
            line(&mut out, c);
        }
        if !self.write.is_empty() {
            out.push_str(&format!(
                "\nWrite test on {}:\n",
                self.write_target.as_deref().unwrap_or("?")
            ));
            for c in &self.write {
                line(&mut out, c);
            }
        }
        out.push('\n');
        out.push_str(&match self.problems() {
            0 => "ALL OK".to_string(),
            1 => "1 problem".to_string(),
            n => format!("{n} problems"),
        });
        out.push('\n');
        redact(&out, secrets)
    }
}

/// Replaces every secret (ignoring ones too short to be a real token) by `***`.
pub(super) fn redact(text: &str, secrets: &[&str]) -> String {
    secrets
        .iter()
        .filter(|s| s.len() >= 4)
        .fold(text.to_string(), |acc, s| acc.replace(s, "***"))
}

// ---------------------------------------------------------------------------------------
// Hints

pub(super) const HINT_CREDS: &str =
    "sign in once in nit (it saves the email and token in the system \
    credential store), or set NIT_EMAIL and NIT_TOKEN in this terminal and run again";
pub(super) const HINT_NEW_TOKEN: &str =
    "check that the email is your Atlassian account email and the \
    token an Atlassian API token (Security, API tokens, \"Create API token with scopes\", \
    Bitbucket) that is not expired or revoked; create a new one and sign in again";

/// A hint for an API error; `scope` names the access the call needs.
pub(super) fn api_hint(e: &ProviderError, scope: &str) -> Option<String> {
    match e {
        ProviderError::Auth => Some(HINT_NEW_TOKEN.into()),
        ProviderError::Forbidden | ProviderError::Rejected { status: 403, .. } => Some(format!(
            "the token was accepted but lacks access: it needs {scope}. Create a token with that \
             scope and sign in again (or ask for access to the repository)"
        )),
        ProviderError::NotFound | ProviderError::Rejected { status: 404, .. } => Some(
            "it does not exist or the token cannot see it: check --repo / --pr and that your \
             account has access"
                .into(),
        ),
        ProviderError::Network(m) if m == CERT_UNTRUSTED => Some(cert_hint()),
        ProviderError::Network(_) => Some("network problem, see check 9".into()),
        ProviderError::RateLimited => {
            Some("Bitbucket is rate-limiting; wait a few minutes and run again".into())
        }
        _ => None,
    }
}

pub(super) fn cert_hint() -> String {
    "a TLS-inspecting proxy (company network) presents a certificate Windows does not trust: \
     ask IT to install the company root certificate in Windows (Trusted Root Certification \
     Authorities)"
        .into()
}
