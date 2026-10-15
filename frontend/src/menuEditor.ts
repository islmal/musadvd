// Menu editor: WYSIWYG design of custom DVD menus (title menus and root menus) on the TV frame.
//
// Layout: menu manager (left) – stage with drag & drop / resize (centre) – properties (right).
// The stage is the TV frame in storage pixels (720x576 PAL / 720x480 NTSC) shown at 4:3, so
// texts and pictures look as they will on the TV. Buttons are axis-aligned rectangles (no
// rotation, at most 36 per menu, no overlaps) – the DVD subpicture only knows `x0 y0 x1 y1`.
import { open, save } from "@tauri-apps/plugin-dialog";
import { checkLayout, imagePreview, loadLayout, saveLayout, textLines, titleFrame } from "./api";
import {
  FONTS,
  MAX_BUTTONS,
  OVERLAY_LABELS,
  type GradientOverlay,
  chapterLabel,
  draftLayout,
  madeForOtherTitles,
  emptyMenu,
  isButton,
  isMoving,
  newId,
  text as newText,
  type Action,
  type CustomLayout,
  type CustomMenu,
  type EditorTitle,
  type Fit,
  type LayoutElement,
  type TextElement,
} from "./layout";

type Child = Node | string | null | undefined | false;

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

export interface EditorOptions {
  layout: CustomLayout | null;
  titles: EditorTitle[];
  /** "PAL" or "NTSC". */
  tv: string;
  /** Default heading for a new draft. */
  heading: string;
  /** `titlesKey` of the videos `layout` was made for (null = unknown). */
  layoutTitlesKey: string | null;
  onApply: (layout: CustomLayout) => void;
}


const IMAGE_FILTER = [{ name: "Bilder", extensions: ["png", "jpg", "jpeg", "bmp", "gif", "webp"] }];
const AUDIO_FILTER = [{ name: "Audio", extensions: ["mp3", "wav", "flac", "m4a", "aac", "ac3", "ogg", "opus"] }];
const LAYOUT_FILTER = [{ name: "Menü-Layout", extensions: ["json"] }];
const FIT_LABELS: [Fit, string][] = [
  ["contain", "Einpassen"],
  ["cover", "Füllen (zuschneiden)"],
  ["stretch", "Strecken"],
];
const SNAP = 2;

let open_ = false;

/** Opens the menu editor as a full-window overlay. */
export function openMenuEditor(opts: EditorOptions) {
  if (open_) return;
  open_ = true;
  const { titles, tv } = opts;
  // A stored layout for other videos would show their titles: start from a draft for the
  // current ones instead (the old layout stays reachable with "Rückgängig").
  const stale = !!opts.layout && madeForOtherTitles(opts.layout, opts.layoutTitlesKey, titles);
  let L: CustomLayout = structuredClone(opts.layout && !stale ? opts.layout : draftLayout(titles, tv, opts.heading));
  let menuIdx = 0;
  let selId: string | null = null;
  let errors: string[] = [];
  let dirty = false;
  let confirmClose = false;
  let showSafe = true;
  const history: string[] = stale && opts.layout ? [JSON.stringify(opts.layout)] : [];
  const previews = new Map<string, Promise<string>>();

  // ---- helpers --------------------------------------------------------------------------------
  const menu = (): CustomMenu => L.menus[Math.min(menuIdx, L.menus.length - 1)];
  const selected = (): LayoutElement | undefined => menu()?.elements.find((e) => e.id === selId);
  const W = () => L.canvas.width;
  const H = () => L.canvas.height;
  const titleName = (n: number) => `${n}: ${titles[n - 1]?.label ?? "(fehlt)"}`;
  const snap = (v: number) => Math.round(v / SNAP) * SNAP;
  const clampBox = (e: LayoutElement) => {
    e.w = Math.max(8, Math.min(Math.round(e.w), W()));
    e.h = Math.max(8, Math.min(Math.round(e.h), H()));
    e.x = Math.max(0, Math.min(Math.round(e.x), W() - e.w));
    e.y = Math.max(0, Math.min(Math.round(e.y), H() - e.h));
  };

  function remember() {
    history.push(JSON.stringify(L));
    if (history.length > 60) history.shift();
    dirty = true;
  }
  function undo() {
    const last = history.pop();
    if (!last) return;
    L = JSON.parse(last);
    if (!L.menus.some((m) => m.elements.some((e) => e.id === selId))) selId = null;
    renderAll();
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
  const frameOf = (title: number, start?: number | null) => {
    const t = titles[title - 1];
    if (!t) return Promise.reject(new Error("Titel fehlt"));
    const at = start ?? Math.min(t.durationSecs * 0.1, 300);
    return preview("frame", `${t.path}@${at.toFixed(1)}`, () => titleFrame(t.path, at));
  };
  const imageOf = (path: string) => preview("image", path, () => imagePreview(path));

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
  const pixelAspect = () => (4 / 3) * (H() / W());

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
        return `▶ Titel ${titleName(a.title)}`;
      case "playChapter":
        return `▶ Titel ${titleName(a.title)}, ${chapterLabel(titles[a.title - 1], a.chapter)}`;
      case "playAll":
        return "▶ Alle abspielen";
      case "menu":
        return `→ ${L.menus.find((m) => m.id === a.menu)?.name ?? "(Menü fehlt)"}`;
      case "setAudio":
        return `♪ Tonspur: ${trackLabel("audio", menu()?.title, a.source)}`;
      case "setSubtitle":
        return a.source === null ? "Untertitel aus" : `Untertitel: ${trackLabel("subtitle", menu()?.title, a.source)}`;
    }
  }

  /** Label of a selected track of `title` (source stream index). */
  function trackLabel(kind: "audio" | "subtitle", title: number | null | undefined, source: number): string {
    const t = title ? titles[title - 1] : undefined;
    const list = kind === "audio" ? t?.audioTracks : t?.subtitleTracks;
    return list?.find((x) => x.source === source)?.label ?? `Stream #${source} (nicht ausgewählt)`;
  }

  const ROLE_LABELS: Record<CustomMenu["role"], string> = {
    title: "Titelmenü (Hauptmenü)",
    root: "Root-Menü eines Titels",
    settings: "Einstellungen eines Titels (Ton/Untertitel)",
  };

  // ---- DOM skeleton -----------------------------------------------------------------------------
  const menusPane = el("aside", { class: "ed-menus", "aria-label": "Menüs" });
  const stage = el("div", { class: "ed-stage" });
  const viewport = el("div", { class: "ed-viewport" }, stage);
  const toolbar = el("div", { class: "ed-toolbar" });
  const props = el("aside", { class: "ed-props", "aria-label": "Eigenschaften" });
  const status = el("footer", { class: "ed-status", role: "status" });
  const headerButtons = el("div", { class: "ed-header-actions" });
  const editor = el(
    "div",
    { class: "ed", role: "dialog", "aria-modal": "true", "aria-label": "Menü-Editor" },
    el("header", { class: "ed-header" }, el("h2", {}, "Menü-Editor"), headerButtons),
    el("div", { class: "ed-body" }, menusPane, el("section", { class: "ed-center" }, toolbar, viewport), props),
    status,
  );
  const root = el("div", { class: "overlay ed-overlay" }, editor);
  document.body.append(root);

  // ---- stage scaling (TV frame shown at 4:3) ----------------------------------------------------
  let scaleX = 1;
  let scaleY = 1;
  function layoutStage() {
    const pad = 24;
    const vw = viewport.clientWidth - pad;
    const vh = viewport.clientHeight - pad;
    const dispH = Math.max(100, Math.min(vh, (vw * 3) / 4));
    const dispW = (dispH * 4) / 3;
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
    renderProps();
    renderStatus();
    scheduleCheck();
  }

  function renderHeader() {
    headerButtons.replaceChildren(
      el("button", { class: "btn", type: "button", onclick: () => void newDraft() }, "Entwurf aus Titeln"),
      el("button", { class: "btn", type: "button", onclick: () => void loadFile() }, "Laden …"),
      el("button", { class: "btn", type: "button", onclick: () => void saveFile() }, "Speichern …"),
      el("button", { class: "btn", type: "button", disabled: history.length === 0, onclick: undo, title: "Strg+Z" }, "Rückgängig"),
      el("span", { class: "ed-spacer" }),
      el(
        "button",
        { class: "btn", type: "button", onclick: () => close(false) },
        confirmClose ? "Änderungen verwerfen?" : "Abbrechen",
      ),
      el("button", { class: "btn primary", type: "button", onclick: () => void apply() }, "Übernehmen"),
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
            selId = null;
            renderAll();
          },
        },
        el("span", { class: "ed-menu-name" }, m.name || "(ohne Namen)", i === firstTitle ? el("span", { class: "ed-badge start" }, "Start") : null),
        el(
          "span",
          { class: "ed-menu-sub" },
          m.role === "title" ? "Titelmenü" : `${m.role === "root" ? "Root-Menü" : "Einstellungen"} · Titel ${m.title ?? "?"}`,
          ` · ${buttons} Button${buttons === 1 ? "" : "s"}`,
          incoming ? ` · ${incoming}× verlinkt` : "",
          isMoving(m) ? " · bewegt" : "",
        ),
      );
    });
    const m = menu();
    const parts: Child[] = [
      el("h3", {}, "Menüs"),
      el("div", { class: "ed-menu-list" }, ...items),
      el(
        "div",
        { class: "ed-menu-actions" },
        el("button", { class: "btn", type: "button", onclick: () => addMenu("title") }, "+ Titelmenü"),
        el("button", { class: "btn", type: "button", disabled: titles.length === 0, onclick: () => addMenu("root") }, "+ Root-Menü"),
        el("button", { class: "btn", type: "button", disabled: titles.length === 0, onclick: () => addMenu("settings") }, "+ Einstellungen"),
      ),
      m
        ? el(
            "div",
            { class: "ed-menu-actions" },
            el("button", { class: "btn", type: "button", title: "Nach oben", disabled: menuIdx === 0, onclick: () => moveMenu(-1) }, "↑"),
            el("button", { class: "btn", type: "button", title: "Nach unten", disabled: menuIdx >= L.menus.length - 1, onclick: () => moveMenu(1) }, "↓"),
            el("button", { class: "btn", type: "button", onclick: duplicateMenu }, "Duplizieren"),
            el("button", { class: "btn danger", type: "button", disabled: L.menus.length <= 1, onclick: deleteMenu }, "Löschen"),
          )
        : null,
      el(
        "p",
        { class: "ed-hint" },
        "Titelmenüs liegen im Hauptmenü der DVD (Taste „Title“); das erste erscheint beim Einlegen. Root-Menüs gehören zu einem Titel (Taste „Menu“ während der Wiedergabe).",
      ),
    ];
    menusPane.replaceChildren(...parts.filter((c): c is Node | string => !!c));
  }

  function renderToolbar() {
    const buttons = menu()?.elements.filter(isButton).length ?? 0;
    toolbar.replaceChildren(
      el("button", { class: "btn", type: "button", onclick: addText }, "+ Text"),
      el("button", { class: "btn", type: "button", onclick: () => void addImage() }, "+ Bild"),
      el("button", { class: "btn", type: "button", disabled: titles.length === 0, onclick: addVideo }, "+ Video"),
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
        "Sicherer Bereich",
      ),
      el("span", { class: `ed-counter${buttons > MAX_BUTTONS ? " bad" : ""}` }, `${buttons}/${MAX_BUTTONS} Buttons`),
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

  function renderElement(e: LayoutElement): HTMLElement {
    const inner = el("div", { class: "ed-inner" });
    inner.style.opacity = String(e.opacity);
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
    } else {
      const img = el("img", { class: `ed-media fit-${e.fit}`, alt: "", draggable: false }) as HTMLImageElement;
      inner.append(img);
      // Fitted with the TV's pixel shape (square-pixel preview), like the build.
      img.addEventListener("load", () => {
        const sideways = e.rotation % 180 === 90;
        const r = fitOnScreen(img.naturalWidth, img.naturalHeight, sideways ? e.h : e.w, sideways ? e.w : e.h, e.fit, sideways);
        Object.assign(img.style, { position: "absolute", objectFit: "fill", width: `${r.w}px`, height: `${r.h}px`, left: `${r.x}px`, top: `${r.y}px` });
      });
      const src = e.type === "image" ? imageOf(e.src) : frameOf(e.title, e.startSecs);
      src.then((url) => (img.src = url)).catch(() => inner.classList.add("ed-missing"));
      if (e.type === "video") inner.append(el("span", { class: "ed-video-badge" }, `▶ ${titles[e.title - 1]?.label ?? `Titel ${e.title} (fehlt)`}`));
    }
    const node = el(
      "div",
      {
        class: `ed-el${isButton(e) ? " is-button" : ""}${e.id === selId ? " selected" : ""}`,
        title: actionLabel(e.action),
        "data-id": e.id,
        style: `--ed-hl: ${e.highlight?.color ?? L.highlight.color}`,
      },
      inner,
    );
    applyBox(node, e);
    if (e.id === selId) {
      for (const corner of ["nw", "ne", "sw", "se"]) {
        node.append(el("span", { class: `ed-handle ${corner}`, "data-handle": corner }));
      }
    }
    node.addEventListener("pointerdown", (ev) => startDrag(ev, e, node));
    return node;
  }

  function renderStage() {
    const m = menu();
    stage.replaceChildren();
    if (!m) return;
    stage.style.backgroundColor = m.background.color;
    const bgSrc = m.background.video
      ? frameOf(m.background.video.title, m.background.video.startSecs)
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
  }

  // ---- drag & resize ------------------------------------------------------------------------------
  function startDrag(ev: PointerEvent, e: LayoutElement, node: HTMLElement) {
    if (ev.button !== 0) return;
    ev.stopPropagation();
    ev.preventDefault();
    const handle = (ev.target as HTMLElement).dataset.handle;
    if (selId !== e.id) {
      selId = e.id;
      renderStage();
      renderProps();
      // Continue the drag on the re-rendered node.
      const fresh = stage.querySelector<HTMLElement>(`[data-id="${e.id}"]`);
      if (fresh) node = fresh;
    }
    const start = { x: ev.clientX, y: ev.clientY, ex: e.x, ey: e.y, ew: e.w, eh: e.h };
    const before = JSON.stringify(L);
    let moved = false;
    const onMove = (mv: PointerEvent) => {
      const dx = (mv.clientX - start.x) / scaleX;
      const dy = (mv.clientY - start.y) / scaleY;
      if (!moved && Math.abs(dx) + Math.abs(dy) < 2) return;
      moved = true;
      if (!handle) {
        e.x = snap(start.ex + dx);
        e.y = snap(start.ey + dy);
      } else {
        let { ex: x, ey: y, ew: w, eh: h } = start;
        if (handle.includes("e")) w = start.ew + dx;
        if (handle.includes("s")) h = start.eh + dy;
        if (handle.includes("w")) {
          x = start.ex + dx;
          w = start.ew - dx;
        }
        if (handle.includes("n")) {
          y = start.ey + dy;
          h = start.eh - dy;
        }
        if (mv.shiftKey) {
          // Keep the aspect ratio.
          const r = start.ew / start.eh;
          if (Math.abs(w - start.ew) > Math.abs(h - start.eh)) h = w / r;
          else w = h * r;
        }
        e.x = snap(x);
        e.y = snap(y);
        e.w = snap(Math.max(8, w));
        e.h = snap(Math.max(8, h));
      }
      clampBox(e);
      applyBox(node, e);
    };
    const onUp = () => {
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup", onUp);
      if (moved) {
        history.push(before);
        dirty = true;
        renderProps();
        renderHeader();
        scheduleCheck();
      }
    };
    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup", onUp);
  }

  stage.addEventListener("pointerdown", (ev) => {
    if (ev.target === stage || (ev.target as HTMLElement).classList.contains("ed-bg") || (ev.target as HTMLElement).classList.contains("ed-safe")) {
      selId = null;
      renderStage();
      renderProps();
    }
  });

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
    const e = selected();
    props.replaceChildren(...(e ? elementProps(m, e) : menuProps(m)));
  }

  function menuProps(m: CustomMenu): HTMLElement[] {
    const bg = m.background;
    const bgKind = bg.video ? "video" : bg.image ? "image" : "color";
    const sound = m.audio;
    return [
      el("h3", {}, "Menü"),
      group(
        "Allgemein",
        row("Name", bind(el("input", { type: "text", value: m.name }) as HTMLInputElement, (v) => (m.name = v), { live: true })),
        row(
          "Art",
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
          ? row("Titel", select(titleOptions(), String(m.title ?? 1), (v) => (m.title = Number(v)), { disabled: titles.length === 0 }))
          : null,
        row(
          "Dauer (s)",
          num(m.durationSecs ?? 20, (v) => (m.durationSecs = Math.max(1, Math.min(600, v))), { min: "1", max: "600", step: "1" }),
        ),
        el("p", { class: "ed-hint" }, isMoving(m) ? "Bewegtes Menü: wird als Schleife dieser Länge kodiert." : "Standbild (keine Videos, kein Ton)."),
      ),
      group(
        "Hintergrund",
        row(
          "Art",
          select(
            [
              ["color", "Farbe"],
              ["image", "Bild"],
              ["video", "Video eines Titels"],
            ],
            bgKind,
            (v) => {
              if (v === "color") {
                bg.image = null;
                bg.video = null;
              } else if (v === "video") {
                bg.image = null;
                bg.video = { title: 1, startSecs: null };
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
        row("Farbe", bind(el("input", { type: "color", value: bg.color }) as HTMLInputElement, (v) => (bg.color = v.toUpperCase()), { live: true })),
        row(
          "Abdunkeln",
          select(OVERLAY_LABELS, bg.gradientOverlay ?? "none", (v) => (bg.gradientOverlay = v as GradientOverlay)),
        ),
        bg.image ? row("Bild", el("span", { class: "ed-path", title: bg.image }, bg.image.split(/[\\/]/).pop() ?? "")) : null,
        bg.video
          ? row("Titel", select(titleOptions(), String(bg.video.title), (v) => (bg.video = { ...bg.video!, title: Number(v) })))
          : null,
        bg.video
          ? row(
              "Start (s)",
              // Without an explicit start the engine uses 10 % of the runtime (max. 5 min).
              num(
                bg.video.startSecs ?? Math.round(Math.min((titles[bg.video.title - 1]?.durationSecs ?? 0) * 0.1, 300)),
                (v) => (bg.video = { ...bg.video!, startSecs: Math.max(0, v) }),
                { min: "0", step: "1" },
              ),
            )
          : null,
        bgKind !== "color"
          ? row(
              "Weichzeichner",
              bind(el("input", { type: "range", min: "0", max: "50", step: "1", value: String(bg.blur) }) as HTMLInputElement, (v) => (bg.blur = Number(v)), {
                live: true,
              }),
            )
          : null,
      ),
      group(
        "Ton",
        row(
          "Quelle",
          select(
            [
              ["none", "Stumm"],
              ...titles.map((_, i): [string, string] => [`title:${i + 1}`, `Ton von Titel ${titleName(i + 1)}`]),
              ["file", "Audiodatei …"],
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
        sound.type === "file" ? row("Datei", el("span", { class: "ed-path", title: sound.path }, sound.path.split(/[\\/]/).pop() ?? "")) : null,
      ),
      group(
        "Button-Markierung (alle Menüs)",
        row("Ausgewählt", bind(el("input", { type: "color", value: L.highlight.color }) as HTMLInputElement, (v) => (L.highlight.color = v.toUpperCase()), { live: true })),
        row("Aktiviert", bind(el("input", { type: "color", value: L.highlight.selectColor }) as HTMLInputElement, (v) => (L.highlight.selectColor = v.toUpperCase()), { live: true })),
        row(
          "Deckkraft",
          bind(el("input", { type: "range", min: "0.2", max: "1", step: "0.05", value: String(L.highlight.opacity) }) as HTMLInputElement, (v) => (L.highlight.opacity = Number(v)), {
            live: true,
          }),
        ),
        el("p", { class: "ed-hint" }, "Text-Buttons leuchten in Schriftform auf, Bild- und Video-Buttons mit einem Rahmen."),
      ),
    ];
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
    };
    const out: HTMLElement[] = [
      row(
        "Aktion",
        select(
          [
            ["none", "Keine (kein Button)"],
            ["playTitle", "Titel abspielen"],
            ["playChapter", "Kapitel abspielen"],
            ["menu", "Menü öffnen"],
            ["playAll", "Alle Titel abspielen"],
            // Streams belong to a titleset: only in a title's root/settings menus.
            ...(streamTitle ? ([["setAudio", "Tonspur wählen"], ["setSubtitle", "Untertitel wählen"]] as [string, string][]) : []),
          ],
          type,
          (v) => {
            const firstOther = L.menus.find((m) => m.id !== menu().id)?.id ?? menu().id;
            setAction(
              v === "playTitle"
                ? { type: "playTitle", title: 1 }
                : v === "playChapter"
                  ? { type: "playChapter", title: 1, chapter: 1 }
                  : v === "menu"
                    ? { type: "menu", menu: firstOther }
                    : v === "playAll"
                      ? { type: "playAll" }
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
      out.push(el("p", { class: "ed-hint" }, "Ton- und Untertitelwahl gibt es nur in Root- und Einstellungsmenüs eines Titels."));
    }
    if (a?.type === "setAudio" && streamTitle) {
      const tracks = streamTitle.audioTracks ?? [];
      out.push(
        row(
          "Tonspur",
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
          "Untertitel",
          select(
            [["off", "Aus"], ...subs.map((x): [string, string] => [String(x.source), x.label])],
            a.source === null ? "off" : String(a.source),
            (v) => setAction({ type: "setSubtitle", source: v === "off" ? null : Number(v) }),
          ),
        ),
      );
    }
    if (a?.type === "playTitle" || a?.type === "playChapter") {
      out.push(
        row(
          "Titel",
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
          "Kapitel",
          select(
            Array.from({ length: Math.max(1, n) }, (_, i): [string, string] => {
              const name = chapterLabel(titles[a.title - 1], i + 1);
              return [String(i + 1), name === `Kapitel ${i + 1}` ? name : `${i + 1}: ${name}`];
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
          "Ziel",
          select(
            L.menus.map((m): [string, string] => [
              m.id,
              `${m.name}${m.role === "title" ? "" : ` (${m.role === "root" ? "Root" : "Einstellungen"}, Titel ${m.title})`}`,
            ]),
            a.menu,
            (v) => setAction({ type: "menu", menu: v }),
          ),
        ),
      );
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
        "Eigene Farben",
      ),
    ];
    if (own) {
      out.push(
        row("Ausgewählt", bind(el("input", { type: "color", value: own.color }) as HTMLInputElement, (v) => (own.color = v.toUpperCase()), { live: true })),
        row("Aktiviert", bind(el("input", { type: "color", value: own.selectColor }) as HTMLInputElement, (v) => (own.selectColor = v.toUpperCase()), { live: true })),
        el("p", { class: "ed-hint" }, "Eine DVD-Menüseite zeigt höchstens 3 verschiedene Farbpaare; Buttons mit derselben Markierungsfarbe brauchen dieselbe Aktivierungsfarbe."),
      );
    }
    return out;
  }

  function elementProps(m: CustomMenu, e: LayoutElement): HTMLElement[] {
    const kind = e.type === "text" ? "Text" : e.type === "image" ? "Bild" : "Video";
    const typeGroup: Child[] = [];
    if (e.type === "text") {
      const t = e;
      typeGroup.push(
        group(
          "Text",
          bind(el("textarea", { rows: 3, class: "ed-textarea" }, t.text) as HTMLTextAreaElement, (v) => (t.text = v), { live: true }),
          row("Schrift", select(FONTS.map((f): [string, string] => [f, f]), t.font, (v) => (t.font = v))),
          row("Größe (px)", num(t.size, (v) => (t.size = Math.max(4, Math.min(300, v))), { min: "4", max: "300", step: "1" })),
          row("Farbe", bind(el("input", { type: "color", value: t.color }) as HTMLInputElement, (v) => (t.color = v.toUpperCase()), { live: true })),
          el(
            "div",
            { class: "ed-seg", role: "group", "aria-label": "Ausrichtung" },
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
                al === "left" ? "Links" : al === "center" ? "Mitte" : "Rechts",
              ),
            ),
          ),
          el(
            "div",
            { class: "ed-checks" },
            ...([
              ["bold", "Fett"],
              ["italic", "Kursiv"],
              ["shadow", "Schatten"],
              ["wrap", "Zeilenumbruch"],
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
          "Bild",
          row("Datei", el("span", { class: "ed-path", title: img.src }, img.src.split(/[\\/]/).pop() ?? "")),
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
            "Anderes Bild …",
          ),
          row("Anpassung", select(FIT_LABELS, img.fit, (v) => (img.fit = v as Fit))),
        ),
      );
    } else {
      const v = e;
      typeGroup.push(
        group(
          "Video",
          row("Titel", select(titleOptions(), String(v.title), (x) => (v.title = Number(x)))),
          row("Start (s)", num(v.startSecs ?? Math.round(Math.min((titles[v.title - 1]?.durationSecs ?? 0) * 0.1, 300)), (x) => (v.startSecs = Math.max(0, x)), { min: "0", step: "1" })),
          row("Anpassung", select(FIT_LABELS, v.fit, (x) => (v.fit = x as Fit))),
          el("p", { class: "ed-hint" }, "Das Video läuft im Menü als Schleife; ein Bild zeigt die Startposition."),
        ),
      );
    }
    const index = m.elements.indexOf(e);
    return [
      el("h3", {}, kind),
      ...(typeGroup as HTMLElement[]),
      group("Button", ...actionControls(e), isButton(e) ? el("p", { class: "ed-hint" }, actionLabel(e.action)) : null, ...buttonColorControls(e)),
      group(
        "Position & Größe",
        el(
          "div",
          { class: "ed-grid2" },
          row("X", num(e.x, (v) => ((e.x = v), clampBox(e)), { step: "1" })),
          row("Y", num(e.y, (v) => ((e.y = v), clampBox(e)), { step: "1" })),
          row("Breite", num(e.w, (v) => ((e.w = v), clampBox(e)), { step: "1", min: "8" })),
          row("Höhe", num(e.h, (v) => ((e.h = v), clampBox(e)), { step: "1", min: "8" })),
        ),
        row(
          "Drehung",
          select(
            [
              ["0", "0°"],
              ["90", "90°"],
              ["180", "180°"],
              ["270", "270°"],
            ],
            String(e.rotation),
            (v) => (e.rotation = Number(v)),
            { disabled: isButton(e), title: isButton(e) ? "Buttons können nicht gedreht werden (DVD-Hitboxen sind Rechtecke)" : undefined },
          ),
        ),
        row(
          "Deckkraft",
          bind(el("input", { type: "range", min: "0.1", max: "1", step: "0.05", value: String(e.opacity) }) as HTMLInputElement, (v) => (e.opacity = Number(v)), { live: true }),
        ),
      ),
      el(
        "div",
        { class: "ed-row-buttons" },
        el("button", { class: "btn", type: "button", disabled: index === m.elements.length - 1, onclick: () => reorder(e, 1) }, "Nach vorne"),
        el("button", { class: "btn", type: "button", disabled: index === 0, onclick: () => reorder(e, -1) }, "Nach hinten"),
        el("button", { class: "btn", type: "button", onclick: () => duplicate(e) }, "Duplizieren"),
        el("button", { class: "btn danger", type: "button", onclick: () => removeElement(e) }, "Löschen"),
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
          `Neuer Entwurf für die ${titles.length} aktuellen Videos – das gespeicherte Layout war für andere Videos („Rückgängig“ holt es zurück).`,
        ),
      );
    }
    if (H() !== tvCanvas) {
      notes.push(el("li", { class: "warn" }, `Layout für 720×${H()} entworfen – wird für ${tv} auf 720×${tvCanvas} skaliert.`));
    }
    if (errors.length === 0) {
      notes.push(el("li", { class: "ok" }, `✓ Layout ist gültig (${L.menus.length} Menüs).`));
    } else {
      for (const e of errors.slice(0, 8)) notes.push(el("li", { class: "error" }, e));
      if (errors.length > 8) notes.push(el("li", { class: "error" }, `… und ${errors.length - 8} weitere`));
    }
    status.replaceChildren(el("ul", {}, ...notes), el("span", { class: "ed-keys" }, "Ziehen: verschieben · Ecken: Größe (Umschalt: Seitenverhältnis) · Pfeile: 1 px (Umschalt: 10) · Entf: löschen · Strg+D: duplizieren · Strg+Z: rückgängig"));
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
    selId = e.id;
    renderAll();
  }
  function addText() {
    addElement(newText(Math.round(W() / 2 - 120), Math.round(H() / 2 - 25), 240, 50, "Neuer Text", { size: 28, wrap: true }));
  }
  async function addImage() {
    const p = await chooseFile(IMAGE_FILTER);
    if (!p) return;
    addElement({ id: newId("i"), type: "image", src: p, fit: "contain", x: Math.round(W() / 2 - 100), y: Math.round(H() / 2 - 75), w: 200, h: 150, rotation: 0, opacity: 1, action: null });
  }
  function addVideo() {
    addElement({
      id: newId("v"),
      type: "video",
      title: 1,
      startSecs: null,
      fit: "cover",
      x: Math.round(W() / 2 - 120),
      y: Math.round(H() / 2 - 68),
      w: 240,
      h: 136,
      rotation: 0,
      opacity: 1,
      action: { type: "playTitle", title: 1 },
    });
  }
  function removeElement(e: LayoutElement) {
    remember();
    menu().elements = menu().elements.filter((x) => x !== e);
    selId = null;
    renderAll();
  }
  function duplicate(e: LayoutElement) {
    const copy = { ...structuredClone(e), id: newId("d"), x: e.x + 16, y: e.y + 16 };
    addElement(copy);
  }
  function reorder(e: LayoutElement, dir: number) {
    const list = menu().elements;
    const i = list.indexOf(e);
    const j = i + dir;
    if (j < 0 || j >= list.length) return;
    remember();
    [list[i], list[j]] = [list[j], list[i]];
    renderAll();
  }
  function addMenu(role: CustomMenu["role"]) {
    remember();
    const n = L.menus.filter((m) => m.role === role).length + 1;
    const name = role === "title" ? `Titelmenü ${n}` : role === "root" ? `Root-Menü ${n}` : `Einstellungen ${n}`;
    const m = emptyMenu(name, role, role === "title" ? null : 1);
    L.menus.push(m);
    menuIdx = L.menus.length - 1;
    selId = null;
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
    copy.name = `${copy.name} (Kopie)`;
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
    selId = null;
    renderAll();
  }

  async function chooseFile(filters: { name: string; extensions: string[] }[]): Promise<string | null> {
    try {
      const p = await open({ multiple: false, directory: false, filters });
      return typeof p === "string" ? p : null;
    } catch (e) {
      flash(`Dateiauswahl fehlgeschlagen: ${e}`, "error");
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
      if (open_) renderStatus();
    }, 8000);
    renderStatus();
  }

  async function newDraft() {
    remember();
    L = draftLayout(titles, tv, opts.heading);
    menuIdx = 0;
    selId = null;
    renderAll();
  }
  async function loadFile() {
    const p = await chooseFile(LAYOUT_FILTER);
    if (!p) return;
    try {
      const loaded = await loadLayout(p);
      remember();
      L = loaded;
      menuIdx = 0;
      selId = null;
      renderAll();
      flash(`Geladen: ${p}`, "ok");
    } catch (e) {
      flash(`Laden fehlgeschlagen: ${e}`, "error");
    }
  }
  async function saveFile() {
    let p: string | null;
    try {
      p = await save({ filters: LAYOUT_FILTER, defaultPath: "menue-layout.json" });
    } catch (e) {
      // e.g. the dialog is not permitted (capability) – never fail silently.
      flash(`Speichern-Dialog konnte nicht geöffnet werden: ${e}`, "error");
      return;
    }
    if (typeof p !== "string" || !p) return; // cancelled
    const path = /\.json$/i.test(p) ? p : `${p}.json`;
    try {
      await saveLayout(path, L);
      dirty = false;
      flash(`Gespeichert: ${path}`, "ok");
    } catch (e) {
      flash(`Speichern fehlgeschlagen: ${e}`, "error");
    }
  }

  async function apply() {
    const result = await checkLayout(L, titles, tv).catch((e) => [String(e)]);
    errors = result;
    renderStatus();
    if (result.length > 0) return;
    opts.onApply(structuredClone(L));
    close(true);
  }

  function close(force: boolean) {
    if (!force && dirty && !confirmClose) {
      confirmClose = true;
      renderHeader();
      window.setTimeout(() => {
        confirmClose = false;
        if (open_) renderHeader();
      }, 4000);
      return;
    }
    ro.disconnect();
    window.removeEventListener("keydown", onKey, true);
    window.clearTimeout(checkTimer);
    window.clearTimeout(noticeTimer);
    root.remove();
    open_ = false;
  }

  function onKey(ev: KeyboardEvent) {
    const target = ev.target as HTMLElement;
    const typing = target.matches("input, textarea, select");
    if ((ev.ctrlKey || ev.metaKey) && ev.key.toLowerCase() === "z" && !typing) {
      ev.preventDefault();
      undo();
      return;
    }
    const e = selected();
    if (!e || typing) return;
    const step = ev.shiftKey ? 10 : 1;
    const moves: Record<string, [number, number]> = { ArrowLeft: [-step, 0], ArrowRight: [step, 0], ArrowUp: [0, -step], ArrowDown: [0, step] };
    if (moves[ev.key]) {
      ev.preventDefault();
      remember();
      e.x += moves[ev.key][0];
      e.y += moves[ev.key][1];
      clampBox(e);
      renderStage();
      renderProps();
      scheduleCheck();
    } else if (ev.key === "Delete" || ev.key === "Backspace") {
      ev.preventDefault();
      removeElement(e);
    } else if ((ev.ctrlKey || ev.metaKey) && ev.key.toLowerCase() === "d") {
      ev.preventDefault();
      duplicate(e);
    } else if (ev.key === "Escape") {
      selId = null;
      renderStage();
      renderProps();
    }
  }
  window.addEventListener("keydown", onKey, true);

  renderAll();
}
