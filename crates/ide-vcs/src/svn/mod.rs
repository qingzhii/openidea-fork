//! Subversion support: a working copy driven through the `svn` CLI, serving the
//! same [`Backend`] surface as Git's [`Repo`] (see the trait's docs and #743).
//!
//! Subversion has no index, so [`FileStatus::staged`] is always `None` and
//! every working change — scheduled operations included — reports as
//! `unstaged`, as the trait prescribes for index-less backends.
//! [`Backend::stage`] still exists ("open for add/edit" in a checkout model):
//! it schedules `svn add` for version-control-less files and `svn delete` for
//! files missing from disk, the two states a commit cannot pick up on its own.
//! Revision strings follow svn's own syntax — `BASE`, `HEAD`, `r123` — with
//! the working copy's `BASE` playing the role HEAD plays for Git: the side the
//! Changes view, gutters and diffs compare against. Git-only concepts
//! (branches, remotes, stash, rebase, annotate) have no svn answer here;
//! [`Backend::as_git`] returns `None` and callers hide their UI.

pub(crate) mod cli;
pub(crate) mod parse;

#[cfg(test)]
mod tests;

pub use cli::{set_default_svn_executable, svn_executable, svnadmin_executable};

use crate::{
    BlameLine, BlockHistory, ChangeKind, ChangedFile, Commit, CommitOptions, FileStatus, LineHunk, LogQuery,
    PendingChange, RevisionChange, TextEncoding, backend::Backend,
};
use ide_core::{CancellationToken, IdeError, IdeResult, IpcPath};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// An svn working copy. Cheap to keep around; every read shells out to svn,
/// which always reflects the current on-disk state.
#[derive(Debug)]
pub struct SvnWorkingCopy {
    root: PathBuf,
    /// The working copy's `svn info --xml` (revision, URL, repository root),
    /// cached; write operations mark it stale and the next read refreshes it.
    /// URLs stay percent-encoded: that is the spelling `svn log -v` and
    /// `svn diff --summarize` print their paths in, so repository paths map
    /// without re-encoding; [`percent_decode`] only serves display getters.
    info: Mutex<parse::Info>,
    stale: AtomicBool,
}

impl SvnWorkingCopy {
    /// Finds the working copy containing `path`, walking up like
    /// [`crate::Repo::discover`]. `None` when there is no Subversion 1.7+
    /// administrative area (`.svn` holding `wc.db`) above it.
    pub fn discover(path: &Path) -> IdeResult<Option<Arc<Self>>> {
        let start = if path.is_file() { path.parent().unwrap_or(path) } else { path };
        let mut dir = start.to_path_buf();
        loop {
            if dir.join(".svn").join("wc.db").is_file() {
                let wc = Self { root: dir.clone(), info: Mutex::new(run_info(&dir)?), stale: AtomicBool::new(false) };
                return Ok(Some(Arc::new(wc)));
            }
            if !dir.pop() {
                return Ok(None);
            }
        }
    }

    /// The working-copy root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The cached (or, once stale, freshly read) `svn info` of the working-copy
    /// root.
    fn info(&self) -> IdeResult<parse::Info> {
        if self.stale.swap(false, Ordering::AcqRel) {
            let fresh = run_info(&self.root)?;
            *self.info.lock().unwrap_or_else(|p| p.into_inner()) = fresh;
        }
        Ok(self.info.lock().unwrap_or_else(|p| p.into_inner()).clone())
    }

    /// Marks the cached `svn info` stale; write operations call this so the
    /// next read picks up the new revision (and, after `svn switch`, URL).
    fn invalidate(&self) {
        self.stale.store(true, Ordering::Release);
    }

    /// The working copy's base revision (`svn info`), refreshing the cache
    /// after write operations.
    pub fn revision(&self) -> IdeResult<u64> {
        Ok(self.info()?.revision)
    }

    /// The URL of the working-copy root itself, percent-decoded for display.
    pub fn url(&self) -> IdeResult<String> {
        Ok(percent_decode(&self.info()?.url))
    }

    /// The URL of the repository root this working copy checks out from,
    /// percent-decoded for display.
    pub fn repo_url(&self) -> IdeResult<String> {
        Ok(percent_decode(&self.info()?.repo_root))
    }

    /// The repository UUID, for grouping and future remote handling.
    pub fn uuid(&self) -> IdeResult<String> {
        Ok(self.info()?.uuid)
    }

    /// Repository-relative path with `/` separators. Accepts absolute paths
    /// inside the working copy or paths already relative to it.
    fn rel_path(&self, path: &Path) -> IdeResult<String> {
        let path = if path.is_absolute() { path } else { &self.root.join(path) };
        let rel = path
            .strip_prefix(&self.root)
            .map_err(|_| {
                IdeError::new(
                    ide_core::error::ErrorKind::InvalidInput,
                    format!("{} is not inside the SVN working copy at {}", path.display(), self.root.display()),
                )
            })?
            .to_string_lossy()
            .replace('\\', "/");
        Ok(rel)
    }

    /// A repository-root-relative path (`/trunk/a.txt`, as `svn log -v` and
    /// `svn diff --summarize` print them) translated into this working copy's
    /// relative path. Files outside the working copy's subtree (other branches
    /// in the same repository) map to nothing.
    fn wc_rel_from_repo_path(&self, repo_path: &str, info: &parse::Info) -> Option<String> {
        let subpath = info.url.strip_prefix(&info.repo_root).unwrap_or_default();
        let rel = repo_path.strip_prefix(subpath)?;
        let rel = rel.trim_start_matches('/');
        (!rel.is_empty()).then(|| rel.to_owned())
    }

    /// svn's revision argument for the backend's revision string: `None` (the
    /// backend's "default") means `BASE`.
    fn rev(rev: Option<&str>) -> String {
        match rev {
            None => "BASE".to_owned(),
            Some(rev) => rev.trim().trim_start_matches('r').to_owned(),
        }
    }

    /// The revision number behind the backend's revision string (`r1234`).
    fn number(commit: &str) -> IdeResult<u64> {
        Self::rev(Some(commit)).parse().map_err(|_| {
            IdeError::new(ide_core::error::ErrorKind::InvalidInput, format!("not an SVN revision: {commit}"))
        })
    }

    /// Working-area status of `paths` (absolute) or the whole working copy.
    fn status_of(&self, paths: &[PathBuf]) -> IdeResult<Vec<FileStatus>> {
        Ok(self
            .status_entries(paths)?
            .into_iter()
            .map(|e| FileStatus {
                path: IpcPath(e.path),
                rel_path: e.rel_path,
                // No index: every change reads as unstaged, scheduled ones included.
                staged: None,
                unstaged: e.kind,
                renamed_from: None,
            })
            .collect())
    }

    /// One batched status lookup, keyed by path.
    fn status_map(&self, paths: &[PathBuf]) -> IdeResult<HashMap<PathBuf, Option<ChangeKind>>> {
        let mut map = HashMap::new();
        for entry in self.status_entries(paths)? {
            map.insert(entry.path, entry.kind);
        }
        Ok(map)
    }

    /// `svn status --xml` of `paths` (absolute) or the whole working copy.
    fn status_entries(&self, paths: &[PathBuf]) -> IdeResult<Vec<parse::StatusEntry>> {
        let mut args: Vec<&str> = vec!["--non-interactive", "status", "--xml"];
        let targets: Vec<String> = paths.iter().map(|p| p.to_string_lossy().into_owned()).collect();
        if !paths.is_empty() {
            args.push("--depth");
            args.push("empty");
            args.push("--");
            args.extend(targets.iter().map(String::as_str));
        }
        let out = cli::run_raw(&self.root, &args)?;
        if out.code == Some(0) {
            parse::status(&out.stdout_str(), &self.root)
        } else if cli::stderr_contains(&out, "E200005") || cli::stderr_contains(&out, "E155007") {
            // A path stopped being part of the working copy (deleted, or the
            // working copy was removed): nothing to report for it.
            Ok(Vec::new())
        } else {
            Err(cli::failure(&args, &out))
        }
    }

    /// Prepares `targets` for a commit: files the IDE created but that are
    /// still unversioned are scheduled for add, files that vanished from disk
    /// are scheduled for deletion — `svn commit` skips both otherwise.
    fn prepare_commit_targets(&self, targets: &[PathBuf]) -> IdeResult<()> {
        let map = self.status_map(targets)?;
        for path in targets {
            match map.get(path) {
                Some(Some(ChangeKind::Untracked)) => self.add(path)?,
                Some(_) if !path.exists() => self.schedule_delete(path)?,
                _ => {}
            }
        }
        Ok(())
    }

    /// `svn update` of the whole working copy; returns the CLI output for the
    /// VCS console.
    pub fn update(&self, cancel: &CancellationToken) -> IdeResult<String> {
        let args: Vec<&str> = vec!["--non-interactive", "update", "--"];
        let out = cli::run_cancellable(&self.root, &args, cancel)?;
        if out.code == Some(0) {
            self.invalidate();
            let mut text = out.stdout_str();
            if text.trim().is_empty() {
                text = String::from_utf8_lossy(&out.stderr).into_owned();
            }
            Ok(text)
        } else {
            Err(cli::failure(&args, &out))
        }
    }

    /// Repairs a working copy's administrative state after an interrupted
    /// operation (`svn cleanup`).
    pub fn cleanup(&self) -> IdeResult<()> {
        let args: Vec<&str> = vec!["--non-interactive", "cleanup"];
        cli::run(&self.root, &args)?;
        self.invalidate();
        Ok(())
    }

    /// `svn revert` of one path, tolerating entries that stopped being part of
    /// the working copy between the status read and the write. `-R` gives the
    /// directory recursion (`--depth infinity`).
    fn revert(&self, path: &Path, recursive: bool) -> IdeResult<()> {
        let rel = self.rel_path(path)?;
        let args: Vec<&str> = if recursive {
            vec!["--non-interactive", "revert", "-R", "--", &rel]
        } else {
            vec!["--non-interactive", "revert", "--", &rel]
        };
        let out = cli::run_raw(&self.root, &args)?;
        if out.code != Some(0) && !cli::stderr_contains(&out, "E155007") && !cli::stderr_contains(&out, "E200005") {
            return Err(cli::failure(&args, &out));
        }
        self.invalidate();
        Ok(())
    }

    /// `svn add`, tolerating already-versioned entries.
    fn add(&self, path: &Path) -> IdeResult<()> {
        let rel = self.rel_path(path)?;
        let args: Vec<&str> = vec!["--non-interactive", "add", "--force", "--parents", "--", &rel];
        let out = cli::run_raw(&self.root, &args)?;
        if out.code != Some(0) && !cli::stderr_contains(&out, "E200009") {
            return Err(cli::failure(&args, &out));
        }
        self.invalidate();
        Ok(())
    }

    /// Schedules a deletion (`svn delete --keep-local`), tolerating entries
    /// that are not versioned. `--keep-local` never touches the disk: the
    /// caller owns the working tree.
    fn schedule_delete(&self, path: &Path) -> IdeResult<()> {
        let rel = self.rel_path(path)?;
        let args: Vec<&str> = vec!["--non-interactive", "delete", "--force", "--keep-local", "--", &rel];
        let out = cli::run_raw(&self.root, &args)?;
        if out.code != Some(0) && !cli::stderr_contains(&out, "E155007") && !cli::stderr_contains(&out, "E200005") {
            return Err(cli::failure(&args, &out));
        }
        self.invalidate();
        Ok(())
    }

    /// The changed files of one revision, as `svn log -v -r <rev>` lists them.
    fn log_entry_changes(&self, revision: u64) -> IdeResult<Vec<(char, String, Option<String>, u64)>> {
        let rev = revision.to_string();
        let args: Vec<&str> = vec!["--non-interactive", "log", "--xml", "-v", "-r", &rev];
        let out = cli::run(&self.root, &args)?;
        Ok(parse::log(&out.stdout_str())?
            .into_iter()
            .flat_map(|entry| {
                entry.paths.into_iter().map(move |(action, path, copyfrom)| (action, path, copyfrom, entry.revision))
            })
            .collect())
    }

    /// Raw svn diff of the working copy (or `paths`) against `against`
    /// (`BASE` by default), with the internal diff so no external `diff-cmd`
    /// can alter the output.
    fn diff_bytes(&self, paths: &[PathBuf], against: Option<&str>, cancel: &CancellationToken) -> IdeResult<Vec<u8>> {
        let rev = against.map(|r| Self::rev(Some(r)));
        let targets: Vec<String> = paths.iter().map(|p| self.rel_path(p)).collect::<IdeResult<_>>()?;
        let mut args: Vec<&str> = vec!["--non-interactive", "diff", "--internal-diff"];
        if let Some(rev) = &rev {
            args.push("-r");
            args.push(rev);
        }
        if !targets.is_empty() {
            args.push("--");
            args.extend(targets.iter().map(String::as_str));
        }
        let out = cli::run_cancellable_raw(&self.root, &args, cancel)?;
        if out.code == Some(0) {
            Ok(out.stdout)
        } else if cli::stderr_contains(&out, "E200005") || cli::stderr_contains(&out, "E155010") {
            // Not under version control (yet): no base side to diff against.
            Ok(Vec::new())
        } else {
            Err(cli::failure(&args, &out))
        }
    }
}

/// `svn info --xml` of the working copy at `dir`.
fn run_info(dir: &Path) -> IdeResult<parse::Info> {
    let args = ["info", "--xml", "--non-interactive"];
    let out = cli::run(dir, &args)?;
    parse::info(&out.stdout_str())
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = |b: u8| (b as char).to_digit(16);
            if let (Some(high), Some(low)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((high * 16 + low) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_committed_revision(text: &str) -> Option<u64> {
    let index = text.find("Committed revision ")?;
    let rest = &text[index + "Committed revision ".len()..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

fn remove_from_disk(path: &Path) {
    let result = if path.is_dir() { std::fs::remove_dir_all(path) } else { std::fs::remove_file(path) };
    if let Err(error) = result {
        tracing::warn!("removing unversioned {}: {error}", path.display());
    }
}

impl Backend for SvnWorkingCopy {
    fn root(&self) -> &Path {
        &self.root
    }

    fn kind(&self) -> crate::VcsKind {
        crate::VcsKind::Svn
    }

    fn as_svn(&self) -> Option<&SvnWorkingCopy> {
        Some(self)
    }

    fn status(&self) -> IdeResult<Vec<FileStatus>> {
        self.status_of(&[])
    }

    fn status_paths(&self, paths: &[PathBuf]) -> IdeResult<Vec<FileStatus>> {
        // Absolute paths only; ones outside the root are ignored.
        let inside: Vec<PathBuf> =
            paths.iter().filter(|p| p.is_absolute() && p.strip_prefix(&self.root).is_ok()).cloned().collect();
        if inside.is_empty() {
            return Ok(Vec::new());
        }
        self.status_of(&inside)
    }

    fn file_at(&self, path: &Path, rev: Option<&str>, encoding: TextEncoding) -> IdeResult<Option<String>> {
        let rel = self.rel_path(path)?;
        let rev = Self::rev(rev);
        let args: Vec<&str> = vec!["--non-interactive", "cat", "-r", &rev, "--", &rel];
        let out = cli::run_raw(&self.root, &args)?;
        if out.code == Some(0) {
            Ok(Some(crate::text_encoding::decode(&out.stdout, encoding).into_owned()))
        } else if cli::stderr_contains(&out, "E200005")
            || cli::stderr_contains(&out, "E160013")
            || cli::stderr_contains(&out, "E200009")
            || cli::stderr_contains(&out, "has no pristine version")
        {
            // Not under version control, scheduled for add, or absent at that
            // revision.
            Ok(None)
        } else {
            Err(cli::failure(&args, &out))
        }
    }

    fn file_mode_at(&self, _path: &Path, _rev: Option<&str>) -> IdeResult<Option<String>> {
        // Subversion has no mode bits (the executable flag is a versioned
        // property), so nothing is reported and the diff viewer skips its
        // "File mode changed" display.
        Ok(None)
    }

    fn diff_text(&self, path: &Path, _staged: bool, encoding: TextEncoding) -> IdeResult<String> {
        // No index: the only diff is BASE ↔ working tree.
        let bytes = self.diff_bytes(&[path.to_path_buf()], None, &CancellationToken::new())?;
        Ok(crate::text_encoding::decode(&bytes, encoding).into_owned())
    }

    fn working_tree_patch(
        &self,
        path: Option<&Path>,
        against: Option<&str>,
        max_bytes: usize,
        cancel: &CancellationToken,
    ) -> IdeResult<String> {
        let paths = path.map(|p| vec![p.to_path_buf()]).unwrap_or_default();
        let text = String::from_utf8_lossy(&self.diff_bytes(&paths, against, cancel)?).into_owned();
        Ok(if text.len() <= max_bytes {
            text
        } else {
            let mut end = max_bytes;
            while end > 0 && !text.is_char_boundary(end) {
                end -= 1;
            }
            text[..end].to_owned()
        })
    }

    fn line_hunks(
        &self,
        path: &Path,
        current_text: &str,
        against_index: bool,
        encoding: TextEncoding,
    ) -> IdeResult<Vec<LineHunk>> {
        // No index: the gutter always compares against BASE.
        let _ = against_index;
        let diff = self.diff_bytes(&[path.to_path_buf()], None, &CancellationToken::new())?;
        if diff.is_empty() {
            // Unversioned files have no base side, like git's untracked ones:
            // one Added hunk spanning the whole text.
            return Ok(crate::diff::all_added(current_text));
        }
        Ok(parse::diff_line_hunks(&crate::text_encoding::decode(&diff, encoding)))
    }

    fn blame(
        &self,
        _path: &Path,
        _text: Option<&str>,
        _revision: Option<&str>,
        _encoding: TextEncoding,
    ) -> IdeResult<Vec<BlameLine>> {
        Err(IdeError::unavailable("annotate is not supported for SVN working copies yet"))
    }

    fn block_history(
        &self,
        _path: &Path,
        _text: Option<&str>,
        _lines: (u32, u32),
        _encoding: TextEncoding,
        _cancel: &CancellationToken,
    ) -> IdeResult<BlockHistory> {
        Err(IdeError::unavailable("selection history is not supported for SVN working copies yet"))
    }

    fn log(&self, query: &LogQuery, cancel: &CancellationToken) -> IdeResult<Vec<Commit>> {
        let mut args: Vec<&str> = vec!["--non-interactive", "log", "--xml"];
        if !query.summary_only {
            args.push("-v");
        }
        // svn has no author/date/regex filters; when those are in play a wider
        // window is fetched and filtered here. `--search` covers plain text
        // natively (case-insensitive substring over author and message).
        let filtered = query.text_regex
            || query.text_match_case
            || query.author.is_some()
            || !query.authors.is_empty()
            || query.since_ms.is_some()
            || query.until_ms.is_some();
        // Best-effort `A..B` range: both sides must be plain revision numbers
        // (svn has no range expression). The left side is exclusive, like git's.
        let range = query.range.as_deref().and_then(|range| {
            let (from, to) = range.split_once("..")?;
            let from = Self::rev(Some(from)).parse::<u64>().ok()?;
            let to = Self::rev(Some(to)).parse::<u64>().ok()?;
            Some(format!("{}:{}", from.saturating_add(1), to))
        });
        let mut window = query.limit.max(1) + query.skip;
        if filtered {
            window = (window * 10).min(4000);
        }
        let limit = window.to_string();
        if query.limit > 0 && range.is_none() {
            // A range bounds its own window; --limit would cut it short.
            args.push("--limit");
            args.push(&limit);
        }
        if let Some(range) = &range {
            args.push("-r");
            args.push(range);
        } else {
            // Without a revision svn walks down from the working-copy root's
            // own revision, which commits never move — the log would go stale
            // until an update. The repository's history starts at HEAD.
            args.push("-r");
            args.push("HEAD:1");
        }
        let search: Option<String> = query
            .text
            .as_ref()
            .or(query.speed_search.as_ref())
            .filter(|text| !query.text_regex && !text.is_empty())
            .map(|text| text.trim().to_owned())
            .filter(|text| !text.is_empty());
        if let Some(search) = &search {
            args.push("--search");
            args.push(search);
        }
        let targets: Vec<String> = query
            .paths
            .iter()
            .map(|p| &p.0)
            .chain(query.path.iter().map(|p| &p.0))
            .map(|p| self.rel_path(p))
            .collect::<IdeResult<_>>()?;
        if !targets.is_empty() {
            args.push("--");
            args.extend(targets.iter().map(String::as_str));
        }
        let out = cli::run_cancellable(&self.root, &args, cancel)?;
        if out.code != Some(0) {
            // An empty repository has no revision 1 to walk down from.
            if cli::stderr_contains(&out, "E160006") {
                return Ok(Vec::new());
            }
            return Err(cli::failure(&args, &out));
        }
        let mut entries = parse::log(&out.stdout_str())?;
        // svn's order depends on the `-r` direction; the linear history is
        // always walked newest first.
        entries.sort_unstable_by(|a, b| b.revision.cmp(&a.revision));
        if filtered {
            let authors: Vec<String> =
                query.authors.iter().chain(query.author.iter()).map(|a| a.to_lowercase()).collect();
            let pattern = query.text_regex.then(|| {
                let case = if query.text_match_case { "" } else { "(?i)" };
                regex_lite::Regex::new(&format!("{case}{}", query.text.clone().unwrap_or_default())).ok()
            });
            let needle = (!query.text_regex)
                .then(|| query.text.as_ref().map(|t| if query.text_match_case { t.clone() } else { t.to_lowercase() }));
            entries.retain(|entry| {
                let author_ok = authors.is_empty()
                    || authors.iter().any(|a| {
                        let name = entry.author.to_lowercase();
                        if query.authors_exact { name == *a } else { name.contains(a) }
                    });
                let text_ok = if let Some(regex) = &pattern {
                    regex.as_ref().is_some_and(|r| r.is_match(&entry.message))
                } else {
                    match &needle {
                        Some(Some(needle)) => entry.message.to_lowercase().contains(needle),
                        _ => true,
                    }
                };
                let time_ok = entry.time_ms.is_none_or(|ms| {
                    query.since_ms.is_none_or(|since| ms >= since) && query.until_ms.is_none_or(|until| ms < until)
                });
                author_ok && text_ok && time_ok
            });
        }
        let total = entries.len() as u32;
        let skipped = query.skip.min(total);
        let take = if query.limit > 0 { query.limit as usize } else { total as usize };
        let page: Vec<&parse::LogEntry> = entries.iter().skip(skipped as usize).take(take).collect();
        // Linear history, newest first: each commit's parent is the next older
        // revision in the page; the oldest shown row has none.
        let mut out = Vec::with_capacity(page.len());
        for (index, entry) in page.iter().enumerate() {
            let id = format!("r{}", entry.revision);
            let parent = page.get(index + 1).map(|prev| format!("r{}", prev.revision));
            let summary = entry.message.lines().next().unwrap_or_default().to_owned();
            let time_ms = entry.time_ms.unwrap_or(0.0);
            out.push(Commit {
                id: id.clone(),
                short_id: id,
                parents: parent.into_iter().collect(),
                graph_parents: None,
                graph_dashed: Vec::new(),
                history_path: None,
                log_position: Some(skipped + index as u32),
                log_total: (query.limit > 0).then_some(total),
                log_merge_time_ms: None,
                graph_boundary_parents: Vec::new(),
                on_current_branch: None,
                author: entry.author.clone(),
                email: String::new(),
                committer: entry.author.clone(),
                committer_email: String::new(),
                time_ms,
                summary,
                message: entry.message.clone(),
                message_truncated: false,
                refs: Vec::new(),
                commit_time_ms: time_ms,
                dashed: Vec::new(),
            });
        }
        Ok(out)
    }

    fn log_details(&self, revision: &str) -> IdeResult<crate::LogDetails> {
        let rev = Self::rev(Some(revision));
        let args: Vec<&str> = vec!["--non-interactive", "log", "--xml", "-r", &rev];
        let out = cli::run(&self.root, &args)?;
        parse::log(&out.stdout_str())?
            .into_iter()
            .next()
            .map(|entry| crate::LogDetails {
                message: entry.message,
                // Branch names and signatures are Git's; svn has neither.
                containing_branches: Vec::new(),
                signature: String::from("N"),
                signer: String::new(),
                key: String::new(),
                fingerprint: String::new(),
            })
            .ok_or_else(|| IdeError::not_found("commit not found"))
    }

    fn commit_files(&self, commit: &str) -> IdeResult<Vec<ChangedFile>> {
        let revision = Self::number(commit)?;
        let info = self.info()?;
        Ok(self
            .log_entry_changes(revision)?
            .into_iter()
            .filter_map(|(action, repo_path, copyfrom, _)| {
                let kind = match action {
                    'A' => ChangeKind::Added,
                    'D' => ChangeKind::Deleted,
                    'R' => ChangeKind::Renamed,
                    _ => ChangeKind::Modified,
                };
                let path = self.wc_rel_from_repo_path(&repo_path, &info)?;
                let old_path = copyfrom.as_ref().and_then(|from| self.wc_rel_from_repo_path(from, &info));
                Some(ChangedFile { path, kind, old_path, link: None })
            })
            .collect())
    }

    fn commits_changes(&self, commits: &[String]) -> IdeResult<Vec<RevisionChange>> {
        let info = self.info()?;
        let mut out = Vec::new();
        for commit in commits {
            let number = Self::number(commit)?;
            for (action, repo_path, copyfrom, revision) in self.log_entry_changes(number)? {
                let Some(path) = self.wc_rel_from_repo_path(&repo_path, &info) else { continue };
                let kind = match action {
                    'A' => ChangeKind::Added,
                    'D' => ChangeKind::Deleted,
                    'R' => ChangeKind::Renamed,
                    _ => ChangeKind::Modified,
                };
                out.push(RevisionChange {
                    path,
                    kind,
                    old_path: copyfrom.as_ref().and_then(|from| self.wc_rel_from_repo_path(from, &info)),
                    before: (action != 'A' && revision > 1).then(|| format!("r{}", revision - 1)),
                    after: (action != 'D').then(|| format!("r{revision}")),
                    link: None,
                });
            }
        }
        Ok(out)
    }

    fn diff_revisions(&self, from: &str, to: &str) -> IdeResult<Vec<ChangedFile>> {
        let from = Self::rev(Some(from));
        let to = Self::rev(Some(to));
        let range = format!("{from}:{to}");
        let args: Vec<&str> = vec!["--non-interactive", "diff", "--summarize", "--xml", "-r", &range];
        let out = cli::run(&self.root, &args)?;
        let info = self.info()?;
        Ok(parse::summarize(&out.stdout_str())?
            .into_iter()
            .filter_map(|row| {
                let path = self.wc_rel_from_repo_path(&row.path, &info)?;
                Some(ChangedFile { path, kind: row.kind, old_path: None, link: None })
            })
            .collect())
    }

    fn diff_with_worktree(&self, rev: &str) -> IdeResult<Vec<ChangedFile>> {
        Ok(parse::unified_sections(&self.diff_bytes(&[], Some(rev), &CancellationToken::new())?)
            .into_iter()
            .map(|section| ChangedFile { path: section.path, kind: section.kind, old_path: None, link: None })
            .collect())
    }

    fn numstat_with_worktree(&self, rev: &str) -> IdeResult<Vec<(String, u32, u32)>> {
        Ok(parse::unified_sections(&self.diff_bytes(&[], Some(rev), &CancellationToken::new())?)
            .into_iter()
            .map(|section| (section.path, section.added_lines, section.deleted_lines))
            .collect())
    }

    fn stage(&self, paths: &[PathBuf]) -> IdeResult<()> {
        let map = self.status_map(paths)?;
        for path in paths {
            match map.get(path) {
                // Open for add.
                Some(Some(ChangeKind::Untracked)) => self.add(path)?,
                // Missing from disk: schedule the deletion so a commit can take it.
                Some(Some(ChangeKind::Deleted)) if !path.exists() => self.schedule_delete(path)?,
                // Modified (content or property) files need no preparation:
                // svn commits working content directly.
                _ => {}
            }
        }
        Ok(())
    }

    fn unstage(&self, _paths: &[PathBuf]) -> IdeResult<()> {
        // No index: a modified file's "unstage" would have to revert it, which
        // destroys working content. Scheduled operations only leave the stage
        // through discard ([`Backend::discard`]).
        Ok(())
    }

    fn discard(&self, paths: &[PathBuf]) -> IdeResult<()> {
        let map = self.status_map(paths)?;
        for path in paths {
            match map.get(path).copied().flatten() {
                // Unversioned entries are deleted, as `git clean` does; plain
                // `svn revert` only prints "Skipped" for them.
                Some(ChangeKind::Untracked) => remove_from_disk(path),
                // Modified, missing, scheduled and conflicted entries return to
                // BASE (a scheduled add becomes unversioned, content intact —
                // `p4 revert` semantics). `-R` covers directories.
                Some(_) => self.revert(path, path.is_dir())?,
                None => {}
            }
        }
        Ok(())
    }

    fn commit(
        &self,
        message: &str,
        options: &CommitOptions,
        _encoding: &dyn Fn(&Path) -> IdeResult<TextEncoding>,
    ) -> IdeResult<String> {
        if options.amend || options.amend_target.is_some() {
            return Err(IdeError::unavailable("SVN does not support amending commits"));
        }
        let mut targets: Vec<PathBuf> = options
            .stage
            .iter()
            .map(|p| &p.0)
            .chain(options.partial.iter().map(|p| &p.path.0))
            .filter(|path| !options.exclude.iter().any(|excluded| excluded.0 == **path))
            .cloned()
            .collect();
        targets.sort();
        targets.dedup();
        if targets.is_empty() {
            return Err(IdeError::new(ide_core::error::ErrorKind::InvalidInput, "nothing selected to commit"));
        }
        // Make sure scheduled adds and disk deletions are in place first.
        self.prepare_commit_targets(&targets)?;
        let targets_owned: Vec<String> = targets.iter().map(|p| p.to_string_lossy().into_owned()).collect();
        let mut args: Vec<&str> = vec!["--non-interactive", "commit", "--encoding", "UTF-8", "--file", "-"];
        args.extend(targets_owned.iter().map(String::as_str));
        let cancel = CancellationToken::new();
        let out = cli::run_cancellable_stdin(&self.root, &args, Some(message.as_bytes()), &cancel)?;
        self.invalidate();
        if out.code != Some(0) {
            return Err(cli::failure(&args, &out));
        }
        // "Committed revision N." lands on stderr, the transfer lines on stdout.
        let printed = format!("{}{}", out.stdout_str(), String::from_utf8_lossy(&out.stderr));
        match parse_committed_revision(&printed) {
            Some(revision) => Ok(format!("r{revision}")),
            None => Ok(String::new()),
        }
    }

    fn pending_changes(
        &self,
        options: &CommitOptions,
        max_bytes: usize,
        encoding: &dyn Fn(&Path) -> IdeResult<TextEncoding>,
    ) -> IdeResult<Vec<PendingChange>> {
        // A selection previews only its targets; otherwise every versioned
        // change of the working copy (unversioned files are not part of an svn
        // commit until scheduled).
        let mut selected: Vec<PathBuf> = options
            .stage
            .iter()
            .map(|p| &p.0)
            .chain(options.partial.iter().map(|p| &p.path.0))
            .filter(|path| !options.exclude.iter().any(|excluded| excluded.0 == **path))
            .cloned()
            .collect();
        selected.sort();
        selected.dedup();
        let mut changes: Vec<PendingChange> = self
            .status_entries(&selected)?
            .into_iter()
            .filter_map(|entry| match entry.kind {
                Some(ChangeKind::Untracked) | None => None,
                Some(kind) => Some(PendingChange {
                    path: entry.path,
                    old_path: None,
                    status: match kind {
                        ChangeKind::Added => "added",
                        ChangeKind::Deleted => "deleted",
                        ChangeKind::Renamed => "renamed",
                        _ => "modified",
                    },
                    diff: None,
                    truncated: false,
                }),
            })
            .collect();
        if changes.is_empty() {
            return Ok(changes);
        }
        // One diff covers every listed file; sections are matched by their
        // repository-relative `Index:` path.
        let diff = self.diff_bytes(&selected, None, &CancellationToken::new())?;
        let parsed = parse::unified_sections(&diff);
        let sections: HashMap<String, &parse::DiffSection> =
            parsed.iter().map(|section| (section.path.clone(), section)).collect();
        let mut budget = max_bytes;
        for change in &mut changes {
            let rel = self.rel_path(&change.path)?;
            let Some(section) = sections.get(&rel) else { continue };
            let charset = encoding(&change.path).unwrap_or_else(|_| encoding_rs::UTF_8.into());
            let text = crate::text_encoding::decode_raw(&section.text, charset);
            if text.len() <= budget {
                budget -= text.len();
                change.diff = Some(text.into_owned());
            } else {
                let mut cut = budget.min(text.len());
                // Whole lines only, at a character boundary.
                while cut > 0 && !text.is_char_boundary(cut) {
                    cut -= 1;
                }
                let kept = text[..cut].rfind('\n').map_or(0, |i| i + 1);
                budget -= kept;
                change.diff = Some(text[..kept].to_owned());
                change.truncated = true;
            }
        }
        Ok(changes)
    }

    fn files_added(&self, paths: &[PathBuf]) -> IdeResult<()> {
        for path in paths {
            if path.exists() {
                self.add(path)?;
            }
        }
        Ok(())
    }

    fn files_deleted(&self, paths: &[PathBuf]) -> IdeResult<()> {
        for path in paths {
            self.schedule_delete(path)?;
        }
        Ok(())
    }

    fn file_moved(&self, from: &Path, to: &Path) -> IdeResult<()> {
        // `svn move` needs the source at its old location and an empty
        // destination; the IDE calls this after moving on disk, so it usually
        // cannot run. When the workspace state does allow it, the real move
        // keeps the file's history connected.
        if from.exists() && !to.exists() {
            let from_rel = self.rel_path(from)?;
            let to_rel = self.rel_path(to)?;
            let args: Vec<&str> = vec!["--non-interactive", "move", "--force", "--parents", "--", &from_rel, &to_rel];
            let out = cli::run_raw(&self.root, &args)?;
            if out.code == Some(0) {
                self.invalidate();
                return Ok(());
            }
        }
        // Otherwise record the move as a delete of the old path plus an add of
        // the new one: history follows only through svn's rename detection.
        let map = self.status_map(&[from.to_path_buf()])?;
        if map.contains_key(&from.to_path_buf()) {
            self.schedule_delete(from)?;
        }
        self.add(to)
    }
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn percent_decodes_urls() {
        assert_eq!(percent_decode("file:///repo/%E9%9D%92%E7%A8%9A/trunk"), "file:///repo/青稚/trunk");
        assert_eq!(percent_decode("file:///repo/plain"), "file:///repo/plain");
        assert_eq!(percent_decode("100%25"), "100%");
    }

    #[test]
    fn parses_committed_revision() {
        assert_eq!(parse_committed_revision("\nCommitted revision 7.\n"), Some(7));
        assert_eq!(
            parse_committed_revision("Sending        a.txt\nTransmitting file data .done\nCommitted revision 123."),
            Some(123)
        );
        assert_eq!(parse_committed_revision("nope"), None);
    }

    #[test]
    fn maps_repo_paths_to_working_copy_relative() {
        let wc = SvnWorkingCopy {
            root: PathBuf::from("/wc"),
            info: Mutex::new(parse::Info {
                revision: 1,
                url: "file:///repo/trunk".to_owned(),
                repo_root: "file:///repo".to_owned(),
                uuid: String::new(),
            }),
            stale: AtomicBool::new(false),
        };
        let info = wc.info().unwrap();
        assert_eq!(wc.wc_rel_from_repo_path("/trunk/a.txt", &info).as_deref(), Some("a.txt"));
        assert_eq!(wc.wc_rel_from_repo_path("/trunk/sub/b.txt", &info).as_deref(), Some("sub/b.txt"));
        assert_eq!(wc.wc_rel_from_repo_path("/branches/other/x", &info), None, "outside the working copy's subtree");
        assert_eq!(wc.wc_rel_from_repo_path("/trunk", &info), None, "the working copy root itself");
    }
}
