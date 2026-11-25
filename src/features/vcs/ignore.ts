/** Project-host ignore writes; shared by the project menu and Find Action. */
import type { IgnoreDestination } from "@/bindings";
import { call, commands } from "@/platform/ipc";
import { reloadAll } from "@/features/project-tree/store";
import { editorService, notifications } from "@/workbench/contributions";
import { refreshNow } from "./store";
import { runOp } from "./ops";

export async function addToIgnore(paths: string[], destination: IgnoreDestination) {
  if (!paths.length) return;
  const changed = await runOp(
    destination === "gitignore" ? "Add to .gitignore" : "Add to .git/info/exclude",
    () => call(commands.vcsAddToIgnore(paths, destination)),
    { refresh: false },
  );
  // Also refresh after a partial failure in a multi-repository selection.
  await Promise.all([refreshNow(), reloadAll()]);
  if (changed && !changed.length) {
    // Existing rules already cover the selection (e.g. a `_*/` directory pattern).
    notifications().notify({ group: "vcs", level: "info", title: "Already ignored" });
    return;
  }
  // Show the rule where it was written: the nearest .gitignore may be a nested one.
  const file = changed?.[0];
  if (file) await editorService().openFile(file, { focus: true });
}

/**
 * Subversion has no rule file: the names go into the parent directories' `svn:ignore`
 * property (`vcs_svn_add_to_ignore`), which stays local until that directory is
 * committed — so there is nothing to open, and the file system watcher is unaffected.
 */
export async function addToSvnIgnore(paths: string[]) {
  if (!paths.length) return;
  // `null` is the command's success value; on failure runOp has already reported it.
  const done = await runOp("Add to svn:ignore", () => call(commands.vcsSvnAddToIgnore(paths)));
  // Also refresh after a partial failure in a multi-repository selection.
  await Promise.all([refreshNow(), reloadAll()]);
  if (done !== null) return;
  notifications().notify({
    group: "vcs",
    level: "info",
    title: "Added to svn:ignore",
    body: "Takes effect for others once the parent directory is committed.",
  });
}
