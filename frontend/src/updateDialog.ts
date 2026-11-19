// Modal update dialog: release notes, Install / Later, live download progress and a verbose step log.
import type { Update } from "@tauri-apps/plugin-updater";
import { onUpdateLog, onUpdateProgress, selfUpdate, updateInstallMode, type InstallMode } from "./api";
import { t } from "./i18n";

function el<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  cls: string,
  text?: string,
): HTMLElementTagNameMap[K] {
  const e = document.createElement(tag);
  e.className = cls;
  if (text !== undefined) e.textContent = text;
  return e;
}

const mb = (n: number) => `${(n / 1048576).toFixed(1)} MB`;

export function openUpdateDialog(update: Update): void {
  const overlay = el("div", "overlay");
  const card = el("div", "overlay-card update-card");
  card.setAttribute("role", "dialog");
  card.setAttribute("aria-modal", "true");
  overlay.append(card);

  card.append(el("h2", "", t("update.title")), el("p", "update-text", t("update.available", { version: update.version })));
  if (update.body) card.append(el("pre", "update-notes", update.body));

  const progressBlock = el("div", "progress-block");
  progressBlock.hidden = true;
  const pctLabel = el("span", "", "");
  const sizeLabel = el("span", "", "");
  const label = el("div", "progress-label");
  label.append(pctLabel, sizeLabel);
  const bar = el("div", "progress");
  const fill = el("div", "progress-value");
  bar.append(fill);
  progressBlock.append(label, bar);

  const details = el("details", "update-log-wrap");
  details.hidden = true;
  details.append(el("summary", "", t("update.log")));
  const logBox = el("pre", "update-log");
  details.append(logBox);

  const error = el("p", "update-error");
  error.hidden = true;

  const actions = el("div", "overlay-actions");
  const later = el("button", "btn", t("update.later"));
  later.type = "button";
  const install = el("button", "btn primary", t("update.install"));
  install.type = "button";
  const hint = el("p", "update-text", t("update.system"));
  hint.hidden = true;
  actions.append(later, install);
  card.append(hint, progressBlock, details, error, actions);

  const log = (line: string) => {
    const d = new Date();
    const ts = [d.getHours(), d.getMinutes(), d.getSeconds()].map((n) => String(n).padStart(2, "0")).join(":");
    logBox.textContent += `[${ts}] ${line}\n`;
    logBox.scrollTop = logBox.scrollHeight;
  };

  let busy = false;
  const close = () => {
    if (!busy) overlay.remove();
  };
  later.addEventListener("click", close);
  overlay.addEventListener("keydown", (e) => e.key === "Escape" && close());

  const setProgress = (done: number, total: number | null) => {
    if (total && total > 0) {
      const pct = Math.min(100, (done / total) * 100);
      fill.style.width = `${pct}%`;
      pctLabel.textContent = `${pct.toFixed(1)} %`;
      sizeLabel.textContent = `${mb(done)} / ${mb(total)}`;
    } else {
      // Unknown total: full-width bar, only the downloaded size is meaningful.
      fill.style.width = "100%";
      fill.classList.add("indeterminate");
      pctLabel.textContent = "";
      sizeLabel.textContent = mb(done);
    }
  };

  let mode: InstallMode = "installer";
  const run = async () => {
    busy = true;
    install.disabled = later.disabled = true;
    error.hidden = true;
    progressBlock.hidden = details.hidden = false;
    details.open = true;
    fill.classList.remove("indeterminate");
    setProgress(0, null);
    pctLabel.textContent = "0.0 %";
    const unlisten: Array<() => void> = [];
    try {
      if (mode === "installer") {
        let done = 0;
        let total: number | null = null;
        log(t("update.logDownloading", { version: update.version }));
        await update.downloadAndInstall((ev) => {
          if (ev.event === "Started") {
            total = ev.data.contentLength ?? null;
            log(total ? t("update.logStarted", { size: mb(total) }) : t("update.logStartedUnknown"));
          } else if (ev.event === "Progress") {
            done += ev.data.chunkLength;
            setProgress(done, total);
          } else {
            setProgress(total ?? done, total ?? done);
            log(t("update.logDownloaded"));
            log(t("update.logVerifying"));
            log(t("update.logInstaller"));
          }
        });
      } else {
        unlisten.push(await onUpdateProgress((p) => setProgress(p.downloaded, p.total)));
        unlisten.push(await onUpdateLog(log));
        await selfUpdate(update.version);
      }
      log(t("update.logRestarting"));
    } catch (e) {
      const msg = String(e);
      log(t("update.logFailed", { error: msg }));
      error.textContent = t("update.failed", { error: msg });
      error.hidden = false;
      busy = false;
      install.disabled = later.disabled = false;
      later.textContent = t("update.close");
    } finally {
      unlisten.forEach((u) => u());
    }
  };
  install.addEventListener("click", () => void run());

  // Install button stays hidden until the install mode is known; "system" installs get a hint only.
  install.hidden = true;
  document.body.append(overlay);
  later.focus();
  updateInstallMode()
    .then((m) => {
      mode = m;
    })
    .catch(() => {})
    .finally(() => {
      if (mode === "system") hint.hidden = false;
      else install.hidden = false;
    });
}
