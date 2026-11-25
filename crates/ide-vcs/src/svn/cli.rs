//! Running the `svn` command-line client. Unlike Git there is no in-process
//! library here — every operation shells out, so the helpers mirror
//! [`crate::cli`]'s contract (never fail on a non-zero exit for `run_raw`,
//! cancellable variants that kill the child, failures classified into
//! [`ide_core::error::ErrorKind`]) with Subversion's specifics:
//! `--non-interactive` on network operations so a missing cached credential
//! surfaces as an error instead of a prompt no one can answer.

use ide_core::{CancellationToken, IdeError, IdeResult};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::sync::RwLock;
use std::time::Duration;

/// Subversion executable of the user's global settings (`None` = auto-detect).
static DEFAULT: RwLock<Option<PathBuf>> = RwLock::new(None);

/// Sets (or clears, with `None` / an empty path) the svn executable used
/// everywhere, from the global settings only.
pub fn set_default_svn_executable(path: Option<&Path>) {
    let path = path.filter(|p| !p.as_os_str().is_empty()).map(Path::to_path_buf);
    *DEFAULT.write().unwrap_or_else(|e| e.into_inner()) = path;
}

/// Locates the auto-detected svn executable once per process.
///
/// GUI apps on macOS start with a minimal `PATH`, so well-known install
/// locations (TortoiseSVN and SlikSVN on Windows) are probed after the `PATH`
/// lookup.
fn detected_svn() -> Option<&'static Path> {
    static SVN: OnceLock<Option<PathBuf>> = OnceLock::new();
    SVN.get_or_init(|| {
        if let Some(explicit) = std::env::var_os("OPENIDEA_SVN").map(PathBuf::from).filter(|p| p.is_file()) {
            return Some(explicit);
        }
        if let Ok(found) = which::which("svn") {
            return Some(found);
        }
        let candidates: &[&str] = if cfg!(windows) {
            &[
                r"C:\Program Files\TortoiseSVN\bin\svn.exe",
                r"C:\Program Files (x86)\TortoiseSVN\bin\svn.exe",
                r"C:\Program Files\SlikSvn\bin\svn.exe",
                r"C:\Program Files (x86)\SlikSvn\bin\svn.exe",
                r"C:\Program Files\Subversion\bin\svn.exe",
            ]
        } else {
            &["/opt/homebrew/bin/svn", "/usr/local/bin/svn", "/usr/bin/svn", "/snap/bin/svn"]
        };
        candidates.iter().map(PathBuf::from).find(|p| p.is_file())
    })
    .as_deref()
}

/// The svn executable in use: the default override, else the detected one.
pub fn svn_executable() -> Option<PathBuf> {
    if let Some(default) = DEFAULT.read().unwrap_or_else(|e| e.into_inner()).clone() {
        return Some(default);
    }
    detected_svn().map(Path::to_path_buf)
}

/// The svnadmin executable, for creating throwaway repositories in tests:
/// `OPENIDEA_SVNADMIN`, else `svnadmin` next to the svn client (installers
/// always put both in one `bin` directory), else the `PATH` and well-known
/// install locations.
pub fn svnadmin_executable() -> Option<PathBuf> {
    static ADMIN: OnceLock<Option<PathBuf>> = OnceLock::new();
    ADMIN
        .get_or_init(|| {
            if let Some(explicit) = std::env::var_os("OPENIDEA_SVNADMIN").map(PathBuf::from).filter(|p| p.is_file()) {
                return Some(explicit);
            }
            if let Some(svn) = svn_executable() {
                let sibling = svn.parent().map(|dir| dir.join(svnadmin_name()));
                if sibling.as_ref().is_some_and(|p| p.is_file()) {
                    return sibling;
                }
            }
            if let Ok(found) = which::which("svnadmin") {
                return Some(found);
            }
            let candidates: &[&str] = if cfg!(windows) {
                &[
                    r"C:\Program Files\TortoiseSVN\bin\svnadmin.exe",
                    r"C:\Program Files\SlikSvn\bin\svnadmin.exe",
                    r"C:\Program Files\Subversion\bin\svnadmin.exe",
                ]
            } else {
                &["/opt/homebrew/bin/svnadmin", "/usr/local/bin/svnadmin", "/usr/bin/svnadmin"]
            };
            candidates.iter().map(PathBuf::from).find(|p| p.is_file())
        })
        .clone()
}

#[cfg(windows)]
fn svnadmin_name() -> &'static str {
    "svnadmin.exe"
}

#[cfg(not(windows))]
fn svnadmin_name() -> &'static str {
    "svnadmin"
}

/// The executable svn children run.
fn executable() -> IdeResult<PathBuf> {
    svn_executable().ok_or_else(|| {
        IdeError::unavailable(
            "svn executable not found; install the Subversion command-line tools (TortoiseSVN or SlikSVN) to work with SVN working copies",
        )
    })
}

fn command(root: &Path) -> IdeResult<Command> {
    let mut cmd = Command::new(executable()?);
    cmd.current_dir(root).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    // `LC_ALL=C` keeps svn's messages English so error classification can
    // match its text on localized systems; the `svn: E<code>` prefixes are
    // locale-independent either way.
    cmd.env("LC_ALL", "C");
    ide_core::process::prepare(&mut cmd);
    Ok(cmd)
}

/// The collected svn output, shaped like [`crate::cli::Output`]: `code` is
/// `None` when the child was killed (cancel) or could not be waited on.
pub(crate) struct Output {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub code: Option<i32>,
}

impl Output {
    pub fn stdout_str(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
}

/// Runs svn to completion, optionally feeding `stdin`. Never fails on a
/// non-zero exit — callers inspect [`Output`] (svn reports many recoverable
/// situations as `W…` warnings with useful exit codes).
pub(crate) fn run_raw(root: &Path, args: &[&str]) -> IdeResult<Output> {
    run_raw_stdin(root, args, None)
}

pub(crate) fn run_raw_stdin(root: &Path, args: &[&str], stdin: Option<&[u8]>) -> IdeResult<Output> {
    let mut cmd = command(root)?;
    if stdin.is_some() {
        cmd.stdin(Stdio::piped());
    }
    cmd.args(args.iter());
    tracing::debug!(?args, "svn");
    let child = cmd.spawn().map_err(|e| spawn_error(root, e))?;
    finish(child, stdin)
}

/// [`run_raw`] that fails on a non-zero exit.
pub(crate) fn run(root: &Path, args: &[&str]) -> IdeResult<Output> {
    let out = run_raw(root, args)?;
    if out.code == Some(0) { Ok(out) } else { Err(failure(args, &out)) }
}

/// Runs svn, aborting once `cancel` fires by killing the child (the reader
/// threads then see EOF and the poll loop returns). svn commands against the
/// repository (log, cat of remote revisions, commit, update) can block for a
/// long time, so everything the UI can cancel goes through here.
pub(crate) fn run_cancellable(root: &Path, args: &[&str], cancel: &CancellationToken) -> IdeResult<Output> {
    let out = run_cancellable_raw(root, args, cancel)?;
    if out.code == Some(0) { Ok(out) } else { Err(failure(args, &out)) }
}

/// [`run_cancellable`] preserving exit codes for commands with documented
/// non-error states (`svn diff` of an unversioned path, `svn revert` of a
/// skipped one).
pub(crate) fn run_cancellable_raw(root: &Path, args: &[&str], cancel: &CancellationToken) -> IdeResult<Output> {
    run_cancellable_stdin(root, args, None, cancel)
}

pub(crate) fn run_cancellable_stdin(
    root: &Path,
    args: &[&str],
    stdin: Option<&[u8]>,
    cancel: &CancellationToken,
) -> IdeResult<Output> {
    let mut cmd = command(root)?;
    if stdin.is_some() {
        cmd.stdin(Stdio::piped());
    }
    cmd.args(args.iter());
    tracing::debug!(?args, "svn");
    let mut child = cmd.spawn().map_err(|e| spawn_error(root, e))?;
    // Write the message before polling so a blocked pipe never delays the
    // cancel check; `commit`'s message is small and svn reads it up front.
    if let (Some(data), Some(mut pipe)) = (stdin, child.stdin.take()) {
        use std::io::Write;
        if cancel.is_cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(IdeError::cancelled());
        }
        let _ = pipe.write_all(data);
    }
    finish_cancellable(child, cancel)
}

/// [`run_cancellable`] that streams every stdout/stderr line to `on_output`
/// as it arrives, for the long-running commands the VCS console shows
/// (`checkout`). Fails on a non-zero exit, with the last `svn:` lines as the
/// message — mirroring [`crate::cli::run_streaming`]'s contract.
pub(crate) fn run_streaming(
    root: &Path,
    args: &[&str],
    cancel: &CancellationToken,
    on_output: &dyn Fn(String),
) -> IdeResult<()> {
    if cancel.is_cancelled() {
        return Err(IdeError::cancelled());
    }
    let mut cmd = command(root)?;
    cmd.args(args.iter());
    tracing::debug!(?args, "svn (streaming)");
    let mut child = cmd.spawn().map_err(|e| spawn_error(root, e))?;
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let mut readers = Vec::new();
    if let Some(out) = child.stdout.take() {
        let pipe_tx = tx.clone();
        readers.push(std::thread::spawn(move || read_lines(out, pipe_tx)));
    }
    if let Some(err) = child.stderr.take() {
        let pipe_tx = tx.clone();
        readers.push(std::thread::spawn(move || read_lines(err, pipe_tx)));
    }
    drop(tx);

    // The tail of the `svn:` lines: warnings and errors are the failure message.
    let mut stderr_tail: Vec<String> = Vec::new();
    loop {
        if cancel.is_cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            for reader in readers {
                let _ = reader.join();
            }
            return Err(IdeError::cancelled());
        }
        match rx.recv_timeout(Duration::from_millis(25)) {
            Ok(line) => {
                if line.starts_with("svn: ") {
                    if stderr_tail.len() > 40 {
                        stderr_tail.remove(0);
                    }
                    stderr_tail.push(line.clone());
                }
                on_output(line);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    for reader in readers {
        let _ = reader.join();
    }
    let status = child.wait()?;
    if status.success() {
        return Ok(());
    }
    let out = Output { stdout: Vec::new(), stderr: stderr_tail.join("\n").into_bytes(), code: status.code() };
    Err(failure(args, &out))
}

/// Forwards every line of `pipe` as it arrives; the pipe closing ends the
/// stream.
fn read_lines<R: std::io::Read>(pipe: R, tx: std::sync::mpsc::Sender<String>) {
    let mut reader = std::io::BufReader::new(pipe);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match std::io::BufRead::read_until(&mut reader, b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let line = String::from_utf8_lossy(&buf).trim_end_matches(['\r', '\n']).to_owned();
                if !line.is_empty() {
                    let _ = tx.send(line);
                }
            }
        }
    }
}

/// Drains stdout/stderr on threads (a full pipe would otherwise deadlock a
/// `try_wait` poll loop on large `svn log`/`svn diff` output) and collects the
/// result after the child exits.
fn finish(mut child: std::process::Child, stdin: Option<&[u8]>) -> IdeResult<Output> {
    if let (Some(data), Some(mut pipe)) = (stdin, child.stdin.take()) {
        use std::io::Write;
        let _ = pipe.write_all(data);
    }
    finish_cancellable(child, &CancellationToken::new())
}

fn finish_cancellable(mut child: std::process::Child, cancel: &CancellationToken) -> IdeResult<Output> {
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let stdout = std::thread::spawn(move || read_all(stdout_pipe));
    let stderr = std::thread::spawn(move || read_all(stderr_pipe));
    loop {
        if cancel.is_cancelled() {
            let _ = child.kill();
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                let _ = child.wait();
                let stdout = stdout.join().unwrap_or_default();
                let stderr = stderr.join().unwrap_or_default();
                if cancel.is_cancelled() {
                    return Err(IdeError::cancelled());
                }
                return Ok(Output { stdout, stderr, code: status.code() });
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(e) => return Err(e.into()),
        }
    }
}

fn read_all<R: std::io::Read>(pipe: Option<R>) -> Vec<u8> {
    let mut buf = Vec::new();
    if let Some(mut pipe) = pipe {
        let _ = pipe.read_to_end(&mut buf);
    }
    buf
}

fn spawn_error(root: &Path, e: std::io::Error) -> IdeError {
    if e.kind() == std::io::ErrorKind::NotFound {
        IdeError::unavailable(format!("could not run svn from {}: {e}", root.display()))
    } else {
        e.into()
    }
}

/// svn's machine-readable error markers: `svn: E200005: ...` on stderr, and
/// recoverable situations print `svn: warning: W155007: ...`, often exiting 0.
pub(crate) fn stderr_contains(out: &Output, needle: &str) -> bool {
    String::from_utf8_lossy(&out.stderr).contains(needle)
}

pub(crate) fn failure(args: &[&str], out: &Output) -> IdeError {
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let sub = args.first().copied().unwrap_or("svn");
    let mut msg = if stderr.is_empty() { stdout.clone() } else { stderr };
    if msg.is_empty() {
        msg = match out.code {
            Some(c) => format!("svn {sub} failed with exit code {c}"),
            None => format!("svn {sub} was terminated"),
        };
    }
    IdeError::new(classify(&msg), msg)
}

fn classify(msg: &str) -> ide_core::error::ErrorKind {
    use ide_core::error::ErrorKind;
    // E160013 = path not found in revision, E200005 = item not under version
    // control, E155007 = target not a working copy, E200009 = illegal target
    // (also "has no pristine version"), E155010 = node not found.
    if msg.contains("E160013")
        || msg.contains("E200005")
        || msg.contains("E155010")
        || msg.contains("not found in revision")
        || msg.contains("was not found")
    {
        ErrorKind::NotFound
    } else if msg.contains("E155015") || msg.contains("already exists") {
        ErrorKind::AlreadyExists
    } else if msg.contains("E155007") || msg.contains("E200009") {
        ErrorKind::InvalidInput
    } else if msg.contains("E72000") || msg.contains("Access is denied") || msg.contains("Permission denied") {
        ErrorKind::PermissionDenied
    } else {
        ErrorKind::Internal
    }
}
