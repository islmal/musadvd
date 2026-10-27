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
import { fmtNumber, t as tr } from "./i18n";

const PHASES: Record<string, string> = {
  preparing: tr("studio.main.vorbereiten"),
  formatting: tr("studio.main.formatieren"),
  writing: tr("studio.main.schreiben"),
  finalizing: tr("studio.main.abschliessen"),
  verifying: tr("studio.main.ueberpruefen"),
  reading: tr("studio.main.lesen"),
  building: tr("studio.main.imageErstellen"),
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
const phaseLabel = el("span", {}, tr("studio.main.bereit"));
const pctLabel = el("span", {}, "");
const cancelBtn = el("button", { class: "btn danger", disabled: true, onclick: () => void api.cancel() }, tr("studio.main.abbrechen"));
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
    if (running) return say(tr("studio.main.esLaeuftBereitsEin"));
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
    phaseLabel.textContent = running ? tr("studio.main.starte") : tr("studio.main.bereit");
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
    cell(tr("studio.main.geschwindigkeit"), `${p.speedX.toFixed(1)}x`),
    cell(tr("studio.main.sektor"), `${fmtNumber(p.lba)} / ${fmtNumber(p.total)}`),
    cell(tr("studio.main.hauptpuffer"), `${p.ring} %`),
    cell(tr("studio.main.laufwerkspuffer"), p.device === null ? "—" : `${p.device} %`),
    cell(tr("studio.main.verbleibend"), fmtTime(p.etaSecs)),
  );
}

function renderBottom() {
  const tabs = el(
    "div",
    { class: "tabs" },
    ...(["status", "graph"] as const).map((t) =>
      el("button", { class: `tab${tab === t ? " active" : ""}`, onclick: () => ((tab = t), renderBottom()) }, t === "status" ? tr("studio.main.vorgang") : tr("studio.main.graph")),
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
    el("div", { class: "meters" }, meter(tr("studio.main.hauptpuffer"), ringBar), meter(tr("studio.main.laufwerkspuffer"), deviceBar)),
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
      ...(drives.length ? drives.map((d) => el("option", { value: d.id, selected: d.id === driveId }, d.name)) : [el("option", {}, tr("studio.main.keinLaufwerkGefunden"))]),
    ),
    el("button", { class: "btn icon", title: tr("studio.main.aktualisieren"), onclick: () => void refreshDrive() }, "↻"),
    el("span", { class: "media-text" }, driveId ? mediaText(view) : "—"),
    el("span", { class: "spacer" }),
    el("label", {}, tr("studio.main.geschwindigkeit2"), el("select", { onchange: (e: Event) => (speed = (e.target as HTMLSelectElement).value) }, ...speeds.map(([v, t]) => el("option", { value: v, selected: v === speed }, t)))),
    el("button", { class: "btn", disabled: !driveId, onclick: () => void tray("eject") }, tr("studio.main.auswerfen")),
    el("button", { class: "btn", disabled: !driveId, onclick: () => void tray("load") }, tr("studio.main.einfahren")),
    el("button", { class: "btn", disabled: !driveId, onclick: () => void openDriveInfo(driveId || null) }, tr("studio.main.info")),
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
  const title = mode === "home" ? tr("studio.main.brennstudio") : MODES.find((m) => m.id === mode)!.title;
  document.title = mode === "home" ? title : tr("studio.main.brennstudio2", { title });
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
      tr("studio.main.datei"),
      [
        { label: tr("studio.main.graphLaden"), run: () => void loadGraph() },
        { label: tr("studio.main.einstellungen"), run: openSettings },
        { sep: true, label: "" },
        { label: tr("studio.main.beenden"), run: () => void getCurrentWindow().close() },
      ],
    ],
    [
      tr("studio.main.ansicht"),
      [
        { label: tr("studio.main.vorgang"), run: () => ((tab = "status"), renderBottom()), checked: tab === "status" },
        { label: tr("studio.main.graph"), run: () => ((tab = "graph"), renderBottom()), checked: tab === "graph" },
        { sep: true, label: "" },
        { label: tr("studio.main.protokollLeeren"), run: () => logList.replaceChildren() },
      ],
    ],
    [tr("studio.main.modus"), [{ label: tr("studio.main.start"), run: () => setMode("home"), checked: mode === "home" }, ...MODES.map((m) => ({ label: m.title, run: () => setMode(m.id), checked: mode === m.id }))]],
    [
      tr("studio.main.werkzeuge"),
      [
        { label: tr("studio.main.volumeLabelAendern"), run: openLabelTool },
        { label: tr("studio.main.layerwechselIfoInspektor"), run: () => void openIfoTool() },
        { label: tr("studio.main.layoutDateienSchreiben"), run: openSidecarTool },
        { label: tr("studio.main.laufwerksUndDiscInfo"), run: () => void openDriveInfo(driveId || null) },
        { label: tr("studio.main.autoGeschwindigkeit"), run: () => openAutoSpeed(settings.burn_studio, () => void saveStudioSettings()) },
        { sep: true, label: "" },
        { label: tr("studio.main.schubladeAuswerfen"), run: () => void tray("eject") },
        { label: tr("studio.main.schubladeEinfahren"), run: () => void tray("load") },
        { label: tr("studio.main.schubladeSperren"), checked: locked, run: () => void toggleLock() },
      ],
    ],
    [tr("studio.main.hilfe"), [{ label: tr("studio.main.ueberDasBrennstudio"), run: about }]],
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
    say(tr("studio.main.einstellungenGespeichert"));
    void refreshDrive();
  });
}

async function saveStudioSettings() {
  try {
    settings = (await saveSettings(settings)) as api.FullSettings;
    say(tr("studio.main.gespeichert"));
  } catch (e) {
    say(String(e));
  }
}

async function toggleLock() {
  try {
    await api.lock(driveId, !locked);
    locked = !locked;
    renderMenu();
    say(locked ? tr("studio.main.schubladeGesperrt") : tr("studio.main.schubladeEntsperrt"));
  } catch (e) {
    say(String(e));
  }
}

async function loadGraph() {
  const p = await open({ multiple: false, defaultPath: await api.graphFolder(), filters: [{ name: tr("studio.main.graphDateien"), extensions: ["ibg"] }] });
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
    tr("studio.main.brennstudio"),
    el(
      "div",
      {},
      el("p", {}, tr("studio.main.brennenLesenUeberpruefenUnd")),
      el("ul", { class: "hint" },
        el("li", {}, tr("studio.main.bookTypAendernBitsetting")),
        el("li", {}, tr("studio.main.sptiDirektEigeneSchreibschleife")),
        el("li", {}, tr("studio.main.keinCssAacsEntschluesseln")),
        el("li", {}, tr("studio.main.graphDateienIbgHaben"))),
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
    o.files.forEach((f) => logLine(tr("studio.main.datei2", { f })));
    if (o.layoutFile) logLine(tr("studio.main.layoutDatei", { layoutFile: o.layoutFile }));
    if (o.md5) logLine(tr("studio.main.md5", { md5: o.md5 }));
    if (o.graph) logLine(tr("studio.main.graph2", { graph: o.graph }));
    o.details.forEach((d) => logLine(d, "warn"));
    if (o.badSectors) logLine(tr("studio.main.sektorenBetroffen", { o: fmtNumber(o.badSectors) }), "warn");
    if (o.ok) setProgress(null);
    phaseLabel.textContent = o.cancelled ? tr("studio.main.abgebrochen") : o.ok ? tr("studio.main.fertig") : tr("studio.main.fehler");
    renderAll();
    void refreshDrive();
    if (o.ok && settings.burn_studio.general.on_complete === "close") void getCurrentWindow().close();
  });
  void getCurrentWindow().onCloseRequested((ev) => {
    if (running) {
      ev.preventDefault();
      void api.cancel();
      say(tr("studio.main.vorgangWirdAbgebrochenDanach"));
    }
  });
  renderAll();
  await refreshDrive();
}

void init().catch((e) => {
  root.textContent = tr("studio.main.brennstudioKonnteNichtGestartet", { e: String(e) });
});
