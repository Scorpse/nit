use super::*;

// ---------------------------------------------------------------------------------------
// Entry point: console, report file, exit code

pub(super) fn report_path() -> PathBuf {
    crate::brand::data_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(REPORT_FILE)
}

pub(super) fn proxy_var() -> Option<&'static str> {
    [
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
        "HTTP_PROXY",
        "http_proxy",
    ]
    .into_iter()
    .find(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty()))
}

/// Whether the token may go to `api_base`: https (`Ok(None)`), plain http on a private
/// address (`Ok` with the cleartext warning), or not at all (`Err`, nothing is sent).
pub(super) fn transport_note(api_base: &str) -> Result<Option<&'static str>, &'static str> {
    if !prcore::origin::transport_allowed(api_base) {
        return Err(
            "the account's API address is not https (plain http only on a private \
                    address); nothing was sent",
        );
    }
    Ok(prcore::origin::is_private_http_url(api_base).then_some(prcore::origin::CLEARTEXT_WARNING))
}

/// Runs the check and returns the process exit code (0: no problem, 1: at least one).
pub fn main(args: CheckArgs) -> i32 {
    let mut out = console::attach();
    let secrets = store::CredentialManagerSecrets::new(crate::brand::NAME);
    let _ = crate::brand::migrate_credentials(
        &secrets,
        &store::CredentialManagerSecrets::new(crate::brand::LEGACY_NAME),
    );
    let env = |k: &str| std::env::var(k).ok();
    // the accounts (no secrets) from the cache, when there is one
    let accounts = crate::brand::data_dir()
        .map(|d| d.join("cache.sqlite"))
        .filter(|p| p.exists())
        .and_then(|p| store::Store::open(&p).ok())
        .and_then(|s| s.accounts().ok())
        .unwrap_or_default();
    let account = match select_account(&accounts, args.account.as_deref()) {
        Ok(a) => a,
        // a first run without a stored GitHub or GitLab account: NIT_GITHUB_TOKEN /
        // NIT_GITHUB_URL, NIT_GITLAB_TOKEN / NIT_GITLAB_URL
        Err(e) => match github::env_account(args.account.as_deref(), env)
            .or_else(|| gitlab::env_account(args.account.as_deref(), env))
        {
            Some(a) => a,
            None => {
                out.write(&format!("--account: {e}\n"));
                return 2;
            }
        },
    };
    let args = match args
        .repo
        .as_ref()
        .map(|r| repo_for(&account, r))
        .transpose()
    {
        Ok(repo) => CheckArgs { repo, ..args },
        Err(e) => {
            out.write(&format!("{e}\n"));
            return 2;
        }
    };
    match transport_note(&account.api_base) {
        Ok(None) => {}
        Ok(Some(warning)) => out.write(&format!("warning: {warning}\n")),
        Err(e) => {
            out.write(&format!("{e}\n"));
            return 2;
        }
    }
    let github_token =
        (account.forge == Forge::GitHub).then(|| github::load_token(&secrets, &account, env));
    let gitlab_token =
        (account.forge == Forge::GitLab).then(|| gitlab::load_token(&secrets, &account, env));
    let creds = load_account_credentials(&secrets, &account, env);
    let mut secret_values = tokens_to_scrub(&secrets, &accounts, &creds, env);
    for (t, _) in [&github_token, &gitlab_token]
        .into_iter()
        .flatten()
        .flatten()
    {
        secret_values.push(t.clone());
    }
    let report = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(rt) => {
            let mut report = match (github_token, gitlab_token) {
                (_, Some(token)) => {
                    let bases = gitlab::bases_of(&account);
                    let inputs = gitlab::Inputs {
                        args: &args,
                        bases: &bases,
                        git: &SystemGit,
                        proxy_var: proxy_var(),
                        today: chrono::Local::now().date_naive(),
                    };
                    rt.block_on(gitlab::run(&inputs, token))
                }
                (Some(token), None) => {
                    let bases = github::bases_of(&account);
                    let inputs = github::Inputs {
                        args: &args,
                        bases: &bases,
                        git: &SystemGit,
                        proxy_var: proxy_var(),
                    };
                    rt.block_on(github::run(&inputs, token))
                }
                (None, None) => {
                    let inputs = Inputs {
                        args: &args,
                        api_base: &account.api_base,
                        git: &SystemGit,
                        proxy_var: proxy_var(),
                    };
                    rt.block_on(run(&inputs, creds))
                }
            };
            report.account = Some(format!(
                "{} {} ({})",
                account.forge.label(),
                account.git_host,
                account.id.as_str()
            ));
            report
        }
        Err(_) => {
            out.write("could not start the async runtime\n");
            return 1;
        }
    };
    let secrets: Vec<&str> = secret_values.iter().map(String::as_str).collect();
    let text = report.render(&secrets);
    let path = report_path();
    let saved = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|_| std::fs::write(&path, &text));
    let where_line = match &saved {
        Ok(()) => format!("Report saved to {}\n", path.display()),
        Err(e) => format!("Could not save the report to {}: {e}\n", path.display()),
    };
    out.write(&format!("\n{where_line}\n{text}"));
    // Windows: started without a console (double-click, shortcut): show the file. Elsewhere
    // the report always goes to stdout (`console::attach`), so there is nothing to open.
    #[cfg(windows)]
    if !out.visible() && saved.is_ok() {
        let _ = std::process::Command::new("notepad.exe").arg(&path).spawn();
    }
    i32::from(report.problems() > 0)
}

/// Where the report is printed: the inherited stdout, the parent's console, or nowhere.
mod console {
    use std::io::Write;

    // only Windows has a console to attach to (or none)
    #[cfg_attr(not(windows), allow(dead_code))]
    pub enum Out {
        Std,
        Console(std::fs::File),
        None,
    }

    impl Out {
        pub fn write(&mut self, text: &str) {
            let _ = match self {
                Out::Std => {
                    let mut s = std::io::stdout();
                    s.write_all(text.as_bytes()).and_then(|_| s.flush())
                }
                Out::Console(f) => f.write_all(text.as_bytes()),
                Out::None => Ok(()),
            };
        }

        #[cfg_attr(not(windows), allow(dead_code))]
        pub fn visible(&self) -> bool {
            !matches!(self, Out::None)
        }
    }

    /// A GUI-subsystem exe has no console: use stdout when it was handed one (a pipe, a
    /// file, a debug build's console), else attach to the console of the parent (the
    /// terminal it was started from).
    #[cfg(windows)]
    pub fn attach() -> Out {
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::System::Console::{
            AttachConsole, GetStdHandle, ATTACH_PARENT_PROCESS, STD_OUTPUT_HANDLE,
        };
        // SAFETY: plain Win32 calls without pointers
        unsafe {
            let h = GetStdHandle(STD_OUTPUT_HANDLE);
            if !h.is_null() && h != INVALID_HANDLE_VALUE {
                return Out::Std;
            }
            if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
                return Out::None;
            }
        }
        std::fs::OpenOptions::new()
            .write(true)
            .open("CONOUT$")
            .map(Out::Console)
            .unwrap_or(Out::None)
    }

    /// macOS and Unix: a program always has its standard output (a terminal, a pipe, or
    /// /dev/null under a launcher), so the report is printed there; no console to attach.
    #[cfg(not(windows))]
    pub fn attach() -> Out {
        Out::Std
    }
}
