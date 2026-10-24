import { getCurrentWebview } from "@tauri-apps/api/webview";
import { openSettings } from "./settings";
import { openBurner } from "./burn";
import { emitTo, listen } from "@tauri-apps/api/event";
import { WebviewWindow } from "@tauri-apps/api/webviewWindow";
import type { EditorInit } from "./menuEditor";
import { isButton, isMoving, madeForOtherTitles, menuClipSecs, titlesKey, type CustomLayout, type EditorTitle } from "./layout";
import { open } from "@tauri-apps/plugin-dialog";
import {
  checkLayout,
  calculateProject,
  cancelConversion,
  onBuildCancelled,
  defaultOutputDir,
  listTemplates,
  onBuildDone,
  onBuildEvent,
  probeFile,
  startConversion,
  getSettings,
  settingsInfo,
  startupFiles,
  startupBurn,
  templatePreview,
  type AppSettings,
  type BuildEvent,
  type CalculationResult,
  type ConversionDone,
  type Eta,
  formatRemaining,
  type Disc,
  type TemplateInfo,
  type VideoMetadata,
} from "./api";

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

const MAX_AUDIO = 8;
const VIDEO_EXTENSIONS = ["mkv", "mp4", "m4v", "avi", "mov", "ts", "m2ts", "mts", "mpg", "mpeg", "vob", "wmv", "webm", "flv", "divx", "ogm"];

interface Title {
  id: number;
  meta: VideoMetadata;
  /** ffprobe stream indices of checked audio tracks. */
  audio: Set<number>;
  /** ffprobe stream indices of enabled subtitles. */
  subs: Set<number>;
  label: string;
  /** Chapter starts in seconds (first is 0). */
  chapters: number[];
}

type Tab = "audio" | "subtitles" | "chapters" | "template";

type DiscChoice = "dvd5" | "dvd9" | "bd25" | "bd50" | "custom";

const state = {
  titles: [] as Title[],
  selected: null as number | null,
  tab: "audio" as Tab,
  disc: "dvd5" as DiscChoice,
  settings: null as AppSettings | null,
  twoPass: false,
  motion: false,
  templates: [] as TemplateInfo[],
  template: "",
  /** Menus from a ConvertX template or from the menu editor. */
  menuMode: "template" as "template" | "custom",
  customLayout: null as CustomLayout | null,
  /** `titlesKey` of the videos the custom layout was made for (null = unknown). */
  customLayoutKey: null as string | null,
  /** Problems of the custom layout with the current titles (empty = ready). */
  layoutErrors: [] as string[],
  /** Heading of the main menu = project name (also the name of the DVD folder). */
  menuTitle: "",
  /** The heading follows the first video's name until the user types their own. */
  menuTitleAuto: true,
  output: "",
  calc: null as CalculationResult | null,
  probing: 0,
  converting: false,
};
let nextId = 1;

const SETTINGS_KEY = "convertx-gui-settings";
const LAYOUT_KEY = "convertx-menu-layout";

/** DVD and Blu-ray menus are designed separately (4:3 TV frame vs 16:9 HD frame). */
type Medium = "dvd" | "bd";
type MenuSlot = { layout: CustomLayout | null; key: string | null; mode: "template" | "custom" };
const mediumOf = (d: DiscChoice): Medium => (d === "bd25" || d === "bd50" ? "bd" : "dvd");
const slots: Record<Medium, MenuSlot> = {
  dvd: { layout: null, key: null, mode: "template" },
  bd: { layout: null, key: null, mode: "template" },
};
/** localStorage key suffix of a medium's layout (the DVD keys predate the Blu-ray ones). */
const slotKey = (m: Medium) => (m === "bd" ? `${LAYOUT_KEY}-bd` : LAYOUT_KEY);

/** Files the live menu state under `from` and loads the one of `to` (called on disc changes). */
function switchMedium(from: Medium, to: Medium) {
  if (from === to) return;
  slots[from] = { layout: state.customLayout, key: state.customLayoutKey, mode: state.menuMode };
  const next = slots[to];
  state.customLayout = next.layout;
  state.customLayoutKey = next.key;
  state.menuMode = next.mode;
  state.layoutErrors = [];
}

function loadSettings() {
  try {
    const s = JSON.parse(localStorage.getItem(SETTINGS_KEY) ?? "{}");
    if (["dvd5", "dvd9", "bd25", "bd50", "custom"].includes(s.disc)) state.disc = s.disc;
    state.twoPass = !!s.twoPass;
    state.motion = !!s.motion;
    if (typeof s.template === "string") state.template = s.template;
    if (typeof s.output === "string") state.output = s.output;
    slots.dvd.mode = s.menuMode === "custom" ? "custom" : "template";
    slots.bd.mode = s.menuModeBd === "custom" ? "custom" : "template";
    for (const m of ["dvd", "bd"] as const) {
      const layout = localStorage.getItem(slotKey(m));
      if (layout) slots[m].layout = JSON.parse(layout) as CustomLayout;
      slots[m].key = localStorage.getItem(`${slotKey(m)}-titles`);
    }
    const live = slots[mediumOf(state.disc)];
    [state.menuMode, state.customLayout, state.customLayoutKey] = [live.mode, live.layout, live.key];
  } catch {
    /* storage unavailable: defaults */
  }
}

function saveSettings() {
  try {
    // The heading is project-specific and therefore not remembered between sessions.
    const { disc, twoPass, motion, template, output } = state;
    slots[mediumOf(disc)] = { layout: state.customLayout, key: state.customLayoutKey, mode: state.menuMode };
    localStorage.setItem(
      SETTINGS_KEY,
      JSON.stringify({ disc, twoPass, motion, template, output, menuMode: slots.dvd.mode, menuModeBd: slots.bd.mode }),
    );
    for (const m of ["dvd", "bd"] as const) {
      const { layout, key } = slots[m];
      if (layout) localStorage.setItem(slotKey(m), JSON.stringify(layout));
      else localStorage.removeItem(slotKey(m));
      if (layout && key !== null) localStorage.setItem(`${slotKey(m)}-titles`, key);
      else localStorage.removeItem(`${slotKey(m)}-titles`);
    }
  } catch {
    /* ignore */
  }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;

type Child = Node | string | null | undefined | false;

/** Creates an element; strings become text nodes (never parsed as HTML). */
function h<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  props: Partial<Record<string, unknown>> = {},
  ...children: Child[]
): HTMLElementTagNameMap[K] {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(props)) {
    if (v === undefined || v === null || v === false) continue;
    if (k === "class") el.className = String(v);
    else if (k.startsWith("on") && typeof v === "function") el.addEventListener(k.slice(2), v as EventListener);
    else if (k in el && typeof v !== "string") (el as unknown as Record<string, unknown>)[k] = v;
    else el.setAttribute(k, v === true ? "" : String(v));
  }
  for (const c of children) if (c !== null && c !== undefined && c !== false) el.append(c);
  return el;
}

function fmtTime(secs: number): string {
  const s = Math.max(0, Math.round(secs));
  const hh = Math.floor(s / 3600);
  const mm = Math.floor((s % 3600) / 60);
  const ss = s % 60;
  return `${hh}:${String(mm).padStart(2, "0")}:${String(ss).padStart(2, "0")}`;
}

/** Parses `h:mm:ss`, `mm:ss` or seconds; null if invalid. */
function parseTime(text: string): number | null {
  const parts = text.trim().replace(",", ".").split(":");
  if (parts.length === 0 || parts.length > 3) return null;
  let total = 0;
  for (const p of parts) {
    if (!/^\d+(\.\d+)?$/.test(p)) return null;
    total = total * 60 + Number(p);
  }
  return total;
}

const fmtNum = (n: number, digits = 0) =>
  n.toLocaleString("de-DE", { minimumFractionDigits: digits, maximumFractionDigits: digits });

const selectedTitle = () => state.titles.find((t) => t.id === state.selected) ?? null;

/** Disc for the engine; "custom" uses the target size from the settings. */
function currentDisc(): Disc {
  if (state.disc === "custom") return { custom: state.settings?.target_size_mb ?? 4300 };
  return state.disc;
}

/** A Blu-ray target: no menus, one autoplaying playlist. */
const isBluray = () => state.disc === "bd25" || state.disc === "bd50";

/** The frame menus are designed for: `PAL` / `NTSC`, or `BD1080` / `BD720` on a Blu-ray. */
function frameName(): string {
  if (isBluray()) return state.settings?.bd_resolution === "720p" ? "BD720" : "BD1080";
  return state.settings?.tv_standard ?? "PAL";
}

/** The setting "only one video per Blu-ray" is on and violated. */
const singleVideoViolated = () => isBluray() && !!state.settings?.bd_single_video && state.titles.length > 1;

function updateCustomDiscLabel() {
  const mb = state.settings?.target_size_mb ?? 4300;
  $("disc-custom").textContent = `Benutzerdefiniert (${fmtNum(mb)} MB)`;
}

function audioKbps(t: Title): number {
  return t.meta.audio.filter((a) => t.audio.has(a.index)).reduce((sum, a) => sum + a.ac3Kbps, 0);
}

function status(text: string, kind: "info" | "error" = "info") {
  const bar = $("status-bar");
  bar.textContent = text;
  bar.dataset.kind = kind;
}

// ---------------------------------------------------------------------------
// Adding / removing titles
// ---------------------------------------------------------------------------

async function addFiles(paths: string[]) {
  const videos = paths.filter((p) => VIDEO_EXTENSIONS.includes(p.split(".").pop()?.toLowerCase() ?? ""));
  if (videos.length === 0) {
    status("Keine unterstützten Videodateien.", "error");
    return;
  }
  state.probing += videos.length;
  renderFooter();
  for (const path of videos) {
    status(`Analysiere ${path} …`);
    try {
      const meta = await probeFile(path);
      const title: Title = {
        id: nextId++,
        meta,
        audio: new Set(meta.audio.slice(0, MAX_AUDIO).map((a) => a.index)),
        subs: new Set(meta.subtitles.filter((s) => s.supported).map((s) => s.index)),
        label: meta.fileName.replace(/\.[^.]+$/, ""),
        chapters: meta.chapters.map((c) => c.startSecs),
      };
      state.titles.push(title);
      if (state.selected === null) state.selected = title.id;
      status(`${meta.fileName} hinzugefügt.`);
    } catch (e) {
      status(`${path}: ${e}`, "error");
    } finally {
      state.probing--;
      renderAll();
      scheduleCalc();
    }
  }
}

async function addViaDialog() {
  const picked = await open({
    multiple: true,
    title: "Videos hinzufügen",
    filters: [{ name: "Videos", extensions: VIDEO_EXTENSIONS }],
  });
  if (picked) await addFiles(Array.isArray(picked) ? picked : [picked]);
}

function removeSelected() {
  const i = state.titles.findIndex((t) => t.id === state.selected);
  if (i < 0) return;
  state.titles.splice(i, 1);
  state.selected = state.titles[Math.min(i, state.titles.length - 1)]?.id ?? null;
  renderAll();
  scheduleCalc();
}

function moveSelected(delta: number) {
  const i = state.titles.findIndex((t) => t.id === state.selected);
  const j = i + delta;
  if (i < 0 || j < 0 || j >= state.titles.length) return;
  [state.titles[i], state.titles[j]] = [state.titles[j], state.titles[i]];
  renderAll();
  scheduleCalc();
}

// ---------------------------------------------------------------------------
// Bitrate calculation (debounced)
// ---------------------------------------------------------------------------

let calcTimer: number | undefined;
let calcSeq = 0;

function scheduleCalc() {
  window.clearTimeout(calcTimer);
  calcTimer = window.setTimeout(runCalc, 150);
}

async function runCalc() {
  const seq = ++calcSeq;
  try {
    const result = await calculateProject({
      disc: currentDisc(),
      titles: state.titles.map((t) => ({
        durationSecs: t.meta.durationSecs,
        audioKbps: audioKbps(t),
        subtitleStreams: t.subs.size,
      })),
      motionMenu: usesMotion(),
      motionDuration: null,
      bdMenuSecs: isBluray() && state.menuMode === "custom" && state.customLayout ? menuClipSecs(state.customLayout, 20) : [],
    });
    if (seq !== calcSeq) return; // a newer calculation is on its way
    state.calc = result;
  } catch (e) {
    state.calc = null;
    status(`Berechnung fehlgeschlagen: ${e}`, "error");
  }
  renderTitleList();
  renderFooter();
}

// ---------------------------------------------------------------------------
// Rendering: left side
// ---------------------------------------------------------------------------

function renderTitleList() {
  const list = $("title-list");
  list.replaceChildren();
  $("dropzone").classList.toggle("compact", state.titles.length > 0);

  state.titles.forEach((t, i) => {
    const v = t.meta.video;
    const calc = state.calc?.titles[i];
    const audioSummary =
      t.meta.audio
        .filter((a) => t.audio.has(a.index))
        .map((a) => `${a.languageName} ${a.dvdLayout}`)
        .join(", ") || "kein Ton";
    const subsSummary = t.subs.size > 0 ? ` · ${t.subs.size} Untertitel` : "";
    const quality = calc ? calc.qualityPercent : 0;
    const qualityClass = calc ? calc.quality : "medium";

    list.append(
      h(
        "li",
        {
          class: `title-item${t.id === state.selected ? " selected" : ""}`,
          tabIndex: 0,
          onclick: () => select(t.id),
          onkeydown: (e: KeyboardEvent) => {
            if (e.key === "Enter" || e.key === " ") select(t.id);
          },
        },
        h("div", { class: "title-head" }, h("span", { class: "title-no" }, String(i + 1)), h("span", { class: "title-name" }, t.label)),
        h(
          "div",
          { class: "title-meta" },
          v ? `${v.width}×${v.height} · ${v.dvdAspect}${v.interlaced ? " · interlaced" : ""} · ` : "",
          fmtTime(t.meta.durationSecs),
          ` · ${t.chapters.length} Kapitel`,
        ),
        h("div", { class: "title-audio" }, audioSummary + subsSummary),
        h(
          "div",
          {
            class: "quality",
            title: isBluray()
              ? "Videoqualität: grün über 15.000 kbps, gelb 8.000–15.000 kbps, rot unter 8.000 kbps"
              : "Videoqualität: grün über 2.500 kbps, gelb 1.200–2.500 kbps, rot unter 1.200 kbps",
          },
          h("div", { class: "quality-bar" }, h("div", { class: `quality-value ${qualityClass}`, style: `width:${quality}%` })),
          h("span", { class: "quality-text" }, calc ? `${fmtNum(calc.videoKbps)} kbps` : "…"),
        ),
      ),
    );
  });

  const hasSel = selectedTitle() !== null;
  ($("btn-remove") as HTMLButtonElement).disabled = !hasSel || state.converting;
  const idx = state.titles.findIndex((t) => t.id === state.selected);
  ($("btn-up") as HTMLButtonElement).disabled = idx <= 0;
  ($("btn-down") as HTMLButtonElement).disabled = idx < 0 || idx >= state.titles.length - 1;
}

function renderFooter() {
  const calc = state.calc;
  const fill = $("fill-value");
  const text = $("fill-text");
  if (!calc || state.titles.length === 0) {
    fill.style.width = "0%";
    fill.className = "fill-value";
    text.textContent = state.probing > 0 ? "Analysiere …" : "Noch keine Videos";
  } else {
    fill.style.width = `${Math.min(100, calc.fillPercent)}%`;
    fill.className = `fill-value ${calc.fits ? (calc.fillPercent > 97 ? "full" : "ok") : "over"}`;
    const base = `${fmtNum(calc.estimatedMb)} / ${fmtNum(calc.targetMb)} MB (${fmtNum(calc.fillPercent)} %)`;
    const hint =
      state.disc === "bd25"
        ? "BD-50 wählen oder Tonspuren abwählen."
        : state.disc === "bd50"
          ? "Videos kürzen oder Tonspuren abwählen."
          : "DVD-9 wählen oder Tonspuren abwählen.";
    text.textContent = singleVideoViolated()
      ? "Nur ein Video pro Blu-ray erlaubt (Einstellung „Nur ein Video pro Blu-ray“)."
      : calc.fits
        ? `${base} · Video ${fmtNum(calc.videoKbps)} kbps${calc.clamp === "max" ? " (Maximum)" : ""}`
        : `${base} – passt nicht! ${hint}`;
  }
  // A Blu-ray without editor layout has no menu (autoplay).
  const menusReady =
    state.menuMode === "template"
      ? isBluray() || !!state.template
      : !!state.customLayout && state.layoutErrors.length === 0 && !layoutIsStale();
  const canConvert =
    state.titles.length > 0 &&
    state.probing === 0 &&
    !state.converting &&
    menusReady &&
    !singleVideoViolated() &&
    !!state.output.trim() &&
    !!calc?.fits;
  const motionToggle = document.getElementById("motion-menu") as HTMLInputElement | null;
  if (motionToggle) {
    motionToggle.disabled = state.menuMode === "custom" || isBluray();
    motionToggle.title = isBluray()
      ? "Blu-ray hat kein Menü"
      : state.menuMode === "custom"
        ? "Beim eigenen Menü bestimmen Videos/Ton im Layout, ob ein Menü sich bewegt"
        : "";
  }
  ($("btn-convert") as HTMLButtonElement).disabled = !canConvert;
}

// ---------------------------------------------------------------------------
// Rendering: right side (tabs)
// ---------------------------------------------------------------------------

function select(id: number) {
  state.selected = id;
  renderAll();
}

function renderTabs() {
  document.querySelectorAll<HTMLButtonElement>(".tab").forEach((b) => {
    const active = b.dataset.tab === state.tab;
    b.classList.toggle("active", active);
    b.setAttribute("aria-selected", String(active));
  });
  const body = $("tab-body");
  body.replaceChildren();
  const t = selectedTitle();

  if (state.tab === "template") {
    body.append(renderTemplateTab());
    return;
  }
  if (!t) {
    body.append(h("div", { class: "empty" }, "Links ein Video hinzufügen oder auswählen."));
    return;
  }

  body.append(
    h(
      "div",
      { class: "title-label-row" },
      h("label", { for: "title-label" }, "Bezeichnung im Menü"),
      h("input", {
        id: "title-label",
        type: "text",
        value: t.label,
        oninput: (e: Event) => {
          t.label = (e.target as HTMLInputElement).value;
          renderTitleList();
        },
      }),
    ),
  );
  if (state.tab === "audio") body.append(renderAudioTab(t));
  if (state.tab === "subtitles") body.append(renderSubtitleTab(t));
  if (state.tab === "chapters") body.append(renderChapterTab(t));
}

function renderAudioTab(t: Title) {
  if (t.meta.audio.length === 0) return h("div", { class: "empty" }, "Dieses Video hat keine Tonspur.");
  const full = t.audio.size >= MAX_AUDIO;
  return h(
    "div",
    { class: "track-list" },
    h("p", { class: "hint" }, `Ausgewählte Spuren werden nach AC-3 umgewandelt (DVD: höchstens ${MAX_AUDIO} Spuren).`),
    ...t.meta.audio.map((a) => {
      const checked = t.audio.has(a.index);
      return h(
        "label",
        { class: "track" },
        h("input", {
          type: "checkbox",
          checked,
          disabled: (!checked && full) || (checked && t.audio.size === 1),
          onchange: (e: Event) => {
            if ((e.target as HTMLInputElement).checked) t.audio.add(a.index);
            else t.audio.delete(a.index);
            renderAll();
            scheduleCalc();
          },
        }),
        h(
          "div",
          { class: "track-main" },
          h("span", { class: "track-lang" }, a.languageName),
          a.title ? h("span", { class: "track-title" }, a.title) : null,
          a.default ? h("span", { class: "badge" }, "Standard") : null,
        ),
        h("span", { class: "track-detail" }, `#${a.index} · ${a.codec.toUpperCase()} ${a.channels} ch → AC-3 ${a.dvdLayout} ${a.ac3Kbps} kbps`),
      );
    }),
  );
}

function renderSubtitleTab(t: Title) {
  if (t.meta.subtitles.length === 0) return h("div", { class: "empty" }, "Dieses Video hat keine Untertitel.");
  return h(
    "div",
    { class: "track-list" },
    h("p", { class: "hint" }, "Text-Untertitel (SRT/ASS) werden als DVD-Untertitel gerendert. Bild-Untertitel (Blu-ray-PGS, DVD-VobSub) werden umgerechnet und auf 3 Farben reduziert. DVB- und XSUB-Untertitel werden nicht unterstützt."),
    ...t.meta.subtitles.map((s) =>
      h(
        "label",
        { class: `track${s.supported ? "" : " disabled"}` },
        h(
          "span",
          { class: "switch" },
          h("input", {
            type: "checkbox",
            checked: t.subs.has(s.index),
            disabled: !s.supported,
            onchange: (e: Event) => {
              if ((e.target as HTMLInputElement).checked) t.subs.add(s.index);
              else t.subs.delete(s.index);
              renderAll();
              scheduleCalc();
            },
          }),
          h("span"),
        ),
        h(
          "div",
          { class: "track-main" },
          h("span", { class: "track-lang" }, s.languageName),
          s.title ? h("span", { class: "track-title" }, s.title) : null,
          s.forced ? h("span", { class: "badge" }, "Forced") : null,
        ),
        h("span", { class: "track-detail" }, `#${s.index} · ${s.codec}${s.supported ? "" : " · nicht unterstützt"}`),
      ),
    ),
  );
}

function renderChapterTab(t: Title) {
  const sorted = [...t.chapters].sort((a, b) => a - b);
  const sourceTitle = (secs: number) => t.meta.chapters.find((c) => Math.abs(c.startSecs - secs) < 0.5)?.title ?? null;
  const intervalInput = h("input", { type: "number", min: "1", max: "60", value: "5", class: "small" }) as HTMLInputElement;

  const update = (chapters: number[]) => {
    t.chapters = chapters;
    renderAll();
  };

  const rows = sorted.map((start, i) =>
    h(
      "tr",
      {},
      h("td", { class: "num" }, String(i + 1)),
      h(
        "td",
        {},
        i === 0
          ? h("span", { class: "mono" }, fmtTime(0))
          : h("input", {
              type: "text",
              class: "mono time",
              value: fmtTime(start),
              onchange: (e: Event) => {
                const input = e.target as HTMLInputElement;
                const v = parseTime(input.value);
                if (v === null || v <= 0 || v >= t.meta.durationSecs - 1) {
                  input.value = fmtTime(start);
                  status("Ungültige Kapitelzeit.", "error");
                  return;
                }
                update(sorted.map((s, j) => (j === i ? v : s)));
              },
            }),
      ),
      h("td", {}, sourceTitle(start) ?? `Kapitel ${i + 1}`),
      h(
        "td",
        { class: "actions" },
        i === 0
          ? null
          : h("button", { class: "btn icon", title: "Kapitel entfernen", onclick: () => update(sorted.filter((_, j) => j !== i)) }, "✕"),
      ),
    ),
  );

  return h(
    "div",
    { class: "chapters" },
    h(
      "div",
      { class: "chapter-tools" },
      h(
        "button",
        {
          class: "btn",
          onclick: () => {
            const last = sorted[sorted.length - 1] ?? 0;
            const next = Math.min(last + 300, t.meta.durationSecs - 2);
            if (next > last + 1) update([...sorted, next]);
          },
        },
        "＋ Kapitel",
      ),
      h("span", { class: "spacer" }),
      "Alle ",
      intervalInput,
      " Min. ",
      h(
        "button",
        {
          class: "btn",
          onclick: () => {
            const step = Math.max(1, Number(intervalInput.value) || 5) * 60;
            const out = [0];
            for (let s = step; s < t.meta.durationSecs - 1; s += step) out.push(s);
            update(out.slice(0, 99));
          },
        },
        "Raster erzeugen",
      ),
      t.meta.hasSourceChapters
        ? h("button", { class: "btn", onclick: () => update(t.meta.chapters.map((c) => c.startSecs)) }, "Kapitel der Quelle")
        : null,
    ),
    h(
      "table",
      { class: "table" },
      h("thead", {}, h("tr", {}, h("th", {}, "#"), h("th", {}, "Start"), h("th", {}, "Titel"), h("th", {}))),
      h("tbody", {}, ...rows),
    ),
    h("p", { class: "hint" }, `${sorted.length} von höchstens 99 Kapiteln · Laufzeit ${fmtTime(t.meta.durationSecs)}`),
  );
}

/** The project's titles as the menu editor sees them. */
function editorTitles(): EditorTitle[] {
  return state.titles.map((t, i) => {
    // Same chapter list the build uses (sorted starts), named like in the chapter tab.
    const starts = [...t.chapters].sort((a, b) => a - b);
    return {
      index: i + 1,
      label: t.label.trim() || t.meta.fileName.replace(/\.[^.]+$/, ""),
      path: t.meta.path,
      chapters: Math.max(1, starts.length),
      durationSecs: t.meta.durationSecs,
      chapterNames: starts.map((s) => t.meta.chapters.find((c) => Math.abs(c.startSecs - s) < 0.5)?.title ?? null),
      chapterStarts: starts,
      // Tracks that go onto the disc (unsupported bitmap formats are skipped by the engine).
      audioTracks: t.meta.audio
        .filter((a) => t.audio.has(a.index))
        .map((a) => ({ source: a.index, label: a.title ? `${a.languageName} – ${a.title}` : a.languageName })),
      subtitleTracks: t.meta.subtitles
        .filter((s) => t.subs.has(s.index) && s.supported)
        .map((s) => ({ source: s.index, label: s.title ? `${s.languageName} – ${s.title}` : s.languageName })),
    };
  });
}

/** The stored custom layout belongs to another set of videos (an earlier project). */
function layoutIsStale(): boolean {
  return !!state.customLayout && state.titles.length > 0 && madeForOtherTitles(state.customLayout, state.customLayoutKey, editorTitles());
}

/** Whether the menus move (size estimate): template motion switch or moving custom menus. */
function usesMotion(): boolean {
  return state.menuMode === "custom" ? !!state.customLayout?.menus.some(isMoving) : state.motion;
}

let layoutCheckSeq = 0;
/** Re-checks the custom layout against the current titles (they may have changed). */
async function refreshLayoutCheck() {
  if (state.menuMode !== "custom" || !state.customLayout) {
    state.layoutErrors = [];
    return;
  }
  const seq = ++layoutCheckSeq;
  const errors = await checkLayout(state.customLayout, editorTitles(), frameName()).catch((e) => [String(e)]);
  if (seq !== layoutCheckSeq) return;
  const changed = JSON.stringify(errors) !== JSON.stringify(state.layoutErrors);
  state.layoutErrors = errors;
  if (changed) {
    renderFooter();
    if (state.tab === "template") renderTabs();
  }
}

function editorInit(): EditorInit {
  return {
    layout: state.customLayout,
    titles: editorTitles(),
    tv: frameName(),
    heading: state.menuTitle.trim() || state.titles[0]?.label || (isBluray() ? "Blu-ray" : "DVD"),
    layoutTitlesKey: state.customLayoutKey,
  };
}

// The editor lives in its own window ("menu-editor", editor.html) and talks via events.
void listen("editor-ready", () => emitTo("menu-editor", "editor-init", editorInit()));
void listen<CustomLayout>("editor-apply", ({ payload }) => {
  state.customLayout = payload;
  state.customLayoutKey = titlesKey(editorTitles());
  state.menuMode = "custom";
  saveSettings();
  void refreshLayoutCheck();
  scheduleCalc();
  renderAll();
  status("Eigenes Menü übernommen.");
});

// The Burn Studio is its own window ("burn-studio", burnStudio.html): created or focused.
async function openStudio() {
  const existing = await WebviewWindow.getByLabel("burn-studio");
  if (existing) {
    await existing.setFocus();
    return;
  }
  const win = new WebviewWindow("burn-studio", {
    url: "burnStudio.html",
    title: "Brennstudio – ConvertX Rust",
    width: 1180,
    height: 800,
    minWidth: 900,
    minHeight: 620,
  });
  void win.once("tauri://error", (ev) => status(`Brennstudio konnte nicht geöffnet werden: ${String(ev.payload)}`));
}

async function openEditor() {
  const existing = await WebviewWindow.getByLabel("menu-editor");
  if (existing) {
    await existing.setFocus();
    return;
  }
  const win = new WebviewWindow("menu-editor", {
    url: "editor.html",
    title: "Menü-Editor – ConvertX Rust",
    width: 1280,
    height: 820,
    minWidth: 960,
    minHeight: 620,
  });
  void win.once("tauri://error", (ev) => status(`Menü-Editor konnte nicht geöffnet werden: ${String(ev.payload)}`));
}

function renderCustomMenuTab(): HTMLElement {
  const l = state.customLayout;
  const wrap = h("div", { class: "custom-menu-tab" });
  if (!l) {
    wrap.append(
      h("p", { class: "hint" }, "Noch kein eigenes Menü. Der Editor erzeugt einen Entwurf aus den Videos des Projekts, den du frei gestalten kannst."),
    );
  } else {
    const titleMenus = l.menus.filter((m) => m.role === "title").length;
    const rootMenus = l.menus.length - titleMenus;
    const buttons = l.menus.reduce((n, m) => n + m.elements.filter(isButton).length, 0);
    const moving = l.menus.filter(isMoving).length;
    wrap.append(
      h(
        "div",
        { class: "template-card" },
        h(
          "div",
          { class: "template-info" },
          h("h3", {}, "Eigenes Menü"),
          h("p", {}, `${titleMenus} Titelmenü(s), ${rootMenus} Root-Menü(s), ${buttons} Buttons, ${moving} bewegte(s) Menü(s)`),
          h("p", {}, h("strong", {}, "Startmenü: "), l.menus.find((m) => m.role === "title")?.name ?? "–"),
          layoutIsStale()
            ? h(
                "p",
                { class: "layout-errors" },
                "Dieses Layout wurde für andere Videos erstellt. Beim Öffnen des Editors entsteht ein neuer Entwurf aus den aktuellen Videos („Rückgängig“ holt das alte zurück).",
              )
            : state.layoutErrors.length === 0
              ? h("p", { class: "ok-text" }, "✓ Passt zu den aktuellen Videos.")
              : h("ul", { class: "layout-errors" }, ...state.layoutErrors.slice(0, 6).map((e) => h("li", {}, e))),
        ),
      ),
    );
  }
  wrap.append(
    h(
      "div",
      { class: "row" },
      h("button", { class: "btn primary", type: "button", disabled: state.titles.length === 0, onclick: openEditor }, l ? "Menü-Editor öffnen …" : "Menü gestalten …"),
      l
        ? h(
            "button",
            {
              class: "btn",
              type: "button",
              onclick: () => {
                state.customLayout = null;
                state.customLayoutKey = null;
                state.layoutErrors = [];
                saveSettings();
                renderAll();
              },
            },
            "Eigenes Menü verwerfen",
          )
        : null,
    ),
  );
  if (state.titles.length === 0) wrap.append(h("p", { class: "hint" }, "Zuerst Videos hinzufügen."));
  return wrap;
}

function renderTemplateTab() {
  const outer = h("div", { class: "template-tab" });
  const modes: ["template" | "custom", string][] = [
    ["template", isBluray() ? "Ohne Menü (Autoplay)" : "ConvertX-Vorlage"],
    ["custom", "Eigenes Menü (Editor)"],
  ];
  outer.append(
    h(
      "div",
      { class: "seg menu-mode", role: "radiogroup", "aria-label": "Menüart" },
      ...modes.map(([mode, label]) =>
        h(
          "button",
          {
            type: "button",
            class: `btn${state.menuMode === mode ? " active" : ""}`,
            role: "radio",
            "aria-checked": String(state.menuMode === mode),
            onclick: () => {
              state.menuMode = mode;
              saveSettings();
              void refreshLayoutCheck();
              scheduleCalc();
              renderAll();
            },
          },
          label,
        ),
      ),
    ),
  );
  if (state.menuMode === "custom") outer.append(renderCustomMenuTab());
  else if (isBluray()) {
    outer.append(
      h("div", { class: "empty" }, "Blu-ray: ohne Menü (Autoplay)"),
      h("p", { class: "hint" }, "Alle Videos laufen nach dem Einlegen der Disc als eine Playlist ab, mit Kapiteln an den Kapitelmarken jedes Videos. ConvertX-Vorlagen gibt es für die Blu-ray nicht – „Eigenes Menü (Editor)“ erzeugt echte Blu-ray-Menüs (HDMV)."),
    );
  } else outer.append(renderTemplateChoice());
  return outer;
}

function renderTemplateChoice() {
  const wrap = h("div", { class: "template-tab" });
  if (state.templates.length === 0) {
    wrap.append(h("div", { class: "empty" }, "Keine ConvertX-Menüvorlagen gefunden (C:\\ProgramData\\VSO\\ConvertXToDVD\\7\\Templates)."));
    return wrap;
  }
  const select = h("select", {
    id: "template-select",
    onchange: (e: Event) => {
      state.template = (e.target as HTMLSelectElement).value;
      saveSettings();
      renderAll();
    },
  }) as HTMLSelectElement;
  const folders = [...new Set(state.templates.map((t) => t.folder))];
  for (const folder of folders) {
    const group = h("optgroup", { label: folder });
    for (const t of state.templates.filter((x) => x.folder === folder)) {
      group.append(h("option", { value: t.path, selected: t.path === state.template }, t.name));
    }
    select.append(group);
  }

  const info = state.templates.find((t) => t.path === state.template);
  const styles: Record<string, string> = {
    MNUS_TITLE: "Titel",
    MNUS_ROOT: "Root",
    MNUS_CHAPTER: "Kapitel",
    MNUS_SETTINGS: "Einstellungen",
    MNUS_BONUS: "Bonus",
    MNUS_SERIES_EPISODES: "Episoden",
  };
  const preview = h("img", { class: "template-preview", alt: "Vorschau" }) as HTMLImageElement;
  if (info?.thumbnail) {
    templatePreview(info.thumbnail)
      .then((url) => (preview.src = url))
      .catch(() => preview.remove());
  }

  wrap.append(
    h("p", { class: "hint" }, "Die Menüvorlage gilt für das ganze Projekt (Hauptmenü und Untermenüs aller Titlesets)."),
    h("div", { class: "row" }, h("label", { for: "template-select" }, "Vorlage"), select),
    h(
      "div",
      { class: "row" },
      h("label", { for: "menu-title" }, "Überschrift Hauptmenü"),
      h("input", {
        id: "menu-title",
        type: "text",
        placeholder: "Name des ersten Videos",
        value: state.menuTitle,
        oninput: (e: Event) => {
          state.menuTitle = (e.target as HTMLInputElement).value;
          // Typing makes it the user's own heading; an empty field follows the first video again.
          state.menuTitleAuto = state.menuTitle.trim() === "";
          if (state.menuTitleAuto) syncMenuTitle();
        },
      }),
    ),
    h("p", { class: "hint" }, String.raw`Auch Name des DVD-Ordners: <Ausgabeordner>\<Überschrift>\VIDEO_TS. Standard ist der Name des ersten Videos.`),
  );
  if (info) {
    const pages = info.pageStyles.map((s) => styles[s] ?? s).join(", ");
    const hasSub = ["MNUS_ROOT", "MNUS_CHAPTER", "MNUS_SETTINGS"].every((s) => info.pageStyles.includes(s));
    wrap.append(
      h(
        "div",
        { class: "template-card" },
        preview,
        h(
          "div",
          { class: "template-info" },
          h("h3", {}, info.name),
          info.description ? h("p", {}, info.description) : null,
          h("p", {}, h("strong", {}, "Seiten: "), pages),
          info.titlesPerPage ? h("p", {}, h("strong", {}, "Titel pro Seite: "), String(info.titlesPerPage)) : null,
          h("p", {}, h("strong", {}, "Video-Slots: "), info.hasVideoSlots ? "ja – Motion Menu animiert Vorschauen" : "nein"),
          hasSub ? null : h("p", { class: "hint" }, "Fehlende Untermenüs (Root/Kapitel/Einstellungen) werden aus einer Vorlage derselben Familie ergänzt."),
        ),
      ),
    );
  }
  return wrap;
}

/** ConvertX default: the project (heading and DVD folder) is named after the first video. */
function syncMenuTitle() {
  if (!state.menuTitleAuto) return;
  state.menuTitle = state.titles[0]?.label.trim() ?? "";
  const input = document.getElementById("menu-title") as HTMLInputElement | null;
  if (input && document.activeElement !== input) input.value = state.menuTitle;
}

function renderAll() {
  syncMenuTitle();
  renderTitleList();
  renderTabs();
  renderFooter();
  // Titles, chapters or labels may have changed: is the custom menu still valid?
  if (state.menuMode === "custom") void refreshLayoutCheck();
}

// ---------------------------------------------------------------------------
// Conversion overlay
// ---------------------------------------------------------------------------

const PHASE_ORDER = ["menus", "encoding", "authoring", "image", "burning"] as const;
const overlay = {
  phase: "menus" as (typeof PHASE_ORDER)[number],
  rawPhase: "probe",
  total: 0,
};

function setTotal(pct: number) {
  overlay.total = Math.max(overlay.total, Math.min(100, pct));
  $("total-bar").style.width = `${overlay.total}%`;
  $("total-pct").textContent = `${fmtNum(overlay.total)} %`;
}

function setPhase(name: (typeof PHASE_ORDER)[number]) {
  overlay.phase = name;
  const current = PHASE_ORDER.indexOf(name);
  document.querySelectorAll<HTMLLIElement>("#phases li").forEach((li) => {
    const i = PHASE_ORDER.indexOf(li.dataset.phase as (typeof PHASE_ORDER)[number]);
    li.className = i < current ? "done" : i === current ? "active" : "";
  });
}

function logLine(text: string, kind: "step" | "warn" | "error" | "ok" = "step") {
  const log = $("log");
  log.append(h("li", { class: kind }, text));
  while (log.children.length > 200) log.firstElementChild?.remove();
  log.scrollTop = log.scrollHeight;
}

function onEvent(e: BuildEvent) {
  switch (e.type) {
    case "phase": {
      const p = e.data;
      overlay.rawPhase = p.phase;
      if (p.phase === "probe") {
        setPhase("menus");
        setTotal(1);
      } else if (p.phase === "thumbnails" || p.phase === "menus") {
        setPhase("menus");
        setTotal(5);
      } else if (p.phase === "menuEncoding") {
        setPhase("menus");
        setTotal(8);
      } else if (p.phase === "encoding") {
        // Titles are encoded in parallel; `index` = titles already finished.
        setPhase("encoding");
        setTotal(15 + (80 * p.index) / Math.max(1, p.count));
      } else if (p.phase === "authoring") {
        setPhase("authoring");
        setTotal(95);
      }
      break;
    }
    case "step":
      $("step-label").textContent = e.data;
      logLine(e.data);
      break;
    case "progress": {
      const { label, fraction } = e.data;
      $("step-label").textContent = label;
      $("step-pct").textContent = `${fmtNum(fraction * 100)} %`;
      $("step-bar").style.width = `${fraction * 100}%`;
      if (overlay.rawPhase === "encoding") {
        // Duration-weighted progress over all titles (2-pass included), computed by the engine.
        setTotal(15 + 80 * fraction);
      } else if (overlay.rawPhase === "menuEncoding") {
        setTotal(8 + 7 * fraction);
      }
      break;
    }
    case "warning":
      logLine(e.data, "warn");
      break;
    case "eta":
      showEta(e.data);
      break;
  }
}

/** "Geschätzte Restzeit: ~14 min 20 s (Phase 2/5: Videos encodieren – Video 2 von 4)". */
function showEta(eta: Eta) {
  const phase = `Phase ${eta.phaseNumber}/${eta.phaseCount}: ${eta.phaseLabel}${eta.detail ? ` – ${eta.detail}` : ""}`;
  const remaining =
    eta.remainingSecs === null
      ? `Restzeit wird berechnet … (${phase})`
      : `Geschätzte Restzeit: ${formatRemaining(eta.remainingSecs)} (${phase})`;
  const burn = eta.burnSecs !== null ? ` · danach ${formatRemaining(eta.burnSecs)} für ISO & Brennen (Schätzung)` : "";
  $("eta-line").textContent = remaining + burn;
}

function onDone(d: ConversionDone) {
  state.converting = false;
  $("overlay-title").textContent = d.ok ? "DVD fertig" : "Konvertierung fehlgeschlagen";
  if (d.ok) {
    setPhase("authoring");
    // The conversion is done; ISO and burning follow in the burn dialog.
    document.querySelectorAll<HTMLLIElement>("#phases li").forEach((li, i) => (li.className = i < 3 ? "done" : ""));
    setTotal(100);
    $("step-bar").style.width = "100%";
    $("step-label").textContent = d.videoTs ?? "";
    $("step-pct").textContent = "";
    logLine(d.message, "ok");
  } else {
    logLine(d.message, "error");
  }
  $("btn-close-overlay").hidden = false;
  $("btn-cancel").hidden = true;
  status(d.ok ? d.message : `Fehler: ${d.message.split("\n")[0]}`, d.ok ? "info" : "error");
  renderAll();
  // Finished DVD: offer burning right away (the dialog's countdown starts only with a blank disc).
  lastDvd = d.ok && d.videoTs ? { videoTs: d.videoTs, projectName: d.projectName } : null;
  $("btn-burn").hidden = !lastDvd;
  $("eta-line").textContent = "";
  if (lastDvd) void openBurner(lastDvd.videoTs, lastDvd.projectName, state.settings, true);
}

/** The last successfully built DVD or Blu-ray (for "Auf Disc brennen"). */
let lastDvd: { videoTs: string; projectName: string | null } | null = null;

/** Backend confirmed the cancel: tools stopped, work directory removed. */
function onCancelled(message: string) {
  state.converting = false;
  $("overlay").hidden = true;
  resetCancelButton();
  status(`${message}. Der temporäre Arbeitsordner wurde gelöscht.`);
  renderAll();
}

function resetCancelButton() {
  const b = $("btn-cancel") as HTMLButtonElement;
  b.disabled = false;
  b.textContent = "Abbrechen";
}

async function requestCancel() {
  const b = $("btn-cancel") as HTMLButtonElement;
  if (b.disabled) return;
  b.disabled = true;
  b.textContent = "Wird abgebrochen …";
  $("step-label").textContent = "Wird abgebrochen …";
  try {
    // `false`: the conversion has just finished – its result arrives as `convert-done`.
    if (!(await cancelConversion())) resetCancelButton();
  } catch (e) {
    resetCancelButton();
    logLine(`Abbrechen fehlgeschlagen: ${e}`, "error");
  }
}

async function convert() {
  if (state.converting) return;
  const overlayEl = $("overlay");
  $("overlay-title").textContent = "Konvertierung läuft";
  $("log").replaceChildren();
  $("btn-close-overlay").hidden = true;
  $("btn-burn").hidden = true;
  $("btn-cancel").hidden = false;
  resetCancelButton();
  $("step-bar").style.width = "0%";
  $("step-pct").textContent = "";
  $("step-label").textContent = "Starte …";
  $("eta-line").textContent = "";
  overlay.total = 0;
  setTotal(0);
  setPhase("menus");
  overlayEl.hidden = false;
  state.converting = true;
  renderAll();
  try {
    await startConversion({
      inputs: state.titles.map((t) => ({
        path: t.meta.path,
        audio: [...t.audio],
        subtitles: [...t.subs],
        label: t.label.trim() || null,
        chapters: [...t.chapters].sort((a, b) => a - b),
      })),
      disc: currentDisc(),
      template: state.menuMode === "template" && !isBluray() ? state.template : "",
      output: state.output.trim(),
      twoPass: state.twoPass,
      motionMenu: state.motion,
      motionDuration: null,
      menuTitle: state.menuTitle.trim() || null,
      customLayout: state.menuMode === "custom" ? state.customLayout : null,
    });
  } catch (e) {
    onDone({ ok: false, message: String(e), videoTs: null, projectName: null, sizeMb: null, targetMb: 0, warnings: [] });
  }
}

// ---------------------------------------------------------------------------
// Wiring
// ---------------------------------------------------------------------------

async function init() {
  loadSettings();

  $("btn-add").addEventListener("click", () => void addViaDialog());
  $("dropzone").addEventListener("click", () => void addViaDialog());
  $("btn-remove").addEventListener("click", removeSelected);
  $("btn-up").addEventListener("click", () => moveSelected(-1));
  $("btn-down").addEventListener("click", () => moveSelected(1));
  document.addEventListener("keydown", (e) => {
    if (e.key === "Delete" && !(e.target instanceof HTMLInputElement) && !state.converting) removeSelected();
  });

  try {
    state.settings = await getSettings();
    const info = await settingsInfo();
    if (info.loadProblem) status(`Einstellungen: ${info.loadProblem}`, "error");
  } catch (e) {
    status(`Einstellungen konnten nicht geladen werden: ${e}`, "error");
  }
  updateCustomDiscLabel();
  $("btn-studio").addEventListener("click", () => void openStudio());
  $("btn-settings").addEventListener("click", async () => {
    if (state.converting || !state.settings) return;
    const info = await settingsInfo();
    openSettings(state.settings, info, (saved) => {
      state.settings = saved;
      updateCustomDiscLabel();
      status("Einstellungen gespeichert.");
      void refreshLayoutCheck();
      scheduleCalc();
    });
  });

  const disc = $("disc") as HTMLSelectElement;
  disc.value = state.disc;
  disc.addEventListener("change", () => {
    const from = mediumOf(state.disc);
    state.disc = disc.value as DiscChoice;
    switchMedium(from, mediumOf(state.disc));
    saveSettings();
    void refreshLayoutCheck();
    scheduleCalc();
    renderAll();
  });
  const twoPass = $("two-pass") as HTMLInputElement;
  twoPass.checked = state.twoPass;
  twoPass.addEventListener("change", () => {
    state.twoPass = twoPass.checked;
    saveSettings();
  });
  const motion = $("motion-menu") as HTMLInputElement;
  motion.checked = state.motion;
  motion.addEventListener("change", () => {
    state.motion = motion.checked;
    saveSettings();
    scheduleCalc();
  });

  const output = $("output") as HTMLInputElement;
  if (!state.output) state.output = await defaultOutputDir().catch(() => "");
  output.value = state.output;
  output.addEventListener("input", () => {
    state.output = output.value;
    saveSettings();
    renderFooter();
  });
  $("btn-output").addEventListener("click", async () => {
    const dir = await open({ directory: true, title: "Ausgabeordner wählen", defaultPath: state.output || undefined });
    if (typeof dir === "string") {
      state.output = dir;
      output.value = dir;
      saveSettings();
      renderFooter();
    }
  });
  $("btn-convert").addEventListener("click", () => void convert());
  $("btn-close-overlay").addEventListener("click", () => ($("overlay").hidden = true));
  $("btn-burn").addEventListener("click", () => {
    if (lastDvd) void openBurner(lastDvd.videoTs, lastDvd.projectName, state.settings, false);
  });
  $("btn-cancel").addEventListener("click", () => void requestCancel());

  document.querySelectorAll<HTMLButtonElement>(".tab").forEach((b) =>
    b.addEventListener("click", () => {
      state.tab = b.dataset.tab as Tab;
      renderTabs();
    }),
  );

  // Native drag & drop delivers file paths (HTML5 drop events do not).
  const dropzone = $("dropzone");
  await getCurrentWebview().onDragDropEvent((e) => {
    const p = e.payload;
    if (p.type === "enter" || p.type === "over") dropzone.classList.add("hover");
    else dropzone.classList.remove("hover");
    if (p.type === "drop" && !state.converting) void addFiles(p.paths);
  });

  await onBuildEvent(onEvent);
  await onBuildDone(onDone);
  await onBuildCancelled(onCancelled);

  try {
    state.templates = await listTemplates();
    if (!state.templates.some((t) => t.path === state.template)) {
      state.template =
        state.templates.find((t) => t.path.endsWith("10_TitlesPerPageNoRoot.ini"))?.path ??
        state.templates.find((t) => t.folder === "Open Style")?.path ??
        state.templates[0]?.path ??
        "";
    }
  } catch (e) {
    status(`Menüvorlagen: ${e}`, "error");
  }
  renderAll();
  status("Bereit. Videos per Drag & Drop oder über „Videos hinzufügen“ laden.");

  const initial = await startupFiles().catch(() => [] as string[]);
  if (initial.length > 0) await addFiles(initial);
  // `convertx-gui <DVD-Ordner>`: burn an existing DVD folder.
  const burnDir = await startupBurn().catch(() => null);
  if (burnDir) {
    const parts = burnDir.split(/[\\/]/).filter(Boolean);
    lastDvd = { videoTs: burnDir, projectName: parts.length > 1 ? parts[parts.length - 2] : null };
    void openBurner(lastDvd.videoTs, lastDvd.projectName, state.settings, false);
  }
}

void init();
