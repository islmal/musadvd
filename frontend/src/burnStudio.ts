// Entry point of the Burn Studio window (burnStudio.html): menu bar, mode switch, drive bar,
// status panel (progress, buffers, log) and the graph tab. The modes live in studio/views.ts.
import { getCurrentWindow } from "@tauri-apps/api/window";
import { open } from "@tauri-apps/plugin-dialog";
import { getSettings, saveSettings } from "./api";
import * as api from "./studio/api";
import { GraphView } from "./studio/graph";
import { openStudioSettings } from "./studio/settings";
import { openAutoSpeed, openDriveInfo, openIfoTool, openLabelTool, openSidecarTool } from "./studio/tools";
import { el, fmtTime, modal } from "./studio/ui";
import { MODES, mediaText, needsDrive, renderHome, renderMode, speedOptions, type Host, type Mode } from "./studio/views";

const PHASES: Record<string, string> = {
  preparing: "Vorbereiten",
  formatting: "Formatieren",
  writing: "Schreiben",
  finalizing: "Abschließen",
  verifying: "Überprüfen",
  reading: "Lesen",
  building: "Image erstellen",
};

let settings: api.FullSettings;
let drives: api.DriveInfo[] = [];
let driveId = localStorage.getItem("studio-drive") ?? "";
let view: api.DriveView | null = null;
let mode: Mode = "home";
let running = false;
let speed = "auto";
let locked = false;
let tab: "status" | "graph" = "status";

const graph = new GraphView();
const root = document.getElementById("studio")!;
const menubar = el("nav", { class: "menubar" });
const driveBar = el("div", { class: "drive-bar" });
const main = el("main", { class: "studio-main" });
const logList = el("ul", { class: "log studio-log" });
const bar = el("div", { class: "progress-value" });
const ringBar = el("div", { class: "progress-value buf" });
const deviceBar = el("div", { class: "progress-value buf dev" });
const stats = el("div", { class: "studio-stats" });
const phaseLabel = el("span", {}, "Bereit");
const pctLabel = el("span", {}, "");
const cancelBtn = el("button", { class: "btn danger", disabled: true, onclick: () => void api.cancel() }, "Abbrechen");
const statusPanel = el("div", { class: "status-tab" });
const bottom = el("section", { class: "studio-bottom" });
const note = el("div", { class: "status-bar" });

function logLine(text: string, kind = "") {
  logList.append(el("li", { class: kind }, text));
  while (logList.childElementCount > 500) logList.firstElementChild?.remove();
  logList.scrollTop = logList.scrollHeight;
}

function say(message: string) {
  note.textContent = message;
  note.dataset.kind = "";
}

const host: Host = {
  settings: () => settings,
  drive: () => drives.find((d) => d.id === driveId) ?? null,
  view: () => view,
  running: () => running,
  speed: () => speed,
  refreshDrive: () => void refreshDrive(),
  note: say,
  run(what, start) {
    if (running) return say("Es läuft bereits ein Vorgang.");
    running = true;
    logList.replaceChildren();
    graph.clear();
    graph.setSampleMs(settings.burn_studio.graph.sample_ms);
    setProgress(null);
    logLine(`${what} …`);
    tab = "status";
    cancelBtn.disabled = false;
    renderAll();
    start().catch((e) => {
      running = false;
      cancelBtn.disabled = true;
      logLine(String(e), "error");
      renderAll();
    });
  },
};

// ---- status panel ----------------------------------------------------------------------------

function setProgress(p: api.Progress | null) {
  if (!p) {
    bar.style.width = "0";
    ringBar.style.width = "0";
    deviceBar.style.width = "0";
    phaseLabel.textContent = running ? "Starte …" : "Bereit";
    pctLabel.textContent = "";
    stats.replaceChildren();
    return;
  }
  bar.style.width = `${(p.fraction * 100).toFixed(1)}%`;
  ringBar.style.width = `${p.ring}%`;
  deviceBar.style.width = `${p.device ?? 0}%`;
  phaseLabel.textContent = `${PHASES[p.phase] ?? p.phase}${p.message ? ` – ${p.message}` : ""}`;
  pctLabel.textContent = `${(p.fraction * 100).toFixed(1)} %`;
  const cell = (k: string, v: string) => el("span", {}, el("small", {}, k), v);
  stats.replaceChildren(
    cell("Geschwindigkeit", `${p.speedX.toFixed(1)}x`),
    cell("Sektor", `${p.lba.toLocaleString("de-DE")} / ${p.total.toLocaleString("de-DE")}`),
    cell("Hauptpuffer", `${p.ring} %`),
    cell("Laufwerkspuffer", p.device === null ? "—" : `${p.device} %`),
    cell("Verbleibend", fmtTime(p.etaSecs)),
  );
}

function renderBottom() {
  const tabs = el(
    "div",
    { class: "tabs" },
    ...(["status", "graph"] as const).map((t) =>
      el("button", { class: `tab${tab === t ? " active" : ""}`, onclick: () => ((tab = t), renderBottom()) }, t === "status" ? "Vorgang" : "Graph"),
    ),
  );
  statusPanel.hidden = tab !== "status";
  graph.root.hidden = tab !== "graph";
  bottom.replaceChildren(tabs, statusPanel, graph.root);
}

function buildStatusPanel() {
  const meter = (label: string, inner: HTMLElement) =>
    el("div", { class: "progress-block" }, el("div", { class: "progress-label" }, el("span", {}, label)), el("div", { class: "progress thin" }, inner));
  statusPanel.append(
    el("div", { class: "progress-block" }, el("div", { class: "progress-label" }, phaseLabel, pctLabel), el("div", { class: "progress" }, bar)),
    el("div", { class: "meters" }, meter("Hauptpuffer", ringBar), meter("Laufwerkspuffer", deviceBar)),
    stats,
    logList,
    el("div", { class: "overlay-actions" }, cancelBtn),
  );
}

// ---- drives ----------------------------------------------------------------------------------

async function refreshDrive() {
  try {
    drives = await api.drives();
  } catch (e) {
    drives = [];
    say(String(e));
  }
  if (!drives.some((d) => d.id === driveId)) driveId = drives[0]?.id ?? "";
  localStorage.setItem("studio-drive", driveId);
  view = null;
  renderDriveBar();
  if (driveId) {
    try {
      view = await api.driveInfo(driveId);
    } catch (e) {
      say(String(e));
    }
  }
  renderDriveBar();
  if (mode === "write-files" || mode === "build") renderMain(); // the fill gauge depends on the medium
}

function renderDriveBar() {
  driveBar.hidden = !needsDrive(mode);
  const speeds = speedOptions(view?.media ?? null);
  if (!speeds.some(([v]) => v === speed)) speed = "auto";
  driveBar.replaceChildren(
    el(
      "select",
      {
        class: "drive-select",
        onchange: (e: Event) => {
          driveId = (e.target as HTMLSelectElement).value;
          localStorage.setItem("studio-drive", driveId);
          void refreshDrive();
        },
      },
      ...(drives.length ? drives.map((d) => el("option", { value: d.id, selected: d.id === driveId }, d.name)) : [el("option", {}, "Kein Laufwerk gefunden")]),
    ),
    el("button", { class: "btn icon", title: "Aktualisieren", onclick: () => void refreshDrive() }, "↻"),
    el("span", { class: "media-text" }, driveId ? mediaText(view) : "—"),
    el("span", { class: "spacer" }),
    el("label", {}, "Geschwindigkeit ", el("select", { onchange: (e: Event) => (speed = (e.target as HTMLSelectElement).value) }, ...speeds.map(([v, t]) => el("option", { value: v, selected: v === speed }, t)))),
    el("button", { class: "btn", disabled: !driveId, onclick: () => void tray("eject") }, "⏏ Auswerfen"),
    el("button", { class: "btn", disabled: !driveId, onclick: () => void tray("load") }, "Einfahren"),
    el("button", { class: "btn", disabled: !driveId, onclick: () => void openDriveInfo(driveId || null) }, "Info"),
  );
}

async function tray(action: "eject" | "load") {
  try {
    await (action === "eject" ? api.eject(driveId) : api.load(driveId));
    setTimeout(() => void refreshDrive(), 2500);
  } catch (e) {
    say(String(e));
  }
}

// ---- main area -------------------------------------------------------------------------------

function renderMain() {
  const title = mode === "home" ? "Brennstudio" : MODES.find((m) => m.id === mode)!.title;
  document.title = mode === "home" ? title : `${title} – Brennstudio`;
  main.replaceChildren(
    mode === "home"
      ? renderHome(setMode)
      : el("div", { class: `mode${running ? " is-running" : ""}` }, el("h2", {}, title), renderMode(mode, host, renderMain)),
  );
}

function setMode(m: Mode) {
  mode = m;
  renderAll();
}

function renderAll() {
  renderMenu();
  renderDriveBar();
  renderMain();
  renderBottom();
}

// ---- menu bar --------------------------------------------------------------------------------

type Entry = { label: string; run?: () => void; checked?: boolean; sep?: boolean };

function menus(): [string, Entry[]][] {
  return [
    [
      "Datei",
      [
        { label: "Graph laden …", run: () => void loadGraph() },
        { label: "Einstellungen …", run: openSettings },
        { sep: true, label: "" },
        { label: "Beenden", run: () => void getCurrentWindow().close() },
      ],
    ],
    [
      "Ansicht",
      [
        { label: "Vorgang", run: () => ((tab = "status"), renderBottom()), checked: tab === "status" },
        { label: "Graph", run: () => ((tab = "graph"), renderBottom()), checked: tab === "graph" },
        { sep: true, label: "" },
        { label: "Protokoll leeren", run: () => logList.replaceChildren() },
      ],
    ],
    ["Modus", [{ label: "Start", run: () => setMode("home"), checked: mode === "home" }, ...MODES.map((m) => ({ label: m.title, run: () => setMode(m.id), checked: mode === m.id }))]],
    [
      "Werkzeuge",
      [
        { label: "Volume-Label ändern …", run: openLabelTool },
        { label: "Layerwechsel (IFO-Inspektor) …", run: () => void openIfoTool() },
        { label: "Layout-Dateien schreiben …", run: openSidecarTool },
        { label: "Laufwerks- und Disc-Info …", run: () => void openDriveInfo(driveId || null) },
        { label: "Auto-Geschwindigkeit …", run: () => openAutoSpeed(settings.burn_studio, () => void saveStudioSettings()) },
        { sep: true, label: "" },
        { label: "Schublade auswerfen", run: () => void tray("eject") },
        { label: "Schublade einfahren", run: () => void tray("load") },
        { label: "Schublade sperren", checked: locked, run: () => void toggleLock() },
      ],
    ],
    ["Hilfe", [{ label: "Über das Brennstudio", run: about }]],
  ];
}

function renderMenu() {
  menubar.replaceChildren(
    ...menus().map(([name, entries]) => {
      const drop = el("div", { class: "menu-drop", hidden: true });
      const item = el("div", { class: "menu-item" }, el("button", { class: "menu-btn", onclick: (e: Event) => { e.stopPropagation(); const open = drop.hidden; closeMenus(); drop.hidden = !open; } }, name), drop);
      for (const en of entries) {
        drop.append(
          en.sep
            ? el("hr")
            : el("button", { class: "menu-entry", onclick: () => { closeMenus(); en.run?.(); } }, el("span", { class: "check" }, en.checked ? "✓" : ""), en.label),
        );
      }
      return item;
    }),
  );
}

const closeMenus = () => menubar.querySelectorAll<HTMLElement>(".menu-drop").forEach((d) => (d.hidden = true));
document.addEventListener("click", closeMenus);

function openSettings() {
  openStudioSettings(settings, (s) => {
    settings = s;
    say("Einstellungen gespeichert.");
    void refreshDrive();
  });
}

async function saveStudioSettings() {
  try {
    settings = (await saveSettings(settings)) as api.FullSettings;
    say("Gespeichert.");
  } catch (e) {
    say(String(e));
  }
}

async function toggleLock() {
  try {
    await api.lock(driveId, !locked);
    locked = !locked;
    renderMenu();
    say(locked ? "Schublade gesperrt." : "Schublade entsperrt.");
  } catch (e) {
    say(String(e));
  }
}

async function loadGraph() {
  const p = await open({ multiple: false, defaultPath: await api.graphFolder(), filters: [{ name: "Graph-Dateien", extensions: ["ibg"] }] });
  if (typeof p !== "string") return;
  try {
    graph.show(await api.graphLoad(p), p.split(/[\\/]/).pop() ?? p);
    tab = "graph";
    renderBottom();
  } catch (e) {
    say(String(e));
  }
}

function about() {
  modal(
    "Brennstudio",
    el(
      "div",
      {},
      el("p", {}, "Brennen, Lesen, Überprüfen und Image-Bau für DVD und Blu-ray auf Basis der ConvertX-Rust-Engine."),
      el("ul", { class: "hint" },
        el("li", {}, "Book-Typ ändern (Bitsetting) ist herstellerspezifisch und nicht umgesetzt."),
        el("li", {}, "„SPTI Direkt“ (eigene Schreibschleife) wurde nur gegen ein simuliertes Laufwerk getestet."),
        el("li", {}, "Kein CSS/AACS-Entschlüsseln beim Lesen."),
        el("li", {}, "Graph-Dateien (.ibg) haben keine ImgBurn-Prüfsummenzeile.")),
    ),
  );
}

// ---- events and start --------------------------------------------------------------------------

async function init() {
  settings = (await getSettings()) as api.FullSettings;
  buildStatusPanel();
  root.append(menubar, driveBar, main, bottom, note);
  await api.onProgress(setProgress);
  await api.onGraph((s) => graph.push(s));
  await api.onLog((m) => logLine(m));
  await api.onDone((o) => {
    running = false;
    cancelBtn.disabled = true;
    const kind = o.cancelled ? "warn" : o.ok ? "ok" : "error";
    logLine(o.message, kind);
    o.files.forEach((f) => logLine(`Datei: ${f}`));
    if (o.layoutFile) logLine(`Layout-Datei: ${o.layoutFile}`);
    if (o.md5) logLine(`MD5: ${o.md5}`);
    if (o.graph) logLine(`Graph: ${o.graph}`);
    o.details.forEach((d) => logLine(d, "warn"));
    if (o.badSectors) logLine(`${o.badSectors.toLocaleString("de-DE")} Sektoren betroffen`, "warn");
    if (o.ok) setProgress(null);
    phaseLabel.textContent = o.cancelled ? "Abgebrochen" : o.ok ? "Fertig" : "Fehler";
    renderAll();
    void refreshDrive();
    if (o.ok && settings.burn_studio.general.on_complete === "close") void getCurrentWindow().close();
  });
  void getCurrentWindow().onCloseRequested((ev) => {
    if (running) {
      ev.preventDefault();
      void api.cancel();
      say("Vorgang wird abgebrochen – danach kann das Fenster geschlossen werden.");
    }
  });
  renderAll();
  await refreshDrive();
}

void init().catch((e) => {
  root.textContent = `Brennstudio konnte nicht gestartet werden: ${String(e)}`;
});
