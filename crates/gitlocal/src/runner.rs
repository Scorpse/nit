use base64::Engine;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MIN_VERSION: (u32, u32) = (2, 31);

/// Basic-auth user name Bitbucket Cloud expects for git over HTTPS with an Atlassian API
/// token (the REST API takes `email:token` instead).
pub const BITBUCKET_GIT_USER: &str = "x-bitbucket-api-token-auth";
/// GitHub: the token is the password, the user name is ignored; `x-access-token` is the
/// documented name for app tokens (spec 7.2).
pub const GITHUB_GIT_USER: &str = "x-access-token";
/// GitLab: any non-blank user name works with a PAT; `oauth2` is the documented one for
/// OAuth tokens (spec 7.2).
pub const GITLAB_GIT_USER: &str = "oauth2";

/// Longest a clone or fetch may run before it is killed. Generous on purpose: a stalled
/// transfer is ended much earlier by the low-speed check (`LOW_SPEED_*`), so this only
/// bounds a transfer that keeps progressing (a very large first clone).
pub const NETWORK_TIMEOUT: Duration = Duration::from_secs(2 * 60 * 60);

/// Longest a diff / show command may run; on a blobless mirror it lazily fetches blobs.
/// A stalled lazy fetch is ended by the low-speed check.
pub const LAZY_FETCH_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// `http.lowSpeedLimit` / `http.lowSpeedTime` of network commands: a transfer that stays
/// below this many bytes per second for `LOW_SPEED_TIME` is aborted by git (curl). Server
/// keep-alives during pack generation (`uploadpack.keepAlive`, every 5 s) keep a slow but
/// working server above it.
pub(crate) const LOW_SPEED_LIMIT: u32 = 1;
pub(crate) const LOW_SPEED_TIME: Duration = Duration::from_secs(5 * 60);

/// Handle to the system `git` executable. Cheap to clone; clones share the spawn counter.
#[derive(Clone, Debug)]
pub struct Git {
    pub(crate) exe: PathBuf,
    /// git processes started through this handle and its clones.
    spawns: Arc<AtomicUsize>,
}

/// File name of the git executable on this platform.
const GIT_EXE: &str = if cfg!(windows) { "git.exe" } else { "git" };

/// The real executable behind a Git for Windows launcher: `<root>\cmd\git.exe` starts
/// `<root>\mingw64\bin\git.exe` (or the clangarm64 / mingw32 one) as a child, which costs
/// one more process per call. None when `exe` is not laid out like a launcher.
pub(crate) fn launcher_target(exe: &Path) -> Option<PathBuf> {
    let dir = exe.parent()?;
    if !dir.file_name()?.eq_ignore_ascii_case("cmd") {
        return None;
    }
    let root = dir.parent()?;
    let name = exe.file_name()?;
    ["mingw64", "clangarm64", "mingw32"]
        .iter()
        .map(|sub| root.join(sub).join("bin").join(name))
        .find(|p| p.is_file())
}

/// `<prefix>/libexec/git-core` (from `git --exec-path`) -> `<prefix>/bin/git[.exe]`.
fn exec_path_target(exec_path: &Path) -> Option<PathBuf> {
    let prefix = exec_path.parent()?.parent()?;
    let p = prefix.join("bin").join(GIT_EXE);
    p.is_file().then_some(p)
}

/// `exe` already is the real binary: `<prefix>/bin/git` next to `<prefix>/libexec/git-core`.
fn is_real_binary(exe: &Path) -> bool {
    exe.parent()
        .filter(|d| d.file_name().is_some_and(|n| n.eq_ignore_ascii_case("bin")))
        .and_then(Path::parent)
        .is_some_and(|prefix| prefix.join("libexec").join("git-core").is_dir())
}

/// First `git` executable on PATH (absolute PATH entries only, so the current directory
/// never supplies one).
fn find_on_path() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|d| d.is_absolute())
        .map(|d| d.join(GIT_EXE))
        .find(|p| p.is_file())
}

fn spawn_error(e: std::io::Error) -> GitError {
    if e.kind() == std::io::ErrorKind::NotFound {
        GitError::NotInstalled
    } else {
        GitError::Io(e.to_string())
    }
}

/// `exe --version` runs and reports at least `MIN_VERSION`.
fn verify_version(exe: &Path) -> Result<(), GitError> {
    let out = command(exe)
        .arg("--version")
        .output()
        .map_err(spawn_error)?;
    check_version(&String::from_utf8_lossy(&out.stdout))
}

/// `exe --exec-path`, without an inherited `GIT_EXEC_PATH` (it would make git report that
/// directory instead of its own, and so redirect which binary is run).
fn exec_path_command(exe: &Path) -> Command {
    let mut cmd = command(exe);
    cmd.env_remove("GIT_EXEC_PATH").arg("--exec-path");
    cmd
}

/// `exe --exec-path`, when it runs.
fn exec_path_of(exe: &Path) -> Option<PathBuf> {
    let out = exec_path_command(exe).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !text.is_empty()).then(|| PathBuf::from(text))
}

/// Picks the executable to run: the real binary behind a launcher when it exists and
/// passes `verify`, otherwise `found` itself (which must pass `verify`).
fn resolve_exe(
    found: PathBuf,
    verify: &dyn Fn(&Path) -> Result<(), GitError>,
    exec_path: &dyn Fn(&Path) -> Option<PathBuf>,
) -> Result<PathBuf, GitError> {
    let real = launcher_target(&found).or_else(|| {
        // a launcher of another layout (scoop shim, ...): ask git where it lives
        let ask = cfg!(windows) && found.is_absolute() && !is_real_binary(&found);
        ask.then(|| exec_path(&found))
            .flatten()
            .and_then(|e| exec_path_target(&e))
    });
    if let Some(real) = real.filter(|r| *r != found) {
        if verify(&real).is_ok() {
            return Ok(real);
        }
    }
    verify(&found)?;
    Ok(found)
}

/// Kills a git process and everything it started (Windows: a job object closed with
/// `KILL_ON_JOB_CLOSE`, which also reaches `git-remote-https` and other grandchildren).
#[cfg(windows)]
mod job {
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    pub(crate) struct Job(HANDLE);

    // SAFETY: a job handle may be used and closed from any thread
    unsafe impl Send for Job {}

    impl Job {
        /// Puts `child` (and so every process it starts from now on) into a new job.
        pub(crate) fn assign(child: &Child) -> Option<Job> {
            // SAFETY: plain Win32 calls on a handle we own; `info` outlives the call
            unsafe {
                let h = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if h.is_null() {
                    return None;
                }
                let job = Job(h);
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let set = SetInformationJobObject(
                    h,
                    JobObjectExtendedLimitInformation,
                    (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                if set == 0 || AssignProcessToJobObject(h, child.as_raw_handle() as HANDLE) == 0 {
                    return None;
                }
                Some(job)
            }
        }

        pub(crate) fn kill(&self) {
            // SAFETY: valid job handle owned by self
            unsafe {
                TerminateJobObject(self.0, 1);
            }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: closing our own handle once; KILL_ON_JOB_CLOSE ends what is left
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

/// Kills a git process and everything it started (macOS / Unix: the child leads its own
/// process group, see `isolate`, and the whole group gets `SIGKILL`; that also reaches
/// `git-remote-https` and other grandchildren that did not start a group of their own).
#[cfg(unix)]
mod job {
    use std::process::Child;

    /// The process group of a child spawned through `isolate` (its id is the child's pid).
    pub(crate) struct Job(libc::pid_t);

    impl Job {
        pub(crate) fn assign(child: &Child) -> Option<Job> {
            libc::pid_t::try_from(child.id())
                .ok()
                .filter(|pid| *pid > 1)
                .map(Job)
        }

        pub(crate) fn kill(&self) {
            // SAFETY: a plain syscall; a negative pid addresses the process group, which only
            // holds the child we started and what it started
            unsafe {
                libc::kill(-self.0, libc::SIGKILL);
            }
        }
    }

    impl Drop for Job {
        /// Like `KILL_ON_JOB_CLOSE` on Windows: whatever git left running is ended when the
        /// command is done. The child itself has been waited for by then; while any member
        /// of the group lives the id cannot be reused, and with none left the call only
        /// fails with `ESRCH`.
        fn drop(&mut self) {
            self.kill();
        }
    }
}

#[cfg(not(any(windows, unix)))]
mod job {
    use std::process::Child;

    pub(crate) struct Job;

    impl Job {
        pub(crate) fn assign(_child: &Child) -> Option<Job> {
            None
        }

        pub(crate) fn kill(&self) {}
    }
}

use job::Job;

/// macOS / Unix: starts the child as the leader of a new process group, so `Job` can end it
/// together with everything it starts. Elsewhere nothing (Windows puts it in a job object).
fn isolate(cmd: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(not(unix))]
    let _ = cmd;
}

/// Credentials of one account for its HTTPS git host. Only ever handed to git through
/// environment variables of the child process (see `Git::run`), as a Basic `user:token`
/// header that git sends to `scope` only: `https://host[:port]/` of the account's git host,
/// never derived from a remote URL and never defaulted.
pub struct GitAuth {
    /// Basic-auth user name: `BITBUCKET_GIT_USER`, `GITHUB_GIT_USER` or `GITLAB_GIT_USER`.
    pub user: String,
    pub token: String,
    /// `https://host[:port]/`, lower-case (see `GitAuth::new`); `http://host[:port]/` only
    /// for a private address (see `GitAuth::cleartext`).
    pub scope: String,
}

impl GitAuth {
    /// Credentials for `host` (`bitbucket.org`, `git.corp.example:8443`), as `user`. A path
    /// prefix (a GitLab relative URL root, `git.corp.example/gitlab`) is dropped: the scope is
    /// always `https://host[:port]/`, the form `auth_for_remote` compares.
    pub fn new(user: &str, token: &str, host: &str) -> GitAuth {
        let host = host.split('/').next().unwrap_or(host);
        GitAuth {
            user: user.to_string(),
            token: token.to_string(),
            scope: format!("https://{}/", host.to_ascii_lowercase()),
        }
    }

    /// Credentials sent over plain `http://` to `host` (a path prefix is dropped as in
    /// `new`), only when it is a private or loopback address written literally
    /// (`prcore::origin::is_private_host`); None for any other host.
    pub fn cleartext(user: &str, token: &str, host: &str) -> Option<GitAuth> {
        let host = host.split('/').next().unwrap_or(host).to_ascii_lowercase();
        prcore::origin::split_authority(&host)
            .filter(|(h, _)| prcore::origin::is_private_host(h))?;
        Some(GitAuth {
            user: user.to_string(),
            token: token.to_string(),
            scope: format!("http://{host}/"),
        })
    }

    /// The URL scheme of the scope: `https`, or `http` for a cleartext account.
    pub fn scheme(&self) -> &'static str {
        if self.scope.starts_with("http://") {
            "http"
        } else {
            "https"
        }
    }

    /// A Bitbucket Cloud API token for `bitbucket.org`.
    pub fn bitbucket(token: &str) -> GitAuth {
        GitAuth::new(BITBUCKET_GIT_USER, token, "bitbucket.org")
    }
}

impl fmt::Debug for GitAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GitAuth")
            .field("user", &self.user)
            .field("token", &"***")
            .field("scope", &self.scope)
            .finish()
    }
}

/// `auth` when `url` is a remote on exactly its scope (scheme, host and port), else None: a
/// remote anywhere else (another host, another port, the other scheme, user info) gets no
/// credential.
pub fn auth_for_remote<'a>(url: &str, auth: &'a GitAuth) -> Option<&'a GitAuth> {
    (url_scope(url).as_deref() == Some(auth.scope.as_str())).then_some(auth)
}

#[derive(Debug, PartialEq, Eq)]
pub enum GitError {
    NotInstalled,
    TooOld(String),
    Failed {
        code: Option<i32>,
        stderr: String,
    },
    Io(String),
    InvalidUrl(String),
    InvalidName(String),
    /// A repository or workspace name Windows reserves for a device (`con`, `nul`, ...).
    ReservedName(String),
    InvalidMirror(String),
    InvalidRef(String),
    NoMergeBase,
    TooLarge {
        limit: usize,
    },
    /// git ran longer than `after` and was killed.
    Timeout {
        after: Duration,
    },
    /// The caller's cancel check fired; git and everything it started were killed.
    Cancelled,
}

fn human_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 60 {
        format!("{} minutes", secs / 60)
    } else if secs >= 1 {
        format!("{secs} seconds")
    } else {
        format!("{} ms", d.as_millis())
    }
}

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GitError::NotInstalled => write!(f, "git is not installed or not on PATH"),
            GitError::TooOld(v) => write!(f, "git {v} is too old (need 2.31 or newer)"),
            GitError::Failed { code, stderr } => match code {
                Some(c) => write!(f, "git failed (exit {c}): {}", stderr.trim()),
                None => write!(f, "git failed: {}", stderr.trim()),
            },
            GitError::Io(m) => write!(f, "git i/o error: {m}"),
            GitError::InvalidUrl(m) => write!(f, "invalid remote url: {m}"),
            GitError::InvalidName(m) => write!(f, "invalid repository name: {m}"),
            GitError::ReservedName(m) => write!(
                f,
                "repository name {m} is reserved on Windows and cannot be stored locally"
            ),
            GitError::InvalidMirror(m) => write!(f, "invalid local mirror: {m}"),
            GitError::InvalidRef(m) => write!(f, "invalid branch or revision: {m}"),
            GitError::NoMergeBase => write!(
                f,
                "the source and destination branches share no common history (unrelated histories)"
            ),
            GitError::TooLarge { limit } => {
                write!(f, "git output exceeds the {limit} byte limit")
            }
            GitError::Timeout { after } => write!(
                f,
                "git did not finish within {} and was stopped (the network may be stalled)",
                human_duration(*after)
            ),
            GitError::Cancelled => write!(f, "git was stopped because the work was cancelled"),
        }
    }
}

impl std::error::Error for GitError {}

fn auth_header_b64(auth: &GitAuth) -> String {
    base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", auth.user, auth.token))
}

fn is_https(url: &str) -> bool {
    url.len() > 8 && url.is_char_boundary(8) && url[..8].eq_ignore_ascii_case("https://")
}

fn is_http(url: &str) -> bool {
    url.len() > 7 && url.is_char_boundary(7) && url[..7].eq_ignore_ascii_case("http://")
}

/// `https://<host[:port]>/` of an https `url`, or `http://<host[:port]>/` of a plain-http
/// `url` on a private address (lower-case); None for anything else, including a host with
/// user info or unexpected characters and plain http to any other host.
pub(crate) fn url_scope(url: &str) -> Option<String> {
    let (scheme, rest) = if is_https(url) {
        ("https", &url[8..])
    } else if is_http(url) {
        ("http", &url[7..])
    } else {
        return None;
    };
    let h = rest.split(['/', '?', '#']).next()?.to_ascii_lowercase();
    let clean = !h.is_empty()
        && h.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']'));
    if !clean {
        return None;
    }
    if scheme == "http"
        && !prcore::origin::split_authority(&h)
            .is_some_and(|(host, _)| prcore::origin::is_private_host(host))
    {
        return None;
    }
    Some(format!("{scheme}://{h}/"))
}

fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Every textual form in which the credentials might show up in git output (also what the
/// app's data-folder leak check looks for).
pub fn secret_forms(a: &GitAuth) -> Vec<String> {
    let std = auth_header_b64(a);
    let urlsafe = std.replace('+', "-").replace('/', "_");
    let mut bases = vec![std, urlsafe, a.token.clone()];
    let unpadded: Vec<String> = bases
        .iter()
        .map(|b| b.trim_end_matches('=').to_string())
        .collect();
    bases.extend(unpadded);
    let encoded: Vec<String> = bases.iter().map(|b| percent_encode(b)).collect();
    bases.extend(encoded);
    bases.retain(|s| !s.is_empty());
    bases.sort_by_key(|s| std::cmp::Reverse(s.len()));
    bases.dedup();
    bases
}

fn scrub(text: &str, auth: Option<&GitAuth>) -> String {
    let Some(a) = auth else {
        return text.to_string();
    };
    // longest first so a long form is never split by a shorter one
    secret_forms(a)
        .iter()
        .fold(text.to_string(), |acc, f| acc.replace(f.as_str(), "***"))
}

fn parse_version(s: &str) -> Option<(u32, u32)> {
    let v = s.trim().strip_prefix("git version ")?;
    let mut parts = v.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

fn check_version(text: &str) -> Result<(), GitError> {
    match parse_version(text) {
        Some(v) if v >= MIN_VERSION => Ok(()),
        _ => Err(GitError::TooOld(text.trim().to_string())),
    }
}

fn command(exe: &Path) -> Command {
    let mut cmd = Command::new(exe);
    // never let the parent's git environment redirect or reconfigure us, and never let a
    // credential prompt (askpass GUI, terminal) wait for input nobody can see
    for var in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_CONFIG_COUNT",
        "GIT_ASKPASS",
        "SSH_ASKPASS",
        "SSH_ASKPASS_REQUIRE",
    ] {
        cmd.env_remove(var);
    }
    for (k, _) in std::env::vars_os() {
        let upper = k.to_string_lossy().to_ascii_uppercase();
        if upper.starts_with("GIT_CONFIG_KEY_") || upper.starts_with("GIT_CONFIG_VALUE_") {
            cmd.env_remove(&k);
        }
    }
    cmd.env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd
}

impl Git {
    pub(crate) fn from_exe(exe: impl Into<PathBuf>) -> Git {
        Git {
            exe: exe.into(),
            spawns: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Finds `git` on PATH, resolves the real binary behind a Windows launcher
    /// (`cmd\git.exe`), and checks it is at least 2.31. Falls back to what PATH gives when
    /// the real binary is missing or does not run. Blocking (one to three spawns).
    pub fn locate() -> Result<Git, GitError> {
        let found = find_on_path().unwrap_or_else(|| PathBuf::from("git"));
        let exe = resolve_exe(found, &verify_version, &exec_path_of)?;
        Ok(Git::from_exe(exe))
    }

    /// The process-wide located git: `locate` runs once, later calls return a clone. A
    /// failure is not remembered, so installing git later is noticed. Blocking on first use.
    pub fn shared() -> Result<Git, GitError> {
        static SHARED: Mutex<Option<Git>> = Mutex::new(None);
        let mut slot = SHARED.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(git) = slot.as_ref() {
            return Ok(git.clone());
        }
        let git = Git::locate()?;
        *slot = Some(git.clone());
        Ok(git)
    }

    /// The executable that is run.
    pub fn exe(&self) -> &Path {
        &self.exe
    }

    /// git processes started through this handle (and its clones) so far.
    pub fn spawn_count(&self) -> usize {
        self.spawns.load(Ordering::Relaxed)
    }

    /// Runs `git <args>` in `dir` and returns stdout. Blocking; call from a worker thread.
    /// Credentials (if any) go only into env vars of the child process; a command with
    /// credentials is killed after `NETWORK_TIMEOUT`.
    pub fn run(
        &self,
        dir: &Path,
        args: &[&str],
        auth: Option<&GitAuth>,
    ) -> Result<Vec<u8>, GitError> {
        let opts = RunOpts {
            auth,
            timeout: auth.map(|_| NETWORK_TIMEOUT),
            ..RunOpts::default()
        };
        self.run_with(dir, args, opts)
    }

    /// Like `run`, but stops and returns `TooLarge` once stdout exceeds `max_bytes`; a
    /// command with credentials is killed after `LAZY_FETCH_TIMEOUT`.
    pub fn run_limited(
        &self,
        dir: &Path,
        args: &[&str],
        auth: Option<&GitAuth>,
        max_bytes: usize,
    ) -> Result<Vec<u8>, GitError> {
        let opts = RunOpts {
            auth,
            max_bytes: Some(max_bytes),
            timeout: auth.map(|_| LAZY_FETCH_TIMEOUT),
            ..RunOpts::default()
        };
        self.run_with(dir, args, opts)
    }

    pub(crate) fn run_with(
        &self,
        dir: &Path,
        args: &[&str],
        opts: RunOpts<'_>,
    ) -> Result<Vec<u8>, GitError> {
        if opts.cancel.is_some_and(|c| c()) {
            return Err(GitError::Cancelled); // not started, so not counted
        }
        let cmd = self.prepared(dir, args, opts.auth, opts.url);
        self.spawns.fetch_add(1, Ordering::Relaxed);
        execute_reporting(
            cmd,
            opts.auth,
            opts.max_bytes,
            opts.timeout,
            opts.cancel,
            opts.progress,
        )
    }

    pub(crate) fn prepared(
        &self,
        dir: &Path,
        args: &[&str],
        auth: Option<&GitAuth>,
        url: Option<&str>,
    ) -> Command {
        let mut cmd = command(&self.exe);
        cmd.current_dir(dir);
        if auth.is_some() {
            // empty values reset the helper list and the askpass program: no stored
            // credential interferes and nothing can prompt
            cmd.args(["-c", "credential.helper=", "-c", "core.askPass="]);
        }
        cmd.args(args);
        if let Some(a) = auth {
            // the header is scoped to the account's origin only, whatever the remote is; an
            // http(s) remote elsewhere does not even get it in the child's environment
            let header = match url {
                Some(u) if (is_https(u) || is_http(u)) && auth_for_remote(u, a).is_none() => {
                    String::new()
                }
                _ => format!("Authorization: Basic {}", auth_header_b64(a)),
            };
            cmd.env("GIT_CONFIG_COUNT", "4")
                .env("GIT_CONFIG_KEY_0", format!("http.{}.extraheader", a.scope))
                .env("GIT_CONFIG_VALUE_0", header)
                // a redirect must not carry the header (or the request) to another host
                .env("GIT_CONFIG_KEY_1", "http.followRedirects")
                .env("GIT_CONFIG_VALUE_1", "false")
                // a stalled transfer fails fast; a slow but progressing one runs on
                .env("GIT_CONFIG_KEY_2", "http.lowSpeedLimit")
                .env("GIT_CONFIG_VALUE_2", LOW_SPEED_LIMIT.to_string())
                .env("GIT_CONFIG_KEY_3", "http.lowSpeedTime")
                .env("GIT_CONFIG_VALUE_3", LOW_SPEED_TIME.as_secs().to_string());
        }
        cmd
    }
}

/// How one git command is run.
#[derive(Clone, Copy, Default)]
pub(crate) struct RunOpts<'a> {
    /// Credentials; makes this a network command (header, no prompts, no redirects).
    pub auth: Option<&'a GitAuth>,
    /// The remote the command talks to; scopes the auth header to its host.
    pub url: Option<&'a str>,
    /// Stop with `TooLarge` once stdout exceeds this many bytes.
    pub max_bytes: Option<usize>,
    /// Kill the child and return `Timeout` after this long.
    pub timeout: Option<Duration>,
    /// Asked about every `CANCEL_POLL` while git runs; true kills the child (and its job)
    /// and returns `Cancelled`.
    pub cancel: Option<Cancel<'a>>,
    /// Receives the progress lines of stderr (the command must ask git for `--progress`).
    pub progress: Option<Progress<'a>>,
}

/// A cancel check: true once the work git does is no longer wanted.
pub type Cancel<'a> = &'a dyn Fn() -> bool;

/// Called on the calling thread with each progress line git printed (`--progress`).
pub type Progress<'a> = &'a dyn Fn(GitProgress);

/// One step of git's `--progress` output: the phase and how far it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitProgress {
    /// "Counting objects", "Receiving objects", "Resolving deltas", ...
    pub phase: String,
    /// 0 to 100.
    pub percent: u8,
}

/// Parses one progress line such as `remote: Counting objects:  12% (1/8)` or
/// `Receiving objects:  45% (450/1000), 1.20 MiB | 1.00 MiB/s`. Only a phase of letters and
/// spaces followed by a percentage is accepted, never a path, URL or message.
pub fn parse_progress(line: &str) -> Option<GitProgress> {
    let line = line.trim();
    let line = line.strip_prefix("remote:").map_or(line, str::trim_start);
    let (phase, rest) = line.split_once(':')?;
    let phase = phase.trim();
    if phase.is_empty()
        || phase.len() > 40
        || !phase.chars().all(|c| c.is_ascii_alphabetic() || c == ' ')
    {
        return None;
    }
    let rest = rest.trim_start();
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 || digits > 3 || !rest[digits..].starts_with('%') {
        return None;
    }
    let percent: u8 = rest[..digits].parse().ok().filter(|p| *p <= 100)?;
    Some(GitProgress {
        phase: phase.to_string(),
        percent,
    })
}

/// How often a running command asks its cancel check.
const CANCEL_POLL: Duration = Duration::from_millis(50);

/// How often progress lines are handed to the caller while git runs.
const PROGRESS_POLL: Duration = Duration::from_millis(100);

/// Kills the child and, through its job, every process it started.
fn kill_tree(child: &mut Child, job: Option<&Job>) {
    if let Some(j) = job {
        j.kill();
    }
    let _ = child.kill();
}

fn kill_and_reap(child: &mut Child, job: Option<&Job>) {
    kill_tree(child, job);
    let _ = child.wait();
}

/// Waits for `child` to exit; at `deadline` it is killed and `Timeout` returned, and when
/// `cancel` fires it is killed and `Cancelled` returned.
fn wait_until(
    child: &mut Child,
    job: Option<&Job>,
    deadline: Option<(Instant, Duration)>,
    cancel: Option<Cancel<'_>>,
) -> Result<std::io::Result<ExitStatus>, GitError> {
    if deadline.is_none() && cancel.is_none() {
        return Ok(child.wait());
    }
    let mut nap = Duration::from_millis(1);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(Ok(status)),
            Ok(None) => {}
            Err(e) => return Ok(Err(e)),
        }
        if cancel.is_some_and(|c| c()) {
            kill_and_reap(child, job);
            return Err(GitError::Cancelled);
        }
        let now = Instant::now();
        let nap_now = match deadline {
            Some((at, after)) if now >= at => {
                kill_and_reap(child, job);
                return Err(GitError::Timeout { after });
            }
            Some((at, _)) => nap.min(at - now),
            None => nap,
        };
        std::thread::sleep(nap_now);
        nap = (nap * 2).min(CANCEL_POLL);
    }
}

/// Spawns `cmd`, collects stdout (bounded by `max_bytes`) and at most 1 MiB of stderr, and
/// kills it (returning `Timeout`) once `timeout` elapsed, or (returning `Cancelled`) once
/// `cancel` says so. Error text is scrubbed of `auth`.
#[cfg(test)]
pub(crate) fn execute(
    cmd: Command,
    auth: Option<&GitAuth>,
    max_bytes: Option<usize>,
    timeout: Option<Duration>,
    cancel: Option<Cancel<'_>>,
) -> Result<Vec<u8>, GitError> {
    execute_reporting(cmd, auth, max_bytes, timeout, cancel, None)
}

/// Like `execute`; progress lines of stderr are also parsed and handed to `progress` on
/// this thread while git runs.
pub(crate) fn execute_reporting(
    mut cmd: Command,
    auth: Option<&GitAuth>,
    max_bytes: Option<usize>,
    timeout: Option<Duration>,
    cancel: Option<Cancel<'_>>,
    progress: Option<Progress<'_>>,
) -> Result<Vec<u8>, GitError> {
    use std::io::Read;
    // work that is no longer wanted is not started: the cancel check used to be asked only
    // once git had run for a poll interval, so a command that finished sooner (most of
    // them, where starting a process is fast) ran to the end and returned its output
    if cancel.is_some_and(|c| c()) {
        return Err(GitError::Cancelled);
    }
    let io = |e: std::io::Error| GitError::Io(scrub(&e.to_string(), auth));
    let deadline = timeout.map(|t| (Instant::now() + t, t));
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    isolate(&mut cmd);
    let mut child = cmd.spawn().map_err(io)?;
    // dropped when this returns: whatever git left running is ended with it
    let job = Job::assign(&child);
    let job = job.as_ref();
    let mut stderr = child.stderr.take().expect("piped stderr");
    // Keep at most 1 MiB of stderr but keep draining so the child never blocks on it; with
    // `progress`, every line (git ends progress lines with `\r`) is parsed on the way.
    let (ptx, prx) = std::sync::mpsc::channel::<GitProgress>();
    let parse = progress.is_some();
    let err_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut line = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = match stderr.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let keep = (1usize << 20).saturating_sub(buf.len()).min(n);
            buf.extend_from_slice(&chunk[..keep]);
            if !parse {
                continue;
            }
            for &b in &chunk[..n] {
                if b == b'\r' || b == b'\n' {
                    if let Some(p) = parse_progress(&String::from_utf8_lossy(&line)) {
                        let _ = ptx.send(p);
                    }
                    line.clear();
                } else if line.len() < 512 {
                    line.push(b);
                }
            }
        }
        buf
    });
    let report = || {
        if let Some(f) = progress {
            for p in prx.try_iter() {
                f(p);
            }
        }
    };
    let stdout = child.stdout.take().expect("piped stdout");
    let limit = max_bytes.map_or(u64::MAX, |m| (m as u64).saturating_add(1));
    let (tx, rx) = std::sync::mpsc::channel();
    // read on its own thread so a stalled child can be timed out; never joined after a
    // timeout, as a grandchild that inherited the pipe could keep it open forever
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let read = stdout.take(limit).read_to_end(&mut buf);
        let _ = tx.send((buf, read));
    });
    let received = loop {
        let left = deadline.map(|(at, _)| at.saturating_duration_since(Instant::now()));
        let tick = match (cancel, progress) {
            (Some(_), _) => Some(CANCEL_POLL),
            (None, Some(_)) => Some(PROGRESS_POLL),
            (None, None) => None,
        };
        let step = match (left, tick) {
            (None, None) => break rx.recv().ok(),
            (Some(left), None) => left,
            (Some(left), Some(t)) => left.min(t),
            (None, Some(t)) => t,
        };
        match rx.recv_timeout(step) {
            Ok(got) => break Some(got),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break None,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                report();
                if cancel.is_some_and(|c| c()) {
                    kill_and_reap(&mut child, job);
                    return Err(GitError::Cancelled);
                }
                if deadline.is_some_and(|(at, _)| Instant::now() >= at) {
                    break None;
                }
            }
        }
    };
    let Some((buf, read)) = received else {
        kill_and_reap(&mut child, job);
        return Err(match deadline {
            Some((_, after)) => GitError::Timeout { after },
            None => GitError::Io("could not read the git output".into()),
        });
    };
    let too_large = max_bytes.is_some_and(|m| buf.len() > m);
    if too_large || read.is_err() {
        kill_tree(&mut child, job);
    }
    let status = wait_until(&mut child, job, deadline, cancel)?.map_err(io)?;
    if let (true, Some(limit)) = (too_large, max_bytes) {
        // do not join: a grandchild that inherited the pipe could keep it open forever
        drop(err_thread);
        return Err(GitError::TooLarge { limit });
    }
    let err_bytes = err_thread.join().unwrap_or_default();
    report();
    read.map_err(io)?;
    if status.success() {
        Ok(buf)
    } else {
        Err(GitError::Failed {
            code: status.code(),
            stderr: scrub(&String::from_utf8_lossy(&err_bytes), auth),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn locate_finds_system_git() {
        assert!(Git::locate().is_ok());
    }

    #[test]
    fn run_error_never_contains_token() {
        let git = Git::locate().unwrap();
        let dir = tempfile::tempdir().unwrap();
        git.run(dir.path(), &["init", "-q"], None).unwrap();
        let auth = GitAuth::bitbucket("s3cr3t-T0KEN");
        let header = auth_header_b64(&auth);
        for needle in [auth.token.as_str(), header.as_str()] {
            let err = git
                .run(dir.path(), &["checkout", needle], Some(&auth))
                .unwrap_err();
            let shown = format!("{err} {err:?}");
            assert!(shown.contains("***"), "{shown}");
            assert!(!shown.contains(&auth.token), "{shown}");
            assert!(!shown.contains(&header), "{shown}");
        }
        let dbg = format!("{auth:?}");
        assert!(!dbg.contains(&auth.token) && !dbg.contains("me@example.com"));
    }

    #[test]
    fn run_limited_bounds_stderr_and_returns_promptly_when_too_large() {
        let git = Git::locate().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let alias = "alias.big=!yes x | head -c 3000000 >&2; yes y | head -c 100000; exit 1";
        let args = ["-c", alias, "big"];
        let err = git
            .run_limited(dir.path(), &args, None, 1 << 20)
            .unwrap_err();
        match err {
            GitError::Failed { stderr, .. } => {
                assert!(stderr.len() <= 1 << 20, "{}", stderr.len())
            }
            e => panic!("{e:?}"),
        }
        let err = git.run_limited(dir.path(), &args, None, 10).unwrap_err();
        assert_eq!(err, GitError::TooLarge { limit: 10 });
    }

    #[test]
    fn check_version_rejects_old_git() {
        assert!(matches!(
            check_version("git version 2.30.0\n"),
            Err(GitError::TooOld(_))
        ));
        assert!(matches!(check_version("garbage"), Err(GitError::TooOld(_))));
        assert!(check_version("git version 2.31.0").is_ok());
        assert!(check_version("git version 3.0.1").is_ok());
    }

    #[test]
    fn scrub_redacts_all_encodings_on_every_line() {
        let auth = GitAuth::bitbucket("tok/+=en?x");
        let std = auth_header_b64(&auth);
        let unpadded = std.trim_end_matches('=').to_string();
        let urlsafe = std.replace('+', "-").replace('/', "_");
        let urlsafe_unpadded = urlsafe.trim_end_matches('=').to_string();
        let pct = percent_encode(&std);
        let tok_pct = percent_encode(&auth.token);
        let forms = [
            std.clone(),
            unpadded,
            urlsafe,
            urlsafe_unpadded,
            pct,
            auth.token.clone(),
            tok_pct,
        ];
        let text = forms
            .iter()
            .map(|f| format!("fatal: bad {f} here"))
            .collect::<Vec<_>>()
            .join("\r\n");
        let out = scrub(&text, Some(&auth));
        for f in &forms {
            assert!(!out.contains(f.as_str()), "{f} in {out}");
        }
        assert_eq!(out.matches("***").count(), forms.len());
        assert_eq!(out.lines().count(), forms.len());
    }

    fn b64_decode(s: &str) -> String {
        let bytes = base64::engine::general_purpose::STANDARD.decode(s).unwrap();
        String::from_utf8(bytes).unwrap()
    }

    fn envs(cmd: &Command) -> Vec<(String, Option<String>)> {
        cmd.get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect()
    }

    fn env_of(cmd: &Command, key: &str) -> Option<String> {
        envs(cmd)
            .into_iter()
            .find(|(k, _)| k == key)
            .and_then(|(_, v)| v)
    }

    #[test]
    fn git_header_uses_static_token_user_not_email() {
        let auth = GitAuth::bitbucket("s3cr3t");
        assert_eq!(
            b64_decode(&auth_header_b64(&auth)),
            "x-bitbucket-api-token-auth:s3cr3t"
        );
        let git = Git::locate().unwrap();
        let cmd = git.prepared(
            Path::new("."),
            &["fetch"],
            Some(&auth),
            Some("https://bitbucket.org/ws/r.git"),
        );
        let value = env_of(&cmd, "GIT_CONFIG_VALUE_0").unwrap();
        let b64 = value.strip_prefix("Authorization: Basic ").unwrap();
        assert_eq!(b64_decode(b64), "x-bitbucket-api-token-auth:s3cr3t");
        for (k, v) in envs(&cmd) {
            let v = v.unwrap_or_default();
            assert!(!v.contains("me@example.com"), "{k}={v}");
        }
    }

    #[test]
    fn network_command_scopes_header_and_blocks_prompts_and_redirects() {
        let auth = GitAuth::bitbucket("tok");
        let git = Git::locate().unwrap();
        let cmd = git.prepared(
            Path::new("."),
            &["fetch"],
            Some(&auth),
            Some("https://Bitbucket.org/ws/r.git"),
        );
        assert_eq!(env_of(&cmd, "GIT_CONFIG_COUNT").as_deref(), Some("4"));
        // a stalled transfer is ended by curl's low-speed check, not by the overall cap
        assert_eq!(
            env_of(&cmd, "GIT_CONFIG_KEY_2").as_deref(),
            Some("http.lowSpeedLimit")
        );
        assert_eq!(
            env_of(&cmd, "GIT_CONFIG_VALUE_2").as_deref(),
            Some(LOW_SPEED_LIMIT.to_string().as_str())
        );
        assert_eq!(
            env_of(&cmd, "GIT_CONFIG_KEY_3").as_deref(),
            Some("http.lowSpeedTime")
        );
        assert_eq!(
            env_of(&cmd, "GIT_CONFIG_VALUE_3").as_deref(),
            Some(LOW_SPEED_TIME.as_secs().to_string().as_str())
        );
        assert_eq!(
            env_of(&cmd, "GIT_CONFIG_KEY_0").as_deref(),
            Some("http.https://bitbucket.org/.extraheader")
        );
        assert_eq!(
            env_of(&cmd, "GIT_CONFIG_KEY_1").as_deref(),
            Some("http.followRedirects")
        );
        assert_eq!(env_of(&cmd, "GIT_CONFIG_VALUE_1").as_deref(), Some("false"));
        let removed: Vec<String> = envs(&cmd)
            .into_iter()
            .filter(|(_, v)| v.is_none())
            .map(|(k, _)| k.to_ascii_uppercase())
            .collect();
        for var in ["GIT_ASKPASS", "SSH_ASKPASS", "SSH_ASKPASS_REQUIRE"] {
            assert!(removed.iter().any(|k| k == var), "{var} not removed");
        }
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let pos = |s: &str| args.iter().position(|a| a == s);
        let fetch = pos("fetch").unwrap();
        assert!(pos("core.askPass=").is_some_and(|p| p < fetch), "{args:?}");
        assert!(
            pos("credential.helper=").is_some_and(|p| p < fetch),
            "{args:?}"
        );
        assert!(args.iter().all(|a| !a.contains("Basic")), "{args:?}");
        // the scope is always the account's: another host gets no header at all, and a
        // non-https url keeps the account's scope (never a guessed one)
        let other = git.prepared(
            Path::new("."),
            &["fetch"],
            Some(&auth),
            Some("https://git.example.com:8443/x/y.git"),
        );
        assert_eq!(
            env_of(&other, "GIT_CONFIG_KEY_0").as_deref(),
            Some("http.https://bitbucket.org/.extraheader")
        );
        assert_eq!(env_of(&other, "GIT_CONFIG_VALUE_0").as_deref(), Some(""));
        let file = git.prepared(Path::new("."), &["fetch"], Some(&auth), Some("file:///x"));
        assert_eq!(
            env_of(&file, "GIT_CONFIG_KEY_0").as_deref(),
            Some("http.https://bitbucket.org/.extraheader")
        );
    }

    #[test]
    fn no_scope_means_no_header() {
        // a command without a remote (lazy fetch of a mirror) uses the account's scope,
        // never bitbucket.org by default
        let gl = GitAuth::new(GITLAB_GIT_USER, "tok", "Git.Corp.Example:8443");
        assert_eq!(gl.scope, "https://git.corp.example:8443/");
        let git = Git::locate().unwrap();
        let cmd = git.prepared(Path::new("."), &["log"], Some(&gl), None);
        assert_eq!(
            env_of(&cmd, "GIT_CONFIG_KEY_0").as_deref(),
            Some("http.https://git.corp.example:8443/.extraheader")
        );
        assert!(envs(&cmd)
            .iter()
            .all(|(_, v)| !v.as_deref().unwrap_or("").contains("bitbucket.org")));
        // and without credentials there is no header configuration at all
        let bare = git.prepared(Path::new("."), &["log"], None, None);
        assert_eq!(env_of(&bare, "GIT_CONFIG_COUNT"), None);
    }

    #[test]
    fn header_not_sent_to_other_host_or_port() {
        let a = GitAuth::new(GITHUB_GIT_USER, "tok", "ghe.corp.example");
        let ok = "https://GHE.corp.example/o/r.git";
        assert!(auth_for_remote(ok, &a).is_some());
        for url in [
            "https://ghe.corp.example:8443/o/r.git",
            "https://ghe.corp.example.evil/o/r.git",
            "https://user@ghe.corp.example/o/r.git",
            "http://ghe.corp.example/o/r.git",
            "https://github.com/o/r.git",
            "file:///ghe.corp.example/o/r.git",
        ] {
            assert!(auth_for_remote(url, &a).is_none(), "{url}");
        }
        // git itself only matches the scope: run with the account's url, ask for others
        let git = Git::locate().unwrap();
        let dir = tempfile::tempdir().unwrap();
        git.run(dir.path(), &["init", "-q"], None).unwrap();
        let opts = RunOpts {
            auth: Some(&a),
            url: Some(ok),
            ..RunOpts::default()
        };
        let get = |url: &str| {
            let args = ["config", "--get-urlmatch", "http.extraheader", url];
            git.run_with(dir.path(), &args, opts)
        };
        assert!(get("https://ghe.corp.example/o/r.git").is_ok());
        for url in [
            "https://ghe.corp.example:8443/o/r.git",
            "https://bitbucket.org/o/r.git",
        ] {
            assert!(get(url).is_err(), "{url}");
        }
    }

    #[test]
    fn cleartext_auth_only_for_a_private_host() {
        let a = GitAuth::cleartext(GITLAB_GIT_USER, "tok", "192.168.1.20:8080/GitLab").unwrap();
        assert_eq!(a.scope, "http://192.168.1.20:8080/");
        assert_eq!(a.scheme(), "http");
        assert_eq!(
            GitAuth::new(GITLAB_GIT_USER, "tok", "gitlab.com").scheme(),
            "https"
        );
        assert!(GitAuth::cleartext(GITLAB_GIT_USER, "tok", "git.localhost").is_some());
        assert!(GitAuth::cleartext(GITLAB_GIT_USER, "tok", "[::1]:3000").is_some());
        for bad in [
            "gitlab.com",
            "8.8.8.8",
            "192.168.1.1.evil.com",
            "192.168.1.1@evil.com",
            "3232235777",
            "0300.0250.1.1",
            "[::ffff:192.168.1.1]",
            "192.168.1.1:x",
            "",
        ] {
            assert!(
                GitAuth::cleartext(GITLAB_GIT_USER, "tok", bad).is_none(),
                "{bad}"
            );
        }
    }

    #[test]
    fn cleartext_header_only_for_the_exact_http_origin() {
        let a = GitAuth::cleartext(GITLAB_GIT_USER, "tok", "192.168.1.20:8080").unwrap();
        let ok = "http://192.168.1.20:8080/root/nit-live-test.git";
        assert!(auth_for_remote(ok, &a).is_some());
        for url in [
            // https on the same host and port is another origin
            "https://192.168.1.20:8080/root/x.git",
            "https://192.168.1.20/root/x.git",
            "http://192.168.1.20/root/x.git",
            "http://192.168.1.20:8081/root/x.git",
            "http://192.168.1.88:8080/root/x.git",
            "http://192.168.1.20:8080.evil.com/root/x.git",
            "http://user@192.168.1.20:8080/root/x.git",
            "http://192.168.1.20:8080@evil.com/root/x.git",
            "file:///192.168.1.20:8080/root/x.git",
        ] {
            assert!(auth_for_remote(url, &a).is_none(), "{url}");
        }
        // an https account never matches the http form of its own host
        let s = GitAuth::new(GITLAB_GIT_USER, "tok", "192.168.1.20:8080");
        assert!(auth_for_remote(ok, &s).is_none());
        // a public http remote is never a scope, whatever the account
        assert_eq!(url_scope("http://gitlab.com/x.git"), None);
        assert_eq!(url_scope("http://8.8.8.8/x.git"), None);

        let git = Git::locate().unwrap();
        let cmd = git.prepared(Path::new("."), &["fetch"], Some(&a), Some(ok));
        assert_eq!(
            env_of(&cmd, "GIT_CONFIG_KEY_0").as_deref(),
            Some("http.http://192.168.1.20:8080/.extraheader")
        );
        assert!(env_of(&cmd, "GIT_CONFIG_VALUE_0")
            .is_some_and(|v| v.starts_with("Authorization: Basic ")));
        for other in [
            "https://192.168.1.20:8080/root/x.git",
            "http://192.168.1.20:8081/root/x.git",
            "http://10.0.0.1:8080/root/x.git",
            "http://gitlab.com/root/x.git",
        ] {
            let cmd = git.prepared(Path::new("."), &["fetch"], Some(&a), Some(other));
            assert_eq!(
                env_of(&cmd, "GIT_CONFIG_VALUE_0").as_deref(),
                Some(""),
                "{other}"
            );
            assert_eq!(
                env_of(&cmd, "GIT_CONFIG_KEY_0").as_deref(),
                Some("http.http://192.168.1.20:8080/.extraheader")
            );
        }
        // git itself matches the header only on the exact origin (scheme, host, port)
        let dir = tempfile::tempdir().unwrap();
        git.run(dir.path(), &["init", "-q"], None).unwrap();
        let opts = RunOpts {
            auth: Some(&a),
            url: Some(ok),
            ..RunOpts::default()
        };
        let get = |url: &str| {
            let args = ["config", "--get-urlmatch", "http.extraheader", url];
            git.run_with(dir.path(), &args, opts)
        };
        assert!(get("http://192.168.1.20:8080/other/repo.git").is_ok());
        for url in [
            "https://192.168.1.20:8080/root/x.git",
            "http://192.168.1.20/root/x.git",
            "http://192.168.1.20:8081/root/x.git",
            "http://192.168.1.88:8080/root/x.git",
        ] {
            assert!(get(url).is_err(), "{url}");
        }
        let dbg = format!("{a:?}");
        assert!(
            !dbg.contains("tok\"") && dbg.contains("http://192.168.1.20:8080/"),
            "{dbg}"
        );
    }

    #[test]
    fn user_name_per_forge_in_header() {
        for (user, host) in [
            (BITBUCKET_GIT_USER, "bitbucket.org"),
            (GITHUB_GIT_USER, "github.com"),
            (GITLAB_GIT_USER, "gitlab.com"),
        ] {
            let a = GitAuth::new(user, "s3cr3t", host);
            assert_eq!(b64_decode(&auth_header_b64(&a)), format!("{user}:s3cr3t"));
            let dbg = format!("{a:?}");
            assert!(!dbg.contains("s3cr3t") && dbg.contains(host), "{dbg}");
        }
        assert_eq!(GITHUB_GIT_USER, "x-access-token");
        assert_eq!(GITLAB_GIT_USER, "oauth2");
    }

    #[test]
    fn header_is_only_sent_to_its_own_host() {
        let git = Git::locate().unwrap();
        let dir = tempfile::tempdir().unwrap();
        git.run(dir.path(), &["init", "-q"], None).unwrap();
        let auth = GitAuth::bitbucket("tok");
        let opts = RunOpts {
            auth: Some(&auth),
            url: Some("https://bitbucket.org/ws/r.git"),
            ..RunOpts::default()
        };
        let get = |url: &str| {
            let args = ["config", "--get-urlmatch", "http.extraheader", url];
            git.run_with(dir.path(), &args, opts)
        };
        let ours = get("https://bitbucket.org/ws/r.git").unwrap();
        assert!(String::from_utf8_lossy(&ours).starts_with("Authorization: Basic "));
        for url in [
            "https://evil.example/ws/r.git",
            "https://bitbucket.org.evil.example/ws/r.git",
            "http://bitbucket.org/ws/r.git",
        ] {
            let err = get(url).unwrap_err();
            assert!(
                matches!(err, GitError::Failed { code: Some(1), .. }),
                "{url}: {err:?}"
            );
        }
        let redirects = git
            .run_with(
                dir.path(),
                &["config", "--get", "http.followRedirects"],
                opts,
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&redirects).trim(), "false");
        let low = git
            .run_with(dir.path(), &["config", "--get", "http.lowSpeedTime"], opts)
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&low).trim(), "300");
    }

    #[test]
    fn scrub_removes_the_new_header_from_stderr() {
        let git = Git::locate().unwrap();
        let dir = tempfile::tempdir().unwrap();
        git.run(dir.path(), &["init", "-q"], None).unwrap();
        let auth = GitAuth::bitbucket("s3cr3t-T0KEN");
        let header = auth_header_b64(&auth);
        // the alias echoes the configured header value to stderr
        let alias = "alias.leak=!git config --get-urlmatch http.extraheader https://bitbucket.org/x >&2; exit 3";
        let opts = RunOpts {
            auth: Some(&auth),
            url: Some("https://bitbucket.org/ws/r.git"),
            ..RunOpts::default()
        };
        let err = git
            .run_with(dir.path(), &["-c", alias, "leak"], opts)
            .unwrap_err();
        let shown = format!("{err} {err:?}");
        assert!(shown.contains("Authorization: Basic ***"), "{shown}");
        assert!(!shown.contains(&header), "{shown}");
    }

    /// A child that would write `marker` after about two seconds unless it is killed first.
    fn slow_child(dir: &Path) -> Command {
        #[cfg(windows)]
        {
            let script = dir.join("slow.cmd");
            std::fs::write(
                &script,
                "@ping -n 3 127.0.0.1 >nul\r\n@echo done>\"%~dp0marker\"\r\n",
            )
            .unwrap();
            let mut cmd = Command::new("cmd");
            cmd.arg("/C").arg(&script);
            cmd
        }
        #[cfg(not(windows))]
        {
            let mut cmd = Command::new("sh");
            cmd.arg("-c").arg(format!(
                "sleep 2; echo done > '{}'",
                dir.join("marker").display()
            ));
            cmd
        }
    }

    #[test]
    fn timeout_kills_the_child_and_returns_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let limit = Duration::from_millis(300);
        let start = Instant::now();
        let err = execute(slow_child(dir.path()), None, None, Some(limit), None).unwrap_err();
        assert_eq!(err, GitError::Timeout { after: limit });
        assert!(
            start.elapsed() < Duration::from_millis(1500),
            "{:?}",
            start.elapsed()
        );
        assert!(err.to_string().contains("stopped"), "{err}");
        // had the child survived it would have written the marker by now
        std::thread::sleep(Duration::from_secs(4));
        assert!(!dir.path().join("marker").exists(), "child was not killed");
        // without a deadline the same child runs to completion
        execute(slow_child(dir.path()), None, None, None, None).unwrap();
        assert!(dir.path().join("marker").exists());
    }

    /// A helper whose child (our grandchild) would write `marker` after about two seconds;
    /// the helper itself keeps running longer than that.
    #[cfg(windows)]
    fn child_with_grandchild(dir: &Path) -> Command {
        let script = dir.join("parent.cmd");
        std::fs::write(
            &script,
            "@start \"\" /b cmd /c \"ping -n 3 127.0.0.1 >nul & echo done>\"%~dp0marker\"\"\r\n\
             @ping -n 8 127.0.0.1 >nul\r\n",
        )
        .unwrap();
        let mut cmd = Command::new("cmd");
        cmd.arg("/C").arg(&script);
        cmd
    }

    /// The same on macOS / Unix: a background subshell (the grandchild) that would write
    /// `marker` after about two seconds, while the shell itself sleeps longer.
    #[cfg(unix)]
    fn child_with_grandchild(dir: &Path) -> Command {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(format!(
            "(sleep 2; echo done > '{}') & sleep 8",
            dir.join("marker").display()
        ));
        cmd
    }

    /// The group is gone with the command: a grandchild that outlives a successful git run
    /// is ended too (Windows: `KILL_ON_JOB_CLOSE`).
    #[cfg(unix)]
    #[test]
    fn a_finished_command_leaves_no_grandchild_behind() {
        let dir = tempfile::tempdir().unwrap();
        let mut cmd = Command::new("sh");
        // the grandchild closes the pipes so the command itself returns at once
        cmd.arg("-c").arg(format!(
            "(sleep 2; echo done > '{}') >/dev/null 2>&1 </dev/null & exit 0",
            dir.path().join("marker").display()
        ));
        execute(cmd, None, None, None, None).unwrap();
        std::thread::sleep(Duration::from_secs(4));
        assert!(!dir.path().join("marker").exists(), "grandchild survived");
    }

    #[test]
    fn cancel_kills_the_child_promptly_and_returns_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let start = Instant::now();
        let stop = || start.elapsed() > Duration::from_millis(200);
        let err = execute(slow_child(dir.path()), None, None, None, Some(&stop)).unwrap_err();
        assert_eq!(err, GitError::Cancelled);
        assert!(
            start.elapsed() < Duration::from_millis(1500),
            "{:?}",
            start.elapsed()
        );
        std::thread::sleep(Duration::from_secs(4));
        assert!(!dir.path().join("marker").exists(), "child was not killed");
        // a token that never fires changes nothing
        let never = || false;
        execute(slow_child(dir.path()), None, None, None, Some(&never)).unwrap();
        assert!(dir.path().join("marker").exists());
    }

    #[test]
    fn an_already_cancelled_command_is_never_started() {
        let git = Git::locate().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let cancelled = || true;
        let opts = RunOpts {
            cancel: Some(&cancelled),
            ..RunOpts::default()
        };
        let before = git.spawn_count();
        // `init` would create .git at once if it ran
        let err = git.run_with(dir.path(), &["init", "-q"], opts).unwrap_err();
        assert_eq!(err, GitError::Cancelled);
        assert!(!dir.path().join(".git").exists(), "git ran anyway");
        assert_eq!(git.spawn_count(), before, "nothing was started");
        // the same through the low-level runner (the self-test's `ls-remote` path)
        let err = execute(slow_child(dir.path()), None, None, None, Some(&cancelled));
        assert_eq!(err.unwrap_err(), GitError::Cancelled);
        std::thread::sleep(Duration::from_secs(3));
        assert!(!dir.path().join("marker").exists(), "the child ran");
    }

    #[cfg(any(windows, unix))]
    #[test]
    fn cancel_kills_grandchildren_too() {
        let dir = tempfile::tempdir().unwrap();
        let start = Instant::now();
        let stop = || start.elapsed() > Duration::from_millis(300);
        let err = execute(
            child_with_grandchild(dir.path()),
            None,
            None,
            None,
            Some(&stop),
        );
        assert_eq!(err.unwrap_err(), GitError::Cancelled);
        std::thread::sleep(Duration::from_secs(4));
        assert!(!dir.path().join("marker").exists(), "grandchild survived");
    }

    #[test]
    fn exec_path_lookup_ignores_an_inherited_git_exec_path() {
        let cmd = exec_path_command(Path::new("git"));
        let removed = envs(&cmd)
            .into_iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("GIT_EXEC_PATH") && v.is_none());
        assert!(removed, "GIT_EXEC_PATH would redirect the lookup");
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(args, ["--exec-path"]);
    }

    #[cfg(any(windows, unix))]
    #[test]
    fn timeout_kills_grandchildren_too() {
        let dir = tempfile::tempdir().unwrap();
        let limit = Duration::from_millis(500);
        let err = execute(
            child_with_grandchild(dir.path()),
            None,
            None,
            Some(limit),
            None,
        )
        .unwrap_err();
        assert_eq!(err, GitError::Timeout { after: limit });
        // a surviving grandchild would have written the marker by now
        std::thread::sleep(Duration::from_secs(4));
        assert!(
            !dir.path().join("marker").exists(),
            "grandchild survived the timeout"
        );
    }

    #[test]
    fn launcher_layout_resolves_to_the_real_binary() {
        let root = tempfile::tempdir().unwrap();
        let cmd_dir = root.path().join("cmd");
        let real_dir = root.path().join("mingw64").join("bin");
        std::fs::create_dir_all(&cmd_dir).unwrap();
        std::fs::create_dir_all(&real_dir).unwrap();
        let launcher = cmd_dir.join(GIT_EXE);
        let real = real_dir.join(GIT_EXE);
        std::fs::write(&launcher, "shim").unwrap();
        std::fs::write(&real, "real").unwrap();
        assert_eq!(launcher_target(&launcher), Some(real.clone()));
        let never = |_: &Path| -> Option<PathBuf> { panic!("no exec-path spawn needed") };
        let ok = |_: &Path| Ok(());
        assert_eq!(resolve_exe(launcher.clone(), &ok, &never), Ok(real.clone()));
        // the real binary does not run: keep the launcher
        let only_launcher = |p: &Path| {
            if p == launcher.as_path() {
                Ok(())
            } else {
                Err(GitError::NotInstalled)
            }
        };
        assert_eq!(
            resolve_exe(launcher.clone(), &only_launcher, &never),
            Ok(launcher.clone())
        );
        // the real binary is missing: keep the launcher (exec-path finds nothing either)
        std::fs::remove_file(&real).unwrap();
        assert_eq!(launcher_target(&launcher), None);
        let nothing = |_: &Path| -> Option<PathBuf> { None };
        assert_eq!(
            resolve_exe(launcher.clone(), &ok, &nothing),
            Ok(launcher.clone())
        );
        // neither runs: the launcher's error
        let none = |_: &Path| Err(GitError::NotInstalled);
        assert_eq!(
            resolve_exe(launcher.clone(), &none, &nothing),
            Err(GitError::NotInstalled)
        );
        // not a launcher layout at all
        assert_eq!(launcher_target(&real_dir.join(GIT_EXE)), None);
    }

    #[cfg(windows)]
    #[test]
    fn other_launchers_resolve_through_exec_path() {
        let root = tempfile::tempdir().unwrap();
        let shim = root.path().join("shims").join(GIT_EXE);
        let prefix = root.path().join("app").join("mingw64");
        let exec = prefix.join("libexec").join("git-core");
        std::fs::create_dir_all(shim.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&exec).unwrap();
        std::fs::create_dir_all(prefix.join("bin")).unwrap();
        std::fs::write(&shim, "shim").unwrap();
        let real = prefix.join("bin").join(GIT_EXE);
        std::fs::write(&real, "real").unwrap();
        let ok = |_: &Path| Ok(());
        let e = exec.clone();
        let ask = move |_: &Path| Some(e.clone());
        assert_eq!(resolve_exe(shim.clone(), &ok, &ask), Ok(real.clone()));
        // the real binary itself is never asked again
        let never = |_: &Path| -> Option<PathBuf> { panic!("already the real binary") };
        assert_eq!(resolve_exe(real.clone(), &ok, &never), Ok(real));
    }

    #[test]
    fn locate_skips_the_windows_launcher_when_installed() {
        let git = Git::locate().unwrap();
        assert!(git.exe().is_absolute() || git.exe() == Path::new("git"));
        if let Some(found) = find_on_path() {
            if launcher_target(&found).is_some() {
                assert_ne!(git.exe(), found.as_path(), "still runs the launcher");
                assert!(launcher_target(&found).as_deref() == Some(git.exe()));
            }
        }
        // shared: one lookup, clones share the counter
        let a = Git::shared().unwrap();
        let b = Git::shared().unwrap();
        assert!(Arc::ptr_eq(&a.spawns, &b.spawns));
        let before = a.spawn_count();
        a.run(Path::new("."), &["--version"], None).unwrap();
        assert!(b.spawn_count() > before);
    }

    #[test]
    fn timeout_display_is_readable() {
        let e = GitError::Timeout {
            after: NETWORK_TIMEOUT,
        };
        assert!(e.to_string().contains("120 minutes"), "{e}");
    }

    #[test]
    fn progress_lines_parse_to_phase_and_percent() {
        let p = |phase: &str, percent| {
            Some(GitProgress {
                phase: phase.into(),
                percent,
            })
        };
        assert_eq!(
            parse_progress("remote: Counting objects:  12% (1/8)"),
            p("Counting objects", 12)
        );
        assert_eq!(
            parse_progress("Receiving objects:  45% (450/1000), 1.20 MiB | 1.00 MiB/s"),
            p("Receiving objects", 45)
        );
        assert_eq!(
            parse_progress("Resolving deltas: 100% (2/2), done."),
            p("Resolving deltas", 100)
        );
        for other in [
            "remote: Enumerating objects: 123, done.",
            "Cloning into bare repository 'ws/r.git.tmp'...",
            "fatal: https://bitbucket.org/x: 50% nonsense",
            "Receiving objects: 150% (1/1)",
            "",
        ] {
            assert_eq!(parse_progress(other), None, "{other}");
        }
    }

    #[test]
    fn parse_version_cases() {
        assert_eq!(parse_version("git version 2.53.0.windows.1"), Some((2, 53)));
        assert_eq!(parse_version("git version 2.31.0"), Some((2, 31)));
        assert_eq!(parse_version("nonsense"), None);
    }
}
