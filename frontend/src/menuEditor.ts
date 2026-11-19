// Menu editor: WYSIWYG design of custom DVD / Blu-ray menus (title, root and settings menus).
//
// Layout: menu manager (left) – stage with drag & drop / resize (centre) – properties (right).
// The stage is the TV frame in storage pixels (720x576 PAL / 720x480 NTSC) shown at 4:3, so
// texts and pictures look as they will on the TV. Buttons are axis-aligned rectangles (no
// rotation, at most 36 per menu, no overlaps) – the DVD subpicture only knows `x0 y0 x1 y1`.
import { ask, open, save } from "@tauri-apps/plugin-dialog";
import { audioWaveform, checkLayout, imagePreview, importConvertxTemplate, loadLayout, saveLayout, textLines, titleFrame } from "./api";
import {
  FONTS,
  maxButtons,
  OVERLAY_LABELS,
  type GradientOverlay,
  chapterLabel,
  draftLayout,
  MENU_PRESETS,
  type MenuPreset,
  rect as newRect,
  madeForOtherTitles,
  remapLayoutByPaths,
  emptyMenu,
  isButton,
  isBdFrame,
  isMoving,
  newId,
  pixelAspectOf,
  text as newText,
  type Action,
  type VideoElement,
  type CustomLayout,
  type CustomMenu,
  type EditorTitle,
  type ElementKey,
  type Fit,
  type LayoutElement,
  type TextElement,
} from "./layout";
import { currentLanguage, t as tr } from "./i18n";

type Child = Node | string | null | undefined | false;
/** A field-level undo step; `execute` re-applies it (for a future redo), `undo` reverts and re-renders. */
interface Command {
  execute(): void;
  undo(): void;
}

function el<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  props: Record<string, unknown> = {},
  ...children: Child[]
): HTMLElementTagNameMap[K] {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(props)) {
    if (v === undefined || v === null || v === false) continue;
    if (k === "class") e.className = String(v);
    else if (k === "style") e.setAttribute("style", String(v));
    else if (k.startsWith("on") && typeof v === "function") e.addEventListener(k.slice(2), v as EventListener);
    else if (k in e && typeof v !== "string") (e as unknown as Record<string, unknown>)[k] = v;
    else e.setAttribute(k, v === true ? "" : String(v));
  }
  for (const c of children) if (c !== null && c !== undefined && c !== false) e.append(c);
  return e;
}

/** What the main window sends when the editor window asks for its data. */
export interface EditorInit {
  layout: CustomLayout | null;
  titles: EditorTitle[];
  /** "PAL" or "NTSC". */
  tv: string;
  /** Default heading for a new draft. */
  heading: string;
  /** `titlesKey` of the videos `layout` was made for (null = unknown). */
  layoutTitlesKey: string | null;
}

export interface EditorOptions extends EditorInit {
  /** Called with a validated copy of the layout; the editor stays open. */
  onApply: (layout: CustomLayout) => void;
  /** Called once the editor has shut down (the window wrapper closes the window). */
  onClose: () => void;
}

const IMAGE_FILTER = [{ name: tr("menuEditor.bilder"), extensions: ["png", "jpg", "jpeg", "bmp", "gif", "webp"] }];
const AUDIO_FILTER = [{ name: tr("menuEditor.audio"), extensions: ["mp3", "wav", "flac", "m4a", "aac", "ac3", "ogg", "opus"] }];
const VIDEO_FILTER = [{ name: "Videos", extensions: ["mp4", "mkv", "avi", "mov", "mpg", "mpeg", "ts", "m2ts"] }];
const LAYOUT_FILTER = [{ name: tr("menuEditor.menueLayout"), extensions: ["json"] }];
const TEMPLATE_FILTER = [{ name: tr("menuEditor.convertxVorlage"), extensions: ["ini"] }];
const FIT_LABELS: [Fit, string][] = [
  ["contain", tr("menuEditor.einpassen")],
  ["cover", tr("menuEditor.fuellenZuschneiden")],
  ["stretch", tr("menuEditor.strecken")],
];
const SNAP = 2;

/** Intro key state at `t` like `menu_anim::lerp`: held before the first and after the last key, linear between. */
export function interpolateKeyframes(keys: ElementKey[], t: number): ElementKey {
  const first = keys[0];
  const last = keys[keys.length - 1];
  if (t <= first.t) return { ...first, t };
  if (t >= last.t) return { ...last, t };
  const i = keys.findIndex((k) => k.t > t);
  const [a, b] = [keys[i - 1], keys[i]];
  const f = b.t > a.t ? (t - a.t) / (b.t - a.t) : 1;
  const mix = (p: number, q: number) => p + (q - p) * f;
  return { t, x: mix(a.x, b.x), y: mix(a.y, b.y), w: mix(a.w, b.w), h: mix(a.h, b.h), opacity: mix(a.opacity, b.opacity) };
}

let closeHook: ((force: boolean) => boolean) | null = null;

/** For the window's close button: true = fine to close now, false = the discard confirm is showing. */
export function requestClose(): boolean {
  return closeHook ? closeHook(false) : true;
}

/** Mounts the menu editor into the document body (the editor window). */
export function mountMenuEditor(opts: EditorOptions) {
  const { titles, tv } = opts;
  if (opts.layout && opts.layoutTitlesKey) {
    const oldPaths = opts.layoutTitlesKey.split("\n");
    const newPaths = titles.map((t) => t.path);
    remapLayoutByPaths(opts.layout, oldPaths, newPaths);
  }
  const stale = !!opts.layout && madeForOtherTitles(opts.layout, opts.layoutTitlesKey, titles);
  let L: CustomLayout = structuredClone(opts.layout ?? draftLayout(titles, tv, opts.heading));
  let menuIdx = 0;
  let selIds: string[] = [];
  let closed = false;
  let clipboard: LayoutElement[] = [];
  const collapsedGroups = new Set<string>();
  let errors: string[] = [];
  let dirty = false;
  let confirmClose = false;
  let showSafe = true;
  /** Undo stack: legacy full-layout JSON snapshots, or small field-level commands (see `beginEdit`). */
  const history: (string | Command)[] = stale && opts.layout ? [JSON.stringify(opts.layout)] : [];
  const previews = new Map<string, Promise<string>>();

  let viewMode: "basic" | "timeline" = "basic";
  let playheadTime = 0.0;
  /** Intro key selected on the timeline (panel shows its values while its element is the only selection). */
  let selKey: { id: string; i: number } | null = null;
  let isPlaying = false;
  let playTimer: number | null = null;
  let timelineZoom = 1;
  let isDrawingAabb = false;
  /** Whether the stage was last rendered with the playhead inside the intro (Cell 1). */
  let lastCell1 = false;

  function snapToClosedGop(secs: number): number {
    const isPal = tv.toLowerCase() === "pal";
    const gopSize = isPal ? 15 : 18;
    const fps = isPal ? 25.0 : 30000 / 1001;
    const frame = Math.round(secs * fps);
    const snappedFrame = Math.round(frame / gopSize) * gopSize;
    return Number((snappedFrame / fps).toFixed(4));
  }

  function formatTimecode(secs: number): string {
    const isPal = tv.toLowerCase() === "pal";
    const fps = isPal ? 25.0 : 30000 / 1001;
    const totalFrames = Math.max(0, Math.round(secs * fps));
    const ff = totalFrames % Math.round(fps);
    const totalSecs = Math.floor(totalFrames / fps);
    const ss = totalSecs % 60;
    const mm = Math.floor(totalSecs / 60) % 60;
    const hh = Math.floor(totalSecs / 3600);
    const pad = (n: number) => String(n).padStart(2, "0");
    return `${pad(hh)}:${pad(mm)}:${pad(ss)}:${pad(ff)}`;
  }

  // ---- helpers --------------------------------------------------------------------------------
  const menu = (): CustomMenu => L.menus[Math.min(menuIdx, L.menus.length - 1)];
  const isSel = (id: string) => selIds.includes(id);
  /** Selected elements of the current menu in z-order. */
  const selection = (): LayoutElement[] => menu()?.elements.filter((e) => isSel(e.id)) ?? [];
  /** The selected element when exactly one is selected. */
  const selected = (): LayoutElement | undefined => {
    const s = selection();
    return s.length === 1 ? s[0] : undefined;
  };
  /** All members of the element's group (just the element when ungrouped). */
  const groupMembers = (e: LayoutElement): LayoutElement[] => (e.group ? menu().elements.filter((x) => x.group === e.group) : [e]);
  const W = () => L.canvas.width;
  const H = () => L.canvas.height;
  const titleName = (n: number) => `${n}: ${titles[n - 1]?.label ?? tr("menuEditor.missing")}`;
  const snap = (v: number) => Math.round(v / SNAP) * SNAP;
  const clampBox = (e: { x: number; y: number; w: number; h: number }) => {
    e.w = Math.max(8, Math.min(Math.round(e.w), W()));
    e.h = Math.max(8, Math.min(Math.round(e.h), H()));
    e.x = Math.max(0, Math.min(Math.round(e.x), W() - e.w));
    e.y = Math.max(0, Math.min(Math.round(e.y), H() - e.h));
  };

  function pushHistory(entry: string | Command) {
    history.push(entry);
    if (history.length > 60) history.shift();
    dirty = true;
  }
  function remember() {
    pushHistory(JSON.stringify(L));
  }
  function undo() {
    const last = history.pop();
    if (!last) return;
    if (typeof last !== "string") {
      last.undo();
      return;
    }
    L = JSON.parse(last);
    const alive = new Set(L.menus.flatMap((m) => m.elements.map((e) => e.id)));
    selIds = selIds.filter((id) => alive.has(id));
    renderAll();
  }

  /**
   * Records `keys` of the objects `resolve` returns (call on pointer-down); the returned `commit`
   * (call on pointer-up) pushes one command holding only those values, or nothing if unchanged.
   * `resolve` is re-run on undo/redo because a snapshot undo replaces `L` with fresh objects.
   */
  function beginEdit<T extends object>(resolve: () => T[], keys: (keyof T)[], render: () => void): () => boolean {
    const read = () => resolve().map((o) => keys.map((k) => structuredClone(o[k])));
    const before = read();
    return () => {
      const after = read();
      if (JSON.stringify(after) === JSON.stringify(before)) return false;
      const write = (vals: T[keyof T][][]) => {
        resolve().forEach((o, i) =>
          keys.forEach((k, j) => {
            const v = structuredClone(vals[i][j]);
            if (v === undefined) delete o[k];
            else o[k] = v;
          }),
        );
        render();
      };
      pushHistory({ execute: () => write(after), undo: () => write(before) });
      return true;
    };
  }
  /** `beginEdit` on fields of the current menu; undo switches back to that menu. */
  function beginMenuEdit(keys: (keyof CustomMenu)[]) {
    const mi = menuIdx;
    return beginEdit(() => [L.menus[mi]], keys, () => {
      if (menuIdx !== mi) {
        menuIdx = mi;
        renderAll();
        return;
      }
      renderHeader();
      renderStage();
      renderTimelinePane();
      renderProps();
      scheduleCheck();
    });
  }

  function preview(kind: "image" | "frame", key: string, load: () => Promise<string>): Promise<string> {
    const k = `${kind}:${key}`;
    let p = previews.get(k);
    if (!p) {
      p = load();
      p.catch(() => previews.delete(k));
      previews.set(k, p);
    }
    return p;
  }
  /** Where the build starts a title's menu clips (the engine's rule when the project did not say). */
  const clipStart = (t: EditorTitle | undefined) => t?.clipStartSecs ?? Math.min((t?.durationSecs ?? 0) * 0.1, 300);
  const frameOf = (title: number, start?: number | null) => {
    const t = titles[title - 1];
    if (!t) return Promise.reject(new Error(tr("menuEditor.titelFehlt")));
    const at = start ?? clipStart(t);
    return preview("frame", `${t.path}@${at.toFixed(1)}`, () => titleFrame(t.path, at));
  };
  const imageOf = (path: string) => preview("image", path, () => imagePreview(path));
  type VideoSource = { title: number; startSecs?: number | null; path?: string };
  /** Preview frame of a video element or background: its external file, or its title. */
  const frameOfVideo = (v: VideoSource) => {
    if (!v.path) return frameOf(v.title, v.startSecs);
    const path = v.path;
    const at = v.startSecs ?? 0;
    return preview("frame", `${path}@${at.toFixed(1)}`, () => titleFrame(path, at));
  };
  const fileName = (p: string) => p.split(/[\\/]/).pop() ?? p;
  /** Name of a video source for badges and the layer list. */
  const videoLabel = (v: VideoSource) => (v.path ? fileName(v.path) : (titles[v.title - 1]?.label ?? tr("menuEditor.titelFehlt2", { title: v.title })));
  /** Start shown for a video without an explicit one (the engine: 10 % of a title, max. 5 min; a file: 0). */
  const defaultStart = (v: VideoSource) => (v.path ? 0 : Math.round(clipStart(titles[v.title - 1])));

  /** Line breaks of wrapped texts, computed by the engine (same fonts and metrics as the build). */
  const lineCache = new Map<string, Promise<string[]>>();
  function linesOf(e: TextElement): Promise<string[]> {
    const key = JSON.stringify([e.text, e.font, e.size, e.bold, e.italic, e.wrap, e.w, e.h, e.rotation, L.canvas, opts.tv]);
    let p = lineCache.get(key);
    if (!p) {
      p = textLines(e, L.canvas, opts.tv);
      p.catch(() => lineCache.delete(key));
      if (lineCache.size > 200) lineCache.clear();
      lineCache.set(key, p);
    }
    return p;
  }

  /** Width of a canvas pixel on the 4:3 TV screen relative to its height (PAL 16/15). */
  const pixelAspect = () => pixelAspectOf(tv, L.canvas);

  /**
   * Size and position of a square-pixel picture of `iw`×`ih` fitted into a `bw`×`bh` box of
   * canvas pixels so that it keeps its proportions on the TV, like the build: the axis that
   * ends up horizontal on the screen (the height when `sideways`) counts `pixelAspect()` wider.
   */
  function fitOnScreen(iw: number, ih: number, bw: number, bh: number, fit: Fit, sideways: boolean) {
    if (fit === "stretch" || iw <= 0 || ih <= 0) return { w: bw, h: bh, x: 0, y: 0 };
    const fx = sideways ? 1 : pixelAspect();
    const fy = sideways ? pixelAspect() : 1;
    const s = (fit === "cover" ? Math.max : Math.min)((bw * fx) / iw, (bh * fy) / ih);
    const w = (iw * s) / fx;
    const h = (ih * s) / fy;
    return { w, h, x: (bw - w) / 2, y: (bh - h) / 2 };
  }

  function actionLabel(a: Action | null | undefined): string {
    if (!a) return "";
    switch (a.type) {
      case "playTitle":
        return tr("menuEditor.titel", { titleName: titleName(a.title) });
      case "playChapter":
        return tr("menuEditor.titel2", { titleName: titleName(a.title), chapterLabel: chapterLabel(titles[a.title - 1], a.chapter) });
      case "playAll":
        return tr("menuEditor.alleAbspielen");
      case "menu":
        return `→ ${L.menus.find((m) => m.id === a.menu)?.name ?? tr("menuEditor.menueFehlt")}`;
      case "setAudio":
        return tr("menuEditor.tonspur", { trackLabel: trackLabel("audio", menu()?.title, a.source) });
      case "setSubtitle":
        return a.source === null ? tr("menuEditor.untertitelAus") : tr("menuEditor.untertitel", { trackLabel: trackLabel("subtitle", menu()?.title, a.source) });
      case "customVm":
        return `VM: ${a.script}`;
    }
  }

  /** Label of a selected track of `title` (source stream index). */
  function trackLabel(kind: "audio" | "subtitle", title: number | null | undefined, source: number): string {
    const t = title ? titles[title - 1] : undefined;
    const list = kind === "audio" ? t?.audioTracks : t?.subtitleTracks;
    return list?.find((x) => x.source === source)?.label ?? tr("menuEditor.streamNichtAusgewaehlt", { source });
  }

  const ROLE_LABELS: Record<CustomMenu["role"], string> = {
    title: tr("menuEditor.titelmenueHauptmenue"),
    root: tr("menuEditor.rootMenueEinesTitels"),
    settings: tr("menuEditor.einstellungenEinesTitelsTon"),
  };

  // ---- DOM skeleton -----------------------------------------------------------------------------
  const menusPane = el("aside", { class: "ed-menus", "aria-label": tr("menuEditor.menues") });
  const stage = el("div", { class: "ed-stage" });
  const viewport = el("div", { class: "ed-viewport" }, stage);
  const toolbar = el("div", { class: "ed-toolbar" });
  // Timeline: header/hint are DOM (rebuilt by renderTimelinePane); tracks are one canvas, kept
  // sticky inside a sized scroller so native scrollbars and the wheel drive the draw offset.
  const tlBar = el("div", { style: "display: contents;" });
  const tlCanvas = el("canvas", { class: "ed-tl-canvas" }) as HTMLCanvasElement;
  const tlSizer = el("div", { class: "ed-tl-sizer" }, tlCanvas);
  const tlScroll = el("div", { class: "ed-tl-scroll" }, tlSizer);
  const timelinePane = el("div", { class: "ed-timeline-pane", style: "display: none;" }, tlBar, tlScroll);
  const centerSection = el("section", { class: "ed-center" }, toolbar, viewport, timelinePane);
  const layersBox = el("section", { class: "ed-layers", "aria-label": tr("menuEditor.elemente") });
  const props = el("aside", { class: "ed-props", "aria-label": tr("menuEditor.eigenschaften") });
  const status = el("footer", { class: "ed-status", role: "status" });
  const headerButtons = el("div", { class: "ed-header-actions" });
  const editor = el(
    "div",
    { class: "ed", role: "dialog", "aria-modal": "true", "aria-label": tr("menuEditor.menueEditor") },
    el("header", { class: "ed-header" }, el("h2", {}, tr("menuEditor.menueEditor")), headerButtons),
    el("div", { class: "ed-body" }, menusPane, centerSection, props),
    status,
  );
  document.body.append(editor);

  // ---- stage scaling (TV frame shown at 4:3) ----------------------------------------------------
  let scaleX = 1;
  let scaleY = 1;
  function layoutStage() {
    const pad = 24;
    const vw = viewport.clientWidth - pad;
    const vh = viewport.clientHeight - pad;
    const aspect = isBdFrame(tv) ? 16 / 9 : 4 / 3;
    const dispH = Math.max(100, Math.min(vh, vw / aspect));
    const dispW = dispH * aspect;
    scaleX = dispW / W();
    scaleY = dispH / H();
    stage.style.width = `${W()}px`;
    stage.style.height = `${H()}px`;
    stage.style.transform = `scale(${scaleX}, ${scaleY})`;
    stage.parentElement!.style.setProperty("--ed-disp-w", `${dispW}px`);
    stage.parentElement!.style.setProperty("--ed-disp-h", `${dispH}px`);
  }
  const ro = new ResizeObserver(() => layoutStage());
  ro.observe(viewport);

  // ---- rendering --------------------------------------------------------------------------------
  function renderAll() {
    menuIdx = Math.max(0, Math.min(menuIdx, L.menus.length - 1));
    renderHeader();
    renderMenus();
    renderToolbar();
    renderStage();
    renderTimelinePane();
    renderProps();
    renderStatus();
    scheduleCheck();
  }

  function renderHeader() {
    headerButtons.replaceChildren(
      el("button", { class: "btn", type: "button", onclick: () => void newDraft() }, tr("menuEditor.entwurfAusTiteln")),
      el(
        "select",
        { class: "btn", title: tr("menuEditor.presetTitle"), onchange: (ev: Event) => void applyPresetFromSelect(ev.target as HTMLSelectElement) },
        el("option", { value: "" }, tr("menuEditor.presetPick")),
        ...MENU_PRESETS.map((p) => el("option", { value: p }, tr(`menuEditor.preset_${p}`))),
      ),
      el("button", { class: "btn", type: "button", onclick: () => void loadFile() }, tr("menuEditor.laden")),
      el("button", { class: "btn", type: "button", onclick: () => void importTemplate() }, tr("menuEditor.ausVorlage")),
      el("button", { class: "btn", type: "button", onclick: () => void saveFile() }, tr("menuEditor.speichern")),
      el("button", { class: "btn", type: "button", disabled: history.length === 0, onclick: undo, title: tr("menuEditor.strgZ") }, tr("menuEditor.rueckgaengig")),
      el("span", { class: "ed-spacer" }),
      el(
        "div",
        { class: "ed-seg ed-view-toggle", role: "group" },
        el(
          "button",
          {
            type: "button",
            class: `btn${viewMode === "basic" ? " active" : ""}`,
            onclick: () => {
              viewMode = "basic";
              stopPlay();
              renderAll();
            },
          },
          "Einfach",
        ),
        el(
          "button",
          {
            type: "button",
            class: `btn${viewMode === "timeline" ? " active" : ""}`,
            onclick: () => {
              viewMode = "timeline";
              timelineZoom = fitZoom();
              renderAll();
            },
          },
          "Erweitert (Timeline)",
        ),
      ),
      el("span", { class: "ed-spacer" }),
      el(
        "button",
        { class: "btn", type: "button", onclick: () => close(false) },
        confirmClose ? tr("menuEditor.aenderungenVerwerfen") : tr("menuEditor.schliessen"),
      ),
      el("button", { class: "btn primary", type: "button", onclick: () => void apply() }, tr("menuEditor.uebernehmen")),
    );
  }

  function renderMenus() {
    const firstTitle = L.menus.findIndex((m) => m.role === "title");
    const items = L.menus.map((m, i) => {
      const buttons = m.elements.filter(isButton).length;
      const incoming = L.menus.reduce(
        (n, o) => n + o.elements.filter((e) => e.action?.type === "menu" && e.action.menu === m.id).length,
        0,
      );
      return el(
        "button",
        {
          class: `ed-menu-item${i === menuIdx ? " active" : ""}`,
          type: "button",
          onclick: () => {
            menuIdx = i;
            selIds = [];
            renderAll();
          },
        },
        el("span", { class: "ed-menu-name" }, m.name || tr("menuEditor.ohneNamen"), i === firstTitle ? el("span", { class: "ed-badge start" }, tr("menuEditor.start")) : null),
        el(
          "span",
          { class: "ed-menu-sub" },
          m.role === "title" ? tr("menuEditor.titelmenue") : tr("menuEditor.titel3", { m: m.role === "root" ? tr("menuEditor.rootMenue") : tr("menuEditor.einstellungen"), m2: m.title ?? "?" }),
          tr("menuEditor.button", { buttons, buttons2: buttons === 1 ? "" : "s" }),
          incoming ? tr("menuEditor.linked", { incoming }) : "",
          isMoving(m) ? tr("menuEditor.moving") : "",
        ),
      );
    });
    const m = menu();
    const parts: Child[] = [
      el("h3", {}, tr("menuEditor.menues")),
      el("div", { class: "ed-menu-list" }, ...items),
      el(
        "div",
        { class: "ed-menu-actions" },
        el("button", { class: "btn", type: "button", onclick: () => addMenu("title") }, tr("menuEditor.titelmenue2")),
        el("button", { class: "btn", type: "button", disabled: titles.length === 0, onclick: () => addMenu("root") }, tr("menuEditor.rootMenue2")),
        el("button", { class: "btn", type: "button", disabled: titles.length === 0, onclick: () => addMenu("settings") }, tr("menuEditor.einstellungen2")),
      ),
      m
        ? el(
            "div",
            { class: "ed-menu-actions" },
            el("button", { class: "btn", type: "button", title: tr("menuEditor.nachOben"), disabled: menuIdx === 0, onclick: () => moveMenu(-1) }, "↑"),
            el("button", { class: "btn", type: "button", title: tr("menuEditor.nachUnten"), disabled: menuIdx >= L.menus.length - 1, onclick: () => moveMenu(1) }, "↓"),
            el("button", { class: "btn", type: "button", onclick: duplicateMenu }, tr("menuEditor.duplizieren")),
            el("button", { class: "btn danger", type: "button", disabled: L.menus.length <= 1, onclick: deleteMenu }, tr("menuEditor.loeschen")),
          )
        : null,
      el(
        "p",
        { class: "ed-hint" },
        tr("menuEditor.titelmenuesLiegenImHauptmenue"),
      ),
      layersBox,
    ];
    menusPane.replaceChildren(...parts.filter((c): c is Node | string => !!c));
  }

  function renderToolbar() {
    const buttons = menu()?.elements.filter(isButton).length ?? 0;
    toolbar.replaceChildren(
      el("button", { class: "btn", type: "button", onclick: addText }, tr("menuEditor.text")),
      el("button", { class: "btn", type: "button", onclick: addRect }, tr("menuEditor.rectangle")),
      el("button", { class: "btn", type: "button", onclick: () => void addImage() }, tr("menuEditor.bild")),
      el("button", { class: "btn", type: "button", disabled: titles.length === 0, onclick: () => addVideo() }, tr("menuEditor.video")),
      el("button", { class: "btn", type: "button", onclick: () => void addExternalVideo() }, tr("menuEditor.externesVideo")),
      el(
        "button",
        {
          class: `btn${isDrawingAabb ? " active" : ""}`,
          type: "button",
          title: "Zeichnet einen 2-Pixel-gerasterten Button-Bereich (AABB)",
          onclick: () => {
            isDrawingAabb = !isDrawingAabb;
            stage.style.cursor = isDrawingAabb ? "crosshair" : "";
            renderToolbar();
          },
        },
        "+ Button-Bereich",
      ),
      el("span", { class: "ed-spacer" }),
      el(
        "label",
        { class: "ed-check" },
        el("input", {
          type: "checkbox",
          checked: showSafe,
          onchange: (e: Event) => {
            showSafe = (e.target as HTMLInputElement).checked;
            renderStage();
          },
        }),
        tr("menuEditor.sichererBereich"),
      ),
      el("span", { class: `ed-counter${buttons > maxButtons(tv) ? " bad" : ""}` }, tr("menuEditor.buttons", { buttons, maxButtons: maxButtons(tv) })),
    );
  }

  function applyBox(node: HTMLElement, e: LayoutElement) {
    node.style.left = `${e.x}px`;
    node.style.top = `${e.y}px`;
    node.style.width = `${e.w}px`;
    node.style.height = `${e.h}px`;
    const inner = node.firstElementChild as HTMLElement | null;
    if (inner) {
      const sideways = e.rotation % 180 === 90;
      const iw = sideways ? e.h : e.w;
      const ih = sideways ? e.w : e.h;
      inner.style.width = `${iw}px`;
      inner.style.height = `${ih}px`;
      inner.style.left = `${(e.w - iw) / 2}px`;
      inner.style.top = `${(e.h - ih) / 2}px`;
      inner.style.transform = e.rotation ? `rotate(${e.rotation}deg)` : "";
    }
  }

  /** Box and opacity of `e` at the playhead: its intro keys while the timeline playhead is in the intro. */
  function boxAt(e: LayoutElement): LayoutElement {
    if (viewMode !== "timeline" || !e.intro?.length || playheadTime >= (menu().introSecs ?? 0)) return e;
    const k = interpolateKeyframes(e.intro, playheadTime);
    return { ...e, x: k.x, y: k.y, w: k.w, h: k.h, opacity: k.opacity };
  }
  /** Moves keyed elements to the playhead without rebuilding the stage (playback, scrubbing). */
  function animateStage() {
    for (const e of menu()?.elements ?? []) {
      if (!e.intro?.length) continue;
      const node = stage.querySelector<HTMLElement>(`[data-id="${CSS.escape(e.id)}"]`);
      if (!node) continue;
      const b = boxAt(e);
      applyBox(node, b);
      (node.firstElementChild as HTMLElement).style.opacity = String(b.opacity);
    }
  }
  function renderElement(e: LayoutElement): HTMLElement {
    const inner = el("div", { class: "ed-inner" });
    inner.style.opacity = String(boxAt(e).opacity);
    if (e.type === "text") {
      inner.classList.add("ed-text");
      inner.textContent = e.text;
      if (e.wrap) {
        // Browser wrapping until the engine's line breaks arrive.
        inner.style.whiteSpace = "pre-wrap";
        linesOf(e)
          .then((lines) => {
            inner.style.whiteSpace = "pre";
            inner.textContent = lines.join("\n");
          })
          .catch(() => undefined);
      }
      Object.assign(inner.style, {
        fontFamily: `"${e.font}", Arial, sans-serif`,
        fontSize: `${e.size}px`,
        color: e.color,
        fontWeight: e.bold ? "700" : "400",
        fontStyle: e.italic ? "italic" : "normal",
        textAlign: e.align,
        textShadow: e.shadow ? "2px 2px 0 rgba(0,0,0,0.67)" : "none",
      });
    } else if (e.type === "rect") {
      Object.assign(inner.style, {
        backgroundColor: e.color,
        boxSizing: "border-box",
        border: e.borderWidth ? `${e.borderWidth}px solid ${e.borderColor ?? "#FFFFFF"}` : "none",
      });
    } else {
      const img = el("img", { class: `ed-media fit-${e.fit}`, alt: "", draggable: false }) as HTMLImageElement;
      inner.append(img);
      // Fitted with the TV's pixel shape (square-pixel preview), like the build.
      img.addEventListener("load", () => {
        const sideways = e.rotation % 180 === 90;
        const r = fitOnScreen(img.naturalWidth, img.naturalHeight, sideways ? e.h : e.w, sideways ? e.w : e.h, e.fit, sideways);
        Object.assign(img.style, { position: "absolute", objectFit: "fill", width: `${r.w}px`, height: `${r.h}px`, left: `${r.x}px`, top: `${r.y}px` });
      });
      const src = e.type === "image" ? imageOf(e.src) : frameOfVideo(e);
      src.then((url) => (img.src = url)).catch(() => inner.classList.add("ed-missing"));
      if (e.type === "video") inner.append(el("span", { class: "ed-video-badge" }, `▶ ${videoLabel(e)}`));
    }
    const isCell1 = viewMode === "timeline" && isButton(e) && playheadTime < (menu().introSecs ?? 0.0);
    const classes = [
      "ed-el",
      isButton(e) ? "is-button" : "",
      e.group ? "in-group" : "",
      isSel(e.id) ? "selected" : "",
      isCell1 ? "hli-inactive" : "",
      e.easterEgg ? "is-easter-egg" : "",
    ]
      .filter(Boolean)
      .join(" ");

    const node = el(
      "div",
      {
        class: classes,
        title: actionLabel(e.action),
        "data-id": e.id,
        style: `--ed-hl: ${e.highlight?.color ?? L.highlight.color}`,
      },
      inner,
    );
    if (isCell1) {
      node.append(el("span", { class: "ed-hli-badge" }, "Subpicture Inaktiv (Cell 1: hli_ss == 0)"));
    }
    if (e.easterEgg) {
      node.append(el("span", { class: "ed-easter-egg-badge" }, "🥚 Easter Egg"));
    }
    applyBox(node, boxAt(e));
    // Cell 1 buttons are only dimmed: they stay editable, otherwise a playhead at 0 locks the whole menu.
    if (isSel(e.id)) {
      for (const corner of ["nw", "ne", "sw", "se"]) {
        node.append(el("span", { class: `ed-handle ${corner}`, "data-handle": corner }));
      }
    }
    node.addEventListener("pointerdown", (ev) => startDrag(ev, e));
    return node;
  }

  function renderStage() {
    const m = menu();
    stage.replaceChildren();
    if (!m) return;
    lastCell1 = viewMode === "timeline" && playheadTime < (m.introSecs ?? 0);
    stage.style.backgroundColor = m.background.color;
    const bgSrc = m.background.video
      ? frameOfVideo(m.background.video)
      : m.background.image
        ? imageOf(m.background.image)
        : null;
    if (bgSrc) {
      const bg = el("div", { class: "ed-bg" });
      bg.style.filter = m.background.blur > 0 ? `blur(${m.background.blur}px)` : "";
      bgSrc
        .then((url) => {
          bg.style.backgroundImage = `url("${url}")`;
          // Cover with the TV's pixel shape, like the build.
          const probe = new Image();
          probe.onload = () => {
            const r = fitOnScreen(probe.naturalWidth, probe.naturalHeight, W(), H(), "cover", false);
            bg.style.backgroundSize = `${r.w}px ${r.h}px`;
          };
          probe.src = url;
        })
        .catch(() => undefined);
      stage.append(bg);
    }
    // Background shading, same look as the ffmpeg filters (encoder::custom_menu_args).
    const overlay = m.background.gradientOverlay ?? "none";
    if (overlay !== "none") stage.append(el("div", { class: `ed-shade shade-${overlay}`, "aria-hidden": "true" }));
    for (const e of m.elements) stage.append(renderElement(e));
    if (showSafe) stage.append(el("div", { class: "ed-safe", "aria-hidden": "true" }));
    layoutStage();
    renderLayers();
  }

  const timelineTotal = () => Math.max(1, (menu().introSecs ?? 0) + (menu().durationSecs ?? 20));
  const pxPerSec = () => 45 * timelineZoom;
  /** Zoom at which the whole menu fits next to the name column. */
  const fitZoom = () => Math.round(Math.max(0.25, Math.min(10, (centerSection.clientWidth - 190) / (timelineTotal() * 45))) * 100) / 100;
  const frameSecs = () => 1 / (tv.toLowerCase() === "pal" ? 25 : 30000 / 1001);

  function stopPlay() {
    if (playTimer !== null) clearInterval(playTimer);
    playTimer = null;
    isPlaying = false;
  }

  /** Moves the playhead (canvas redraw only); the stage only re-renders when Cell 1/2 flips. */
  function setPlayhead(t: number) {
    playheadTime = Math.max(0, Math.min(timelineTotal(), t));
    const tc = timelinePane.querySelector(".ed-timeline-timecode");
    if (tc) tc.textContent = formatTimecode(playheadTime);
    requestTimelineDraw();
    if (playheadTime < (menu().introSecs ?? 0) !== lastCell1) renderStage();
    else animateStage();
  }

  function togglePlay() {
    if (isPlaying) {
      stopPlay();
    } else {
      isPlaying = true;
      let last = performance.now();
      playTimer = window.setInterval(() => {
        const now = performance.now();
        let t = playheadTime + (now - last) / 1000;
        last = now;
        // Like the disc: the intro plays once, then Cell 2 loops.
        if (t >= timelineTotal()) t = menu().introSecs ?? 0;
        setPlayhead(t);
      }, 40);
    }
    renderTimelinePane();
  }

  /**
   * Pointer drag on the timeline canvas (pointer-captured: the canvas is never rebuilt). The menu
   * fields in `edits` become one undo command on release; a pure playhead scrub records nothing.
   */
  function dragOnRuler(ev: PointerEvent, onMove: (secs: number) => void, edits: (keyof CustomMenu)[] = []) {
    ev.preventDefault();
    const commit = edits.length > 0 ? beginMenuEdit(edits) : null;
    const secsAt = (e: PointerEvent) => (e.clientX - tlCanvas.getBoundingClientRect().left - TL_NAME_W + tlScroll.scrollLeft) / pxPerSec();
    const mv = (e: PointerEvent) => onMove(secsAt(e));
    const up = () => {
      tlCanvas.removeEventListener("pointermove", mv);
      tlCanvas.removeEventListener("pointerup", up);
      tlCanvas.removeEventListener("pointercancel", up);
      commit?.();
      renderAll();
    };
    tlCanvas.setPointerCapture(ev.pointerId);
    tlCanvas.addEventListener("pointermove", mv);
    tlCanvas.addEventListener("pointerup", up);
    tlCanvas.addEventListener("pointercancel", up);
    mv(ev);
  }

  // ---- timeline canvas ----------------------------------------------------------------------------
  const TL_NAME_W = 150;
  const TL_RULER_H = 28;
  const TL_GAP = 4;
  const TL_ROW_H = 32;
  const TL_BTN_H = 18;
  type TlHit = { x: number; y: number; w: number; h: number; kind: "ruler" | "markerT" | "markerL" | "element" | "keyframe"; id?: string; key?: number; tip?: string };
  /** Drop indicator (canvas y) while an element row is dragged in the name column. */
  let tlDropY: number | null = null;
  /** Hit boxes in canvas CSS pixels from the last draw, bottom-most first. */
  let tlHits: TlHit[] = [];
  let tlRaf = 0;
  /** Redraws at most once per frame, and only when something asked for it. */
  function requestTimelineDraw() {
    if (!tlRaf) tlRaf = requestAnimationFrame(() => {
      tlRaf = 0;
      drawTimeline();
    });
  }
  /** Decoded video thumbnails by title@secs; `null` while loading. */
  const tlThumbs = new Map<string, HTMLImageElement | null>();
  function tlThumb(v: VideoSource, secs: number): HTMLImageElement | null {
    const k = `${v.path ?? v.title}@${secs.toFixed(1)}`;
    if (!tlThumbs.has(k)) {
      if (tlThumbs.size > 300) tlThumbs.clear();
      tlThumbs.set(k, null);
      frameOfVideo({ ...v, startSecs: secs })
        .then((url) => {
          const img = new Image();
          img.onload = () => {
            tlThumbs.set(k, img);
            requestTimelineDraw();
          };
          img.src = url;
        })
        .catch(() => undefined);
    }
    return tlThumbs.get(k) ?? null;
  }
  const WAVE_PER_SEC = 40;
  /**
   * Waveform peaks by audio source; `null` while loading or when it has no audio. Kept out of the
   * layout so it never lands in the undo stack. Covers the longest menu (60 s intro + 600 s loop).
   */
  const tlWaves = new Map<string, number[] | null>();
  function tlWave(s: CustomMenu["audio"]): number[] | null {
    const t = s.type === "title" ? titles[s.title - 1] : undefined;
    const src = s.type === "file" ? { path: s.path, start: 0 } : t && s.type === "title" ? { path: t.path, start: s.startSecs ?? clipStart(t) } : null;
    if (!src) return null;
    const k = `${src.path}@${src.start}`;
    if (!tlWaves.has(k)) {
      tlWaves.set(k, null);
      audioWaveform(src.path, src.start, 660)
        .then((peaks) => {
          tlWaves.set(k, peaks);
          requestTimelineDraw();
        })
        .catch(() => undefined);
    }
    return tlWaves.get(k) ?? null;
  }

  function drawTimeline() {
    const m = menu();
    if (viewMode !== "timeline" || !m) return;
    const T = m.introSecs ?? 0;
    const loopSecs = m.durationSecs ?? 20;
    const total = timelineTotal();
    const px = pxPerSec();
    const trackW = total * px + 40;
    tlSizer.style.width = `${TL_NAME_W + trackW}px`;
    tlSizer.style.height = `${TL_RULER_H + TL_GAP + 3 * (TL_ROW_H + TL_GAP) + m.elements.length * (TL_BTN_H + TL_GAP) + 8}px`;

    const vw = tlScroll.clientWidth;
    const vh = tlScroll.clientHeight;
    const dpr = window.devicePixelRatio || 1;
    if (tlCanvas.width !== Math.round(vw * dpr) || tlCanvas.height !== Math.round(vh * dpr)) {
      tlCanvas.width = Math.round(vw * dpr);
      tlCanvas.height = Math.round(vh * dpr);
      tlCanvas.style.width = `${vw}px`;
      tlCanvas.style.height = `${vh}px`;
    }
    const ctx = tlCanvas.getContext("2d");
    if (!ctx) return;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, vw, vh);
    const css = getComputedStyle(timelinePane);
    const cssVar = (name: string, fallback: string) => css.getPropertyValue(name).trim() || fallback;
    const muted = cssVar("--muted", "#94a3b8");
    const accent = cssVar("--accent", "#3b82f6");
    const font = (size: number, weight = 400) => `${weight} ${size}px ${css.fontFamily}`;
    const sx = tlScroll.scrollLeft;
    const sy = tlScroll.scrollTop;
    const X = (secs: number) => TL_NAME_W + secs * px - sx;
    const hits: TlHit[] = [];
    ctx.textBaseline = "middle";

    /** One bar of a track; `striped` is the locked (Cell 1) look. */
    const region = (from: number, len: number, y: number, h: number, fill: string, edge: string, label: string, color: string, size = 11, striped = false) => {
      const x = X(from);
      const w = len * px;
      ctx.save();
      ctx.beginPath();
      ctx.rect(x, y, w, h);
      ctx.clip();
      ctx.fillStyle = fill;
      ctx.fillRect(x, y, w, h);
      if (striped) {
        ctx.strokeStyle = "#334155";
        ctx.lineWidth = 8;
        for (let k = x - h; k < x + w; k += 22.6) {
          ctx.beginPath();
          ctx.moveTo(k, y + h);
          ctx.lineTo(k + h, y);
          ctx.stroke();
        }
      }
      ctx.fillStyle = edge;
      ctx.fillRect(x + w - 2, y, 2, h);
      if (label) {
        ctx.fillStyle = color;
        ctx.font = font(size);
        ctx.fillText(label, x + 8, y + h / 2);
      }
      ctx.restore();
    };

    // ---- rows (clipped below the ruler, right of the name column) ----
    const rows: { label: string; h: number; sel: boolean; id?: string; keys?: ElementKey[]; draw: (y: number, h: number) => void }[] = [];
    const vid = m.background.video ?? m.elements.find((e): e is VideoElement => e.type === "video");
    rows.push({
      label: tr("menuEditor.tlVideo"),
      h: TL_ROW_H,
      sel: false,
      draw: (y, h) => {
        if (vid && (vid.path || titles[vid.title - 1])) {
          const n = Math.min(24, Math.ceil(trackW / 64));
          const slot = (total * px) / n;
          ctx.globalAlpha = 0.55;
          for (let i = 0; i < n; i++) {
            const x = X(0) + i * slot;
            if (x + slot < TL_NAME_W || x > vw) continue;
            const t = ((i + 0.5) * total) / n;
            const img = tlThumb(vid, (vid.startSecs ?? 0) + (t >= T ? t - T : t));
            if (!img) continue;
            // background-size: cover
            const s = Math.max(slot / img.naturalWidth, h / img.naturalHeight);
            const cw = slot / s;
            const ch = h / s;
            ctx.drawImage(img, (img.naturalWidth - cw) / 2, (img.naturalHeight - ch) / 2, cw, ch, x, y, slot, h);
          }
          ctx.globalAlpha = 1;
        }
        if (T > 0) region(0, T, y, h, "rgba(59, 130, 246, 0.35)", "#3b82f6", tr("menuEditor.tlIntro", { secs: T.toFixed(1) }), "#93c5fd");
        region(T, loopSecs, y, h, "rgba(34, 197, 94, 0.35)", "#22c55e", tr("menuEditor.tlLoop", { secs: loopSecs.toFixed(1) }), "#86efac");
      },
    });
    rows.push({
      label: tr("menuEditor.tlAudio"),
      h: TL_ROW_H,
      sel: false,
      draw: (y, h) => {
        region(0, total, y, h, "rgba(168, 85, 247, 0.25)", "rgba(168, 85, 247, 0.4)", "", "");
        const wave = tlWave(m.audio);
        if (wave) {
          // One bar per screen column: the loudest peak that falls into it (zoomed out, many do).
          ctx.fillStyle = muted;
          const x0 = X(0);
          const end = Math.min(vw, X(Math.min(total, wave.length / WAVE_PER_SEC)));
          for (let x = Math.max(TL_NAME_W, Math.floor(x0)); x < end; x++) {
            const a = Math.floor(((x - x0) / px) * WAVE_PER_SEC);
            const b = Math.max(a + 1, Math.floor(((x + 1 - x0) / px) * WAVE_PER_SEC));
            let v = 0;
            for (let i = a; i < b && i < wave.length; i++) v = Math.max(v, wave[i]);
            const lh = Math.max(1, (v / 255) * (h - 4));
            ctx.fillRect(x, y + (h - lh) / 2, 1, lh);
          }
        }
        ctx.strokeStyle = "rgba(168, 85, 247, 0.4)";
        ctx.strokeRect(X(0) + 0.5, y + 0.5, total * px - 1, h - 1);
        ctx.fillStyle = css.color;
        ctx.font = font(11);
        ctx.fillText(m.audio.type === "none" ? tr("menuEditor.tlNoAudio") : tr("menuEditor.tlAudioLoops"), X(0) + 8, y + h / 2);
      },
    });
    rows.push({
      label: tr("menuEditor.tlLayers", { n: m.elements.length }),
      h: TL_ROW_H,
      sel: false,
      draw: (y, h) => {
        if (T > 0) region(0, T, y, h, "#1e293b", "#64748b", tr("menuEditor.tlLocked"), "#94a3b8", 10, true);
        region(T, loopSecs, y, h, "rgba(234, 179, 8, 0.35)", "#eab308", tr("menuEditor.tlActive"), "#fef08a", 10);
      },
    });
    // NLE order: the top row is the frontmost layer (last in `elements`)
    for (const e of [...m.elements].reverse()) {
      const name =
        e.type === "text" ? e.text.replace(/\s+/g, " ").trim() || tr("menuEditor.tlText") : e.type === "image" ? tr("menuEditor.bild2") : e.type === "rect" ? tr("menuEditor.rectangle2") : videoLabel(e);
      rows.push({
        label: e.type !== "text" && isButton(e) ? actionLabel(e.action) : name,
        h: TL_BTN_H,
        sel: isSel(e.id),
        id: e.id,
        keys: T > 0 ? e.intro : undefined,
        draw: isButton(e)
          ? (y, h) => {
              if (T > 0) region(0, T, y, h, "#1e293b", "#64748b", "", "", 10, true);
              region(T, loopSecs, y, h, "rgba(234, 179, 8, 0.35)", "#eab308", actionLabel(e.action), "#fef08a", 10);
            }
          : (y, h) => region(0, T + loopSecs, y, h, "rgba(100, 116, 139, 0.35)", "#64748b", name, "#cbd5e1", 10),
      });
    }
    let y = TL_RULER_H + TL_GAP - sy;
    for (const r of rows) {
      if (y + r.h > TL_RULER_H && y < vh) {
        // track background and bars, clipped to the scrolling area
        ctx.save();
        ctx.beginPath();
        ctx.rect(TL_NAME_W, TL_RULER_H, vw - TL_NAME_W, vh - TL_RULER_H);
        ctx.clip();
        ctx.beginPath();
        ctx.roundRect(X(0), y, trackW, r.h, 4);
        ctx.fillStyle = "rgba(0, 0, 0, 0.2)";
        ctx.fill();
        ctx.save();
        ctx.clip();
        r.draw(y, r.h);
        ctx.restore();
        if (r.sel) {
          ctx.strokeStyle = accent;
          ctx.lineWidth = 1;
          ctx.strokeRect(X(0) + 0.5, y + 0.5, trackW - 1, r.h - 1);
        }
        // intro keyframes: NLE diamonds at their time
        const d = r.h / 2 - 2;
        r.keys?.forEach((k, i) => {
          const picked = selKey?.id === r.id && selKey?.i === i;
          const cx = X(k.t);
          const cy = y + r.h / 2;
          ctx.beginPath();
          ctx.moveTo(cx, cy - d);
          ctx.lineTo(cx + d, cy);
          ctx.lineTo(cx, cy + d);
          ctx.lineTo(cx - d, cy);
          ctx.closePath();
          ctx.fillStyle = picked ? accent : "#ffffff";
          ctx.fill();
          ctx.strokeStyle = "#0f172a";
          ctx.lineWidth = 1;
          ctx.stroke();
        });
        ctx.restore();
        // name column (fixed horizontally)
        ctx.save();
        ctx.beginPath();
        ctx.rect(0, TL_RULER_H, TL_NAME_W - 4, vh - TL_RULER_H);
        ctx.clip();
        ctx.fillStyle = r.sel ? accent : css.color;
        ctx.font = font(11, r.id ? (r.sel ? 700 : 400) : 600);
        ctx.fillText(r.label, r.id ? 20 : 8, y + r.h / 2);
        if (r.id) {
          // grip: the row can be dragged to change the stacking order
          ctx.fillStyle = muted;
          ctx.fillText("⋮⋮", 7, y + r.h / 2);
        }
        ctx.restore();
        if (r.id) hits.push({ x: 0, y: Math.max(y, TL_RULER_H), w: vw, h: Math.min(r.h, y + r.h - TL_RULER_H), kind: "element", id: r.id });
        r.keys?.forEach((k, i) => {
          const cx = X(k.t);
          if (cx >= TL_NAME_W) hits.push({ x: cx - 7, y: Math.max(y, TL_RULER_H), w: 14, h: r.h, kind: "keyframe", id: r.id, key: i, tip: `${k.t.toFixed(2)} s` });
        });
      }
      y += r.h + TL_GAP;
    }
    if (tlDropY !== null) {
      ctx.fillStyle = accent;
      ctx.fillRect(0, tlDropY - 1, vw, 2);
    }

    // ---- ruler (fixed vertically) ----
    ctx.fillStyle = cssVar("--panel", "#0f172a");
    ctx.fillRect(0, 0, vw, TL_RULER_H);
    ctx.fillStyle = cssVar("--border", "#334155");
    ctx.fillRect(TL_NAME_W, TL_RULER_H - 1, vw - TL_NAME_W, 1);
    ctx.save();
    ctx.beginPath();
    ctx.rect(TL_NAME_W, 0, vw - TL_NAME_W, vh);
    ctx.clip();
    const tickStep = [1, 2, 5, 10, 30, 60, 120, 300].find((s) => s * px >= 56) ?? 300;
    ctx.font = font(9);
    ctx.textBaseline = "alphabetic";
    for (let sec = 0; sec <= total; sec += tickStep) {
      const x = X(sec);
      if (x < TL_NAME_W - 60 || x > vw) continue;
      ctx.fillStyle = "rgba(255, 255, 255, 0.35)";
      ctx.fillRect(x, TL_RULER_H - 8, 1, 8);
      ctx.fillStyle = muted;
      ctx.fillText(`${Math.floor(sec / 60)}:${String(sec % 60).padStart(2, "0")}`, x + 3, TL_RULER_H - 7);
    }
    hits.push({ x: TL_NAME_W, y: 0, w: vw - TL_NAME_W, h: TL_RULER_H, kind: "ruler" });

    // ---- markers: a 2 px line through the ruler with a pill label (T right of it, L left of it) ----
    ctx.textBaseline = "middle";
    ctx.font = font(9, 700);
    const marker = (secs: number, color: string, label: string, right: boolean, kind: "markerT" | "markerL", tip: string) => {
      const x = X(secs);
      const pw = ctx.measureText(label).width + 12;
      const px0 = right ? x - 1 : x + 3 - pw;
      ctx.fillStyle = color;
      ctx.fillRect(x, 0, 2, TL_RULER_H);
      ctx.beginPath();
      ctx.roundRect(px0, 0, pw, 14, 7);
      ctx.fill();
      ctx.fillStyle = "#fff";
      ctx.fillText(label, px0 + 6, 7);
      hits.push({ x: Math.min(px0, x - 4), y: 0, w: pw + 5, h: TL_RULER_H, kind, tip });
    };
    marker(T, "#3b82f6", tr("menuEditor.tlIntroEnd"), true, "markerT", tr("menuEditor.tlIntroTip", { secs: T.toFixed(2) }));
    marker(total, "#22c55e", tr("menuEditor.tlLoopEnd"), false, "markerL", tr("menuEditor.tlLoopTip", { secs: total.toFixed(2) }));

    // ---- playhead over everything ----
    const phx = X(playheadTime);
    ctx.fillStyle = "#ef4444";
    ctx.fillRect(phx, 0, 2, vh);
    ctx.beginPath();
    ctx.moveTo(phx - 5, 0);
    ctx.lineTo(phx + 7, 0);
    ctx.lineTo(phx + 1, 8);
    ctx.fill();
    ctx.restore();
    tlHits = hits;
  }

  function tlHitAt(ev: PointerEvent): TlHit | undefined {
    const r = tlCanvas.getBoundingClientRect();
    const x = ev.clientX - r.left;
    const y = ev.clientY - r.top;
    for (let i = tlHits.length - 1; i >= 0; i--) {
      const h = tlHits[i];
      if (x >= h.x && x < h.x + h.w && y >= h.y && y < h.y + h.h) return h;
    }
    return undefined;
  }
  tlCanvas.addEventListener("pointermove", (ev) => {
    if (tlCanvas.hasPointerCapture(ev.pointerId)) return;
    const h = tlHitAt(ev);
    const inNames = ev.clientX - tlCanvas.getBoundingClientRect().left < TL_NAME_W;
    tlCanvas.style.cursor =
      !h ? "" : h.kind === "markerT" || h.kind === "markerL" || h.kind === "keyframe" ? "ew-resize" : h.kind === "element" && inNames ? "grab" : "pointer";
    tlCanvas.title = h?.tip ?? "";
  });
  tlCanvas.addEventListener("pointerdown", (ev) => {
    const h = tlHitAt(ev);
    if (ev.button !== 0 || !h) return;
    const m = menu();
    selKey = h.kind === "keyframe" ? { id: h.id!, i: h.key! } : null;
    if (h.kind === "element") {
      const b = m.elements.find((e) => e.id === h.id);
      if (!b) return;
      selIds = groupMembers(b).map((x) => x.id);
      renderStage();
      renderProps();
      requestTimelineDraw();
      if (ev.clientX - tlCanvas.getBoundingClientRect().left < TL_NAME_W) dragRow(ev, b.id);
    } else if (h.kind === "keyframe") {
      selIds = [h.id!];
      const k = m.elements.find((e) => e.id === h.id)?.intro?.[h.key!];
      if (k) setPlayhead(k.t);
      renderStage();
      renderProps();
      dragKeyframe(ev, h.id!, h.key!);
    } else if (h.kind === "markerT") {
      dragOnRuler(ev, (secs) => {
        m.introSecs = snapToClosedGop(Math.max(0, Math.min(60, secs)));
        requestTimelineDraw();
        if (playheadTime < m.introSecs !== lastCell1) renderStage();
      }, ["introSecs"]);
    } else if (h.kind === "markerL") {
      dragOnRuler(ev, (secs) => {
        const t0 = m.introSecs ?? 0;
        m.durationSecs = Math.min(600, Math.max(1, snapToClosedGop(Math.max(t0 + 1, secs)) - t0));
        requestTimelineDraw();
      }, ["durationSecs"]);
    } else {
      stopPlay();
      dragOnRuler(ev, (secs) => setPlayhead(secs));
    }
  });
  /**
   * Drags an element row in the name column; dropping it above/below another row moves the element
   * in front of/behind it in `elements` (the stacking order, one undo step). Below 4 px it stays a click.
   */
  function dragRow(ev: PointerEvent, id: string) {
    ev.preventDefault();
    const y0 = ev.clientY;
    let target: { id: string; after: boolean } | null = null;
    const mv = (e: PointerEvent) => {
      if (tlDropY === null && Math.abs(e.clientY - y0) < 4) return;
      const y = e.clientY - tlCanvas.getBoundingClientRect().top;
      const rows = tlHits.filter((h) => h.kind === "element");
      const h = rows.find((r) => y < r.y + r.h + TL_GAP) ?? rows[rows.length - 1];
      if (!h?.id) return;
      const after = y >= h.y + h.h / 2;
      target = { id: h.id, after };
      tlDropY = after ? h.y + h.h + TL_GAP / 2 : h.y - TL_GAP / 2;
      tlCanvas.style.cursor = "grabbing";
      requestTimelineDraw();
    };
    const up = () => {
      tlCanvas.removeEventListener("pointermove", mv);
      tlCanvas.removeEventListener("pointerup", up);
      tlCanvas.removeEventListener("pointercancel", up);
      const dropped = tlDropY !== null ? target : null;
      tlDropY = null;
      tlCanvas.style.cursor = "";
      // rows run top = frontmost, so visually below a row means before it in `elements`
      if (dropped) moveElement(id, dropped.id, !dropped.after);
      else requestTimelineDraw();
    };
    tlCanvas.setPointerCapture(ev.pointerId);
    tlCanvas.addEventListener("pointermove", mv);
    tlCanvas.addEventListener("pointerup", up);
    tlCanvas.addEventListener("pointercancel", up);
  }
  /** Element by id in the current menu (re-resolved: a snapshot undo replaces the objects). */
  const elementOf = (mi: number, id: string) => L.menus[mi]?.elements.find((e) => e.id === id);
  /** Time-shifts one intro key between its neighbours (and 0..introSecs); one undo step. */
  function dragKeyframe(ev: PointerEvent, id: string, i: number) {
    const mi = menuIdx;
    const m = menu();
    const keys = elementOf(mi, id)?.intro;
    if (!keys?.[i]) return;
    const T = m.introSecs ?? 0;
    const lo = keys[i - 1]?.t ?? 0;
    const hi = keys[i + 1]?.t ?? T;
    const commit = beginEdit(() => [elementOf(mi, id)!], ["intro"], renderAll);
    const secsAt = (e: PointerEvent) => (e.clientX - tlCanvas.getBoundingClientRect().left - TL_NAME_W + tlScroll.scrollLeft) / pxPerSec();
    const mv = (e: PointerEvent) => {
      keys[i].t = Math.round(Math.min(hi, Math.max(lo, secsAt(e))) * 100) / 100;
      tlCanvas.title = `${keys[i].t.toFixed(2)} s`;
      requestTimelineDraw();
    };
    const up = () => {
      tlCanvas.removeEventListener("pointermove", mv);
      tlCanvas.removeEventListener("pointerup", up);
      tlCanvas.removeEventListener("pointercancel", up);
      if (commit()) renderAll();
    };
    ev.preventDefault();
    tlCanvas.setPointerCapture(ev.pointerId);
    tlCanvas.addEventListener("pointermove", mv);
    tlCanvas.addEventListener("pointerup", up);
    tlCanvas.addEventListener("pointercancel", up);
  }
  // Double-click on an element track inside the intro adds a key there (copy of the nearest key, or of the element).
  tlCanvas.addEventListener("dblclick", (ev) => {
    const r = tlCanvas.getBoundingClientRect();
    if (ev.clientX - r.left < TL_NAME_W) return;
    const h = tlHits.find((x) => x.kind === "element" && ev.clientY - r.top >= x.y && ev.clientY - r.top < x.y + x.h);
    const m = menu();
    const T = m.introSecs ?? 0;
    const t = Math.round(((ev.clientX - r.left - TL_NAME_W + tlScroll.scrollLeft) / pxPerSec()) * 100) / 100;
    if (!h?.id || T <= 0 || t < 0 || t > T) return;
    const mi = menuIdx;
    const e = elementOf(mi, h.id);
    if (!e) return;
    const keys = e.intro ?? [];
    if (keys.some((k) => Math.abs(k.t - t) < 0.01)) return;
    const near = keys.reduce<ElementKey | null>((b, k) => (!b || Math.abs(k.t - t) < Math.abs(b.t - t) ? k : b), null);
    const src = near ?? e;
    const commit = beginEdit(() => [elementOf(mi, h.id!)!], ["intro"], renderAll);
    e.intro = [...keys, { t, x: src.x, y: src.y, w: src.w, h: src.h, opacity: src.opacity }].sort((a, b) => a.t - b.t);
    commit();
    renderAll();
  });
  function moveElement(id: string, targetId: string, after: boolean) {
    const all = menu().elements;
    const from = all.findIndex((e) => e.id === id);
    if (from < 0 || id === targetId) return;
    const before = JSON.stringify(L);
    const [e] = all.splice(from, 1);
    all.splice(all.findIndex((x) => x.id === targetId) + (after ? 1 : 0), 0, e);
    if (JSON.stringify(L) === before) return;
    pushHistory(before);
    renderAll();
  }
  tlScroll.addEventListener("scroll", requestTimelineDraw, { passive: true });
  new ResizeObserver(requestTimelineDraw).observe(tlScroll);

  function renderTimelinePane() {
    if (viewMode !== "timeline") {
      stopPlay();
      timelinePane.style.display = "none";
      centerSection.classList.remove("has-timeline");
      return;
    }
    timelinePane.style.display = "flex";
    centerSection.classList.add("has-timeline");

    const m = menu();
    const isPal = tv.toLowerCase() === "pal";
    const T = m.introSecs ?? 0.0;
    playheadTime = Math.min(playheadTime, timelineTotal());

    const timecodeDisplay = el("span", { class: "ed-timeline-timecode", title: "Position des roten Playheads (Std:Min:Sek:Frame)" }, formatTimecode(playheadTime));
    const gopPill = el(
      "span",
      { class: "ed-gop-pill", title: "Schnitte rasten auf Closed-GOP-Grenzen ein (DVD-Pflicht f\u00fcr nahtlose Schleifen)" },
      `Raster: ${isPal ? "0,6 s (PAL)" : "0,6 s (NTSC)"}`,
    );

    const playBtn = el("button", { class: `btn${isPlaying ? " active" : ""}`, type: "button", title: "Vorschau abspielen/pausieren (Leertaste)", onclick: togglePlay }, isPlaying ? "⏸ Pause" : "▶ Vorschau");
    const setTBtn = el(
      "button",
      {
        class: "btn",
        type: "button",
        title: "Das Intro endet an der Position des roten Playheads",
        onclick: () => {
          remember();
          m.introSecs = snapToClosedGop(playheadTime);
          renderAll();
        },
      },
      "Intro endet hier",
    );
    const noIntroBtn =
      T > 0
        ? el(
            "button",
            {
              class: "btn",
              type: "button",
              title: "Kein Intro: Buttons sind von Anfang an aktiv",
              onclick: () => {
                remember();
                m.introSecs = 0;
                renderAll();
              },
            },
            "Kein Intro",
          )
        : null;

    // Redraw only: rebuilding the header would replace the slider mid-drag.
    const setZoom = (z: number) => {
      timelineZoom = Math.max(0.25, Math.min(10, z));
      zoomSlider.value = String(timelineZoom);
      requestTimelineDraw();
    };
    const zoomSlider = el("input", {
      type: "range",
      min: "0.25",
      max: "10",
      step: "any",
      value: String(timelineZoom),
      style: "width: 90px;",
      title: "Zoom",
      oninput: (e: Event) => setZoom(Number((e.target as HTMLInputElement).value)),
    }) as HTMLInputElement;
    const fitBtn = el("button", { class: "btn", type: "button", title: "Zoom so w\u00e4hlen, dass alles sichtbar ist", onclick: () => setZoom(fitZoom()) }, "Einpassen");

    const timelineHeader = el(
      "div",
      { class: "ed-timeline-header" },
      playBtn,
      timecodeDisplay,
      setTBtn,
      noIntroBtn,
      gopPill,
      el("span", { class: "ed-spacer" }),
      fitBtn,
      zoomSlider,
    );

    const hint = el(
      "div",
      { class: "ed-timeline-hint" },
      T > 0
        ? `Das Men\u00fc spielt zuerst das Intro (blau, ${T.toFixed(1)} s) einmal ab und wiederholt dann die Schleife (gr\u00fcn). Buttons sind erst nach dem Intro anklickbar. Blauen Marker ziehen = Introl\u00e4nge, gr\u00fcnen Marker ziehen = Schleifenende, ins Lineal klicken = Playhead setzen.`
        : `Das Men\u00fc l\u00e4uft als Schleife (gr\u00fcn) und alle Buttons sind sofort aktiv. F\u00fcr ein Intro, das nur einmal abgespielt wird: Playhead an die gew\u00fcnschte Stelle setzen und „Intro endet hier“ klicken – oder den blauen Marker ziehen.`,
    );

    tlBar.replaceChildren(timelineHeader, hint);
    requestTimelineDraw();
  }

  // ---- selection, drag & resize -------------------------------------------------------------------
  function refreshSelection() {
    renderStage();
    renderProps();
  }

  /** Bounding box of a set of boxes. */
  function unionBox(list: { x: number; y: number; w: number; h: number }[]) {
    const x0 = Math.min(...list.map((b) => b.x));
    const y0 = Math.min(...list.map((b) => b.y));
    return { x: x0, y: y0, w: Math.max(...list.map((b) => b.x + b.w)) - x0, h: Math.max(...list.map((b) => b.y + b.h)) - y0 };
  }

  /** Moves `list` by (dx, dy), clamped on the selection's bounding box so relative spacing is kept. */
  function shiftBy(list: LayoutElement[], dx: number, dy: number) {
    const u = unionBox(list);
    const ddx = Math.round(Math.max(-u.x, Math.min(dx, W() - u.w - u.x)));
    const ddy = Math.round(Math.max(-u.y, Math.min(dy, H() - u.h - u.y)));
    for (const e of list) {
      e.x += ddx;
      e.y += ddy;
    }
  }

  /**
   * Intro key the stage edits instead of `e`'s base box: the selected key of `e`, or, while another
   * element's key is selected, `e`'s key at the playhead. Anything else clears the key selection.
   */
  function stageKey(e: LayoutElement): ElementKey | null {
    if (viewMode === "timeline" && selKey) {
      const own = selKey.id === e.id ? e.intro?.[selKey.i] : undefined;
      if (own) {
        // the stage must show the key's state, or the drag would jump from the interpolated box
        if (Math.abs(own.t - playheadTime) >= 0.005) setPlayhead(own.t);
        return own;
      }
      const i = e.intro?.findIndex((k) => Math.abs(k.t - playheadTime) < 0.005) ?? -1;
      if (i >= 0) {
        selKey = { id: e.id, i };
        requestTimelineDraw();
        return e.intro![i];
      }
    }
    if (selKey) requestTimelineDraw();
    selKey = null;
    return null;
  }

  function startDrag(ev: PointerEvent, e: LayoutElement) {
    if (ev.button !== 0) return;
    const key = stageKey(e);
    ev.stopPropagation();
    ev.preventDefault();
    const handle = (ev.target as HTMLElement).dataset.handle;
    // Plain click on an already-selected element: collapse the selection if it was not a drag.
    let collapseTo: string[] | null = null;
    if (key) {
      // A key moves alone (no group/multi-select drag).
      if (selIds.length !== 1 || selIds[0] !== e.id) {
        selIds = [e.id];
        refreshSelection();
      }
    } else if (!handle) {
      const ids = (ev.altKey ? [e] : groupMembers(e)).map((x) => x.id);
      if (ev.shiftKey) {
        if (isSel(e.id)) {
          selIds = selIds.filter((id) => !ids.includes(id));
          refreshSelection();
          return;
        }
        selIds = [...selIds, ...ids];
        refreshSelection();
      } else if (!isSel(e.id)) {
        selIds = ids;
        refreshSelection();
      } else {
        collapseTo = ids;
      }
    }
    // The stage may have been re-rendered: continue on the fresh nodes.
    const boxes: { id: string; box: { x: number; y: number; w: number; h: number } }[] = key ? [{ id: e.id, box: key }] : selection().map((t) => ({ id: t.id, box: t }));
    const targets = boxes.map(({ id, box: t }) => ({ t, node: stage.querySelector<HTMLElement>(`[data-id="${CSS.escape(id)}"]`), x: t.x, y: t.y, w: t.w, h: t.h }));
    // Keys may start off-canvas (slide-ins), so only base boxes are clamped to the frame.
    const clamp = (b: { x: number; y: number; w: number; h: number }) => (key ? undefined : clampBox(b));
    const show = (node: HTMLElement | null, b: { x: number; y: number; w: number; h: number }) => {
      if (!node) return;
      applyBox(node, key ? { ...e, x: b.x, y: b.y, w: b.w, h: b.h } : (b as LayoutElement));
      if (key) renderProps();
    };
    const union = unionBox(targets.map((s0) => ({ x: s0.x, y: s0.y, w: s0.w, h: s0.h })));
    const startX = ev.clientX;
    const startY = ev.clientY;
    const before = JSON.stringify(L);
    let moved = false;
    const onMove = (mv: PointerEvent) => {
      const dx = (mv.clientX - startX) / scaleX;
      const dy = (mv.clientY - startY) / scaleY;
      if (!moved && Math.abs(dx) + Math.abs(dy) < 2) return;
      moved = true;
      if (!handle) {
        // Snap the selection's top-left corner, clamp the union box inside the canvas.
        const ddx = key ? snap(union.x + dx) - union.x : Math.max(-union.x, Math.min(snap(union.x + dx) - union.x, W() - union.w - union.x));
        const ddy = key ? snap(union.y + dy) - union.y : Math.max(-union.y, Math.min(snap(union.y + dy) - union.y, H() - union.h - union.y));
        for (const s0 of targets) {
          s0.t.x = Math.round(s0.x + ddx);
          s0.t.y = Math.round(s0.y + ddy);
          clamp(s0.t);
          show(s0.node, s0.t);
        }
        return;
      }
      for (const s0 of targets) {
        let { x, y, w, h } = s0;
        if (handle.includes("e")) w = s0.w + dx;
        if (handle.includes("s")) h = s0.h + dy;
        if (handle.includes("w")) {
          x = s0.x + dx;
          w = s0.w - dx;
        }
        if (handle.includes("n")) {
          y = s0.y + dy;
          h = s0.h - dy;
        }
        if (mv.shiftKey) {
          // Keep each element's aspect ratio.
          const r = s0.w / s0.h;
          if (Math.abs(w - s0.w) > Math.abs(h - s0.h)) h = w / r;
          else w = h * r;
        }
        s0.t.x = snap(x);
        s0.t.y = snap(y);
        s0.t.w = snap(Math.max(8, w));
        s0.t.h = snap(Math.max(8, h));
        clamp(s0.t);
        show(s0.node, s0.t);
      }
    };
    const onUp = () => {
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup", onUp);
      if (moved) {
        pushHistory(before);
        renderProps();
        if (key) renderStage();
        renderHeader();
        scheduleCheck();
      } else if (collapseTo) {
        selIds = collapseTo;
        refreshSelection();
      }
    };
    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup", onUp);
  }

  /** Drag on the empty stage draws a marquee or an AABB button box; a click without movement clears the selection. */
  stage.addEventListener("pointerdown", (ev) => {
    const t = ev.target as HTMLElement;
    if (ev.button !== 0 || !(t === stage || t.classList.contains("ed-bg") || t.classList.contains("ed-safe"))) return;
    ev.preventDefault();

    if (isDrawingAabb) {
      const origin = stage.getBoundingClientRect();
      const pos = (e: PointerEvent) => ({ x: (e.clientX - origin.left) / scaleX, y: (e.clientY - origin.top) / scaleY });
      const start = pos(ev);
      const snapX = (val: number) => Math.max(0, Math.min(W() - 2, Math.floor(val / 2) * 2));
      const snapY = (val: number) => Math.max(0, Math.min(H() - 2, Math.floor(val / 2) * 2));

      const startX = snapX(start.x);
      const startY = snapY(start.y);

      const box = el("div", { class: "ed-aabb-preview" });
      stage.append(box);

      const onMove = (mv: PointerEvent) => {
        const cur = pos(mv);
        const curX = snapX(cur.x);
        const curY = snapY(cur.y);
        const x0 = Math.min(startX, curX);
        const y0 = Math.min(startY, curY);
        const x1 = Math.max(startX, curX) | 1;
        const y1 = Math.max(startY, curY) | 1;
        Object.assign(box.style, {
          left: `${x0}px`,
          top: `${y0}px`,
          width: `${x1 - x0 + 1}px`,
          height: `${y1 - y0 + 1}px`,
        });
      };

      const onUp = (up: PointerEvent) => {
        window.removeEventListener("pointermove", onMove);
        window.removeEventListener("pointerup", onUp);
        box.remove();

        const cur = pos(up);
        const curX = snapX(cur.x);
        const curY = snapY(cur.y);
        const x0 = Math.min(startX, curX);
        const y0 = Math.min(startY, curY);
        const x1 = Math.min(W() - 1, Math.max(startX, curX) | 1);
        const y1 = Math.min(H() - 1, Math.max(startY, curY) | 1);
        const w = x1 - x0 + 1;
        const h = y1 - y0 + 1;

        if (w < 16 || h < 16) {
          isDrawingAabb = false;
          stage.style.cursor = "";
          renderToolbar();
          return;
        }

        const currentButtons = menu().elements.filter(isButton);
        if (currentButtons.length >= 36) {
          alert(tr("menuEditor.maxButtonsAlert"));
          isDrawingAabb = false;
          stage.style.cursor = "";
          renderToolbar();
          return;
        }

        const overlaps = currentButtons.some((b) => {
          return x0 < b.x + b.w && x0 + w > b.x && y0 < b.y + b.h && y0 + h > b.y;
        });

        if (overlaps) {
          alert(tr("menuEditor.overlappingButtonsAlert"));
          isDrawingAabb = false;
          stage.style.cursor = "";
          renderToolbar();
          return;
        }

        remember();
        const newBtn = newText(x0, y0, w, h, `Button ${currentButtons.length + 1}`, {
          size: Math.max(14, Math.min(28, Math.round(h * 0.5))),
          wrap: true,
          align: "center",
          action: { type: "playTitle", title: 1 },
        });
        menu().elements.push(newBtn);
        selIds = [newBtn.id];
        isDrawingAabb = false;
        stage.style.cursor = "";
        renderAll();
      };

      window.addEventListener("pointermove", onMove);
      window.addEventListener("pointerup", onUp);
      return;
    }

    const origin = stage.getBoundingClientRect();
    const pos = (e: PointerEvent) => ({ x: (e.clientX - origin.left) / scaleX, y: (e.clientY - origin.top) / scaleY });
    const a = pos(ev);
    const box = el("div", { class: "ed-marquee" });
    let moved = false;
    const rectOf = (e: PointerEvent) => {
      const b = pos(e);
      return { x: Math.min(a.x, b.x), y: Math.min(a.y, b.y), w: Math.abs(a.x - b.x), h: Math.abs(a.y - b.y) };
    };
    const onMove = (mv: PointerEvent) => {
      const r = rectOf(mv);
      if (!moved && (r.w + r.h) * Math.min(scaleX, scaleY) < 4) return;
      if (!moved) stage.append(box);
      moved = true;
      Object.assign(box.style, { left: `${r.x}px`, top: `${r.y}px`, width: `${r.w}px`, height: `${r.h}px` });
    };
    const onUp = (up: PointerEvent) => {
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup", onUp);
      box.remove();
      if (!moved) {
        if (!up.shiftKey && selIds.length > 0) {
          selIds = [];
          refreshSelection();
        }
        return;
      }
      const r = rectOf(up);
      const hit = new Set<string>();
      for (const e of menu().elements) {
        if (e.x < r.x + r.w && e.x + e.w > r.x && e.y < r.y + r.h && e.y + e.h > r.y) for (const g of groupMembers(e)) hit.add(g.id);
      }
      selIds = up.shiftKey ? [...new Set([...selIds, ...hit])] : [...hit];
      refreshSelection();
    };
    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup", onUp);
  });

  // ---- layer list -----------------------------------------------------------------------------------
  function layerName(e: LayoutElement): string {
    if (e.type === "text") {
      const first = e.text.split("\n")[0].trim();
      return first.length > 28 ? `${first.slice(0, 27)}…` : first || tr("menuEditor.leererText");
    }
    if (e.type === "image") return fileName(e.src);
    if (e.type === "rect") return tr("menuEditor.rectangle2");
    return videoLabel(e);
  }

  /** Pulses the stage nodes of `ids` so the user sees where they are. */
  function locate(ids: string[]) {
    for (const id of ids) {
      const node = stage.querySelector<HTMLElement>(`[data-id="${id}"]`);
      if (!node) continue;
      node.classList.add("locate");
      window.setTimeout(() => node.classList.remove("locate"), 1000);
    }
  }

  function pickFromList(ids: string[], ev: MouseEvent) {
    if (ev.shiftKey) selIds = ids.every(isSel) ? selIds.filter((id) => !ids.includes(id)) : [...new Set([...selIds, ...ids])];
    else selIds = ids;
    refreshSelection();
    locate(ids.filter(isSel));
  }

  const hoverStage = (ids: string[], on: boolean) => {
    for (const id of ids) stage.querySelector(`[data-id="${id}"]`)?.classList.toggle("hover-outline", on);
  };

  function renderLayers() {
    const m = menu();
    if (!m) {
      layersBox.replaceChildren();
      return;
    }
    const groupOrder = [...new Set(m.elements.flatMap((e) => (e.group ? [e.group] : [])))];
    const topFirst = [...m.elements].reverse();
    const layerRow = (e: LayoutElement, indent: boolean) =>
      el(
        "button",
        {
          type: "button",
          class: `ed-layer${isSel(e.id) ? " active" : ""}${indent ? " indent" : ""}`,
          onclick: (ev: MouseEvent) => pickFromList([e.id], ev),
          onpointerenter: () => hoverStage([e.id], true),
          onpointerleave: () => hoverStage([e.id], false),
        },
        el("span", { class: "ed-layer-icon", "aria-hidden": "true" }, e.type === "text" ? "T" : e.type === "image" ? "🖼" : e.type === "rect" ? "▭" : "▶"),
        el("span", { class: "ed-layer-name" }, layerName(e)),
        isButton(e) ? el("span", { class: "ed-layer-action" }, actionLabel(e.action)) : null,
      );
    const rows: HTMLElement[] = [];
    const seen = new Set<string>();
    for (const e of topFirst) {
      if (!e.group) {
        rows.push(layerRow(e, false));
        continue;
      }
      if (seen.has(e.group)) continue;
      seen.add(e.group);
      const g = e.group;
      const members = topFirst.filter((x) => x.group === g);
      const ids = members.map((x) => x.id);
      const folded = collapsedGroups.has(g);
      rows.push(
        el(
          "div",
          { class: `ed-layer-group${ids.every(isSel) ? " active" : ""}` },
          el(
            "button",
            {
              type: "button",
              class: "ed-layer-fold",
              title: folded ? tr("menuEditor.aufklappen") : tr("menuEditor.zuklappen"),
              onclick: () => {
                if (folded) collapsedGroups.delete(g);
                else collapsedGroups.add(g);
                renderLayers();
              },
            },
            folded ? "▸" : "▾",
          ),
          el(
            "button",
            {
              type: "button",
              class: "ed-layer-name",
              onclick: (ev: MouseEvent) => pickFromList(ids, ev),
              onpointerenter: () => hoverStage(ids, true),
              onpointerleave: () => hoverStage(ids, false),
            },
            tr("menuEditor.gruppe", { groupOrder: groupOrder.indexOf(g) + 1, length: members.length }),
          ),
        ),
      );
      if (!folded) for (const x of members) rows.push(layerRow(x, true));
    }
    layersBox.replaceChildren(
      el("h3", {}, tr("menuEditor.elemente")),
      rows.length ? el("div", { class: "ed-layer-list" }, ...rows) : el("p", { class: "ed-hint" }, tr("menuEditor.nochKeineElemente")),
    );
  }

  // ---- properties -------------------------------------------------------------------------------
  const row = (label: string, ...control: Child[]) => el("label", { class: "ed-row" }, el("span", {}, label), ...control);
  const group = (title: string, ...items: Child[]) => el("fieldset", { class: "ed-group" }, el("legend", {}, title), ...items);

  /** Input bound to the model: `input` updates the stage live, `change` records history. */
  function bind(input: HTMLInputElement | HTMLTextAreaElement | HTMLSelectElement, set: (v: string) => void, opts2: { live?: boolean; rerender?: boolean } = {}) {
    let recorded = false;
    const record = () => {
      if (!recorded) {
        remember();
        recorded = true;
      }
    };
    if (opts2.live) {
      input.addEventListener("input", () => {
        record();
        set(input.value);
        renderStage();
        renderToolbar();
        requestTimelineDraw();
      });
    }
    input.addEventListener("change", () => {
      record();
      recorded = false;
      set(input.value);
      renderStage();
      renderMenus();
      renderToolbar();
      renderHeader();
      renderTimelinePane();
      if (opts2.rerender) renderProps();
      scheduleCheck();
    });
    return input;
  }

  const num = (value: number, set: (v: number) => void, attrs: Record<string, unknown> = {}) =>
    bind(el("input", { type: "number", value: String(value), ...attrs }) as HTMLInputElement, (v) => {
      const n = Number(v);
      if (Number.isFinite(n)) set(n);
    }, { live: true });

  const select = (options: [string, string][], value: string, set: (v: string) => void, attrs: Record<string, unknown> = {}) =>
    bind(
      el("select", attrs, ...options.map(([v, label]) => el("option", { value: v, selected: v === value }, label))) as HTMLSelectElement,
      set,
      { rerender: true },
    );

  const titleOptions = (): [string, string][] => titles.map((_, i) => [String(i + 1), titleName(i + 1)]);

  function renderProps() {
    const m = menu();
    if (!m) {
      props.replaceChildren();
      return;
    }
    const sel = selection();
    const e = selected();
    const key = e && selKey?.id === e.id && sel.length === 1 ? e.intro?.[selKey.i] : undefined;
    props.replaceChildren(...(key ? keyProps(m, e!, key) : sel.length > 1 ? multiProps(sel) : e ? elementProps(m, e) : menuProps(m)));
  }

  /** Values of one intro key; `t` stays between its neighbours and inside the intro. */
  function keyProps(m: CustomMenu, e: LayoutElement, k: ElementKey): HTMLElement[] {
    const keys = e.intro!;
    const i = keys.indexOf(k);
    const lo = keys[i - 1]?.t ?? 0;
    const hi = keys[i + 1]?.t ?? (m.introSecs ?? 0);
    const field = (key: "x" | "y" | "w" | "h", min = 0) =>
      row(key.toUpperCase(), num(Math.round(k[key]), (v) => (k[key] = Math.max(min, v)), { step: "1" }));
    return [
      el("h3", {}, tr("menuEditor.keyframe", { n: i + 1, total: keys.length })),
      group(
        tr("menuEditor.keyframeWerte"),
        row(tr("menuEditor.keyframeZeit"), num(k.t, (v) => (k.t = Math.round(Math.min(hi, Math.max(lo, v)) * 100) / 100), { step: "0.1", min: String(lo), max: String(hi) })),
        field("x", -10000),
        field("y", -10000),
        field("w", 1),
        field("h", 1),
        row(tr("menuEditor.deckkraft"), num(k.opacity, (v) => (k.opacity = Math.min(1, Math.max(0, v))), { step: "0.05", min: "0", max: "1" })),
      ),
      el(
        "div",
        { class: "ed-row-buttons" },
        el("button", { class: "btn", type: "button", onclick: () => ((selKey = null), renderProps(), requestTimelineDraw()) }, tr("menuEditor.zumElement")),
        el(
          "button",
          {
            class: "btn danger",
            type: "button",
            onclick: () => {
              remember();
              keys.splice(i, 1);
              if (!keys.length) delete e.intro;
              selKey = null;
              renderAll();
            },
          },
          tr("menuEditor.keyframeLoeschen"),
        ),
      ),
    ];
  }

  function multiProps(sel: LayoutElement[]): HTMLElement[] {
    const groups = new Set(sel.map((e) => e.group ?? null));
    const oneGroup = groups.size === 1 && !groups.has(null);
    return [
      el("h3", {}, tr("menuEditor.elementeAusgewaehlt", { length: sel.length })),
      el(
        "div",
        { class: "ed-row-buttons" },
        el("button", { class: "btn", type: "button", disabled: oneGroup, title: tr("menuEditor.strgG"), onclick: groupSelection }, tr("menuEditor.gruppieren")),
        el("button", { class: "btn", type: "button", disabled: !sel.some((e) => e.group), title: tr("menuEditor.strgUmschaltG"), onclick: ungroupSelection }, tr("menuEditor.gruppeAufloesen")),
        el("button", { class: "btn", type: "button", onclick: () => ((selIds = []), refreshSelection()) }, tr("menuEditor.auswahlAufheben")),
      ),
      el(
        "div",
        { class: "ed-row-buttons" },
        el("button", { class: "btn", type: "button", onclick: () => reorder(sel, 1) }, tr("menuEditor.nachVorne")),
        el("button", { class: "btn", type: "button", onclick: () => reorder(sel, -1) }, tr("menuEditor.nachHinten")),
        el("button", { class: "btn", type: "button", onclick: () => addCopies(sel) }, tr("menuEditor.duplizieren")),
        el("button", { class: "btn danger", type: "button", onclick: () => removeElements(sel) }, tr("menuEditor.loeschen")),
      ),
      el(
        "p",
        { class: "ed-hint" },
        tr("menuEditor.ziehenVerschiebtAlleEck"),
      ),
    ];
  }

  function menuProps(m: CustomMenu): HTMLElement[] {
    const bg = m.background;
    const bgKind = bg.video ? (bg.video.path ? "videoFile" : "video") : bg.image ? "image" : "color";
    const sound = m.audio;
    return [
      el("h3", {}, tr("menuEditor.menue")),
      group(
        tr("menuEditor.allgemein"),
        row(tr("menuEditor.name"), bind(el("input", { type: "text", value: m.name }) as HTMLInputElement, (v) => (m.name = v), { live: true })),
        row(
          tr("menuEditor.art"),
          select(
            Object.entries(ROLE_LABELS) as [string, string][],
            m.role,
            (v) => {
              m.role = v === "root" || v === "settings" ? v : "title";
              m.title = m.role === "title" ? null : (m.title ?? 1);
            },
          ),
        ),
        m.role !== "title"
          ? row(tr("menuEditor.titel4"), select(titleOptions(), String(m.title ?? 1), (v) => (m.title = Number(v)), { disabled: titles.length === 0 }))
          : null,
        row(
          tr("menuEditor.dauerS"),
          num(m.durationSecs ?? 20, (v) => (m.durationSecs = Math.max(1, Math.min(600, v))), { min: "1", max: "600", step: "1" }),
        ),
        row(
          "Intro-Dauer (T)",
          num(
            m.introSecs ?? 0,
            (v) => {
              m.introSecs = snapToClosedGop(Math.max(0, Math.min(60, v)));
              renderAll();
            },
            { min: "0", max: "60", step: tv.toLowerCase() === "pal" ? "0.6" : "0.6006" },
          ),
        ),
        el("p", { class: "ed-hint" }, isMoving(m) ? tr("menuEditor.bewegtesMenueWirdAls") : tr("menuEditor.standbildKeineVideosKein")),
      ),
      group(
        tr("menuEditor.hintergrund"),
        row(
          tr("menuEditor.art"),
          select(
            [
              ["color", tr("menuEditor.farbe")],
              ["image", tr("menuEditor.bild2")],
              ["video", tr("menuEditor.videoEinesTitels")],
              ["videoFile", tr("menuEditor.videodateiWaehlen")],
            ],
            bgKind,
            (v) => {
              if (v === "color") {
                bg.image = null;
                bg.video = null;
              } else if (v === "video") {
                bg.image = null;
                bg.video = { title: 1, startSecs: null };
              } else if (v === "videoFile") {
                // `bind` already recorded the undo state before this change.
                void chooseFile(VIDEO_FILTER).then((p) => {
                  if (p) {
                    bg.image = null;
                    bg.video = { title: 0, startSecs: null, path: p };
                  }
                  renderAll();
                });
              } else {
                bg.video = null;
                void chooseFile(IMAGE_FILTER).then((p) => {
                  if (p) {
                    remember();
                    bg.image = p;
                  }
                  renderAll();
                });
              }
            },
          ),
        ),
        row(tr("menuEditor.farbe"), bind(el("input", { type: "color", value: bg.color }) as HTMLInputElement, (v) => (bg.color = v.toUpperCase()), { live: true })),
        row(
          tr("menuEditor.abdunkeln"),
          select(OVERLAY_LABELS, bg.gradientOverlay ?? "none", (v) => (bg.gradientOverlay = v as GradientOverlay)),
        ),
        bg.image ? row(tr("menuEditor.bild2"), el("span", { class: "ed-path", title: bg.image }, bg.image.split(/[\\/]/).pop() ?? "")) : null,
        bg.video?.path ? row(tr("menuEditor.datei"), el("span", { class: "ed-path", title: bg.video.path }, fileName(bg.video.path))) : null,
        bg.video && !bg.video.path
          ? row(tr("menuEditor.titel4"), select(titleOptions(), String(bg.video.title), (v) => (bg.video = { ...bg.video!, title: Number(v) })))
          : null,
        bg.video
          ? row(
              tr("menuEditor.startS"),
              num(
                bg.video.startSecs ?? defaultStart(bg.video),
                (v) => (bg.video = { ...bg.video!, startSecs: Math.max(0, v) }),
                { min: "0", step: "1" },
              ),
            )
          : null,
        bgKind !== "color"
          ? row(
              tr("menuEditor.weichzeichner"),
              bind(el("input", { type: "range", min: "0", max: "50", step: "1", value: String(bg.blur) }) as HTMLInputElement, (v) => (bg.blur = Number(v)), {
                live: true,
              }),
            )
          : null,
      ),
      group(
        tr("menuEditor.ton"),
        row(
          tr("menuEditor.quelle"),
          select(
            [
              ["none", tr("menuEditor.stumm")],
              ...titles.map((_, i): [string, string] => [`title:${i + 1}`, tr("menuEditor.tonVonTitel", { titleName: titleName(i + 1) })]),
              ["file", tr("menuEditor.audiodatei")],
            ],
            sound.type === "title" ? `title:${sound.title}` : sound.type,
            (v) => {
              if (v === "none") m.audio = { type: "none" };
              else if (v.startsWith("title:")) m.audio = { type: "title", title: Number(v.slice(6)), startSecs: null };
              else
                void chooseFile(AUDIO_FILTER).then((p) => {
                  if (p) {
                    remember();
                    m.audio = { type: "file", path: p };
                  }
                  renderAll();
                });
            },
          ),
        ),
        sound.type === "file" ? row(tr("menuEditor.datei"), el("span", { class: "ed-path", title: sound.path }, sound.path.split(/[\\/]/).pop() ?? "")) : null,
      ),
      group(
        tr("menuEditor.buttonMarkierungAlleMenues"),
        row(tr("menuEditor.ausgewaehlt"), bind(el("input", { type: "color", value: L.highlight.color }) as HTMLInputElement, (v) => (L.highlight.color = v.toUpperCase()), { live: true })),
        row(tr("menuEditor.aktiviert"), bind(el("input", { type: "color", value: L.highlight.selectColor }) as HTMLInputElement, (v) => (L.highlight.selectColor = v.toUpperCase()), { live: true })),
        row(
          tr("menuEditor.deckkraft"),
          bind(el("input", { type: "range", min: "0.2", max: "1", step: "0.05", value: String(L.highlight.opacity) }) as HTMLInputElement, (v) => (L.highlight.opacity = Number(v)), {
            live: true,
          }),
        ),
        el("p", { class: "ed-hint" }, tr("menuEditor.textButtonsLeuchtenIn")),
      ),
    ];
  }

  /** A project video shows what its button plays: the title, the chapter's start, or the title of the menu it opens. */
  function followAction(e: LayoutElement) {
    if (e.type !== "video" || e.path) return;
    const a = e.action;
    const shown = a?.type === "playTitle" || a?.type === "playChapter" ? a.title : a?.type === "menu" ? L.menus.find((m) => m.id === a.menu)?.title : null;
    if (!shown) return;
    if (a?.type === "playChapter") e.startSecs = titles[shown - 1]?.chapterStarts?.[a.chapter - 1] ?? null;
    else if (e.title !== shown) e.startSecs = null;
    e.title = shown;
  }

  /** The other direction: picking the video's title retargets a title/chapter button to it. */
  function retargetAction(e: LayoutElement, title: number) {
    const a = e.action;
    if (a?.type === "playTitle") e.action = { type: "playTitle", title };
    else if (a?.type === "playChapter") e.action = { type: "playChapter", title, chapter: 1 };
  }

  function actionControls(e: LayoutElement): HTMLElement[] {
    const a = e.action ?? null;
    const type = a?.type ?? "none";
    // The title whose streams this menu may select (root/settings menus only).
    const m0 = menu();
    const streamTitle = m0.role !== "title" && m0.title ? titles[m0.title - 1] : undefined;
    const setAction = (next: Action | null) => {
      e.action = next;
      if (next && e.rotation !== 0) e.rotation = 0; // DVD hitboxes cannot rotate
      followAction(e);
    };
    const out: HTMLElement[] = [
      row(
        tr("menuEditor.aktion"),
        select(
          [
            ["none", tr("menuEditor.keineKeinButton")],
            ["playTitle", tr("menuEditor.titelAbspielen")],
            ["playChapter", tr("menuEditor.kapitelAbspielen")],
            ["menu", tr("menuEditor.menueOeffnen")],
            ["playAll", tr("menuEditor.alleTitelAbspielen")],
            ["customVm", "VM-Skript (DVD VM)"],
            // Streams belong to a titleset: only in a title's root/settings menus.
            ...(streamTitle ? ([["setAudio", tr("menuEditor.tonspurWaehlen")], ["setSubtitle", tr("menuEditor.untertitelWaehlen")]] as [string, string][]) : []),
          ],
          type,
          (v) => {
            const firstOther = L.menus.find((m) => m.id !== menu().id)?.id ?? menu().id;
            // A project video keeps showing its title when the button starts to play it.
            const t0 = e.type === "video" && !e.path && e.title > 0 ? e.title : 1;
            setAction(
              v === "playTitle"
                ? { type: "playTitle", title: t0 }
                : v === "playChapter"
                  ? { type: "playChapter", title: t0, chapter: 1 }
                  : v === "menu"
                    ? { type: "menu", menu: firstOther }
                    : v === "playAll"
                      ? { type: "playAll" }
                      : v === "customVm"
                        ? { type: "customVm", script: "g4 = 1;" }
                        : v === "setAudio"
                          ? { type: "setAudio", source: streamTitle?.audioTracks?.[0]?.source ?? 0 }
                          : v === "setSubtitle"
                            ? { type: "setSubtitle", source: null }
                            : null,
            );
          },
        ),
      ),
    ];
    if (!streamTitle) {
      out.push(el("p", { class: "ed-hint" }, tr("menuEditor.tonUndUntertitelwahlGibt")));
    }
    if (a?.type === "customVm") {
      out.push(
        row(
          "VM-Befehle",
          bind(el("input", { type: "text", value: a.script }) as HTMLInputElement, (v) => setAction({ type: "customVm", script: v }), { live: true }),
        ),
        el("p", { class: "ed-hint" }, "Direkte DVD-VM-Anweisungen (z.B. „if (g4 == 1) link button 2; else jump menu 3;“)."),
      );
    }
    if (a?.type === "setAudio" && streamTitle) {
      const tracks = streamTitle.audioTracks ?? [];
      out.push(
        row(
          tr("menuEditor.tonspur2"),
          select(
            tracks.map((x): [string, string] => [String(x.source), x.label]),
            String(a.source),
            (v) => setAction({ type: "setAudio", source: Number(v) }),
            { disabled: tracks.length === 0 },
          ),
        ),
      );
    }
    if (a?.type === "setSubtitle" && streamTitle) {
      const subs = streamTitle.subtitleTracks ?? [];
      out.push(
        row(
          tr("menuEditor.untertitel2"),
          select(
            [["off", tr("menuEditor.aus")], ...subs.map((x): [string, string] => [String(x.source), x.label])],
            a.source === null ? "off" : String(a.source),
            (v) => setAction({ type: "setSubtitle", source: v === "off" ? null : Number(v) }),
          ),
        ),
      );
    }
    if (a?.type === "playTitle" || a?.type === "playChapter") {
      out.push(
        row(
          tr("menuEditor.titel4"),
          select(titleOptions(), String(a.title), (v) => {
            if (a.type === "playChapter") setAction({ type: "playChapter", title: Number(v), chapter: 1 });
            else setAction({ type: "playTitle", title: Number(v) });
          }),
        ),
      );
    }
    if (a?.type === "playChapter") {
      const n = titles[a.title - 1]?.chapters ?? 1;
      out.push(
        row(
          tr("menuEditor.kapitel"),
          select(
            Array.from({ length: Math.max(1, n) }, (_, i): [string, string] => {
              const name = chapterLabel(titles[a.title - 1], i + 1);
              return [String(i + 1), name === tr("menuEditor.kapitel2", { i: i + 1 }) ? name : `${i + 1}: ${name}`];
            }),
            String(a.chapter),
            (v) => setAction({ type: "playChapter", title: a.title, chapter: Number(v) }),
          ),
        ),
      );
    }
    if (a?.type === "menu") {
      out.push(
        row(
          tr("menuEditor.ziel"),
          select(
            L.menus.map((m): [string, string] => [
              m.id,
              `${m.name}${m.role === "title" ? "" : tr("menuEditor.titel5", { m: m.role === "root" ? "Root" : tr("menuEditor.einstellungen"), title: m.title })}`,
            ]),
            a.menu,
            (v) => setAction({ type: "menu", menu: v, transition: a.transition }),
          ),
        ),
        row(
          tr("menuEditor.transition"),
          select(
            [
              ["none", tr("menuEditor.transitionNone")],
              ["file", tr("menuEditor.transitionFile")],
            ],
            a.transition ? "file" : "none",
            (v) => {
              if (v === "none") {
                setAction({ type: "menu", menu: a.menu, transition: null });
                renderProps();
              } else {
                void chooseFile(VIDEO_FILTER).then((p) => {
                  if (p) {
                    remember();
                    setAction({
                      type: "menu",
                      menu: a.menu,
                      transition: {
                        video: { source: "file", path: p, inPointSecs: 0 },
                        durationSecs: 2.0,
                        audio: { type: "none" },
                      },
                    });
                    renderAll();
                  }
                });
              }
            },
          ),
        ),
      );
      if (a.transition) {
        out.push(
          row(
            "Dauer (s)",
            num(
              a.transition.durationSecs,
              (v) => {
                if (a.transition) a.transition.durationSecs = Math.max(0.5, Math.min(30, v));
              },
              { min: "0.5", max: "30", step: "0.5" },
            ),
          ),
        );
      }
    }
    return out;
  }

  /** Own marked/activated colours of a button (at most 3 different pairs per menu). */
  function buttonColorControls(e: LayoutElement): HTMLElement[] {
    if (!isButton(e)) return [];
    const own = e.highlight ?? null;
    const out: HTMLElement[] = [
      el(
        "label",
        { class: "ed-check" },
        el("input", {
          type: "checkbox",
          checked: !!own,
          onchange: (ev: Event) => {
            remember();
            e.highlight = (ev.target as HTMLInputElement).checked ? { color: L.highlight.color, selectColor: L.highlight.selectColor } : null;
            renderAll();
          },
        }),
        tr("menuEditor.eigeneFarben"),
      ),
    ];
    if (own) {
      out.push(
        row(tr("menuEditor.ausgewaehlt"), bind(el("input", { type: "color", value: own.color }) as HTMLInputElement, (v) => (own.color = v.toUpperCase()), { live: true })),
        row(tr("menuEditor.aktiviert"), bind(el("input", { type: "color", value: own.selectColor }) as HTMLInputElement, (v) => (own.selectColor = v.toUpperCase()), { live: true })),
        el("p", { class: "ed-hint" }, tr("menuEditor.eineDvdMenueseiteZeigt")),
      );
    }
    return out;
  }

  function elementProps(m: CustomMenu, e: LayoutElement): HTMLElement[] {
    const kind = e.type === "text" ? tr("menuEditor.text2") : e.type === "image" ? tr("menuEditor.bild2") : e.type === "rect" ? tr("menuEditor.rectangle2") : tr("menuEditor.video2");
    const typeGroup: Child[] = [];
    if (e.type === "text") {
      const t = e;
      typeGroup.push(
        group(
          tr("menuEditor.text2"),
          bind(el("textarea", { rows: 3, class: "ed-textarea" }, t.text) as HTMLTextAreaElement, (v) => (t.text = v), { live: true }),
          row(tr("menuEditor.schrift"), select(FONTS.map((f): [string, string] => [f, f]), t.font, (v) => (t.font = v))),
          row(tr("menuEditor.groessePx"), num(t.size, (v) => (t.size = Math.max(4, Math.min(300, v))), { min: "4", max: "300", step: "1" })),
          row(tr("menuEditor.farbe"), bind(el("input", { type: "color", value: t.color }) as HTMLInputElement, (v) => (t.color = v.toUpperCase()), { live: true })),
          el(
            "div",
            { class: "ed-seg", role: "group", "aria-label": tr("menuEditor.ausrichtung") },
            ...(["left", "center", "right"] as const).map((al) =>
              el(
                "button",
                {
                  type: "button",
                  class: `btn${t.align === al ? " active" : ""}`,
                  onclick: () => {
                    remember();
                    t.align = al;
                    renderStage();
                    renderProps();
                  },
                },
                al === "left" ? tr("menuEditor.links") : al === "center" ? tr("menuEditor.mitte") : tr("menuEditor.rechts"),
              ),
            ),
          ),
          el(
            "div",
            { class: "ed-checks" },
            ...([
              ["bold", tr("menuEditor.fett")],
              ["italic", tr("menuEditor.kursiv")],
              ["shadow", tr("menuEditor.schatten")],
              ["wrap", tr("menuEditor.zeilenumbruch")],
            ] as const).map(([k, label]) =>
              el(
                "label",
                { class: "ed-check" },
                el("input", {
                  type: "checkbox",
                  checked: !!t[k],
                  onchange: (ev: Event) => {
                    remember();
                    t[k] = (ev.target as HTMLInputElement).checked;
                    renderStage();
                  },
                }),
                label,
              ),
            ),
          ),
        ),
      );
    } else if (e.type === "image") {
      const img = e;
      typeGroup.push(
        group(
          tr("menuEditor.bild2"),
          row(tr("menuEditor.datei"), el("span", { class: "ed-path", title: img.src }, img.src.split(/[\\/]/).pop() ?? "")),
          el(
            "button",
            {
              class: "btn",
              type: "button",
              onclick: async () => {
                const p = await chooseFile(IMAGE_FILTER);
                if (p) {
                  remember();
                  img.src = p;
                  renderAll();
                }
              },
            },
            tr("menuEditor.anderesBild"),
          ),
          row(tr("menuEditor.anpassung"), select(FIT_LABELS, img.fit, (v) => (img.fit = v as Fit))),
        ),
      );
    } else if (e.type === "rect") {
      const r = e;
      typeGroup.push(
        group(
          tr("menuEditor.rectangle2"),
          row(tr("menuEditor.rectFill"), bind(el("input", { type: "color", value: r.color }) as HTMLInputElement, (v) => (r.color = v.toUpperCase()), { live: true })),
          row(tr("menuEditor.rectBorderColor"), bind(el("input", { type: "color", value: r.borderColor ?? "#FFFFFF" }) as HTMLInputElement, (v) => (r.borderColor = v.toUpperCase()), { live: true })),
          row(tr("menuEditor.rectBorderWidth"), num(r.borderWidth ?? 0, (v) => (r.borderWidth = Math.max(0, Math.min(100, v))), { min: "0", max: "100", step: "1" })),
        ),
      );
    } else {
      const v = e;
      typeGroup.push(
        group(
          tr("menuEditor.video2"),
          row(
            tr("menuEditor.quelle"),
            select(
              [
                ["title", tr("menuEditor.projektTitel")],
                ["file", tr("menuEditor.videodateiWaehlen")],
              ],
              v.path ? "file" : "title",
              (x) => {
                if (x === "title") {
                  delete v.path;
                  v.title = Math.max(1, v.title);
                  v.startSecs = null;
                  return;
                }
                // `bind` already recorded the undo state before this change.
                void chooseFile(VIDEO_FILTER).then((p) => {
                  if (p) {
                    v.path = p;
                    v.title = 0;
                    v.startSecs = null;
                  }
                  renderAll();
                });
              },
              { disabled: titles.length === 0 && !v.path },
            ),
          ),
          v.path
            ? row(tr("menuEditor.datei"), el("span", { class: "ed-path", title: v.path }, fileName(v.path)))
            : row(tr("menuEditor.titel4"), select(titleOptions(), String(v.title), (x) => {
                v.title = Number(x);
                retargetAction(v, v.title);
                followAction(v);
              })),
          row(tr("menuEditor.startS"), num(v.startSecs ?? defaultStart(v), (x) => (v.startSecs = Math.max(0, x)), { min: "0", step: "1" })),
          row(tr("menuEditor.anpassung"), select(FIT_LABELS, v.fit, (x) => (v.fit = x as Fit))),
          el("p", { class: "ed-hint" }, tr("menuEditor.dasVideoLaeuftIm")),
        ),
      );
    }
    const index = m.elements.indexOf(e);
    return [
      el("h3", {}, kind),
      ...(typeGroup as HTMLElement[]),
      group(tr("menuEditor.button2"), ...actionControls(e), isButton(e) ? el("p", { class: "ed-hint" }, actionLabel(e.action)) : null, ...buttonColorControls(e)),
      ...(isButton(e)
        ? [
            group(
              "Easter Egg (Versteckter Button)",
              el(
                "label",
                { class: "ed-check" },
                el("input", {
                  type: "checkbox",
                  checked: !!e.easterEgg,
                  onchange: (ev: Event) => {
                    remember();
                    e.easterEgg = (ev.target as HTMLInputElement).checked;
                    renderStage();
                    renderProps();
                  },
                }),
                "Als Easter Egg markieren",
              ),
              el("p", { class: "ed-hint" }, "Navigation per Fernbedienung aktiv, aber die Button-Markierung ist zu 100% unsichtbar."),
            ),
          ]
        : []),
      group(
        tr("menuEditor.positionGroesse"),
        el(
          "div",
          { class: "ed-grid2" },
          row("X", num(e.x, (v) => ((e.x = v), clampBox(e)), { step: "1" })),
          row("Y", num(e.y, (v) => ((e.y = v), clampBox(e)), { step: "1" })),
          row(tr("menuEditor.breite"), num(e.w, (v) => ((e.w = v), clampBox(e)), { step: "1", min: "8" })),
          row(tr("menuEditor.hoehe"), num(e.h, (v) => ((e.h = v), clampBox(e)), { step: "1", min: "8" })),
        ),
        row(
          tr("menuEditor.drehung"),
          select(
            [
              ["0", "0°"],
              ["90", "90°"],
              ["180", "180°"],
              ["270", "270°"],
            ],
            String(e.rotation),
            (v) => (e.rotation = Number(v)),
            { disabled: isButton(e), title: isButton(e) ? tr("menuEditor.buttonsKoennenNichtGedreht") : undefined },
          ),
        ),
        row(
          tr("menuEditor.deckkraft"),
          bind(el("input", { type: "range", min: "0.1", max: "1", step: "0.05", value: String(e.opacity) }) as HTMLInputElement, (v) => (e.opacity = Number(v)), { live: true }),
        ),
      ),
      el(
        "div",
        { class: "ed-row-buttons" },
        el("button", { class: "btn", type: "button", disabled: index === m.elements.length - 1, onclick: () => reorder([e], 1) }, tr("menuEditor.nachVorne")),
        el("button", { class: "btn", type: "button", disabled: index === 0, onclick: () => reorder([e], -1) }, tr("menuEditor.nachHinten")),
        el("button", { class: "btn", type: "button", onclick: () => addCopies([e]) }, tr("menuEditor.duplizieren")),
        el("button", { class: "btn danger", type: "button", onclick: () => removeElements([e]) }, tr("menuEditor.loeschen")),
      ),
    ];
  }

  function renderStatus() {
    const tvCanvas = tv === "NTSC" ? 480 : 576;
    const notes: Child[] = [];
    if (notice) notes.push(el("li", { class: notice.kind === "ok" ? "ok" : "error", role: "alert" }, notice.text));
    if (stale && history.length > 0) {
      notes.push(
        el(
          "li",
          { class: "warn" },
          tr("menuEditor.neuerEntwurfFuerDie", { length: titles.length }),
        ),
      );
    }
    if (!isBdFrame(tv) && H() !== tvCanvas) {
      notes.push(el("li", { class: "warn" }, tr("menuEditor.layoutFuer720Entworfen", { H: H(), tv, tvCanvas })));
    }
    if (errors.length === 0) {
      notes.push(el("li", { class: "ok" }, tr("menuEditor.layoutIstGueltigMenues", { length: L.menus.length })));
    } else {
      for (const e of errors.slice(0, 8)) notes.push(el("li", { class: "error" }, e));
      if (errors.length > 8) notes.push(el("li", { class: "error" }, tr("menuEditor.andMore", { n: errors.length - 8 })));
    }
    status.replaceChildren(el("ul", {}, ...notes), el("span", { class: "ed-keys" }, tr("menuEditor.keysHint")));
  }

  let checkTimer: number | undefined;
  let checkSeq = 0;
  function scheduleCheck() {
    window.clearTimeout(checkTimer);
    checkTimer = window.setTimeout(async () => {
      const seq = ++checkSeq;
      try {
        const result = await checkLayout(L, titles, tv);
        if (seq === checkSeq) {
          errors = result;
          renderStatus();
        }
      } catch (e) {
        errors = [String(e)];
        renderStatus();
      }
    }, 350);
  }

  // ---- edits ------------------------------------------------------------------------------------
  function addElement(e: LayoutElement) {
    remember();
    clampBox(e);
    menu().elements.push(e);
    selIds = [e.id];
    renderAll();
  }
  function addText() {
    addElement(newText(Math.round(W() / 2 - 120), Math.round(H() / 2 - 25), 240, 50, tr("menuEditor.neuerText"), { size: 28, wrap: true }));
  }
  function addRect() {
    addElement(newRect(Math.round(W() / 2 - 100), Math.round(H() / 2 - 60), 200, 120, "#FFFFFF"));
  }
  async function addImage() {
    const p = await chooseFile(IMAGE_FILTER);
    if (!p) return;
    addElement({ id: newId("i"), type: "image", src: p, fit: "contain", x: Math.round(W() / 2 - 100), y: Math.round(H() / 2 - 75), w: 200, h: 150, rotation: 0, opacity: 1, action: null });
  }
  function addVideo(path?: string) {
    addElement({
      id: newId("v"),
      type: "video",
      title: path ? 0 : 1,
      startSecs: null,
      fit: "cover",
      ...(path ? { path } : {}),
      x: Math.round(W() / 2 - 120),
      y: Math.round(H() / 2 - 68),
      w: 240,
      h: 136,
      rotation: 0,
      opacity: 1,
      // An external clip has no title to play; it becomes a button only when given an action.
      action: path ? null : { type: "playTitle", title: 1 },
    });
  }
  async function addExternalVideo() {
    const p = await chooseFile(VIDEO_FILTER);
    if (p) addVideo(p);
  }
  function removeElements(list: LayoutElement[]) {
    if (list.length === 0) return;
    remember();
    menu().elements = menu().elements.filter((x) => !list.includes(x));
    selIds = [];
    pruneGroups();
    renderAll();
  }
  /** Copies of `src` (new ids, groups re-mapped to fresh ids), offset by 16 px, on top and selected. */
  function addCopies(src: LayoutElement[]) {
    if (src.length === 0) return;
    remember();
    const gmap = new Map<string, string>();
    const copies = src.map((e) => {
      const c = structuredClone(e);
      c.id = newId("d");
      if (c.group) {
        if (!gmap.has(c.group)) gmap.set(c.group, newId("g"));
        c.group = gmap.get(c.group);
      }
      return c;
    });
    shiftBy(copies, 16, 16);
    menu().elements.push(...copies);
    selIds = copies.map((c) => c.id);
    pruneGroups();
    renderAll();
  }
  /** Moves the selected elements one step past the next unselected neighbour (towards `dir`). */
  function reorder(list: LayoutElement[], dir: number) {
    const before = JSON.stringify(L);
    const all = menu().elements;
    const picked = new Set(list);
    let changed = false;
    if (dir > 0) {
      for (let i = all.length - 2; i >= 0; i--) {
        if (picked.has(all[i]) && !picked.has(all[i + 1])) {
          [all[i], all[i + 1]] = [all[i + 1], all[i]];
          changed = true;
        }
      }
    } else {
      for (let i = 1; i < all.length; i++) {
        if (picked.has(all[i]) && !picked.has(all[i - 1])) {
          [all[i], all[i - 1]] = [all[i - 1], all[i]];
          changed = true;
        }
      }
    }
    if (!changed) return;
    pushHistory(before);
    renderAll();
  }
  /** Dissolves groups left with a single member. */
  function pruneGroups() {
    const count = new Map<string, number>();
    for (const e of menu().elements) if (e.group) count.set(e.group, (count.get(e.group) ?? 0) + 1);
    for (const e of menu().elements) if (e.group && (count.get(e.group) ?? 0) < 2) delete e.group;
  }
  function groupSelection() {
    const sel = selection();
    if (sel.length < 2) return;
    remember();
    const g = newId("g");
    for (const e of sel) e.group = g;
    // Members become one contiguous block at the topmost member's position.
    const all = menu().elements;
    const at = all.slice(0, Math.max(...sel.map((e) => all.indexOf(e)))).filter((x) => !sel.includes(x)).length;
    const rest = all.filter((x) => !sel.includes(x));
    rest.splice(at, 0, ...sel);
    menu().elements = rest;
    pruneGroups();
    renderAll();
  }
  function ungroupSelection() {
    const groups = new Set(selection().flatMap((e) => (e.group ? [e.group] : [])));
    if (groups.size === 0) return;
    remember();
    for (const e of menu().elements) if (e.group && groups.has(e.group)) delete e.group;
    renderAll();
  }
  function addMenu(role: CustomMenu["role"]) {
    remember();
    const n = L.menus.filter((m) => m.role === role).length + 1;
    const name = role === "title" ? tr("menuEditor.titelmenue3", { n }) : role === "root" ? tr("menuEditor.rootMenue3", { n }) : tr("menuEditor.einstellungen3", { n });
    const m = emptyMenu(name, role, role === "title" ? null : 1);
    L.menus.push(m);
    menuIdx = L.menus.length - 1;
    selIds = [];
    renderAll();
  }
  function moveMenu(dir: number) {
    const j = menuIdx + dir;
    if (j < 0 || j >= L.menus.length) return;
    remember();
    [L.menus[menuIdx], L.menus[j]] = [L.menus[j], L.menus[menuIdx]];
    menuIdx = j;
    renderAll();
  }
  function duplicateMenu() {
    remember();
    const copy: CustomMenu = structuredClone(menu());
    copy.id = newId("m");
    copy.name = tr("menuEditor.copySuffix", { name: copy.name });
    copy.elements = copy.elements.map((e) => ({ ...e, id: newId("d") }));
    L.menus.splice(menuIdx + 1, 0, copy);
    menuIdx += 1;
    renderAll();
  }
  function deleteMenu() {
    if (L.menus.length <= 1) return;
    remember();
    const gone = menu().id;
    L.menus.splice(menuIdx, 1);
    // Buttons that opened this menu lose their action.
    for (const m of L.menus) for (const e of m.elements) if (e.action?.type === "menu" && e.action.menu === gone) e.action = null;
    selIds = [];
    renderAll();
  }

  async function chooseFile(filters: { name: string; extensions: string[] }[]): Promise<string | null> {
    try {
      const p = await open({ multiple: false, directory: false, filters });
      return typeof p === "string" ? p : null;
    } catch (e) {
      flash(tr("menuEditor.dateiauswahlFehlgeschlagen", { e }), "error");
      return null;
    }
  }

  /** A message in the status bar that is not replaced by the next validation (8 s). */
  let notice: { text: string; kind: "ok" | "error" } | null = null;
  let noticeTimer: number | undefined;
  function flash(text: string, kind: "ok" | "error") {
    notice = { text, kind };
    window.clearTimeout(noticeTimer);
    noticeTimer = window.setTimeout(() => {
      notice = null;
      if (!closed) renderStatus();
    }, 8000);
    renderStatus();
  }

  async function newDraft() {
    remember();
    L = draftLayout(titles, tv, opts.heading);
    menuIdx = 0;
    selIds = [];
    renderAll();
  }
  /** Rebuilds the draft in a zero-asset preset style; one undo step restores the previous layout. */
  async function applyPresetFromSelect(sel: HTMLSelectElement) {
    const preset = sel.value as MenuPreset;
    sel.value = "";
    if (!MENU_PRESETS.includes(preset)) return;
    if (L.menus.some((m) => m.elements.length > 0)) {
      let go = false;
      try {
        go = await ask(tr("menuEditor.presetOverwrites"), { title: tr(`menuEditor.preset_${preset}`), kind: "warning" });
      } catch (e) {
        flash(tr("menuEditor.dateiauswahlFehlgeschlagen", { e }), "error");
      }
      if (!go) return;
    }
    remember();
    L = draftLayout(titles, tv, opts.heading, preset);
    menuIdx = 0;
    selIds = [];
    renderAll();
  }
  /** Replaces the layout with a converted ConvertX template (undoable like loading a file). */
  async function importTemplate() {
    let go = false;
    try {
      go = await ask(tr("menuEditor.vorlageUeberschreibt"), { title: tr("menuEditor.ausVorlage"), kind: "warning" });
    } catch (e) {
      flash(tr("menuEditor.dateiauswahlFehlgeschlagen", { e }), "error");
    }
    if (!go) return;
    const p = await chooseFile(TEMPLATE_FILTER);
    if (!p) return;
    try {
      const { layout, warnings } = await importConvertxTemplate(p, titles, currentLanguage());
      remember();
      L = layout;
      menuIdx = 0;
      selIds = [];
      selKey = null;
      renderAll();
      flash(warnings.length ? tr("menuEditor.vorlageImportiertHinweise", { n: warnings.length, first: warnings[0] }) : tr("menuEditor.vorlageImportiert"), "ok");
    } catch (e) {
      flash(tr("menuEditor.vorlageFehlgeschlagen", { e }), "error");
    }
  }
  async function loadFile() {
    const p = await chooseFile(LAYOUT_FILTER);
    if (!p) return;
    try {
      const loaded = await loadLayout(p);
      remember();
      L = loaded;
      menuIdx = 0;
      selIds = [];
      renderAll();
      flash(tr("menuEditor.geladen", { p }), "ok");
    } catch (e) {
      flash(tr("menuEditor.ladenFehlgeschlagen", { e }), "error");
    }
  }
  async function saveFile() {
    let p: string | null;
    try {
      p = await save({ filters: LAYOUT_FILTER, defaultPath: "menue-layout.json" });
    } catch (e) {
      // e.g. the dialog is not permitted (capability) – never fail silently.
      flash(tr("menuEditor.speichernDialogKonnteNicht", { e }), "error");
      return;
    }
    if (typeof p !== "string" || !p) return; // cancelled
    const path = /\.json$/i.test(p) ? p : `${p}.json`;
    try {
      await saveLayout(path, L);
      dirty = false;
      flash(tr("menuEditor.gespeichert", { path }), "ok");
    } catch (e) {
      flash(tr("menuEditor.speichernFehlgeschlagen", { e }), "error");
    }
  }

  async function apply() {
    const result = await checkLayout(L, titles, tv).catch((e) => [String(e)]);
    errors = result;
    renderStatus();
    if (result.length > 0) return;
    opts.onApply(structuredClone(L));
    dirty = false;
    flash(tr("menuEditor.uebernommen"), "ok");
    renderHeader();
  }

  /** Returns true when the editor shut down, false while the "discard changes?" confirm is showing. */
  function close(force: boolean): boolean {
    if (!force && dirty && !confirmClose) {
      confirmClose = true;
      renderHeader();
      window.setTimeout(() => {
        confirmClose = false;
        if (!closed) renderHeader();
      }, 4000);
      return false;
    }
    closed = true;
    closeHook = null;
    if (playTimer !== null) {
      clearInterval(playTimer);
      playTimer = null;
    }
    ro.disconnect();
    window.removeEventListener("keydown", onKey, true);
    window.clearTimeout(checkTimer);
    window.clearTimeout(noticeTimer);
    editor.remove();
    opts.onClose();
    return true;
  }
  closeHook = close;

  function onKey(ev: KeyboardEvent) {
    if ((ev.target as HTMLElement).matches("input, textarea, select")) return;
    const ctrl = ev.ctrlKey || ev.metaKey;
    const key = ev.key.toLowerCase();
    if (ctrl && key === "z") {
      ev.preventDefault();
      undo();
      return;
    }
    if (ctrl && key === "v") {
      ev.preventDefault();
      addCopies(clipboard);
      return;
    }
    const sel = selection();
    if (viewMode === "timeline" && !ctrl) {
      const gop = tv.toLowerCase() === "pal" ? 15 : 18;
      if (ev.key === " ") {
        ev.preventDefault();
        togglePlay();
        return;
      }
      if (ev.key === "Home") {
        ev.preventDefault();
        setPlayhead(0);
        return;
      }
      if (sel.length === 0 && (ev.key === "ArrowLeft" || ev.key === "ArrowRight")) {
        ev.preventDefault();
        stopPlay();
        setPlayhead(playheadTime + (ev.key === "ArrowLeft" ? -1 : 1) * (ev.shiftKey ? gop : 1) * frameSecs());
        renderTimelinePane(); // play button label after stopPlay()
        return;
      }
    }
    if (sel.length === 0) return;
    const step = ev.shiftKey ? 10 : 1;
    const moves: Record<string, [number, number]> = { ArrowLeft: [-step, 0], ArrowRight: [step, 0], ArrowUp: [0, -step], ArrowDown: [0, step] };
    if (moves[ev.key]) {
      ev.preventDefault();
      remember();
      shiftBy(sel, moves[ev.key][0], moves[ev.key][1]);
      renderStage();
      renderProps();
      renderHeader();
      scheduleCheck();
    } else if (ev.key === "Delete" || ev.key === "Backspace") {
      ev.preventDefault();
      removeElements(sel);
    } else if (ctrl && key === "d") {
      ev.preventDefault();
      addCopies(sel);
    } else if (ctrl && (key === "c" || key === "x")) {
      ev.preventDefault();
      clipboard = structuredClone(sel);
      if (key === "x") removeElements(sel);
    } else if (ctrl && key === "g") {
      ev.preventDefault();
      if (ev.shiftKey) ungroupSelection();
      else groupSelection();
    } else if (ev.key === "Escape") {
      selIds = [];
      refreshSelection();
    }
  }
  window.addEventListener("keydown", onKey, true);

  renderAll();
}
