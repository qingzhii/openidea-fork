/** Clone Repository › Subversion: a repository URL to `svn checkout` into a new folder. */
import { useRef } from "react";
import { showContextMenu, Input, IconButton, T } from "@/ui";
import type { CloneSourceProps } from "@/workbench/contributions";
import { readRecentUrls } from "./clone";
import styles from "./clone.module.css";

export function SvnCheckoutSource({ url, onUrl, onClone, disabled }: CloneSourceProps) {
  const urlField = useRef<HTMLDivElement>(null);
  const recentUrls = readRecentUrls();

  const showUrlHistory = () => {
    const rect = urlField.current?.getBoundingClientRect();
    if (!rect || !recentUrls.length) return;
    showContextMenu(
      { x: rect.left, y: rect.bottom },
      recentUrls.map((item) => ({ label: item, verbatim: true, run: () => onUrl(item) })),
    );
  };

  return (
    <div className={styles.sourcePane}>
      <div className={styles.fieldRow}>
        <label htmlFor="svn-checkout-url">
          <T>URL:</T>
        </label>
        <div ref={urlField} className={styles.comboField}>
          <Input
            id="svn-checkout-url"
            data-autofocus
            variant="bare"
            className={styles.fieldInput}
            value={url}
            disabled={disabled}
            placeholder="file:///srv/svn/repo/trunk or https://svn.example.com/repo/trunk"
            onChange={(e) => onUrl(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") {
                e.preventDefault();
                onClone(url);
              }
            }}
          />
          <IconButton
            icon="chevron-down"
            size="normal"
            className={styles.fieldButton}
            title={recentUrls.length ? "Recent repository URLs" : "No recent repository URLs"}
            disabled={disabled || !recentUrls.length}
            onClick={showUrlHistory}
          />
        </div>
      </div>
      {/* Shallow clone is a git concept: svn always checks out the full history. */}
      <div className={styles.shallowRow}>
        <span className={styles.hint}>
          <T>Checks out a working copy with svn; the URL may point at any directory of the repository.</T>
        </span>
      </div>
    </div>
  );
}
