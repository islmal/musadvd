// The six modes of the Burn Studio window: home, write image, write files, read disc, build image,
// verify disc and Discovery test burn.
import { open, save } from "@tauri-apps/plugin-dialog";
import * as api from "./api";
import { openLayerBreak } from "./layerbreak";
import { speedChoices } from "./speeds";
import { el, fmtBytes, fmtSectors, modal } from "./ui";
import { fmtNumber, t } from "../i18n";

export type Mode = "home" | "write-image" | "write-files" | "read" | "build" | "verify" | "discovery";

export const MODES: { id: Exclude<Mode, "home">; title: string; text: string; icon: string }[] = [
  { id: "write-image", title: t("studio.views.imageAufDiscSchreiben"), text: t("studio.views.isoBinCueDvd"), icon: "💿" },
  { id: "write-files", title: t("studio.views.dateienOrdnerAufDisc"), text: t("studio.views.imageImHintergrundBauen"), icon: "🗂" },
  { id: "read", title: t("studio.views.discInImageDatei"), text: t("studio.views.discAlsIsoSichern"), icon: "📀" },
  { id: "build", title: t("studio.views.imageAusDateienOrdnern"), text: t("studio.views.udfIsoJolietImage"), icon: "🛠" },
  { id: "verify", title: t("studio.views.discUeberpruefen"), text: t("studio.views.mitEinemImageVergleichen"), icon: "✅" },
  { id: "discovery", title: t("studio.views.discoveryTestbrand"), text: t("studio.views.schreibqualitaetUndGeschwindigkeitMessen"), icon: "📈" },
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
  const opts: [string, string][] = [["auto", t("studio.views.auto")], ["max", t("studio.views.maximal")]];
  const reportedX: number[] = [];
  if (media?.present) {
    for (const s of media.speeds) {
      reportedX.push(s / oneX(media));
    }
  }
  opts.push(...speedChoices(reportedX));
  return opts;
}

export function mediaText(v: api.DriveView | null): string {
  const m = v?.media;
  if (!m) return v?.mediaError ?? "—";
  if (!m.present) return t("studio.views.keineDisc");
  const kind = m.blank ? t("common.blank") : m.rewritable ? t("common.rewritable") : t("common.written");
  const id = v?.details?.media_id ? ` · ${v.details.media_id}` : "";
  return `${m.physical}${id} · ${kind}${m.blank ? `, ${t("common.free")} ${fmtSectors(m.free_sectors)}` : ""}${m.writable ? "" : ` · ${t("common.notWritable")}`}`;
}

// ---- file list (build + write files) ---------------------------------------------------------

const CAPACITY: [string, string, number][] = [
  ["disc", t("studio.views.discImLaufwerk"), 0],
  ["dvd5", t("studio.views.dvd547"), 2_295_104],
  ["dvd9", t("studio.views.dvd985"), 4_173_824],
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

const CONTENT_BADGE: Record<string, string> = { dvdvideo: t("studio.views.dvdVideo"), bluray: t("studio.views.bluRay"), data: t("studio.views.daten") };

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
    el("button", { class: "btn", onclick: async () => { const p = await open({ multiple: true }); if (Array.isArray(p)) void addPaths(p); } }, t("studio.views.dateien")),
    el("button", { class: "btn", onclick: async () => { const p = await open({ directory: true, multiple: true }); if (Array.isArray(p)) void addPaths(p); } }, t("studio.views.ordner")),
    el("button", { class: "btn", disabled: bs.paths.length === 0, onclick: () => { bs.paths = []; bs.plan = null; bs.answer = null; bs.layerBreak = null; rerender(); } }, t("studio.views.alleEntfernen")),
  );
  const rows = bs.paths.map((p) =>
    el(
      "tr",
      {},
      el("td", {}, `${p.isDir ? "📁" : "📄"} ${p.name}`, p.content && p.content !== "data" ? el("span", { class: "badge" }, CONTENT_BADGE[p.content]) : null),
      el("td", { class: "num" }, p.isDir ? t("studio.views.dateien2", { files: p.files }) : ""),
      el("td", { class: "num" }, fmtBytes(p.bytes)),
      el("td", {}, el("button", { class: "btn icon", title: t("studio.views.entfernen"), onclick: () => { bs.paths = bs.paths.filter((x) => x !== p); bs.answer = null; bs.layerBreak = null; rerender(); void replan(); } }, "✕")),
    ),
  );
  const list = rows.length
    ? el("table", { class: "studio-table" }, el("tbody", {}, ...rows))
    : el("div", { class: "empty" }, t("studio.views.nochNichtsAusgewaehltDateien"));

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
      el("label", {}, t("studio.views.zielmedium"), el("select", { onchange: (e: Event) => { bs.target = (e.target as HTMLSelectElement).value; rerender(); } }, ...CAPACITY.map(([v, t]) => el("option", { value: v, selected: bs.target === v }, t)))),
      el("span", { class: "hint" }, cap > 0 ? t("studio.views.sectorsOf", { sectors: fmtSectors(sectors), cap: fmtSectors(cap) }) : sectors ? fmtSectors(sectors) : "—"),
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
        el("strong", {}, t("studio.views.derOrdnerSiehtNach", { what })),
        el("span", {}, t("studio.views.alsDiscAufbauenDateireihenfolge", { what })),
        el("button", { class: "btn primary", onclick: () => { bs.answer = true; void replan(); } }, t("studio.views.als", { what })),
        el("button", { class: "btn", onclick: () => { bs.answer = false; void replan(); } }, t("studio.views.alsDaten")),
      ),
    );
  } else if (plan) {
    info.append(el("p", {}, el("span", { class: "badge" }, plan.contentName), t("studio.views.label", { label: plan.label, fmtSectors: fmtSectors(plan.sectors) })));
    if (plan.layerBreak) info.append(el("p", {}, t("studio.views.layerwechselBeiSektor", { plan: fmtNumber(plan.layerBreak) })));
    if (plan.content === "dvdvideo" && bs.paths.length === 1) {
      info.append(
        el(
          "div",
          { class: "studio-toolbar" },
          el("button", { class: "btn", onclick: () => void openLayerBreak(bs.paths[0].path, (c) => { bs.layerBreak = { l0Sectors: c.l0_sectors, vts: c.vts, padding: c.padding_sectors }; void replan(); }) }, t("studio.views.layerwechselWaehlen")),
          bs.layerBreak ? el("button", { class: "btn", onclick: () => { bs.layerBreak = null; void replan(); } }, t("studio.views.automatisch")) : null,
        ),
      );
    }
    plan.warnings.forEach((w) => info.append(el("p", { class: "hint" }, `⚠ ${w}`)));
    if (plan.skipped.length) {
      info.append(
        el(
          "details",
          {},
          el("summary", {}, t("studio.views.dateienUebersprungen", { length: plan.skipped.length })),
          el("ul", { class: "log" }, ...plan.skipped.slice(0, 200).map((s) => el("li", {}, `${s.path} (${s.reason})`))),
        ),
      );
    }
  }
  const label = el("input", { type: "text", value: bs.label, placeholder: plan?.label || t("studio.views.volumeLabelLeerAutomatisch"), maxLength: 32, spellcheck: false });
  label.addEventListener("change", () => { bs.label = label.value; void replan(); });
  root.append(toolbar, list, gauge, el("label", { class: "opt-row" }, el("span", { class: "opt-label" }, t("studio.views.volumeLabel")), label), info);
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
  const { input, row } = pathInput(remembered.image, t("studio.views.imageDateiIsoCue"), async () => {
    const p = await open({ multiple: false, filters: [{ name: t("studio.views.discImages"), extensions: IMAGE_EXT }] });
    return typeof p === "string" ? p : null;
  });
  input.addEventListener("change", () => (remembered.image = input.value));
  row.querySelector("button")!.addEventListener("click", () => setTimeout(() => (remembered.image = input.value), 0));
  const erase = el("input", { type: "checkbox", checked: remembered.erase, onchange: (e: Event) => (remembered.erase = (e.target as HTMLInputElement).checked) });
  const go = el("button", { class: "btn primary big" }, t("studio.views.brennen"));
  go.addEventListener("click", async () => {
    const d = host.drive();
    if (!d) return host.note(t("studio.views.bitteEinLaufwerkWaehlen"));
    if (!input.value.trim()) return host.note(t("studio.views.bitteEinImageWaehlen"));
    const image = input.value.trim();
    const startBurn = () => {
      host.run(t("studio.views.imageSchreiben", { baseName: baseName(image) }), () => api.writeImage(d.id, image, host.speed(), remembered.erase));
    };

    const isScrambled = await api.imageScrambled(image).catch(() => null);
    if (isScrambled === true) {
      modal(
        t("studio.views.cssModalTitle"),
        el("div", {}, el("p", {}, t("studio.views.cssModalText"))),
        [
          { label: t("studio.views.abbrechen"), primary: false },
          { label: t("studio.views.trotzdemBrennen"), primary: true, onClick: () => startBurn() },
        ]
      );
      return;
    }
    startBurn();
  });
  return el(
    "div",
    {},
    el("div", { class: "opt-group" }, el("label", { class: "opt-row" }, el("span", { class: "opt-label" }, t("studio.views.image")), row)),
    el("label", { class: "opt-check" }, erase, el("span", {}, t("studio.views.wiederbeschreibbareDiscMitInhalt"))),
    el("p", { class: "hint" }, t("studio.views.einDvdMdsNeben")),
    actionBar(go),
  );
}

function viewFiles(host: Host, mode: "write-files" | "build", rerender: () => void): HTMLElement {
  const wrap = el("div", {}, renderFileList(host, rerender));
  if (mode === "write-files") {
    const erase = el("input", { type: "checkbox", checked: remembered.erase, onchange: (e: Event) => (remembered.erase = (e.target as HTMLInputElement).checked) });
    const go = el("button", { class: "btn primary big", disabled: !bs.plan }, t("studio.views.brennen"));
    go.addEventListener("click", () => {
      const d = host.drive();
      if (!d) return host.note(t("studio.views.bitteEinLaufwerkWaehlen"));
      host.run(t("studio.views.dateienSchreiben"), () => api.writeFiles(d.id, buildInput(), host.speed(), remembered.erase));
    });
    wrap.append(
      el("label", { class: "opt-check" }, erase, el("span", {}, t("studio.views.wiederbeschreibbareDiscMitInhalt"))),
      el("p", { class: "hint" }, host.settings().burn_studio.write.verify ? t("studio.views.mitUeberpruefungWirdDas") : t("studio.views.dasImageWirdBeim")),
      actionBar(go),
    );
  } else {
    const { input, row } = pathInput(remembered.output, t("studio.views.ausgabeImageIso"), async () => {
      const p = await save({ filters: [{ name: t("studio.views.isoImage"), extensions: ["iso"] }], defaultPath: bs.plan?.label ? `${bs.plan.label}.iso` : "image.iso" });
      return p ?? null;
    });
    input.addEventListener("change", () => (remembered.output = input.value));
    row.querySelector("button")!.addEventListener("click", () => setTimeout(() => (remembered.output = input.value), 0));
    const go = el("button", { class: "btn primary big", disabled: !bs.plan }, t("studio.views.imageErstellen"));
    go.addEventListener("click", () => {
      const out = input.value.trim();
      if (!out) return host.note(t("studio.views.bitteEineAusgabedateiWaehlen"));
      remembered.output = out;
      host.run(t("studio.views.imageErstellen"), () => api.build(buildInput(), out));
    });
    wrap.append(el("div", { class: "opt-group" }, el("label", { class: "opt-row" }, el("span", { class: "opt-label" }, t("studio.views.ausgabe")), row)), actionBar(go));
  }
  return wrap;
}

function viewRead(host: Host): HTMLElement {
  const { input, row } = pathInput(remembered.output, t("studio.views.zielImageIso"), async () => {
    const p = await save({ filters: [{ name: t("studio.views.isoImage"), extensions: ["iso"] }], defaultPath: "disc.iso" });
    return p ?? null;
  });
  input.addEventListener("change", () => (remembered.output = input.value));
  row.querySelector("button")!.addEventListener("click", () => setTimeout(() => (remembered.output = input.value), 0));
  const go = el("button", { class: "btn primary big" }, t("studio.views.lesen"));
  go.addEventListener("click", () => {
    const d = host.drive();
    if (!d) return host.note(t("studio.views.bitteEinLaufwerkWaehlen"));
    const out = input.value.trim();
    if (!out) return host.note(t("studio.views.bitteEineAusgabedateiWaehlen"));
    remembered.output = out;
    host.run(t("studio.views.discLesen"), () => api.readDisc(d.id, out));
  });
  const r = host.settings().burn_studio.read;
  return el(
    "div",
    {},
    el("div", { class: "opt-group" }, el("label", { class: "opt-row" }, el("span", { class: "opt-label" }, t("studio.views.ziel")), row)),
    el("p", { class: "hint" }, t("studio.views.teilenLayoutDateiLesefehler", { split: r.split, layout_file: r.layout_file, r: r.ignore_read_errors ? t("studio.views.readErrorsIgnore") : t("studio.views.readErrorsAbort") })),
    actionBar(go),
  );
}

function viewVerify(host: Host, rerender: () => void): HTMLElement {
  const compare = remembered.verifyMode === "image";
  const { input, row } = pathInput(remembered.verifyImage, t("studio.views.imageDatei"), async () => {
    const p = await open({ multiple: false, filters: [{ name: t("studio.views.discImages"), extensions: IMAGE_EXT }] });
    return typeof p === "string" ? p : null;
  });
  input.addEventListener("change", () => (remembered.verifyImage = input.value));
  row.querySelector("button")!.addEventListener("click", () => setTimeout(() => (remembered.verifyImage = input.value), 0));
  const radio = (value: string, label: string) =>
    el("label", { class: "opt-check" }, el("input", { type: "radio", name: "vmode", checked: remembered.verifyMode === value, onchange: () => { remembered.verifyMode = value; rerender(); } }), el("span", {}, label));
  const go = el("button", { class: "btn primary big" }, t("studio.views.ueberpruefen"));
  go.addEventListener("click", () => {
    const d = host.drive();
    if (!d) return host.note(t("studio.views.bitteEinLaufwerkWaehlen"));
    const image = compare ? input.value.trim() : "";
    if (compare && !image) return host.note(t("studio.views.bitteEinImageWaehlen"));
    remembered.verifyImage = image || remembered.verifyImage;
    host.run(compare ? t("studio.views.discMitImageVergleichen") : t("studio.views.lesbarkeitPruefen"), () => api.verify(d.id, compare ? image : null));
  });
  return el(
    "div",
    {},
    el("div", { class: "opt-group" }, radio("image", t("studio.views.mitImageVergleichen")), compare ? el("label", { class: "opt-row" }, el("span", { class: "opt-label" }, t("studio.views.image")), row) : null, radio("read", t("studio.views.nurLesbarkeitPruefenJeder"))),
    actionBar(go),
  );
}

function viewDiscovery(host: Host): HTMLElement {
  const sectors = el("input", { type: "number", min: "1", placeholder: t("studio.views.ganzeDisc"), value: remembered.discoverySectors });
  sectors.addEventListener("change", () => (remembered.discoverySectors = sectors.value));
  const go = el("button", { class: "btn primary big" }, t("studio.views.testbrandStarten"));
  go.addEventListener("click", () => {
    const d = host.drive();
    if (!d) return host.note(t("studio.views.bitteEinLaufwerkWaehlen"));
    const n = sectors.value ? Math.round(Number(sectors.value)) : null;
    modal(
      t("studio.views.discoveryStarten"),
      el("p", {}, t("studio.views.esWerdenNullenAuf")),
      [{ label: t("studio.views.abbrechen") }, { label: t("studio.views.brennen"), primary: true, onClick: () => host.run(t("studio.views.discovery"), () => api.discovery(d.id, host.speed(), n)) }],
    );
  });
  return el(
    "div",
    {},
    el("div", { class: "opt-group" }, el("label", { class: "opt-row" }, el("span", { class: "opt-label" }, t("studio.views.sektoren")), sectors, el("small", { class: "opt-hint" }, t("studio.views.leerGesamteFreieKapazitaet")))),
    el("p", { class: "hint" }, t("studio.views.derGraphZeigtGeschwindigkeit")),
    actionBar(go),
  );
}

export function renderHome(go: (m: Mode) => void): HTMLElement {
  return el(
    "div",
    { class: "home" },
    el("h2", {}, t("studio.views.wasMoechtenSieTun")),
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
