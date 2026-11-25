//! End-to-end SVN workflows as the IDE drives them, mirroring
//! `git_workflows.rs`: a throwaway repository created with `svnadmin` and a
//! checkout in a temp dir. The executables come from the same detection the
//! product itself uses ([`ide_vcs::svn_executable`]: `OPENIDEA_SVN` /
//! `OPENIDEA_SVNADMIN`, then `PATH`, then well-known install locations); when
//! none is found the tests print a hint and skip, they do not fail.

use ide_core::IpcPath;
use ide_vcs::{Backend, ChangeKind, CommitOptions, HunkKind, LogQuery, SvnWorkingCopy, TextEncoding};
use std::path::{Path, PathBuf};
use std::process::Command;

fn svn_exe() -> Option<PathBuf> {
    ide_vcs::svn_executable()
}

fn svnadmin_exe() -> Option<PathBuf> {
    ide_vcs::svnadmin_executable()
}

fn utf8() -> TextEncoding {
    TextEncoding::from(encoding_rs::UTF_8)
}

/// Percent-encodes a path into a `file://` URL (the temp dir can hold
/// non-ASCII user names and spaces).
fn file_url(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let mut url = String::from("file:///");
    for byte in text.trim_start_matches('/').as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' | b':' => {
                url.push(*byte as char)
            }
            other => url.push_str(&format!("%{other:02X}")),
        }
    }
    url
}

struct Fixture {
    tmp: tempfile::TempDir,
    repo_url: String,
    dir: PathBuf,
}

impl Fixture {
    /// A repository (created with `svnadmin`) plus a checkout of its trunk,
    /// holding one revision with `a.txt` versioned. `None` skips the test.
    fn new() -> Option<Self> {
        let (Some(svn), Some(svnadmin)) = (svn_exe(), svnadmin_exe()) else {
            eprintln!(
                "skipping: install the Subversion command-line tools (or set OPENIDEA_SVN / OPENIDEA_SVNADMIN) to run the SVN workflows"
            );
            return None;
        };
        let svn = svn.to_string_lossy().into_owned();
        let svnadmin = svnadmin.to_string_lossy().into_owned();
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        run(tmp.path(), &svnadmin, &["create", &repo.to_string_lossy()]);
        let repo_url = file_url(&repo);
        let dir = tmp.path().join("wc");
        run(tmp.path(), &svn, &["checkout", "--non-interactive", "-q", &repo_url, &dir.to_string_lossy()]);
        let fx = Fixture { tmp, repo_url, dir };
        fx.write("a.txt", "one\n");
        fx.write("b.txt", "two\n");
        run(&fx.dir, &svn, &["add", "-q", "a.txt", "b.txt"]);
        run(&fx.dir, &svn, &["commit", "--non-interactive", "-q", "-m", "initial"]);
        // A commit never moves the working-copy root's own revision; without
        // the update the root sits at r0 and `svn log` in it sees no history.
        run(&fx.dir, &svn, &["update", "-q"]);
        Some(fx)
    }

    /// A second checkout of the same repository.
    fn second_checkout(&self) -> PathBuf {
        let other = self.tmp.path().join("wc-other");
        run(
            self.tmp.path(),
            &svn_exe().unwrap().to_string_lossy(),
            &["checkout", "--non-interactive", "-q", &self.repo_url, &other.to_string_lossy()],
        );
        other
    }

    fn write(&self, rel: &str, content: &str) {
        let p = self.dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.dir.join(rel)).unwrap()
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.join(rel)
    }

    /// The working copy as the backend sees it.
    fn wc(&self) -> std::sync::Arc<SvnWorkingCopy> {
        SvnWorkingCopy::discover(&self.dir).unwrap().expect("working copy discovered")
    }
}

fn run(dir: &Path, program: &str, args: &[&str]) {
    let out = Command::new(program).args(args).current_dir(dir).output().expect(program);
    assert!(out.status.success(), "{program} {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

fn select(paths: &[PathBuf]) -> CommitOptions {
    CommitOptions {
        amend: false,
        amend_target: None,
        sign_off: false,
        author: None,
        author_date: None,
        run_hooks: true,
        partial: Vec::new(),
        stage: paths.iter().map(|p| IpcPath(p.clone())).collect(),
        exclude: Vec::new(),
    }
}

/// a) A clean checkout has no working changes.
#[test]
fn clean_checkout_has_empty_status() {
    let Some(fx) = Fixture::new() else { return };
    let wc = fx.wc();
    assert!(Backend::status(&*wc).unwrap().is_empty());
    assert_eq!(wc.revision().unwrap(), 1);
    assert_eq!(wc.uuid().unwrap().len(), 36);
    assert!(wc.repo_url().unwrap().ends_with("/repo"));
}

/// b) Modify, add and delete files; status classifies each.
#[test]
fn status_classifies_modifications_adds_and_deletes() {
    let Some(fx) = Fixture::new() else { return };
    let wc = fx.wc();
    fx.write("a.txt", "changed\n");
    fx.write("new.txt", "brand new\n");
    std::fs::remove_file(fx.path("b.txt")).unwrap();

    let status = Backend::status(&*wc).unwrap();
    let kind_of = |rel: &str| {
        status
            .iter()
            .find(|f| f.rel_path == rel)
            .map(|f| (f.staged, f.unstaged))
            .unwrap_or_else(|| panic!("{rel} not in {status:?}"))
    };
    assert_eq!(kind_of("a.txt"), (None, Some(ChangeKind::Modified)));
    // b.txt is versioned and missing from disk: a deletion the commit must be
    // told about.
    assert_eq!(kind_of("b.txt"), (None, Some(ChangeKind::Deleted)));
    assert_eq!(kind_of("new.txt"), (None, Some(ChangeKind::Untracked)));
    assert!(!status.iter().any(|f| f.staged.is_some()), "svn has no index: {status:?}");

    // status_paths answers only the asked paths.
    let status = Backend::status_paths(&*wc, &[fx.path("a.txt"), fx.path("new.txt")]).unwrap();
    assert_eq!(status.len(), 2, "{status:?}");
    assert!(status.iter().all(|f| f.rel_path != "b.txt"));
    // Paths outside the working copy are ignored.
    assert!(Backend::status_paths(&*wc, &[PathBuf::from("Z:/elsewhere/x")]).unwrap().is_empty());
}

/// c) diff_text reflects the working change against BASE.
#[test]
fn diff_text_reflects_the_modification() {
    let Some(fx) = Fixture::new() else { return };
    let wc = fx.wc();
    fx.write("a.txt", "ONE\ntwo\n");
    let diff = Backend::diff_text(&*wc, &fx.path("a.txt"), false, utf8()).unwrap();
    assert!(diff.contains("Index: a.txt"), "{diff}");
    assert!(diff.contains("-one"), "{diff}");
    assert!(diff.contains("+ONE"), "{diff}");
    assert!(diff.contains("+two"), "{diff}");
    // Unchanged files diff to nothing.
    fx.write("a.txt", "one\n");
    assert_eq!(Backend::diff_text(&*wc, &fx.path("a.txt"), false, utf8()).unwrap(), "");
    // Unversioned files have no base side.
    fx.write("scratch.txt", "x\n");
    assert_eq!(Backend::diff_text(&*wc, &fx.path("scratch.txt"), false, utf8()).unwrap(), "");
}

/// d) Stage a new file (svn add), commit it; the log shows the new revision
/// and the commit's changed files.
#[test]
fn stage_commit_and_log_show_the_new_revision() {
    let Some(fx) = Fixture::new() else { return };
    let wc = fx.wc();
    fx.write("feature.txt", "feature\n");
    fx.write("a.txt", "one\ntwo\n");

    Backend::stage(&*wc, &[fx.path("feature.txt")]).unwrap();
    let status = Backend::status(&*wc).unwrap();
    let staged = status.iter().find(|f| f.rel_path == "feature.txt").unwrap();
    assert_eq!(staged.unstaged, Some(ChangeKind::Added), "scheduled add");
    assert_eq!(staged.staged, None);

    let committed = Backend::commit(
        &*wc,
        "add feature\n\nwith a body",
        &select(&[fx.path("feature.txt"), fx.path("a.txt")]),
        &|_| Ok(utf8()),
    )
    .unwrap();
    assert!(committed.starts_with('r') && committed != "r1", "{committed}");

    let cancel = ide_core::CancellationToken::new();
    let log = Backend::log(&*wc, &LogQuery { limit: 10, ..LogQuery::default() }, &cancel).unwrap();
    assert_eq!(log.len(), 2, "{log:?}");
    assert_eq!(log[0].id, committed, "newest first");
    assert_eq!(log[0].summary, "add feature");
    assert_eq!(log[0].parents, vec![log[1].id.clone()], "linear history");
    assert_eq!(log[1].summary, "initial");
    assert_eq!(log[1].parents, Vec::<String>::new());
    assert!(log.iter().all(|c| c.refs.is_empty()));

    let files = Backend::commit_files(&*wc, &committed).unwrap();
    assert_eq!(files.len(), 2, "{files:?}");
    assert!(files.iter().any(|f| f.path == "feature.txt" && f.kind == ChangeKind::Added));
    assert!(files.iter().any(|f| f.path == "a.txt" && f.kind == ChangeKind::Modified));
    // The root's revision only moves on update, never on commit — but the
    // update must see the new HEAD.
    wc.update(&cancel).unwrap();
    assert_eq!(wc.revision().unwrap(), 2, "info cache refreshed");
}

/// e) Discard (svn revert) restores a modified file to BASE and drops
/// unversioned ones.
#[test]
fn discard_reverts_modifications_and_untracked_files() {
    let Some(fx) = Fixture::new() else { return };
    let wc = fx.wc();
    fx.write("a.txt", "mangled\n");
    fx.write("scratch.txt", "scratch\n");

    Backend::discard(&*wc, &[fx.path("a.txt"), fx.path("scratch.txt")]).unwrap();
    assert_eq!(fx.read("a.txt"), "one\n", "modified file back to BASE");
    assert!(!fx.path("scratch.txt").exists(), "unversioned file deleted");
    assert!(Backend::status(&*wc).unwrap().is_empty());
}

/// f) A second checkout commits; this one's update picks the new revision up.
#[test]
fn update_after_another_checkout_committed() {
    let Some(fx) = Fixture::new() else { return };
    let other = fx.second_checkout();
    std::fs::write(other.join("a.txt"), "from elsewhere\n").unwrap();
    run(&other, &svn_exe().unwrap().to_string_lossy(), &["commit", "--non-interactive", "-q", "-m", "remote change"]);

    let wc = fx.wc();
    assert_eq!(wc.revision().unwrap(), 1, "not updated yet");
    let cancel = ide_core::CancellationToken::new();
    let printed = wc.update(&cancel).unwrap();
    assert!(printed.contains("a.txt"), "svn update prints the updated file: {printed}");
    assert_eq!(wc.revision().unwrap(), 2);
    assert_eq!(fx.read("a.txt"), "from elsewhere\n");
    assert!(Backend::status(&*wc).unwrap().is_empty());

    let log = Backend::log(&*wc, &LogQuery { limit: 5, ..LogQuery::default() }, &cancel).unwrap();
    assert_eq!(log[0].summary, "remote change");
}

/// The gutter must tell "unchanged" from "unversioned": `svn diff` is empty
/// for both, but only an unversioned file has no base side. Regression for
/// unchanged versioned files rendering as one whole-file Added hunk.
#[test]
fn line_hunks_distinguish_unchanged_from_unversioned() {
    let Some(fx) = Fixture::new() else { return };
    let wc = fx.wc();
    let encoding = utf8();

    // Versioned file without edits: no hunks at all.
    let hunks = Backend::line_hunks(&*wc, &fx.path("a.txt"), &fx.read("a.txt"), false, encoding).unwrap();
    assert!(hunks.is_empty(), "unchanged versioned file must have no hunks: {hunks:?}");

    // After an edit the hunks cover exactly the changed lines (0-based start).
    fx.write("a.txt", "one\ntwo\n");
    let hunks = Backend::line_hunks(&*wc, &fx.path("a.txt"), &fx.read("a.txt"), false, encoding).unwrap();
    assert_eq!(hunks.len(), 1, "one hunk for the edited lines: {hunks:?}");
    assert_eq!(hunks[0].start, 1, "0-based line of the added 'two'");
    assert_eq!(hunks[0].lines, 1);

    // An unversioned file has no base side: one Added hunk spanning it.
    fx.write("brand-new.txt", "brand new\n");
    let hunks =
        Backend::line_hunks(&*wc, &fx.path("brand-new.txt"), &fx.read("brand-new.txt"), false, encoding).unwrap();
    assert_eq!(hunks.len(), 1, "unversioned file renders as one Added hunk: {hunks:?}");
    assert!(matches!(hunks[0].kind, HunkKind::Added));
    assert_eq!(hunks[0].lines, 1);
}

/// g) The IDE file hooks schedule real operations: files_added opens for add,
/// file_moved records delete + add of the committed file, files_deleted
/// schedules the deletion — and everything commits cleanly.
#[test]
fn ide_file_hooks_schedule_changes() {
    let Some(fx) = Fixture::new() else { return };
    let wc = fx.wc();

    // New file from the project tree.
    fx.write("created.txt", "created\n");
    Backend::files_added(&*wc, &[fx.path("created.txt")]).unwrap();
    let status = Backend::status(&*wc).unwrap();
    assert_eq!(status.iter().find(|f| f.rel_path == "created.txt").unwrap().unstaged, Some(ChangeKind::Added));

    // A rename through the project tree: the old path was committed, so it
    // reports as a deletion (a never-committed file would just lose its
    // uncommitted add instead).
    std::fs::rename(fx.path("b.txt"), fx.path("moved.txt")).unwrap();
    Backend::file_moved(&*wc, &fx.path("b.txt"), &fx.path("moved.txt")).unwrap();
    let status = Backend::status(&*wc).unwrap();
    assert_eq!(status.iter().find(|f| f.rel_path == "b.txt").unwrap().unstaged, Some(ChangeKind::Deleted));
    assert_eq!(status.iter().find(|f| f.rel_path == "moved.txt").unwrap().unstaged, Some(ChangeKind::Added));

    // Everything scheduled here commits cleanly.
    let committed = Backend::commit(
        &*wc,
        "hooks",
        &select(&[fx.path("b.txt"), fx.path("created.txt"), fx.path("moved.txt")]),
        &|_| Ok(utf8()),
    )
    .unwrap();
    assert!(committed.starts_with('r'), "{committed}");
    assert!(Backend::status(&*wc).unwrap().is_empty());

    // A deletion through the project tree, after the file is versioned.
    std::fs::remove_file(fx.path("moved.txt")).unwrap();
    Backend::files_deleted(&*wc, &[fx.path("moved.txt")]).unwrap();
    let status = Backend::status(&*wc).unwrap();
    assert_eq!(status.iter().find(|f| f.rel_path == "moved.txt").unwrap().unstaged, Some(ChangeKind::Deleted));
    Backend::commit(&*wc, "drop moved", &select(&[fx.path("moved.txt")]), &|_| Ok(utf8())).unwrap();
    assert!(Backend::status(&*wc).unwrap().is_empty());
}

/// Gutter hunks come from the parsed single-file diff.
#[test]
fn line_hunks_mark_changed_lines() {
    let Some(fx) = Fixture::new() else { return };
    let wc = fx.wc();
    fx.write("a.txt", "one\ntwo\n");
    let hunks = Backend::line_hunks(&*wc, &fx.path("a.txt"), "one\ntwo\n", false, utf8()).unwrap();
    assert_eq!(hunks.len(), 1);
    assert_eq!(hunks[0].kind, HunkKind::Added);
    assert_eq!((hunks[0].start, hunks[0].lines), (1, 1));
    assert_eq!(hunks[0].old_text, "");

    // Scheduled add: no base side, one whole-file hunk from the diff.
    fx.write("whole.txt", "all\nnew\n");
    Backend::stage(&*wc, &[fx.path("whole.txt")]).unwrap();
    let hunks = Backend::line_hunks(&*wc, &fx.path("whole.txt"), "all\nnew\n", false, utf8()).unwrap();
    assert_eq!(hunks.len(), 1);
    assert_eq!((hunks[0].kind, hunks[0].start, hunks[0].lines), (HunkKind::Added, 0, 2));
}
