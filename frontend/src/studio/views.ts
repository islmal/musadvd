// The six modes of the Burn Studio window: home, write image, write files, read disc, build image,
// verify disc and Discovery test burn.
import { open, save } from "@tauri-apps/plugin-dialog";
import * as api from "./api";
import { openLayerBreak } from "./layerbreak";
import { el, fmtBytes, fmtSectors, modal } from "./ui";

export type Mode = "home" | "write-image" | "write-files" | "read" | "build" | "verify" | "discovery";

export const MODES: { id: Exclude<Mode, "home">; title: string; text: string; icon: string }[] = [
  { id: "write-image", title: "Image auf Disc schreiben", text: "ISO, BIN/CUE, DVD- oder MDS-Datei brennen", icon: "💿" },
  { id: "write-files", title: "Dateien/Ordner auf Disc schreiben", text: "Image im Hintergrund bauen und direkt brennen", icon: "🗂" },
  { id: "read", title: "Disc in Image-Datei lesen", text: "Disc als ISO sichern (mit MD5, Layout-Datei)", icon: "📀" },
  { id: "build", title: "Image aus Dateien/Ordnern erstellen", text: "UDF/ISO/Joliet-Image, DVD-Video oder Blu-ray", icon: "🛠" },
  { id: "verify", title: "Disc überprüfen", text: "Mit einem Image vergleichen oder auf Lesbarkeit prüfen", icon: "✅" },
  { id: "discovery", title: "Discovery (Testbrand)", text: "Schreibqualität und Geschwindigkeit messen", icon: "📈" },
];

/** What the views need from the window shell. */
export interface Host {
  settings(): api.FullSettings;
  drive(): api.DriveInfo | null;
  view(): api.DriveView | null;
  running(): boolean;
  /** Starts an operation: clears log and graph, shows the status panel. */
  run(what: string, start: () => Promise<void>): void;
  /** The speed selection of the drive bar (`auto`, `max`, `8`). */
  speed(): string;
  refreshDrive(): void;
  note(message: string): void;
}

const IMAGE_EXT = ["iso", "cue", "dvd", "mds"];
const baseName = (p: string) => p.split(/[\\/]/).pop() ?? p;

// ---- drive bar -------------------------------------------------------------------------------

const oneX = (media: api.MediaInfo) => (media.media_type === "bd25" || media.media_type === "bd50" ? 4_495_500 / 2048 : 1_385_000 / 2048);

export function speedOptions(media: api.MediaInfo | null): [string, string][] {
  const opts: [string, string][] = [["auto", "Auto"], ["max", "Maximal"]];
  if (media?.present) {
    const seen = new Set<string>();
    for (const s of media.speeds) {
      // The exact value goes to the engine (8x is reported as ~8.2x of the nominal rate).
      const x = s / oneX(media);
      const value = x.toFixed(1).replace(/\.0$/, "");
      if (!seen.has(value)) {
        seen.add(value);
        opts.push([value, `${Math.abs(x - Math.round(x)) < 0.3 ? Math.round(x) : value}x`]);
      }
    }
  }
  return opts;
}

export function mediaText(v: api.DriveView | null): string {
  const m = v?.media;
  if (!m) return v?.mediaError ?? "—";
  if (!m.present) return "keine Disc";
  const kind = m.blank ? "leer" : m.rewritable ? "beschrieben (wiederbeschreibbar)" : "beschrieben";
  const id = v?.details?.media_id ? ` · ${v.details.media_id}` : "";
  return `${m.physical}${id} · ${kind}${m.blank ? `, frei ${fmtSectors(m.free_sectors)}` : ""}${m.writable ? "" : " · nicht beschreibbar"}`;
}

// ---- file list (build + write files) ---------------------------------------------------------

const CAPACITY: [string, string, number][] = [
  ["disc", "Disc im Laufwerk", 0],
  ["dvd5", "DVD-5 (4,7 GB)", 2_295_104],
  ["dvd9", "DVD-9 (8,5 GB)", 4_173_824],
  ["bd25", "BD-25", 12_219_392],
  ["bd50", "BD-50", 24_438_784],
];

interface BuildState {
  paths: api.PathInfo[];
  label: string;
  answer: boolean | null;
  layerBreak: api.BuildInput["layerBreak"];
  plan: api.PlanInfo | null;
  planError: string | null;
  target: string;
  seq: number;
}

const bs: BuildState = { paths: [], label: "", answer: null, layerBreak: null, plan: null, planError: null, target: "disc", seq: 0 };

const buildInput = (): api.BuildInput => ({
  sources: bs.paths.map((p) => ({ path: p.path, dest: "" })),
  label: bs.label,
  answer: bs.answer,
  layerBreak: bs.layerBreak,
});

const CONTENT_BADGE: Record<string, string> = { dvdvideo: "DVD-Video", bluray: "Blu-ray", data: "Daten" };

function renderFileList(host: Host, rerender: () => void): HTMLElement {
  const root = el("div", { class: "filelist" });
  const replan = async () => {
    const seq = ++bs.seq;
    if (bs.paths.length === 0) {
      bs.plan = null;
      bs.planError = null;
      return rerender();
    }
    try {
      const plan = await api.buildPlan(buildInput());
      if (seq !== bs.seq) return;
      bs.plan = plan;
      bs.planError = null;
    } catch (e) {
      if (seq !== bs.seq) return;
      bs.plan = null;
      bs.planError = String(e);
    }
    rerender();
  };
  const addPaths = async (picked: string[]) => {
    const infos = await api.inspect(picked);
    for (const i of infos) if (!bs.paths.some((p) => p.path === i.path)) bs.paths.push(i);
    bs.answer = null;
    bs.layerBreak = null;
    rerender();
    void replan();
  };
  const toolbar = el(
    "div",
    { class: "studio-toolbar" },
    el("button", { class: "btn", onclick: async () => { const p = await open({ multiple: true }); if (Array.isArray(p)) void addPaths(p); } }, "＋ Dateien"),
    el("button", { class: "btn", onclick: async () => { const p = await open({ directory: true, multiple: true }); if (Array.isArray(p)) void addPaths(p); } }, "＋ Ordner"),
    el("button", { class: "btn", disabled: bs.paths.length === 0, onclick: () => { bs.paths = []; bs.plan = null; bs.answer = null; bs.layerBreak = null; rerender(); } }, "Alle entfernen"),
  );
  const rows = bs.paths.map((p) =>
    el(
      "tr",
      {},
      el("td", {}, `${p.isDir ? "📁" : "📄"} ${p.name}`, p.content && p.content !== "data" ? el("span", { class: "badge" }, CONTENT_BADGE[p.content]) : null),
      el("td", { class: "num" }, p.isDir ? `${p.files} Dateien` : ""),
      el("td", { class: "num" }, fmtBytes(p.bytes)),
      el("td", {}, el("button", { class: "btn icon", title: "Entfernen", onclick: () => { bs.paths = bs.paths.filter((x) => x !== p); bs.answer = null; bs.layerBreak = null; rerender(); void replan(); } }, "✕")),
    ),
  );
  const list = rows.length
    ? el("table", { class: "studio-table" }, el("tbody", {}, ...rows))
    : el("div", { class: "empty" }, "Noch nichts ausgewählt. Dateien oder Ordner hinzufügen – ein einzelner VIDEO_TS-/BDMV-Ordner wird als Disc erkannt.");

  // Fill gauge against the chosen target.
  const media = host.view()?.media;
  const cap = bs.target === "disc" ? (media?.present ? (media.blank ? media.free_sectors : media.total_sectors) : 0) : CAPACITY.find((c) => c[0] === bs.target)![2];
  const sectors = bs.plan?.sectors ?? 0;
  const pct = cap > 0 ? Math.min(100, (sectors / cap) * 100) : 0;
  const over = cap > 0 && sectors > cap;
  const gauge = el(
    "div",
    { class: "fill" },
    el(
      "div",
      { class: "studio-toolbar" },
      el("label", {}, "Zielmedium: ", el("select", { onchange: (e: Event) => { bs.target = (e.target as HTMLSelectElement).value; rerender(); } }, ...CAPACITY.map(([v, t]) => el("option", { value: v, selected: bs.target === v }, t)))),
      el("span", { class: "hint" }, cap > 0 ? `${fmtSectors(sectors)} von ${fmtSectors(cap)}` : sectors ? fmtSectors(sectors) : "—"),
    ),
    el("div", { class: "fill-bar" }, el("div", { class: "fill-value", style: `width:${pct}%${over ? ";background:var(--bad)" : ""}` })),
  );

  const info = el("div", { class: "plan-info" });
  if (bs.planError) info.append(el("p", { class: "layout-errors" }, bs.planError));
  const plan = bs.plan;
  if (plan?.needsAnswer) {
    const what = CONTENT_BADGE[plan.needsAnswer];
    info.append(
      el(
        "div",
        { class: "prompt" },
        el("strong", {}, `Der Ordner sieht nach ${what} aus.`),
        el("span", {}, ` Als ${what}-Disc aufbauen (Dateireihenfolge, Layout) oder als normale Daten?`),
        el("button", { class: "btn primary", onclick: () => { bs.answer = true; void replan(); } }, `Als ${what}`),
        el("button", { class: "btn", onclick: () => { bs.answer = false; void replan(); } }, "Als Daten"),
      ),
    );
  } else if (plan) {
    info.append(el("p", {}, el("span", { class: "badge" }, plan.contentName), ` Label ${plan.label} · ${fmtSectors(plan.sectors)}`));
    if (plan.layerBreak) info.append(el("p", {}, `Layerwechsel bei Sektor ${plan.layerBreak.toLocaleString("de-DE")}`));
    if (plan.content === "dvdvideo" && bs.paths.length === 1) {
      info.append(
        el(
          "div",
          { class: "studio-toolbar" },
          el("button", { class: "btn", onclick: () => void openLayerBreak(bs.paths[0].path, (c) => { bs.layerBreak = { l0Sectors: c.l0_sectors, vts: c.vts, padding: c.padding_sectors }; void replan(); }) }, "Layerwechsel wählen …"),
          bs.layerBreak ? el("button", { class: "btn", onclick: () => { bs.layerBreak = null; void replan(); } }, "Automatisch") : null,
        ),
      );
    }
    plan.warnings.forEach((w) => info.append(el("p", { class: "hint" }, `⚠ ${w}`)));
    if (plan.skipped.length) {
      info.append(
        el(
          "details",
          {},
          el("summary", {}, `${plan.skipped.length} Dateien übersprungen`),
          el("ul", { class: "log" }, ...plan.skipped.slice(0, 200).map((s) => el("li", {}, `${s.path} (${s.reason})`))),
        ),
      );
    }
  }
  const label = el("input", { type: "text", value: bs.label, placeholder: plan?.label || "Volume-Label (leer = automatisch)", maxLength: 32, spellcheck: false });
  label.addEventListener("change", () => { bs.label = label.value; void replan(); });
  root.append(toolbar, list, gauge, el("label", { class: "opt-row" }, el("span", { class: "opt-label" }, "Volume-Label"), label), info);
  if (bs.paths.length && !bs.plan && !bs.planError) void replan();
  return root;
}

// ---- views -----------------------------------------------------------------------------------

function actionBar(...buttons: HTMLElement[]) {
  return el("div", { class: "studio-actions" }, ...buttons);
}

function pathInput(initial: string, placeholder: string, pick: () => Promise<string | null>) {
  const input = el("input", { type: "text", value: initial, placeholder, spellcheck: false });
  const row = el("span", { class: "opt-input wide" }, input, el("button", { class: "btn icon", onclick: async () => { const p = await pick(); if (p) input.value = p; } }, "…"));
  return { input, row };
}

const remembered = { image: "", output: "", verifyImage: "", discoverySectors: "", erase: false, verifyMode: "image" };

function viewWriteImage(host: Host): HTMLElement {
  const { input, row } = pathInput(remembered.image, "Image-Datei (.iso, .cue, .dvd, .mds)", async () => {
    const p = await open({ multiple: false, filters: [{ name: "Disc-Images", extensions: IMAGE_EXT }] });
    return typeof p === "string" ? p : null;
  });
  input.addEventListener("change", () => (remembered.image = input.value));
  row.querySelector("button")!.addEventListener("click", () => setTimeout(() => (remembered.image = input.value), 0));
  const erase = el("input", { type: "checkbox", checked: remembered.erase, onchange: (e: Event) => (remembered.erase = (e.target as HTMLInputElement).checked) });
  const go = el("button", { class: "btn primary big" }, "Brennen");
  go.addEventListener("click", () => {
    const d = host.drive();
    if (!d) return host.note("Bitte ein Laufwerk wählen.");
    if (!input.value.trim()) return host.note("Bitte ein Image wählen.");
    const image = input.value.trim();
    host.run(`Image schreiben: ${baseName(image)}`, () => api.writeImage(d.id, image, host.speed(), remembered.erase));
  });
  return el(
    "div",
    {},
    el("div", { class: "opt-group" }, el("label", { class: "opt-row" }, el("span", { class: "opt-label" }, "Image"), row)),
    el("label", { class: "opt-check" }, erase, el("span", {}, "Wiederbeschreibbare Disc mit Inhalt überschreiben")),
    el("p", { class: "hint" }, "Ein .dvd/.mds neben dem Image liefert den Layerwechsel (Dual Layer); eine .cue wird mit einer Datenspur (MODE1/2048 oder /2352) gelesen."),
    actionBar(go),
  );
}

function viewFiles(host: Host, mode: "write-files" | "build", rerender: () => void): HTMLElement {
  const wrap = el("div", {}, renderFileList(host, rerender));
  if (mode === "write-files") {
    const erase = el("input", { type: "checkbox", checked: remembered.erase, onchange: (e: Event) => (remembered.erase = (e.target as HTMLInputElement).checked) });
    const go = el("button", { class: "btn primary big", disabled: !bs.plan }, "Brennen");
    go.addEventListener("click", () => {
      const d = host.drive();
      if (!d) return host.note("Bitte ein Laufwerk wählen.");
      host.run("Dateien schreiben", () => api.writeFiles(d.id, buildInput(), host.speed(), remembered.erase));
    });
    wrap.append(
      el("label", { class: "opt-check" }, erase, el("span", {}, "Wiederbeschreibbare Disc mit Inhalt überschreiben")),
      el("p", { class: "hint" }, host.settings().burn_studio.write.verify ? "Mit Überprüfung wird das Image zuerst als temporäre Datei erstellt (Platz im Temp-Ordner nötig)." : "Das Image wird beim Brennen direkt in den Ringpuffer gebaut (keine temporäre Datei)."),
      actionBar(go),
    );
  } else {
    const { input, row } = pathInput(remembered.output, "Ausgabe-Image (.iso)", async () => {
      const p = await save({ filters: [{ name: "ISO-Image", extensions: ["iso"] }], defaultPath: bs.plan?.label ? `${bs.plan.label}.iso` : "image.iso" });
      return p ?? null;
    });
    input.addEventListener("change", () => (remembered.output = input.value));
    row.querySelector("button")!.addEventListener("click", () => setTimeout(() => (remembered.output = input.value), 0));
    const go = el("button", { class: "btn primary big", disabled: !bs.plan }, "Image erstellen");
    go.addEventListener("click", () => {
      const out = input.value.trim();
      if (!out) return host.note("Bitte eine Ausgabedatei wählen.");
      remembered.output = out;
      host.run("Image erstellen", () => api.build(buildInput(), out));
    });
    wrap.append(el("div", { class: "opt-group" }, el("label", { class: "opt-row" }, el("span", { class: "opt-label" }, "Ausgabe"), row)), actionBar(go));
  }
  return wrap;
}

function viewRead(host: Host): HTMLElement {
  const { input, row } = pathInput(remembered.output, "Ziel-Image (.iso)", async () => {
    const p = await save({ filters: [{ name: "ISO-Image", extensions: ["iso"] }], defaultPath: "disc.iso" });
    return p ?? null;
  });
  input.addEventListener("change", () => (remembered.output = input.value));
  row.querySelector("button")!.addEventListener("click", () => setTimeout(() => (remembered.output = input.value), 0));
  const go = el("button", { class: "btn primary big" }, "Lesen");
  go.addEventListener("click", () => {
    const d = host.drive();
    if (!d) return host.note("Bitte ein Laufwerk wählen.");
    const out = input.value.trim();
    if (!out) return host.note("Bitte eine Ausgabedatei wählen.");
    remembered.output = out;
    host.run("Disc lesen", () => api.readDisc(d.id, out));
  });
  const r = host.settings().burn_studio.read;
  return el(
    "div",
    {},
    el("div", { class: "opt-group" }, el("label", { class: "opt-row" }, el("span", { class: "opt-label" }, "Ziel"), row)),
    el("p", { class: "hint" }, `Teilen: ${r.split}, Layout-Datei: ${r.layout_file}, Lesefehler ${r.ignore_read_errors ? "ignorieren" : "abbrechen"} (Einstellungen ▸ Lesen). Kopiergeschützte Discs (CSS/AACS) werden nicht entschlüsselt.`),
    actionBar(go),
  );
}

function viewVerify(host: Host, rerender: () => void): HTMLElement {
  const compare = remembered.verifyMode === "image";
  const { input, row } = pathInput(remembered.verifyImage, "Image-Datei", async () => {
    const p = await open({ multiple: false, filters: [{ name: "Disc-Images", extensions: IMAGE_EXT }] });
    return typeof p === "string" ? p : null;
  });
  input.addEventListener("change", () => (remembered.verifyImage = input.value));
  row.querySelector("button")!.addEventListener("click", () => setTimeout(() => (remembered.verifyImage = input.value), 0));
  const radio = (value: string, label: string) =>
    el("label", { class: "opt-check" }, el("input", { type: "radio", name: "vmode", checked: remembered.verifyMode === value, onchange: () => { remembered.verifyMode = value; rerender(); } }), el("span", {}, label));
  const go = el("button", { class: "btn primary big" }, "Überprüfen");
  go.addEventListener("click", () => {
    const d = host.drive();
    if (!d) return host.note("Bitte ein Laufwerk wählen.");
    const image = compare ? input.value.trim() : "";
    if (compare && !image) return host.note("Bitte ein Image wählen.");
    remembered.verifyImage = image || remembered.verifyImage;
    host.run(compare ? "Disc mit Image vergleichen" : "Lesbarkeit prüfen", () => api.verify(d.id, compare ? image : null));
  });
  return el(
    "div",
    {},
    el("div", { class: "opt-group" }, radio("image", "Mit Image vergleichen"), compare ? el("label", { class: "opt-row" }, el("span", { class: "opt-label" }, "Image"), row) : null, radio("read", "Nur Lesbarkeit prüfen (jeder Sektor)")),
    actionBar(go),
  );
}

function viewDiscovery(host: Host): HTMLElement {
  const sectors = el("input", { type: "number", min: "1", placeholder: "ganze Disc", value: remembered.discoverySectors });
  sectors.addEventListener("change", () => (remembered.discoverySectors = sectors.value));
  const go = el("button", { class: "btn primary big" }, "Testbrand starten");
  go.addEventListener("click", () => {
    const d = host.drive();
    if (!d) return host.note("Bitte ein Laufwerk wählen.");
    const n = sectors.value ? Math.round(Number(sectors.value)) : null;
    modal(
      "Discovery starten?",
      el("p", {}, "Es werden Nullen auf die eingelegte, leere Disc geschrieben. Die Disc ist danach verbraucht (nur wiederbeschreibbare Medien lassen sich löschen). Im Modus „IMAPI“ ist das ein echter Brand – ein echter Testmodus existiert nur mit „SPTI Direkt“ auf DVD-R/-RW."),
      [{ label: "Abbrechen" }, { label: "Brennen", primary: true, onClick: () => host.run("Discovery", () => api.discovery(d.id, host.speed(), n)) }],
    );
  });
  return el(
    "div",
    {},
    el("div", { class: "opt-group" }, el("label", { class: "opt-row" }, el("span", { class: "opt-label" }, "Sektoren"), sectors, el("small", { class: "opt-hint" }, "Leer = gesamte freie Kapazität."))),
    el("p", { class: "hint" }, "Der Graph zeigt Geschwindigkeit und Puffer über die ganze Disc."),
    actionBar(go),
  );
}

export function renderHome(go: (m: Mode) => void): HTMLElement {
  return el(
    "div",
    { class: "home" },
    el("h2", {}, "Was möchten Sie tun?"),
    el(
      "div",
      { class: "cards" },
      ...MODES.map((m) => el("button", { class: "card", onclick: () => go(m.id) }, el("span", { class: "card-icon" }, m.icon), el("strong", {}, m.title), el("span", { class: "hint" }, m.text))),
    ),
  );
}

export function renderMode(mode: Exclude<Mode, "home">, host: Host, rerender: () => void): HTMLElement {
  switch (mode) {
    case "write-image":
      return viewWriteImage(host);
    case "write-files":
    case "build":
      return viewFiles(host, mode, rerender);
    case "read":
      return viewRead(host);
    case "verify":
      return viewVerify(host, rerender);
    case "discovery":
      return viewDiscovery(host);
  }
}

/** Modes that need a drive (everything except building an image). */
export const needsDrive = (m: Mode) => m !== "build" && m !== "home";
