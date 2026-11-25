//! Parsing `svn --xml` output and svn's unified diff format into the shared
//! backend types. The XML shapes are the ones Subversion 1.8–1.14 prints
//! (verified against the 1.14 CLI): `status` groups changelisted entries under
//! sibling `<changelist>` elements, absolute targets repeat the given path
//! spelling, and `log -v` paths are repository-root relative with a leading
//! slash.

use crate::{ChangeKind, HunkKind, LineHunk};
use ide_core::{IdeError, IdeResult};
use std::path::{Path, PathBuf};

fn parse_doc<'a>(xml: &'a str, what: &str) -> IdeResult<roxmltree::Document<'a>> {
    roxmltree::Document::parse(xml)
        .map_err(|e| IdeError::new(ide_core::error::ErrorKind::Protocol, format!("svn {what}: invalid XML: {e}")))
}

fn tag<'a>(node: roxmltree::Node<'a, 'a>, name: &str) -> Option<roxmltree::Node<'a, 'a>> {
    node.children().find(|c| c.is_element() && c.tag_name().name() == name)
}

fn tags<'a>(node: roxmltree::Node<'a, 'a>, name: &'a str) -> impl Iterator<Item = roxmltree::Node<'a, 'a>> + 'a {
    node.children().filter(move |c| c.is_element() && c.tag_name().name() == name)
}

fn text<'a>(node: roxmltree::Node<'a, 'a>, name: &str) -> Option<&'a str> {
    tag(node, name).and_then(|n| n.text())
}

/// Subversion dates are ISO 8601 with optional fractional seconds, e.g.
/// `2026-11-24T16:13:58.601133Z`. Servers normally emit UTC (`Z`), but a
/// `±hh:mm` offset is valid XML too, so both spellings parse. Sub-millisecond
/// fractions are truncated.
pub(crate) fn time_ms(iso: &str) -> Option<f64> {
    let (date, time) = iso.split_once('T')?;
    let mut date = date.split('-');
    let year: i64 = date.next()?.parse().ok()?;
    let month: u32 = date.next()?.parse().ok()?;
    let day: u32 = date.next()?.parse().ok()?;
    let (clock, offset_ms) = match time.split_once(['+', '-']) {
        Some((clock, offset)) => {
            let mut offset = offset.split(':');
            let hours: f64 = offset.next()?.parse().ok()?;
            let minutes: f64 = offset.next().and_then(|m| m.parse().ok()).unwrap_or(0.0);
            (clock, (hours * 60.0 + minutes) * 60_000.0)
        }
        None => (time.trim_end_matches('Z'), 0.0),
    };
    let mut clock = clock.split(':');
    let hour: i64 = clock.next()?.parse().ok()?;
    let minute: i64 = clock.next()?.parse().ok()?;
    let second: f64 = clock.next().and_then(|s| s.parse().ok()).unwrap_or(0.0);
    let ms = days_from_civil(year, month, day) * 86_400_000
        + (hour * 3_600 + minute * 60) * 1_000
        + (second * 1_000.0) as i64
        - offset_ms as i64;
    Some(ms as f64)
}

/// Days from 1970-01-01 (Howard Hinnant's `days_from_civil`).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = ((m + 9) % 12) as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// One working-copy entry of `svn status --xml`.
#[derive(Debug, PartialEq)]
pub(crate) struct StatusEntry {
    pub path: PathBuf,
    pub rel_path: String,
    /// The change as the working area reports it. Subversion has no index, so
    /// `FileStatus::staged` is always `None` and everything the user did —
    /// scheduled operations included — reads as `unstaged`.
    pub kind: Option<ChangeKind>,
}

/// `svn status --xml` of a working copy run from its root. Entries appear both
/// under `<target>` (no changelist) and under `<changelist name="…">`; paths
/// are relative to the root unless an absolute target was passed, in which case
/// svn repeats the caller's spelling (with Windows separators).
pub(crate) fn status(xml: &str, root: &Path) -> IdeResult<Vec<StatusEntry>> {
    let doc = parse_doc(xml, "status")?;
    let status = tag(doc.root(), "status").ok_or_else(|| missing("status"))?;
    let mut out = Vec::new();
    for section in status.children().filter(|c| c.is_element()) {
        if !matches!(section.tag_name().name(), "target" | "changelist") {
            continue;
        }
        for entry in tags(section, "entry") {
            let Some(path_attr) = entry.attribute("path") else { continue };
            let Some(wc_status) = tag(entry, "wc-status") else { continue };
            let Some(kind) = kind(wc_status) else { continue };
            let (path, rel_path) = normalize(path_attr, root);
            out.push(StatusEntry { path, rel_path, kind: Some(kind) });
        }
    }
    Ok(out)
}

/// svn's status vocabulary mapped onto the backend's change kinds. `item="none"`
/// with `props="modified"` is a property-only change — Subversion stores
/// properties outside the file, and the Commit window treats it as a regular
/// modification.
fn kind(wc_status: roxmltree::Node) -> Option<ChangeKind> {
    let item = wc_status.attribute("item").unwrap_or("none");
    let prop_changed = wc_status.attribute("props") == Some("modified");
    match item {
        "added" => Some(ChangeKind::Added),
        "deleted" => Some(ChangeKind::Deleted),
        // D + A scheduled in one go: the net effect reads as a modification.
        "replaced" => Some(ChangeKind::Modified),
        // Removed from disk without `svn delete`; `svn revert` restores it.
        "missing" => Some(ChangeKind::Deleted),
        // Conservative on both: the file needs the user's attention either way.
        "conflicted" | "obstructed" => Some(ChangeKind::Conflicted),
        "unversioned" => Some(ChangeKind::Untracked),
        "modified" => Some(ChangeKind::Modified),
        _ if prop_changed => Some(ChangeKind::Modified),
        _ => None,
    }
}

fn missing(what: &str) -> IdeError {
    IdeError::new(ide_core::error::ErrorKind::Protocol, format!("svn: XML has no <{what}> element"))
}

fn normalize(path_attr: &str, root: &Path) -> (PathBuf, String) {
    let unix = path_attr.replace('\\', "/");
    if Path::new(&unix).is_absolute() {
        let path = PathBuf::from(&unix);
        let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().replace('\\', "/");
        (path, rel)
    } else {
        let rel = unix.trim_end_matches('/');
        (root.join(rel), rel.to_owned())
    }
}

/// One `<logentry>` of `svn log --xml [-v]`.
#[derive(Debug)]
pub(crate) struct LogEntry {
    pub revision: u64,
    pub author: String,
    pub time_ms: Option<f64>,
    pub message: String,
    /// `(action, repository-root-relative path, copyfrom path)`; empty without `-v`.
    pub paths: Vec<(char, String, Option<String>)>,
}

pub(crate) fn log(xml: &str) -> IdeResult<Vec<LogEntry>> {
    let doc = parse_doc(xml, "log")?;
    let mut out = Vec::new();
    if let Some(log) = tag(doc.root(), "log") {
        for entry in tags(log, "logentry") {
            let revision: u64 =
                entry.attribute("revision").and_then(|r| r.trim_start_matches('r').parse().ok()).ok_or_else(|| {
                    IdeError::new(ide_core::error::ErrorKind::Protocol, "svn log: logentry without revision")
                })?;
            let paths = tag(entry, "paths")
                .map(|paths| {
                    tags(paths, "path")
                        .filter_map(|path| {
                            let action = path.attribute("action")?.chars().next()?;
                            let copyfrom = path.attribute("copyfrom-path").map(str::to_owned);
                            let name = path.text()?;
                            Some((action, name.to_owned(), copyfrom))
                        })
                        .collect()
                })
                .unwrap_or_default();
            out.push(LogEntry {
                revision,
                author: text(entry, "author").unwrap_or_default().to_owned(),
                time_ms: text(entry, "date").and_then(time_ms),
                message: text(entry, "msg").unwrap_or_default().to_owned(),
                paths,
            });
        }
    }
    Ok(out)
}

/// One `<entry>` of `svn blame --xml`: line `line_number` (1-based) was last
/// changed by `revision`. The revision rides on the nested `<commit>` (verified
/// against 1.14: `<entry line-number="N">` carries no revision of its own);
/// the author and date live there too, and a local commit can omit them
/// (anonymous `file://` commits have no `<author>`), which read as empty.
#[derive(Debug, PartialEq)]
pub(crate) struct BlameEntry {
    pub line_number: u32,
    pub revision: u64,
    pub author: String,
    pub time_ms: Option<f64>,
}

pub(crate) fn blame(xml: &str) -> IdeResult<Vec<BlameEntry>> {
    let doc = parse_doc(xml, "blame")?;
    let mut out = Vec::new();
    if let Some(target) = tag(doc.root(), "blame").and_then(|blame| tag(blame, "target")) {
        for entry in tags(target, "entry") {
            let Some(line_number) = entry.attribute("line-number").and_then(|n| n.parse().ok()) else { continue };
            let commit = tag(entry, "commit");
            let revision = commit
                .as_ref()
                .and_then(|commit| commit.attribute("revision"))
                // Older server spellings repeat the revision on the entry.
                .or_else(|| entry.attribute("revision"))
                .and_then(|revision| revision.parse().ok());
            let Some(revision) = revision else { continue };
            out.push(BlameEntry {
                line_number,
                revision,
                author: commit.as_ref().and_then(|commit| text(*commit, "author")).unwrap_or_default().to_owned(),
                time_ms: commit.as_ref().and_then(|commit| text(*commit, "date")).and_then(time_ms),
            });
        }
    }
    Ok(out)
}

/// One `<entry>` of `svn ls --xml`: an item of the listed directory.
#[derive(Debug, PartialEq)]
pub(crate) struct LsEntry {
    pub name: String,
    /// The server's `kind` attribute: `"dir"`, `"file"`, or another reported value.
    pub kind: String,
}

pub(crate) fn ls(xml: &str) -> IdeResult<Vec<LsEntry>> {
    let doc = parse_doc(xml, "ls")?;
    let mut out = Vec::new();
    if let Some(list) = tag(doc.root(), "lists").and_then(|lists| tag(lists, "list")) {
        for entry in tags(list, "entry") {
            let Some(name) = text(entry, "name") else { continue };
            out.push(LsEntry { name: name.to_owned(), kind: entry.attribute("kind").unwrap_or_default().to_owned() });
        }
    }
    Ok(out)
}

/// `svn info --xml` of a working-copy root.
#[derive(Debug, Clone)]
pub(crate) struct Info {
    pub revision: u64,
    /// URL of the working-copy directory itself (percent-encoded).
    pub url: String,
    /// URL of the repository root above it.
    pub repo_root: String,
    pub uuid: String,
}

pub(crate) fn info(xml: &str) -> IdeResult<Info> {
    let doc = parse_doc(xml, "info")?;
    let entry = tag(doc.root(), "info").and_then(|info| tag(info, "entry")).ok_or_else(|| missing("entry"))?;
    let repository = tag(entry, "repository").ok_or_else(|| missing("repository"))?;
    Ok(Info {
        revision: entry.attribute("revision").and_then(|r| r.parse().ok()).unwrap_or(0),
        url: text(entry, "url").unwrap_or_default().to_owned(),
        repo_root: text(repository, "root").unwrap_or_default().to_owned(),
        uuid: text(repository, "uuid").unwrap_or_default().to_owned(),
    })
}

/// `svn diff --summarize --xml` (revision-to-revision): repository-root
/// relative paths with their item.
#[derive(Debug, PartialEq)]
pub(crate) struct SummarizedPath {
    pub path: String,
    pub kind: ChangeKind,
}

pub(crate) fn summarize(xml: &str) -> IdeResult<Vec<SummarizedPath>> {
    let doc = parse_doc(xml, "diff")?;
    let mut out = Vec::new();
    if let Some(paths) = tag(doc.root(), "diff").and_then(|diff| tag(diff, "paths")) {
        for path in tags(paths, "path") {
            let Some(name) = path.text() else { continue };
            let kind = match path.attribute("item").unwrap_or("none") {
                "added" => ChangeKind::Added,
                "deleted" => ChangeKind::Deleted,
                "replaced" | "modified" => ChangeKind::Modified,
                _ => continue,
            };
            out.push(SummarizedPath { path: name.to_owned(), kind });
        }
    }
    Ok(out)
}

/// One `Index:` section of svn's unified diff output. The section stays raw
/// bytes so each caller can decode it with the file's own encoding.
#[derive(Debug, PartialEq)]
pub(crate) struct DiffSection {
    pub path: String,
    pub kind: ChangeKind,
    pub added_lines: u32,
    pub deleted_lines: u32,
    /// The section verbatim, from `Index:` up to (not including) the next one.
    pub text: Vec<u8>,
}

/// Splits raw `svn diff` output into per-file sections. Added files show
/// `(revision 0)` or `(nonexistent)` on the minus side, deleted files on the
/// plus side; a section with headers but no `@@` hunks is a property-only
/// change.
pub(crate) fn unified_sections(diff: &[u8]) -> Vec<DiffSection> {
    fn is(line: &[u8], prefix: &str) -> bool {
        line.starts_with(prefix.as_bytes())
    }
    let mut starts: Vec<usize> = Vec::new();
    let mut offset = 0usize;
    for line in diff.split_inclusive(|b| *b == b'\n') {
        if is(line, "Index: ") {
            starts.push(offset);
        }
        offset += line.len();
    }
    let mut sections = Vec::with_capacity(starts.len());
    for (i, start) in starts.iter().enumerate() {
        let end = starts.get(i + 1).copied().unwrap_or(diff.len());
        let text = &diff[*start..end.min(diff.len())];
        if let Some(section) = build_section(text) {
            sections.push(section);
        }
    }
    sections
}

fn build_section(text: &[u8]) -> Option<DiffSection> {
    fn is(line: &[u8], prefix: &str) -> bool {
        line.starts_with(prefix.as_bytes())
    }
    let path_line = text.split(|b| *b == b'\n').find(|l| is(l, "Index: "))?;
    let path = String::from_utf8_lossy(&path_line["Index: ".len()..]).trim().to_owned();
    let header = |prefix: &str| {
        text.split(|b| *b == b'\n')
            .find(|l| is(l, prefix))
            .map(|l| String::from_utf8_lossy(l).into_owned())
            .unwrap_or_default()
    };
    let minus = header("--- ");
    let plus = header("+++ ");
    let base = |header: &str| {
        header.contains("(revision 0)") || header.contains("(nonexistent)") || header.contains("/dev/null")
    };
    let kind = if base(&minus) {
        ChangeKind::Added
    } else if base(&plus) {
        ChangeKind::Deleted
    } else {
        ChangeKind::Modified
    };
    let mut added_lines = 0u32;
    let mut deleted_lines = 0u32;
    let mut in_props = false;
    for line in text.split(|b| *b == b'\n') {
        if is(line, "Property changes on:") {
            // Property diffs trail the content hunks inside the same `Index:`
            // section; their `+value` lines are not line changes.
            in_props = true;
            continue;
        }
        if in_props || is(line, "+++") || is(line, "---") {
            continue;
        }
        if line.starts_with(b"+") {
            added_lines += 1;
        } else if line.starts_with(b"-") {
            deleted_lines += 1;
        }
    }
    Some(DiffSection { path, kind, added_lines, deleted_lines, text: text.to_vec() })
}

/// Gutter hunks of one file's `svn diff` (unified format): one
/// [`LineHunk`] per contiguous `+`/`-` run, context lines separating runs.
/// Coordinates are 0-based lines of the current text; pure deletions have
/// `lines == 0` and `start` = the line that now follows the removed block, as
/// [`crate::diff_hunks`] reports them for Git.
pub(crate) fn diff_line_hunks(diff: &str) -> Vec<LineHunk> {
    /// A contiguous `+`/`-` run within one `@@` block.
    struct Run {
        added: Vec<String>,
        deleted: Vec<String>,
        /// 0-based first line of the run in the current text.
        start: u32,
        /// 0-based first line of the run in the base text.
        old_start: u32,
    }

    fn flush(run: Option<Run>, out: &mut Vec<LineHunk>) {
        let Some(run) = run else { return };
        let kind = if run.added.is_empty() {
            HunkKind::Deleted
        } else if run.deleted.is_empty() {
            HunkKind::Added
        } else {
            HunkKind::Modified
        };
        out.push(LineHunk {
            kind,
            start: run.start,
            lines: run.added.len() as u32,
            old_start: run.old_start,
            old_lines: run.deleted.len() as u32,
            old_text: run.deleted.into_iter().collect(),
        });
    }

    let mut out = Vec::new();
    let mut run: Option<Run> = None;
    let mut old_pos = 0u32;
    let mut new_pos = 0u32;
    let mut in_hunk = false;
    let mut in_props = false;
    for line in diff.lines() {
        if let Some(header) = line.strip_prefix("@@ ") {
            flush(run.take(), &mut out);
            in_props = false;
            in_hunk = true;
            // `@@ -old,start +new,start @@ context`
            let mut sides = header.split_whitespace();
            let old = sides.next().unwrap_or("").trim_start_matches('-');
            let new = sides.next().unwrap_or("").trim_start_matches('+');
            let parse_start = |spec: &str| -> u32 {
                spec.split(',').next().and_then(|s| s.parse::<u32>().ok()).unwrap_or(1).saturating_sub(1)
            };
            old_pos = parse_start(old);
            new_pos = parse_start(new);
            continue;
        }
        if in_hunk && line.starts_with("Index: ") {
            // A second file's section begins: single-file diffs never reach this.
            flush(run.take(), &mut out);
            break;
        }
        if line.starts_with("Property changes on:") {
            // Property values are listed with `+value` lines inside the same
            // section; they are not line changes.
            in_props = true;
            flush(run.take(), &mut out);
            continue;
        }
        if !in_hunk || in_props {
            continue;
        }
        if let Some(rest) = line.strip_prefix('+') {
            let current =
                run.get_or_insert(Run { added: Vec::new(), deleted: Vec::new(), start: new_pos, old_start: old_pos });
            current.added.push(format!("{rest}\n"));
            new_pos += 1;
        } else if let Some(rest) = line.strip_prefix('-') {
            let current =
                run.get_or_insert(Run { added: Vec::new(), deleted: Vec::new(), start: new_pos, old_start: old_pos });
            current.deleted.push(format!("{rest}\n"));
            old_pos += 1;
        } else if line.starts_with('\\') {
            // `\ No newline at end of file`: belongs to the run it follows.
        } else {
            flush(run.take(), &mut out);
            if line.starts_with(' ') {
                old_pos += 1;
                new_pos += 1;
            }
        }
    }
    flush(run, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: &str = if cfg!(windows) { r"C:\wc" } else { "/wc" };

    /// 2026-11-24T16:13:58.601133Z in epoch milliseconds.
    const T1_MS: f64 = 1_795_536_838_601.0;
    const T2_MS: f64 = 1_795_537_200_000.0; // same day, 16:20:00.000000Z

    #[test]
    fn parses_status_with_changelist_sections() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<status>
<target
   path=".">
<entry
   path="b.txt">
<wc-status
   item="unversioned"
   props="none">
</wc-status>
</entry>
<entry
   path="gone.txt">
<wc-status
   item="missing"
   props="none">
</wc-status>
</entry>
<entry
   path="props.txt">
<wc-status
   item="none"
   props="modified"
   revision="3">
</wc-status>
</entry>
<entry
   path="conflict.txt">
<wc-status
   item="conflicted"
   props="modified">
</wc-status>
</entry>
<entry
   path="obstructed.txt">
<wc-status
   item="obstructed"
   props="none">
</wc-status>
</entry>
<entry
   path="external-dir">
<wc-status
   item="external"
   props="none">
</wc-status>
</entry>
<entry
   path="clean.txt">
<wc-status
   item="none"
   props="none"
   revision="1">
</wc-status>
</entry>
</target>
<changelist
   name="my-feature">
<entry
   path="a.txt">
<wc-status
   props="none"
   item="modified"
   revision="1">
<commit
   revision="1">
<author>writer</author>
<date>2026-11-24T16:13:58.601133Z</date>
</commit>
</wc-status>
</entry>
<entry
   path="new.txt">
<wc-status
   props="none"
   item="added">
</wc-status>
</entry>
<entry
   path="removed.txt">
<wc-status
   props="none"
   item="deleted">
</wc-status>
</entry>
<entry
   path="replaced.txt">
<wc-status
   props="none"
   item="replaced">
</wc-status>
</entry>
</changelist>
</status>"#;
        let entries = status(xml, Path::new(ROOT)).unwrap();
        let find = |rel: &str| entries.iter().find(|e| e.rel_path == rel).unwrap();
        assert_eq!(find("b.txt").kind, Some(ChangeKind::Untracked));
        assert_eq!(find("gone.txt").kind, Some(ChangeKind::Deleted));
        assert_eq!(find("props.txt").kind, Some(ChangeKind::Modified));
        assert_eq!(find("conflict.txt").kind, Some(ChangeKind::Conflicted));
        assert_eq!(find("obstructed.txt").kind, Some(ChangeKind::Conflicted));
        assert_eq!(find("a.txt").kind, Some(ChangeKind::Modified));
        assert_eq!(find("new.txt").kind, Some(ChangeKind::Added));
        assert_eq!(find("removed.txt").kind, Some(ChangeKind::Deleted));
        assert_eq!(find("replaced.txt").kind, Some(ChangeKind::Modified));
        assert_eq!(find("new.txt").path, Path::new(ROOT).join("new.txt"));
        assert!(entries.iter().all(|e| !e.rel_path.contains('\\')));
        assert!(entries.iter().find(|e| e.rel_path == "external-dir").is_none(), "externals are skipped");
        assert!(entries.iter().find(|e| e.rel_path == "clean.txt").is_none(), "unchanged files are not listed");
    }

    #[test]
    fn parses_absolute_target_spellings() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<status>
<target
   path="C:\wc\sub">
<entry
   path="C:\wc\sub\page.html">
<wc-status
   props="none"
   item="modified"
   revision="4">
</wc-status>
</entry>
</target>
</status>"#;
        let entries = status(xml, Path::new(ROOT)).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].rel_path, "sub/page.html");
        assert_eq!(entries[0].path, Path::new(ROOT).join("sub/page.html"));
    }

    #[test]
    fn parses_log_entries_with_changed_paths() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<log>
<logentry
   revision="2">
<author>dev</author>
<date>2026-11-24T16:20:00.000000Z</date>
<paths>
<path
   action="M"
   prop-mods="false"
   text-mods="true"
   kind="file">/trunk/a.txt</path>
<path
   action="R"
   copyfrom-path="/trunk/old.txt"
   copyfrom-rev="1"
   kind="file">/trunk/new.txt</path>
</paths>
<msg>fix the thing

with details</msg>
</logentry>
<logentry
   revision="1">
<author>writer</author>
<date>2026-11-24T16:13:58.601133Z</date>
<msg>init</msg>
</logentry>
</log>"#;
        let entries = log(xml).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].revision, 2);
        assert_eq!(entries[0].message, "fix the thing\n\nwith details");
        assert_eq!(entries[0].paths.len(), 2);
        assert_eq!(entries[0].paths[1], ('R', "/trunk/new.txt".to_owned(), Some("/trunk/old.txt".to_owned())));
        assert!(entries[1].paths.is_empty(), "no -v: no paths element");
        assert_eq!(entries[0].time_ms, Some(T2_MS));
        assert_eq!(entries[1].time_ms, Some(T1_MS));
    }

    #[test]
    fn parses_info() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<info>
<entry
   kind="dir"
   path="."
   revision="7">
<url>file:///repo/trunk</url>
<repository>
<root>file:///repo</root>
<uuid>3d9a2007-cfb3-1947-b86d-8fb7f118d501</uuid>
</repository>
<commit
   revision="7">
</commit>
</entry>
</info>"#;
        let info = info(xml).unwrap();
        assert_eq!(info.revision, 7);
        assert_eq!(info.url, "file:///repo/trunk");
        assert_eq!(info.repo_root, "file:///repo");
        assert_eq!(info.uuid, "3d9a2007-cfb3-1947-b86d-8fb7f118d501");
    }

    #[test]
    fn parses_blame_entries() {
        // The 1.14 shape, as `svn blame --xml -r BASE` prints it: the entry
        // carries only the line number, the nested `<commit>` the revision
        // (plus author and date).
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<blame>
<target
   path="a.txt">
<entry
   line-number="1">
<commit
   revision="1">
<author>writer</author>
<date>2026-11-24T16:13:58.601133Z</date>
</commit>
</entry>
<entry
   line-number="2">
<commit
   revision="3">
<date>2026-11-24T16:20:00.000000Z</date>
</commit>
</entry>
<entry
   line-number="3"
   revision="4">
</entry>
</target>
</blame>"#;
        let entries = blame(xml).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(
            entries[0],
            BlameEntry { line_number: 1, revision: 1, author: "writer".to_owned(), time_ms: Some(T1_MS) }
        );
        assert_eq!(entries[1].revision, 3);
        assert_eq!(entries[1].author, "", "anonymous local commits have no author element");
        assert_eq!(entries[1].time_ms, Some(T2_MS));
        assert_eq!(
            entries[2],
            BlameEntry { line_number: 3, revision: 4, author: String::new(), time_ms: None },
            "older spellings repeat the revision on the entry"
        );
    }

    #[test]
    fn parses_ls_entries() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<lists>
<list
   path="file:///repo/branches">
<entry
   kind="dir">
<name>my-feature</name>
<commit
   revision="3">
<author>writer</author>
<date>2026-11-24T16:20:00.000000Z</date>
</commit>
</entry>
<entry
   kind="file">
<name>README</name>
</entry>
</list>
</lists>"#;
        assert_eq!(
            ls(xml).unwrap(),
            vec![
                LsEntry { name: "my-feature".to_owned(), kind: "dir".to_owned() },
                LsEntry { name: "README".to_owned(), kind: "file".to_owned() },
            ]
        );
    }

    #[test]
    fn parses_summarize() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<diff>
<paths>
<path
   props="none"
   kind="file"
   item="modified">/trunk/a.txt</path>
<path
   props="none"
   kind="file"
   item="added">/trunk/c.txt</path>
</paths>
</diff>"#;
        let rows = summarize(xml).unwrap();
        assert_eq!(
            rows,
            vec![
                SummarizedPath { path: "/trunk/a.txt".into(), kind: ChangeKind::Modified },
                SummarizedPath { path: "/trunk/c.txt".into(), kind: ChangeKind::Added },
            ]
        );
    }

    #[test]
    fn splits_unified_sections_with_kinds_and_numstat() {
        let diff = "Index: added.txt\n===================================================================\n--- added.txt\t(revision 0)\n+++ added.txt\t(working copy)\n@@ -0,0 +1,2 @@\n+one\n+two\nIndex: mod.txt\n===================================================================\n--- mod.txt\t(revision 1)\n+++ mod.txt\t(working copy)\n@@ -1,1 +1,2 @@\n hello\n+world\n-old\nIndex: gone.txt\n===================================================================\n--- gone.txt\t(revision 1)\n+++ gone.txt\t(nonexistent)\n@@ -1,1 +0,0 @@\n-byeee\nIndex: props.txt\n===================================================================\n\nProperty changes on: props.txt\n___________________________________________________________________\nModified: svn:ignore\n## -0,0 +1,1 ##\n+*.log\n";
        let sections = unified_sections(diff.as_bytes());
        assert_eq!(sections.len(), 4);
        assert_eq!(sections[0].path, "added.txt");
        assert_eq!(sections[0].kind, ChangeKind::Added);
        assert_eq!((sections[0].added_lines, sections[0].deleted_lines), (2, 0));
        assert_eq!(sections[1].kind, ChangeKind::Modified);
        assert_eq!((sections[1].added_lines, sections[1].deleted_lines), (1, 1));
        assert_eq!(sections[2].kind, ChangeKind::Deleted);
        assert_eq!((sections[2].added_lines, sections[2].deleted_lines), (0, 1));
        assert_eq!(sections[3].kind, ChangeKind::Modified, "property-only change");
        assert_eq!((sections[3].added_lines, sections[3].deleted_lines), (0, 0));
        assert!(sections[0].text.starts_with(b"Index: added.txt"));
        assert!(sections[3].text.starts_with(b"Index: props.txt"), "sections are verbatim slices");
    }

    #[test]
    fn parses_iso_dates() {
        assert_eq!(time_ms("1970-01-01T00:00:00Z"), Some(0.0));
        assert_eq!(time_ms("2026-11-24T16:13:58.601133Z"), Some(T1_MS));
        assert_eq!(time_ms("2026-11-24T17:13:58.601133+01:00"), Some(T1_MS));
        assert_eq!(time_ms("2024-01-01T00:00:00Z"), Some(1_704_067_200_000.0));
        assert_eq!(time_ms("not-a-date"), None);
    }

    #[test]
    fn parses_line_hunks_from_a_unified_diff() {
        // Modified file, several runs inside one hunk separated by context.
        let diff = "Index: a.txt\n===================================================================\n--- a.txt\t(revision 1)\n+++ a.txt\t(working copy)\n@@ -1,5 +1,6 @@\n first\n-old second\n+NEW second\n third\n fourth\n+fifth\n sixth\n";
        let hunks = diff_line_hunks(diff);
        assert_eq!(hunks.len(), 2, "{hunks:?}");
        assert_eq!(hunks[0].kind, HunkKind::Modified);
        assert_eq!((hunks[0].start, hunks[0].lines), (1, 1));
        assert_eq!((hunks[0].old_start, hunks[0].old_lines), (1, 1));
        assert_eq!(hunks[0].old_text, "old second\n");
        assert_eq!(hunks[1].kind, HunkKind::Added);
        assert_eq!((hunks[1].start, hunks[1].lines), (4, 1));
        assert_eq!((hunks[1].old_start, hunks[1].old_lines), (4, 0));
        assert_eq!(hunks[1].old_text, "");

        // Pure deletions: `lines == 0`, `start` = the line of the current text
        // that follows the removed block (the file's line count at the end).
        let diff = "Index: a.txt\n===================================================================\n--- a.txt\t(revision 1)\n+++ a.txt\t(working copy)\n@@ -2,3 +1,2 @@\n two\n-three\n four\n-fifth\n";
        let hunks = diff_line_hunks(diff);
        assert_eq!(hunks.len(), 2, "{hunks:?}");
        assert_eq!(hunks[0].kind, HunkKind::Deleted);
        assert_eq!((hunks[0].start, hunks[0].lines), (1, 0));
        assert_eq!((hunks[0].old_start, hunks[0].old_lines), (2, 1));
        assert_eq!(hunks[0].old_text, "three\n");
        assert_eq!(hunks[1].kind, HunkKind::Deleted);
        assert_eq!((hunks[1].start, hunks[1].lines), (2, 0), "start equals the new text's line count");
        assert_eq!(hunks[1].old_text, "fifth\n");

        // Scheduled add: the whole file is added from `(nonexistent)`.
        let diff = "Index: b.txt\n===================================================================\n--- b.txt\t(nonexistent)\n+++ b.txt\t(working copy)\n@@ -0,0 +1,2 @@\n+one\n+two\n";
        let hunks = diff_line_hunks(diff);
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].kind, HunkKind::Added);
        assert_eq!((hunks[0].start, hunks[0].lines), (0, 2));

        // Property changes after the hunks add no line hunks.
        let diff = "Index: c.txt\n===================================================================\n--- c.txt\t(revision 1)\n+++ c.txt\t(working copy)\n@@ -1 +1 @@\n-old\n+new\n\nProperty changes on: c.txt\n___________________________________________________________________\nModified: svn:ignore\n## -0,0 +1,1 ##\n+*.log\n";
        assert_eq!(diff_line_hunks(diff).len(), 1);
    }
}
