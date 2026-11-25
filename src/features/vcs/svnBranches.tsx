/**
 * Title bar Subversion popup: the repository's conventional layout (`/trunk`,
 * `/branches/*`, `/tags/*`) to switch the working copy between, and the actions to copy
 * and delete a branch (IDEA's SVN branches popup, reached through the revision widget).
 *
 * Branch and tag names are shown as the repository spells them; the working copy counts
 * as on a branch when its URL is the branch URL or a directory inside it.
 */
import { useEffect, useState, type KeyboardEvent } from "react";
import { Channel } from "@tauri-apps/api/core";
import { create } from "zustand";
import type { SvnBranch } from "@/bindings";
import { call, callDesktop, commands, nextRequestId } from "@/platform/ipc";
import {
  Checkbox,
  Dialog,
  Icon,
  Input,
  List,
  Popup,
  SearchField,
  Select,
  firstSelectable,
  listNavigate,
  T,
} from "@/ui";
import { useRowHeight } from "@/ui/density";
import type { PopupAnchor } from "@/ui/Popup";
import { confirmDialog, openDialog } from "./dialogs";
import { currentRepo } from "./store";
import { errorMessage, logConsole, runOp } from "./ops";
import styles from "./BranchWidget.module.css";
import formStyles from "./common.module.css";
import { tx } from "@/platform/i18n";

const ROW_HEIGHT = 24;

export const useSvnBranchesPopup = create<{ open: boolean }>(() => ({ open: false }));
export const showSvnBranchesPopup = () => useSvnBranchesPopup.setState({ open: true });

export function SvnBranchesPopupHost() {
  const open = useSvnBranchesPopup((s) => s.open);
  if (!open) return null;
  const anchor = document.querySelector("[data-svn-widget]") ?? { x: window.innerWidth / 2 - 160, y: 60 };
  return <SvnBranchesPopup anchor={anchor} onClose={() => useSvnBranchesPopup.setState({ open: false })} />;
}

type Row =
  | { type: "action"; key: string; label: string; icon: string; disabled?: boolean; run: () => void }
  | { type: "section"; key: string; label: string }
  | { type: "branch"; key: string; branch: SvnBranch; tag: boolean; current: boolean };

const isSection = (r: Row) => r.type === "section";

/** The working copy is on `branch` when its URL is the branch's or one of its directories. */
function onBranch(url: string | undefined, branch: SvnBranch): boolean {
  return !!url && (url === branch.url || url.startsWith(`${branch.url}/`));
}

/** `/trunk` (it keeps the mainline) and the branch the working copy is on cannot go. */
const deletable = (url: string | undefined) => (b: SvnBranch) => !b.url.endsWith("/trunk") && !onBranch(url, b);

/** A branch name that can join a repository URL (one path segment, no separator tricks). */
function branchNameError(name: string): string | null {
  const n = name.trim();
  if (!n) return "Enter a branch name";
  if (/[/\\]/.test(n)) return "The name must not contain slashes";
  if (/^-/.test(n)) return "The name must not start with a dash";
  if (/\s/.test(n)) return "The name must not contain spaces";
  return null;
}

/** Console sink for svn's own output of the network operations (switch, copy, delete). */
function consoleChannel() {
  const channel = new Channel<string>();
  channel.onmessage = (line) => logConsole("info", line);
  return channel;
}

function SvnBranchesPopup({ anchor, onClose }: { anchor: PopupAnchor; onClose: () => void }) {
  const repo = currentRepo();
  const rowHeight = useRowHeight(ROW_HEIGHT);
  const [query, setQuery] = useState("");
  const [branches, setBranches] = useState<SvnBranch[] | null>(null);
  const [tags, setTags] = useState<SvnBranch[] | null>(null);
  const [url, setUrl] = useState<string | undefined>(undefined);
  const [error, setError] = useState<string | null>(null);
  const [active, setActive] = useState(-1);

  useEffect(() => {
    let live = true;
    // The listing talks to the repository: cancellable, and cancelled on close.
    const request = nextRequestId();
    call(commands.vcsSvnBranches(repo, request)).then(
      (b) => {
        if (!live) return;
        setBranches(b.branches);
        setTags(b.tags);
      },
      (e) => {
        if (!live) return;
        setBranches([]);
        setTags([]);
        setError(errorMessage(e));
      },
    );
    return () => {
      live = false;
      void callDesktop("cancel_request", { id: request }).catch(() => undefined);
    };
  }, [repo]);

  // The working copy's URL decides which row counts as current; refreshed after operations
  // through the status (a switch bumps the store's version, so the widget refetches).
  useEffect(() => {
    let live = true;
    call(commands.vcsSvnInfo(repo)).then(
      (info) => live && setUrl(info.url),
      () => live && setUrl(undefined),
    );
    return () => {
      live = false;
    };
  }, [repo]);

  const switchTo = (branch: SvnBranch) => {
    onClose();
    void runOp(
      tx`Switching to ${branch.name}`,
      () => call(commands.vcsSvnSwitch(repo, nextRequestId(), branch.url, consoleChannel())),
      { success: tx`Switched to ${branch.name}` },
    );
  };

  const openNewBranch = () => {
    onClose();
    openDialog((close) => <NewBranchDialog repo={repo} onClose={close} />);
  };

  const openDeleteBranch = () => {
    onClose();
    openDialog((close) => (
      <DeleteBranchDialog repo={repo} branches={(branches ?? []).filter(deletable(url))} onClose={close} />
    ));
  };

  const allRows: Row[] = [
    { type: "action", key: "action:new", label: "New Branch…", icon: "plus", run: openNewBranch },
    {
      type: "action",
      key: "action:delete",
      label: "Delete Branch…",
      icon: "trash",
      disabled: !(branches ?? []).some(deletable(url)),
      run: openDeleteBranch,
    },
  ];
  if (branches?.length)
    allRows.push(
      { type: "section", key: "section:branches", label: "Branches" },
      ...branches.map((b): Row => ({ type: "branch", key: b.url, branch: b, tag: false, current: onBranch(url, b) })),
    );
  if (tags?.length)
    allRows.push(
      { type: "section", key: "section:tags", label: "Tags" },
      ...tags.map((b): Row => ({ type: "branch", key: b.url, branch: b, tag: true, current: onBranch(url, b) })),
    );
  const rows = allRows.filter((row) => {
    const q = query.trim().toLowerCase();
    return row.type !== "branch" || !q || row.branch.name.toLowerCase().includes(q);
  });

  const activeIndex =
    active >= 0 && rows[active] && !isSection(rows[active]!) && !(rows[active] as { disabled?: boolean }).disabled
      ? active
      : firstSelectable(rows, isSection);

  const activate = (i: number) => {
    const row = rows[i];
    if (!row || isSection(row)) return;
    if (row.type === "action") {
      if (!row.disabled) row.run();
      return;
    }
    if (!row.current) switchTo(row.branch);
  };

  const onKeyDown = (e: KeyboardEvent) => {
    if (e.key === "Enter") {
      e.preventDefault();
      activate(activeIndex);
      return;
    }
    const next = listNavigate(e.key, { items: rows, active: activeIndex, isSection, wrap: true });
    if (next !== null) {
      e.preventDefault();
      setActive(next);
    }
  };

  const loaded = branches !== null;
  return (
    <Popup
      open
      anchor={anchor}
      onClose={onClose}
      placement="bottom-start"
      aria-label="SVN Branches"
      className={styles.popup}
      resizable="both"
      minWidth={280}
      minHeight={200}
    >
      <div className={styles.search}>
        <SearchField
          className={styles.searchGrow}
          data-autofocus
          value={query}
          onChange={(v) => {
            setQuery(v);
            setActive(-1);
          }}
          placeholder="Search branches and tags"
          aria-label="Search branches and tags"
          variant="bare"
          onKeyDown={onKeyDown}
        />
      </div>
      <div
        style={{ height: Math.min(440, Math.max(rowHeight * 3, rows.length * rowHeight)) }}
        className={styles.listWrap}
      >
        <List
          items={rows}
          getKey={(r) => r.key}
          isSection={isSection}
          renderSection={(r) => (
            <span className={styles.section}>
              <T>{(r as { label: string }).label}</T>
            </span>
          )}
          activeIndex={activeIndex}
          onActiveChange={setActive}
          onActivate={(_r, i) => activate(i)}
          focusable={false}
          hoverSelects
          rowHeight={rowHeight}
          aria-label="SVN branches and tags"
          className={styles.list}
          empty={
            <span className={styles.empty}>{error ?? (loaded ? <T>No branches or tags</T> : <T>Loading…</T>)}</span>
          }
          renderItem={(r) => {
            if (r.type === "action")
              return (
                <span className={styles.row} data-row-key={r.key} aria-disabled={r.disabled || undefined}>
                  <Icon name={r.disabled ? "check" : r.icon} size={14} className={styles.rowIcon} />
                  <span className={styles.label}>
                    <T>{r.label}</T>
                  </span>
                </span>
              );
            if (r.type === "branch")
              return (
                <span className={styles.row} data-row-key={r.key} title={r.branch.name}>
                  <Icon
                    name={r.current ? "current-branch" : r.tag ? "tag" : "branch"}
                    size={14}
                    className={styles.rowIcon}
                  />
                  <span className={styles.label}>{r.branch.name}</span>
                  {r.current && (
                    <span className={styles.tag}>
                      <T>current</T>
                    </span>
                  )}
                </span>
              );
            return null;
          }}
        />
      </div>
    </Popup>
  );
}

/** Copy Branch: a repository-side `svn copy` of the working copy's current location. */
function NewBranchDialog({ repo, onClose }: { repo: string | null; onClose: () => void }) {
  const [name, setName] = useState("");
  /** The message follows the name until the user edits it (svn commits the copy immediately). */
  const [message, setMessage] = useState<string | null>(null);
  const [switchAfter, setSwitchAfter] = useState(false);
  const [busy, setBusy] = useState(false);
  const invalid = branchNameError(name);
  const text = message ?? (name.trim() ? `Create branch ${name.trim()}` : "");

  const create = async () => {
    if (invalid || busy) return;
    setBusy(true);
    const created = await runOp(
      tx`Creating branch ${name.trim()}`,
      () => call(commands.vcsSvnCopyBranch(repo, nextRequestId(), name.trim(), text, switchAfter, consoleChannel())),
      { success: tx`Created branch ${name.trim()}` },
    );
    setBusy(false);
    // `null` is the command's success value; `undefined` means it failed (and said so).
    if (created === null) onClose();
  };

  return (
    <Dialog
      open
      title="New Branch"
      onClose={onClose}
      width={440}
      buttons={[
        { label: "Create", role: "default", disabled: !!invalid || busy, onClick: () => void create() },
        { label: "Cancel", role: "cancel", onClick: onClose },
      ]}
    >
      <div className={formStyles.form}>
        <label className={formStyles.fieldLabel} htmlFor="svn-branch-name">
          <T>Name:</T>
        </label>
        <Input
          id="svn-branch-name"
          data-autofocus
          value={name}
          disabled={busy}
          invalid={!!invalid && !!name.trim()}
          placeholder="my-feature"
          onChange={(e) => setName(e.target.value)}
        />
        {invalid && !!name.trim() && (
          <div className={formStyles.error} role="alert">
            {invalid}
          </div>
        )}
        <label className={formStyles.fieldLabel} htmlFor="svn-branch-message">
          <T>Log message:</T>
        </label>
        <Input id="svn-branch-message" value={text} disabled={busy} onChange={(e) => setMessage(e.target.value)} />
        <Checkbox
          checked={switchAfter}
          disabled={busy}
          onChange={setSwitchAfter}
          label={<T>Switch the working copy to the new branch</T>}
        />
      </div>
    </Dialog>
  );
}

/** Delete Branch: removes `/branches/<name>` in the repository, committing immediately. */
function DeleteBranchDialog({
  repo,
  branches,
  onClose,
}: {
  repo: string | null;
  /** Deletable branches: not `/trunk`, not the one the working copy is on. */
  branches: readonly SvnBranch[];
  onClose: () => void;
}) {
  const [name, setName] = useState<string | null>(branches[0]?.name ?? null);
  const [busy, setBusy] = useState(false);

  const remove = async () => {
    const branch = branches.find((b) => b.name === name);
    if (!branch || busy) return;
    if (
      !(await confirmDialog({
        title: "Delete Branch",
        message: tx`Delete branch ${branch.name} in the repository? This commits the deletion immediately.`,
        okLabel: "Delete",
        danger: true,
      }))
    )
      return;
    setBusy(true);
    const deleted = await runOp(
      tx`Deleting branch ${branch.name}`,
      () =>
        call(
          commands.vcsSvnDeleteBranch(
            repo,
            nextRequestId(),
            branch.name,
            `Delete branch ${branch.name}`,
            consoleChannel(),
          ),
        ),
      { success: tx`Deleted branch ${branch.name}` },
    );
    setBusy(false);
    if (deleted === null) onClose();
  };

  return (
    <Dialog
      open
      title="Delete Branch"
      onClose={onClose}
      width={440}
      buttons={[
        { label: "Delete", role: "default", disabled: !name || busy, onClick: () => void remove() },
        { label: "Cancel", role: "cancel", onClick: onClose },
      ]}
    >
      {branches.length ? (
        <div className={formStyles.form}>
          <label className={formStyles.fieldLabel} htmlFor="svn-branch-delete">
            <T>Branch:</T>
          </label>
          <Select
            id="svn-branch-delete"
            aria-label="Branch to delete"
            value={name}
            options={branches.map((b) => ({ value: b.name, label: b.name }))}
            onChange={setName}
            disabled={busy}
            autoFocus
          />
        </div>
      ) : (
        <div className={formStyles.hint}>
          <T>No deletable branch: /trunk and the branch the working copy is on stay.</T>
        </div>
      )}
    </Dialog>
  );
}
