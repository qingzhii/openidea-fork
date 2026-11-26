//! End-to-end SVN workflows as the IDE drives them, mirroring
//! `git_workflows.rs`: a throwaway repository created with `svnadmin` and a
//! checkout in a temp dir. The executables come from the same detection the
//! product itself uses ([`ide_vcs::svn_executable`]: `OPENIDEA_SVN` /
//! `OPENIDEA_SVNADMIN`, then `PATH`, then well-known install locations); when
//! none is found the tests print a hint and skip, they do not fail.

use ide_core::IpcPath;
use ide_vcs::{Backend, ChangeKind, CommitOptions, HunkKind, LogQuery, SvnResolveAccept, SvnWorkingCopy, TextEncoding};
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

    // The details panel diffs a commit against its first parent; the parent
    // must be deterministic even when the log page starts at the newest
    // commit (regression: page-adjacent parents left the newest row without
    // one, so the panel showed no changed files).
    let paged = Backend::log(&*wc, &LogQuery { limit: 1, ..LogQuery::default() }, &cancel).unwrap();
    assert_eq!(paged.len(), 1);
    assert_eq!(paged[0].parents, vec![log[1].id.clone()], "parent across page boundary");
    let files = Backend::diff_revisions(&*wc, &paged[0].parents[0], &paged[0].id).unwrap();
    assert_eq!(files.len(), 2, "diff against the first parent lists the committed files: {files:?}");

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

    // The gutter follows the LIVE buffer, not the disk file: with the disk
    // still at BASE ("one\n"), an unsaved buffer edit adds one line.
    let hunks = Backend::line_hunks(&*wc, &fx.path("a.txt"), "one\nedited unsaved\n", false, encoding).unwrap();
    assert_eq!(hunks.len(), 1, "unsaved buffer edits mark the changed line: {hunks:?}");
    assert_eq!(hunks[0].start, 1);
    assert_eq!(hunks[0].lines, 1);
    assert!(matches!(hunks[0].kind, HunkKind::Added), "{hunks:?}");
    assert!(hunks[0].old_text.is_empty(), "nothing was committed for that line: {hunks:?}");
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

/// h) blame attributes each line to the commit that last changed it; the
/// open buffer's uncommitted lines carry an all-zero commit ("Not Committed
/// Yet"), including mid-file edits — not the neighbouring line's old commit.
#[test]
fn blame_maps_lines_to_revisions_and_marks_uncommitted_edits() {
    let Some(fx) = Fixture::new() else { return };
    let wc = fx.wc();
    // r2 appends two lines; r3 then rewrites only the middle one — blame must
    // keep r1 for the untouched first line.
    fx.write("a.txt", "one\ntwo\nthree\n");
    Backend::commit(&*wc, "add lines", &select(&[fx.path("a.txt")]), &|_| Ok(utf8())).unwrap();
    fx.write("a.txt", "one\nTWO\nthree\n");
    let r3 = Backend::commit(&*wc, "retitle", &select(&[fx.path("a.txt")]), &|_| Ok(utf8())).unwrap();

    let blame = Backend::blame(&*wc, &fx.path("a.txt"), None, None, utf8()).unwrap();
    assert_eq!(blame.len(), 3, "{blame:?}");
    assert_eq!(blame[0].commit, "r1", "line 1 is untouched since r1");
    assert_eq!(blame[1].commit, r3, "line 2 was rewritten by the last commit");
    assert_eq!(blame[2].commit, "r2", "line 3 was added by r2");

    // The author and date come from the commit, like the Log shows them.
    let cancel = ide_core::CancellationToken::new();
    let log = Backend::log(&*wc, &LogQuery { limit: 10, ..LogQuery::default() }, &cancel).unwrap();
    let r3_commit = log.iter().find(|c| c.id == r3).unwrap();
    assert_eq!(blame[1].author, r3_commit.author);
    assert_eq!(blame[1].time_ms, r3_commit.time_ms);

    // Two buffer lines BASE does not have: uncommitted, all-zero commits.
    let edited = format!("{}four\nfive\n", fx.read("a.txt"));
    let blame = Backend::blame(&*wc, &fx.path("a.txt"), Some(&edited), None, utf8()).unwrap();
    assert_eq!(blame.len(), 5, "{blame:?}");
    assert_eq!(blame[3].author, "Not Committed Yet");
    assert!(blame[3..5].iter().all(|line| line.commit.bytes().all(|b| b == b'0')), "{blame:?}");

    // A mid-file edit is uncommitted, not attributed to an old line's commit;
    // the untouched lines keep their revisions.
    let blame = Backend::blame(&*wc, &fx.path("a.txt"), Some("one\nTWO!\nthree\n"), None, utf8()).unwrap();
    assert_eq!(blame.len(), 3, "{blame:?}");
    assert!(blame[1].commit.bytes().all(|b| b == b'0'), "edited line 2 must be uncommitted: {blame:?}");
    assert_eq!(blame[0].commit, "r1");
    assert_eq!(blame[2].commit, "r2");

    // An unversioned file has nothing committed at all.
    fx.write("scratch.txt", "brand\nnew\n");
    let blame = Backend::blame(&*wc, &fx.path("scratch.txt"), None, None, utf8()).unwrap();
    assert_eq!(blame.len(), 2, "{blame:?}");
    assert!(blame.iter().all(|line| line.commit.bytes().all(|b| b == b'0')));
}

/// i) A `branch` naming an svn revision pins the walk: one commit plus its
/// parent (the Annotate popup's previous-revision hop). Non-revision values
/// keep the default walk.
#[test]
fn log_pins_a_branch_revision_with_its_parent() {
    let Some(fx) = Fixture::new() else { return };
    let wc = fx.wc();
    fx.write("a.txt", "one\ntwo\n");
    let r2 = Backend::commit(&*wc, "second", &select(&[fx.path("a.txt")]), &|_| Ok(utf8())).unwrap();
    let cancel = ide_core::CancellationToken::new();

    let pinned =
        Backend::log(&*wc, &LogQuery { limit: 1, branch: Some("r1".into()), ..LogQuery::default() }, &cancel).unwrap();
    assert_eq!(pinned.len(), 1, "{pinned:?}");
    assert_eq!(pinned[0].id, "r1");
    assert!(pinned[0].parents.is_empty(), "r1 is the root: {pinned:?}");

    let pinned =
        Backend::log(&*wc, &LogQuery { limit: 1, branch: Some(r2.clone()), ..LogQuery::default() }, &cancel).unwrap();
    assert_eq!(pinned[0].id, r2);
    assert_eq!(pinned[0].parents, vec!["r1".to_owned()], "the previous revision, for Annotate");

    // A limit above one walks down from the pinned revision; the page itself
    // provides the parents then.
    let page =
        Backend::log(&*wc, &LogQuery { limit: 10, branch: Some("r2".into()), ..LogQuery::default() }, &cancel).unwrap();
    assert_eq!(page.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), vec![r2.as_str(), "r1"]);
    assert_eq!(page[0].parents, vec!["r1".to_owned()]);

    // `main` has no svn meaning: the default walk answers.
    let default =
        Backend::log(&*wc, &LogQuery { limit: 10, branch: Some("main".into()), ..LogQuery::default() }, &cancel)
            .unwrap();
    assert_eq!(default.len(), 2, "{default:?}");
}

/// A conflicting update leaves the file Conflicted with its `.mine`/`.rN`
/// side files; `svn resolve --accept` picks a side, removes the side files
/// and clears the state. Resolving a path without a conflict is tolerated.
#[test]
fn resolve_accepts_each_side_and_clears_the_conflict() {
    let Some(fx) = Fixture::new() else { return };
    let wc = fx.wc();
    let other = fx.second_checkout();

    // One iteration of "both sides edit the same lines, the other commits,
    // this working copy updates into a conflict".
    let reconflict = |local: &str, remote: &str, message: &str| {
        fx.write("a.txt", local);
        std::fs::write(other.join("a.txt"), remote).unwrap();
        run(&other, &svn_exe().unwrap().to_string_lossy(), &["commit", "--non-interactive", "-q", "-m", message]);
        run(&fx.dir, &svn_exe().unwrap().to_string_lossy(), &["update", "--non-interactive"]);
        let status = Backend::status(&*wc).unwrap();
        assert_eq!(
            status.iter().find(|f| f.rel_path == "a.txt").unwrap().unstaged,
            Some(ChangeKind::Conflicted),
            "{status:?}"
        );
    };
    let assert_resolved = |expect: &str| {
        // After `theirs-full` the file can match BASE exactly and drop out of
        // status entirely; absence is just as resolved.
        let kind = Backend::status(&*wc).unwrap().into_iter().find(|f| f.rel_path == "a.txt").map(|f| f.unstaged);
        assert_ne!(kind, Some(Some(ChangeKind::Conflicted)), "conflict state cleared");
        assert_eq!(fx.read("a.txt"), expect);
        for suffix in ["mine", "r1", "r2", "r3", "r4", "r5"] {
            assert!(!fx.path(&format!("a.txt.{suffix}")).exists(), "side file .{suffix} removed with the conflict");
        }
    };

    // `working`: the user edits the conflict markers away, then the resolve
    // accepts the file byte-for-byte as it stands.
    reconflict("local edit\n", "remote edit\n", "from elsewhere");
    fx.write("a.txt", "local edit\n");
    wc.resolve(&[fx.path("a.txt")], SvnResolveAccept::Working).unwrap();
    assert_resolved("local edit\n");

    // `mine-full` / `theirs-full`: pre-update content vs the incoming revision.
    reconflict("mine 2\n", "theirs 2\n", "from elsewhere 2");
    wc.resolve(&[fx.path("a.txt")], SvnResolveAccept::MineFull).unwrap();
    assert_resolved("mine 2\n");

    reconflict("mine 3\n", "theirs 3\n", "from elsewhere 3");
    wc.resolve(&[fx.path("a.txt")], SvnResolveAccept::TheirsFull).unwrap();
    assert_resolved("theirs 3\n");

    // A path without a conflict is skipped with a warning, not an error.
    wc.resolve(&[fx.path("b.txt")], SvnResolveAccept::Working).unwrap();
}

/// j) Add to svn:ignore: the name merges into the parent's `svn:ignore`
/// (LF-separated, no duplicates, CRLF values from other tools survive), the
/// file disappears from status, versioned and missing paths are refused, and
/// each parent directory keeps its own value.
#[test]
fn add_to_ignore_merges_names_into_the_parent_properties() {
    let Some(fx) = Fixture::new() else { return };
    let wc = fx.wc();
    let svn = svn_exe().unwrap().to_string_lossy().into_owned();
    let propget = |target: &str| {
        let out = Command::new(&svn)
            .args(["propget", "--non-interactive", "svn:ignore", target])
            .current_dir(&fx.dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "propget {target}: {}", String::from_utf8_lossy(&out.stderr));
        // Windows propget prints CRLF and pads a trailing newline.
        String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n").trim_end().to_owned()
    };

    fx.write("build.log", "noise\n");
    let status = Backend::status(&*wc).unwrap();
    assert_eq!(status.iter().find(|f| f.rel_path == "build.log").unwrap().unstaged, Some(ChangeKind::Untracked));

    wc.add_to_ignore(&[fx.path("build.log")]).unwrap();
    assert_eq!(propget(".").lines().collect::<Vec<_>>(), vec!["build.log"]);
    assert!(
        !Backend::status(&*wc).unwrap().iter().any(|f| f.rel_path == "build.log"),
        "ignored entries are not listed anymore"
    );

    // Idempotent, and a CRLF value another tool wrote keeps its entries.
    run(&fx.dir, &svn, &["propset", "--non-interactive", "svn:ignore", "old\r\nbuild.log\r\n", "."]);
    wc.add_to_ignore(&[fx.path("build.log")]).unwrap();
    assert_eq!(propget(".").lines().collect::<Vec<_>>(), vec!["old", "build.log"], "merged, deduplicated, LF only");

    // Versioned and missing paths are refused.
    assert!(wc.add_to_ignore(&[fx.path("a.txt")]).is_err(), "versioned files cannot be ignored");
    assert!(wc.add_to_ignore(&[fx.path("ghost.txt")]).is_err(), "missing paths cannot be ignored");

    // A nested directory gets its own value; the parent must be versioned
    // (svn add schedules children too, so the directory goes in first).
    std::fs::create_dir(fx.path("sub")).unwrap();
    run(&fx.dir, &svn, &["add", "-q", "sub"]);
    run(&fx.dir, &svn, &["commit", "--non-interactive", "-q", "-m", "add the sub directory"]);
    fx.write("sub/inner.log", "x\n");
    wc.add_to_ignore(&[fx.path("sub/inner.log")]).unwrap();
    assert_eq!(propget("sub").lines().collect::<Vec<_>>(), vec!["inner.log"], "each parent writes its own value");
    assert_eq!(propget(".").lines().collect::<Vec<_>>(), vec!["old", "build.log"], "root value untouched");

    // An unversioned parent has no property to write: a clean error, not a
    // silent half-write.
    std::fs::create_dir(fx.path("plain")).unwrap();
    fx.write("plain/thing.txt", "x\n");
    assert!(wc.add_to_ignore(&[fx.path("plain/thing.txt")]).is_err(), "svn:ignore needs a versioned parent");
}

/// k) The branch layout: `/branches` and `/tags` read as empty while missing;
/// copy_branch creates the branch (and its parent directories), switch moves
/// the working copy's URL and base, the current branch refuses deletion, and
/// deleting it for real removes it from the layout.
#[test]
fn copy_switch_commit_and_delete_branches() {
    let Some(fx) = Fixture::new() else { return };
    let wc = fx.wc();
    let cancel = ide_core::CancellationToken::new();

    // The fixture repository has no layout yet.
    let layout = wc.branches(&cancel).unwrap();
    assert!(layout.branches.is_empty() && layout.tags.is_empty(), "{layout:?}");

    let branch_url = wc.copy_branch("my-branch", "create the branch", false, &cancel).unwrap();
    assert!(branch_url.ends_with("/branches/my-branch"), "{branch_url}");
    let layout = wc.branches(&cancel).unwrap();
    assert_eq!(layout.branches.iter().map(|b| b.name.as_str()).collect::<Vec<_>>(), vec!["my-branch"]);
    assert!(layout.tags.is_empty(), "no /tags in this repository");

    // Switch to the branch, commit a file only it has…
    wc.switch_to(&branch_url, &cancel).unwrap();
    assert!(wc.url().unwrap().ends_with("/branches/my-branch"), "{}", wc.url().unwrap());
    fx.write("branch.txt", "on the branch\n");
    Backend::stage(&*wc, &[fx.path("branch.txt")]).unwrap();
    Backend::commit(&*wc, "branch work", &select(&[fx.path("branch.txt")]), &|_| Ok(utf8())).unwrap();

    // …and deleting the branch the working copy sits on is refused.
    assert!(wc.delete_branch("my-branch", "bye", &cancel).is_err(), "the current branch cannot be deleted");

    // Back on the repository root: the branch-only file is gone, status clean.
    wc.switch_to(&fx.repo_url, &cancel).unwrap();
    assert!(!fx.path("branch.txt").exists());
    assert!(Backend::status(&*wc).unwrap().is_empty(), "{:?}", Backend::status(&*wc).unwrap());
    assert_eq!(Backend::file_at(&*wc, &fx.path("a.txt"), None, utf8()).unwrap().as_deref(), Some("one\n"));

    wc.delete_branch("my-branch", "bye", &cancel).unwrap();
    let layout = wc.branches(&cancel).unwrap();
    assert!(layout.branches.is_empty(), "{layout:?}");
}

/// l) Get from VCS: `svn::checkout` fills a fresh directory (files land, the
/// working copy discovers with the right URL and UUID), refuses a non-empty
/// destination without touching it, and a cancelled run leaves no
/// half-checked-out directory behind.
#[test]
fn checkout_fills_a_directory_and_cleans_up_after_a_cancelled_run() {
    let Some(fx) = Fixture::new() else { return };

    let dest = fx.tmp.path().join("fresh-wc");
    let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let sink = lines.clone();
    let cancel = ide_core::CancellationToken::new();
    ide_vcs::svn::checkout(&fx.repo_url, &dest, &cancel, &move |line| sink.lock().unwrap().push(line)).unwrap();
    assert!(dest.join(".svn").join("wc.db").is_file());
    assert_eq!(std::fs::read_to_string(dest.join("a.txt")).unwrap(), "one\n");
    assert!(!lines.lock().unwrap().is_empty(), "the CLI's progress lines are streamed: {:?}", lines.lock().unwrap());
    let wc = SvnWorkingCopy::discover(&dest).unwrap().expect("checked-out working copy discovered");
    assert!(wc.repo_url().unwrap().ends_with("/repo"), "{}", wc.repo_url().unwrap());
    assert_eq!(wc.uuid().unwrap().len(), 36);

    // A non-empty destination is refused before svn runs — svn itself would
    // merge the checkout into it.
    let occupied = fx.tmp.path().join("occupied");
    std::fs::create_dir(&occupied).unwrap();
    std::fs::write(occupied.join("keep.txt"), "keep\n").unwrap();
    assert!(ide_vcs::svn::checkout(&fx.repo_url, &occupied, &cancel, &|_| {}).is_err());
    assert_eq!(std::fs::read_to_string(occupied.join("keep.txt")).unwrap(), "keep\n", "existing content untouched");

    // A cancelled checkout does not leave a half-populated directory.
    let cancelled = fx.tmp.path().join("cancelled-wc");
    let cancel = ide_core::CancellationToken::new();
    cancel.cancel();
    assert!(ide_vcs::svn::checkout(&fx.repo_url, &cancelled, &cancel, &|_| {}).is_err());
    assert!(!cancelled.exists(), "no leftover directory: {}", cancelled.display());
}
