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
mod tests;
