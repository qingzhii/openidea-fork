//! End-to-end tests against a real repository created with `svnadmin`, run in
//! a temporary checkout. Skipped when no svn client is installed — the parse
//! fixtures in [`super::parse`] cover the XML grammar without one.

use super::*;
use crate::{ChangeKind, HunkKind};
use std::process::Command;

fn svn_available() -> bool {
    cli::svn_executable().is_some() && cli::svnadmin_executable().is_some()
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

fn run(dir: &Path, program: &str, args: &[&str]) {
    let output = Command::new(program).args(args).current_dir(dir).output().expect(program);
    assert!(output.status.success(), "{program} {args:?}: {}", String::from_utf8_lossy(&output.stderr));
}

/// A checkout with one revision: `a.txt` and `src/main.rs` versioned.
fn fixture() -> Option<(tempfile::TempDir, PathBuf)> {
    if !svn_available() {
        return None;
    }
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    let wc = dir.path().join("wc");
    run(dir.path(), cli::svnadmin_executable().unwrap().to_str().unwrap(), &["create", &repo.to_string_lossy()]);
    let url = file_url(&repo);
    run(
        dir.path(),
        cli::svn_executable().unwrap().to_str().unwrap(),
        &["checkout", "--non-interactive", "-q", &url, &wc.to_string_lossy()],
    );
    std::fs::write(wc.join("a.txt"), "hello\n").unwrap();
    std::fs::create_dir(wc.join("src")).unwrap();
    std::fs::write(wc.join("src").join("main.rs"), "fn main() {}\n").unwrap();
    run(&wc, cli::svn_executable().unwrap().to_str().unwrap(), &["add", "-q", "a.txt", "src"]);
    run(&wc, cli::svn_executable().unwrap().to_str().unwrap(), &["commit", "-q", "-m", "init"]);
    // A commit never moves the working-copy root's own revision; without the
    // update the root sits at r0 and `svn log` in it sees no history.
    run(&wc, cli::svn_executable().unwrap().to_str().unwrap(), &["update", "-q"]);
    Some((dir, wc))
}

fn utf8() -> crate::TextEncoding {
    crate::TextEncoding::from(encoding_rs::UTF_8)
}

#[test]
fn discovers_and_reads_status() {
    let Some((_dir, wc)) = fixture() else { return };
    let found = SvnWorkingCopy::discover(&wc).unwrap().expect("working copy discovered");
    assert_eq!(found.root(), wc);
    assert_eq!(found.revision().unwrap(), 1);

    std::fs::write(wc.join("a.txt"), "hello\nworld\n").unwrap();
    std::fs::write(wc.join("new.txt"), "unversioned\n").unwrap();
    let status = found.status().unwrap();
    let modified = status.iter().find(|f| f.rel_path == "a.txt").unwrap();
    assert_eq!(modified.unstaged, Some(ChangeKind::Modified));
    assert_eq!(modified.staged, None, "svn has no index: staged is always None");
    let untracked = status.iter().find(|f| f.rel_path == "new.txt").unwrap();
    assert_eq!(untracked.unstaged, Some(ChangeKind::Untracked));
    assert!(!status.iter().any(|f| f.rel_path == "src/main.rs"), "unchanged files are not listed");
}

#[test]
fn status_paths_answers_only_the_asked_paths() {
    let Some((_dir, wc)) = fixture() else { return };
    let found = SvnWorkingCopy::discover(&wc).unwrap().unwrap();
    std::fs::write(wc.join("a.txt"), "hello\nworld\n").unwrap();
    std::fs::write(wc.join("b.txt"), "new\n").unwrap();
    let status = found.status_paths(&[wc.join("a.txt")]).unwrap();
    assert_eq!(status.len(), 1);
    assert_eq!(status[0].rel_path, "a.txt");
    // Paths outside the working copy are ignored.
    assert!(found.status_paths(&[PathBuf::from("Z:/elsewhere/x")]).unwrap().is_empty());
}

#[test]
fn stage_reports_unstaged_and_unstage_is_a_no_op() {
    let Some((_dir, wc)) = fixture() else { return };
    let found = SvnWorkingCopy::discover(&wc).unwrap().unwrap();
    let new = wc.join("new.txt");
    std::fs::write(&new, "x\n").unwrap();

    found.stage(&[new.clone()]).unwrap();
    let status = found.status().unwrap();
    let entry = status.iter().find(|f| f.path.0 == new).unwrap();
    assert_eq!(entry.staged, None);
    assert_eq!(entry.unstaged, Some(ChangeKind::Added), "scheduled add reads as unstaged");

    found.unstage(&[new.clone()]).unwrap();
    let status = found.status().unwrap();
    let entry = status.iter().find(|f| f.path.0 == new).unwrap();
    assert_eq!(entry.unstaged, Some(ChangeKind::Added), "unstage is a no-op without an index");
    assert!(new.exists());
}

#[test]
fn discard_restores_a_modified_file() {
    let Some((_dir, wc)) = fixture() else { return };
    let found = SvnWorkingCopy::discover(&wc).unwrap().unwrap();
    let a = wc.join("a.txt");
    std::fs::write(&a, "changed\n").unwrap();

    found.discard(&[a.clone()]).unwrap();
    assert_eq!(std::fs::read_to_string(&a).unwrap(), "hello\n");
}

#[test]
fn discard_deletes_unversioned_files() {
    let Some((_dir, wc)) = fixture() else { return };
    let found = SvnWorkingCopy::discover(&wc).unwrap().unwrap();
    let new = wc.join("scratch.txt");
    std::fs::write(&new, "x\n").unwrap();

    found.discard(&[new.clone()]).unwrap();
    assert!(!new.exists());
}

#[test]
fn file_at_reads_base_and_none_for_unversioned() {
    let Some((_dir, wc)) = fixture() else { return };
    let found = SvnWorkingCopy::discover(&wc).unwrap().unwrap();
    std::fs::write(wc.join("a.txt"), "hello\nworld\n").unwrap();
    let scheduled = wc.join("scheduled.txt");
    std::fs::write(&scheduled, "fresh\n").unwrap();
    found.stage(&[scheduled.clone()]).unwrap();

    assert_eq!(found.file_at(&wc.join("a.txt"), None, utf8()).unwrap().as_deref(), Some("hello\n"));
    assert_eq!(found.file_at(&wc.join("a.txt"), Some("BASE"), utf8()).unwrap().as_deref(), Some("hello\n"));
    assert_eq!(found.file_at(&scheduled, None, utf8()).unwrap(), None, "a scheduled add has no pristine version yet");
    assert_eq!(found.file_at(&wc.join("missing.txt"), None, utf8()).unwrap(), None);
}

#[test]
fn commit_log_and_changes() {
    let Some((_dir, wc)) = fixture() else { return };
    let found = SvnWorkingCopy::discover(&wc).unwrap().unwrap();
    std::fs::write(wc.join("a.txt"), "hello\nworld\n").unwrap();

    let committed = found
        .commit(
            "second\n\nlonger body",
            &CommitOptions {
                amend: false,
                amend_target: None,
                sign_off: false,
                author: None,
                author_date: None,
                run_hooks: true,
                partial: Vec::new(),
                stage: vec![IpcPath(wc.join("a.txt"))],
                exclude: Vec::new(),
            },
            &|_| Ok(utf8()),
        )
        .unwrap();
    assert!(committed.starts_with('r'), "{committed}");
    // Commits never move the working-copy root's revision (only update does),
    // but the next `svn info` must still come from a fresh read.
    assert_eq!(found.revision().unwrap(), 1);

    let cancel = CancellationToken::new();
    let query = LogQuery { limit: 10, ..LogQuery::default() };
    let commits = found.log(&query, &cancel).unwrap();
    assert_eq!(commits.first().unwrap().id, committed, "newest first");
    assert_eq!(commits.first().unwrap().summary, "second");
    assert!(commits.first().unwrap().message.contains("longer body"));
    assert_eq!(commits.len(), 2, "r1 and r2, with a wired parent chain");
    assert_eq!(commits[1].parents, Vec::<String>::new());
    assert_eq!(commits[0].parents, vec![commits[1].id.clone()]);
    assert!(commits.iter().all(|c| c.refs.is_empty()));

    let files = found.commit_files(&committed).unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].path, "a.txt");
    assert_eq!(files[0].kind, ChangeKind::Modified);

    let changes = found.commits_changes(&[committed.clone()]).unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].path, "a.txt");
    assert_eq!(changes[0].after.as_deref(), Some(committed.as_str()));
    assert!(changes[0].before.is_some(), "r1 held the previous content");
}

#[test]
fn commit_adds_unversioned_targets() {
    let Some((_dir, wc)) = fixture() else { return };
    let found = SvnWorkingCopy::discover(&wc).unwrap().unwrap();
    std::fs::write(wc.join("fresh.txt"), "fresh\n").unwrap();
    let committed = found
        .commit(
            "add fresh",
            &CommitOptions {
                amend: false,
                amend_target: None,
                sign_off: false,
                author: None,
                author_date: None,
                run_hooks: true,
                partial: Vec::new(),
                stage: vec![IpcPath(wc.join("fresh.txt"))],
                exclude: Vec::new(),
            },
            &|_| Ok(utf8()),
        )
        .unwrap();
    let files = found.commit_files(&committed).unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].kind, ChangeKind::Added);
}

#[test]
fn line_hunks_compare_against_base() {
    let Some((_dir, wc)) = fixture() else { return };
    let found = SvnWorkingCopy::discover(&wc).unwrap().unwrap();
    let a = wc.join("a.txt");
    std::fs::write(&a, "hello\nworld\n").unwrap();
    let hunks = found.line_hunks(&a, "hello\nworld\n", false, utf8()).unwrap();
    assert_eq!(hunks.len(), 1);
    assert_eq!(hunks[0].kind, HunkKind::Added);
    assert_eq!((hunks[0].start, hunks[0].lines), (1, 1));

    // Unversioned: everything is added, as with git's untracked files.
    let scratch = wc.join("scratch.txt");
    std::fs::write(&scratch, "all\nnew\n").unwrap();
    let hunks = found.line_hunks(&scratch, "all\nnew\n", false, utf8()).unwrap();
    assert_eq!(hunks.len(), 1);
    assert_eq!(hunks[0].kind, HunkKind::Added);
    assert_eq!((hunks[0].start, hunks[0].lines), (0, 2));
}

#[test]
fn block_history_is_unsupported() {
    let Some((_dir, wc)) = fixture() else { return };
    let found = SvnWorkingCopy::discover(&wc).unwrap().unwrap();
    // Annotate itself works now ([`Backend::blame`]); selection history's
    // per-block diff walk stays Git-only.
    let error = found.block_history(&wc.join("a.txt"), None, (0, 1), utf8(), &CancellationToken::new()).unwrap_err();
    assert!(error.to_string().to_lowercase().contains("not supported"), "{error}");
}

#[test]
fn revision_diffs_and_numstat() {
    let Some((_dir, wc)) = fixture() else { return };
    let found = SvnWorkingCopy::discover(&wc).unwrap().unwrap();
    std::fs::write(wc.join("a.txt"), "hello\nworld\n").unwrap();
    std::fs::write(wc.join("added.txt"), "fresh\n").unwrap();
    let committed = found
        .commit(
            "second",
            &CommitOptions {
                amend: false,
                amend_target: None,
                sign_off: false,
                author: None,
                author_date: None,
                run_hooks: true,
                partial: Vec::new(),
                stage: vec![IpcPath(wc.join("a.txt")), IpcPath(wc.join("added.txt"))],
                exclude: Vec::new(),
            },
            &|_| Ok(utf8()),
        )
        .unwrap();

    let changes = found.diff_revisions("1", &committed[1..]).unwrap();
    assert_eq!(changes.len(), 2, "{changes:?}");
    assert!(changes.iter().any(|f| f.path == "a.txt" && f.kind == ChangeKind::Modified));
    assert!(changes.iter().any(|f| f.path == "added.txt" && f.kind == ChangeKind::Added));

    // The working copy matches r2: nothing differs yet.
    assert!(found.diff_with_worktree(&committed).unwrap().is_empty());
    std::fs::write(wc.join("a.txt"), "hello\nworld\nmore\n").unwrap();
    let dirty = found.diff_with_worktree(&committed).unwrap();
    assert_eq!(dirty.len(), 1);
    assert_eq!(dirty[0].path, "a.txt");
    assert_eq!(dirty[0].kind, ChangeKind::Modified);

    let stats = found.numstat_with_worktree(&committed).unwrap();
    let a = stats.iter().find(|(path, _, _)| path == "a.txt").unwrap();
    assert_eq!((a.1, a.2), (1, 0));
}

#[test]
fn pending_changes_list_versioned_files_with_diffs() {
    let Some((_dir, wc)) = fixture() else { return };
    let found = SvnWorkingCopy::discover(&wc).unwrap().unwrap();
    std::fs::write(wc.join("a.txt"), "hello\nworld\n").unwrap();
    std::fs::write(wc.join("unversioned.txt"), "no\n").unwrap();
    let scheduled = wc.join("scheduled.txt");
    std::fs::write(&scheduled, "scheduled\n").unwrap();
    found.stage(&[scheduled]).unwrap();

    let changes = found.pending_changes(&CommitOptions::default(), 1 << 20, &|_| Ok(utf8())).unwrap();
    let paths: Vec<&str> = changes.iter().map(|c| c.path.file_name().unwrap().to_str().unwrap()).collect();
    assert_eq!(paths, vec!["a.txt", "scheduled.txt"], "unversioned files are not pending: {paths:?}");
    assert_eq!(changes[0].status, "modified");
    assert!(changes[0].diff.as_deref().is_some_and(|d| d.contains("+world")), "{:?}", changes[0].diff);
    assert_eq!(changes[1].status, "added");
    assert!(changes[1].diff.as_deref().is_some_and(|d| d.contains("+scheduled")), "{:?}", changes[1].diff);

    // A selection narrows the preview.
    let changes = found
        .pending_changes(
            &CommitOptions { stage: vec![IpcPath(wc.join("a.txt"))], ..CommitOptions::default() },
            1 << 20,
            &|_| Ok(utf8()),
        )
        .unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].status, "modified");
}

#[test]
fn ide_file_operations_schedule_adds_and_deletes() {
    let Some((_dir, wc)) = fixture() else { return };
    let found = SvnWorkingCopy::discover(&wc).unwrap().unwrap();
    let created = wc.join("created.txt");
    std::fs::write(&created, "x\n").unwrap();
    found.files_added(&[created.clone()]).unwrap();
    let status = found.status().unwrap();
    assert_eq!(status.iter().find(|f| f.path.0 == created).unwrap().unstaged, Some(ChangeKind::Added));

    // The hook's add only survives as a deletion once the file is committed:
    // deleting a merely scheduled add cancels the add instead.
    found
        .commit(
            "created",
            &CommitOptions {
                amend: false,
                amend_target: None,
                sign_off: false,
                author: None,
                author_date: None,
                run_hooks: true,
                partial: Vec::new(),
                stage: vec![IpcPath(created.clone())],
                exclude: Vec::new(),
            },
            &|_| Ok(utf8()),
        )
        .unwrap();

    // The IDE removes the file first, then reports it.
    std::fs::remove_file(&created).unwrap();
    found.files_deleted(&[created.clone()]).unwrap();
    let status = found.status().unwrap();
    assert_eq!(status.iter().find(|f| f.path.0 == created).unwrap().unstaged, Some(ChangeKind::Deleted));
}

#[test]
fn file_moved_records_delete_and_add() {
    let Some((_dir, wc)) = fixture() else { return };
    let found = SvnWorkingCopy::discover(&wc).unwrap().unwrap();
    let from = wc.join("a.txt");
    let to = wc.join("renamed.txt");
    std::fs::rename(&from, &to).unwrap();
    found.file_moved(&from, &to).unwrap();

    let status = found.status().unwrap();
    assert_eq!(status.iter().find(|f| f.rel_path == "a.txt").unwrap().unstaged, Some(ChangeKind::Deleted));
    assert_eq!(status.iter().find(|f| f.rel_path == "renamed.txt").unwrap().unstaged, Some(ChangeKind::Added));
}

#[test]
fn a_missing_file_reports_deleted_and_stage_schedules_the_deletion() {
    let Some((_dir, wc)) = fixture() else { return };
    let found = SvnWorkingCopy::discover(&wc).unwrap().unwrap();
    let a = wc.join("a.txt");
    std::fs::remove_file(&a).unwrap();
    let status = found.status().unwrap();
    assert_eq!(status.iter().find(|f| f.path.0 == a).unwrap().unstaged, Some(ChangeKind::Deleted));

    found.stage(&[a.clone()]).unwrap();
    let status = found.status().unwrap();
    assert_eq!(status.iter().find(|f| f.path.0 == a).unwrap().unstaged, Some(ChangeKind::Deleted));
    assert!(!status.iter().find(|f| f.path.0 == a).unwrap().staged.is_some(), "staged stays None");

    // Discard restores the file from BASE: the working copy is clean again.
    found.discard(&[a.clone()]).unwrap();
    let status = found.status().unwrap();
    assert!(status.iter().find(|f| f.path.0 == a).is_none());
    assert!(a.exists());
}
