// Tab "Schnitt & Bild": timeline trimmer (start/end of the used section) and crop inspector
// (black bars cut away, with automatic detection and a preview of the output frame).
import { detectCrop, sourceFrame, titleFrame, type Crop, type VideoMetadata } from "./api";

/** The part of a project title this tab edits. */
export interface EditTarget {
  meta: VideoMetadata;
  /** Used section in seconds of the source (null = file start / end). */
  trimStart: number | null;
  trimEnd: number | null;
  crop: Crop | null;
}

export interface EditHooks {
  /** A value changed: refresh the title list and the size estimate (the tab itself is kept). */
  changed(): void;
  /** Frame aspect (width / height) the picture of display aspect `dar` ends up in on the disc. */
  targetAspect(dar: number): number;
  status(text: string, kind?: "info" | "error"): void;
}

const MIN_SECS = 1;
const MIN_PIXELS = 64;

type Child = Node | string | null | undefined | false;

function h<K extends keyof HTMLElementTagNameMap>(tag: K, props: Record<string, unknown> = {}, ...children: Child[]): HTMLElementTagNameMap[K] {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(props)) {
    if (v === undefined || v === null || v === false) continue;
    if (k === "class") e.className = String(v);
    else if (k.startsWith("on") && typeof v === "function") e.addEventListener(k.slice(2), v as EventListener);
    else if (k in e && typeof v !== "string") (e as unknown as Record<string, unknown>)[k] = v;
    else e.setAttribute(k, v === true ? "" : String(v));
  }
  for (const c of children) if (c !== null && c !== undefined && c !== false) e.append(c);
  return e;
}

/** `h:mm:ss.d` */
export function fmtPrecise(secs: number): string {
  const t = Math.max(0, Math.round(secs * 10) / 10);
  const hh = Math.floor(t / 3600);
  const mm = Math.floor((t % 3600) / 60);
  const ss = t - hh * 3600 - mm * 60;
  return `${hh}:${String(mm).padStart(2, "0")}:${ss.toFixed(1).padStart(4, "0")}`;
}

/** Parses `h:mm:ss(.d)`, `mm:ss` or plain seconds; null if invalid. */
export function parsePrecise(text: string): number | null {
  const parts = text.trim().replace(",", ".").split(":");
  if (parts.length === 0 || parts.length > 3) return null;
  let total = 0;
  for (const p of parts) {
    if (!/^\d+(\.\d+)?$/.test(p)) return null;
    total = total * 60 + Number(p);
  }
  return total;
}

const clamp = (v: number, lo: number, hi: number) => Math.min(hi, Math.max(lo, v));
const even = (v: number) => Math.round(v / 2) * 2;

export function renderEditTab(t: EditTarget, hooks: EditHooks): HTMLElement {
  const root = h("div", { class: "edit-tab" });
  root.append(renderTrim(t, hooks), renderCrop(t, hooks));
  return root;
}

// ---------------------------------------------------------------------------
// Timeline trimmer
// ---------------------------------------------------------------------------

function renderTrim(t: EditTarget, hooks: EditHooks): HTMLElement {
  const dur = t.meta.durationSecs;
  let start = t.trimStart ?? 0;
  let end = t.trimEnd ?? dur;

  const startInput = h("input", { type: "text", class: "mono time", "aria-label": "Start" }) as HTMLInputElement;
  const endInput = h("input", { type: "text", class: "mono time", "aria-label": "Ende" }) as HTMLInputElement;
  const length = h("span", { class: "hint" });
  const range = h("div", { class: "trim-range" });
  const hStart = h("div", { class: "trim-handle", role: "slider", tabIndex: 0, "aria-label": "Startpunkt", title: "Startpunkt ziehen" });
  const hEnd = h("div", { class: "trim-handle", role: "slider", tabIndex: 0, "aria-label": "Endpunkt", title: "Endpunkt ziehen" });
  const track = h("div", { class: "trim-track" }, range, hStart, hEnd);
  const startImg = h("img", { class: "trim-frame", alt: "Bild am Start" }) as HTMLImageElement;
  const endImg = h("img", { class: "trim-frame", alt: "Bild am Ende" }) as HTMLImageElement;

  const store = () => {
    t.trimStart = start > 0.05 ? start : null;
    t.trimEnd = end < dur - 0.05 ? end : null;
  };
  const paint = () => {
    const a = (start / dur) * 100;
    const b = (end / dur) * 100;
    range.style.left = `${a}%`;
    range.style.width = `${b - a}%`;
    hStart.style.left = `${a}%`;
    hEnd.style.left = `${b}%`;
    hStart.setAttribute("aria-valuenow", start.toFixed(1));
    hEnd.setAttribute("aria-valuenow", end.toFixed(1));
    if (document.activeElement !== startInput) startInput.value = fmtPrecise(start);
    if (document.activeElement !== endInput) endInput.value = fmtPrecise(end);
    length.textContent = `Länge ${fmtPrecise(end - start)} von ${fmtPrecise(dur)}`;
  };

  let frameTimer: number | undefined;
  const loadFrames = () => {
    window.clearTimeout(frameTimer);
    frameTimer = window.setTimeout(() => {
      titleFrame(t.meta.path, start).then((u) => (startImg.src = u)).catch(() => startImg.removeAttribute("src"));
      titleFrame(t.meta.path, Math.max(0, end - 0.5)).then((u) => (endImg.src = u)).catch(() => endImg.removeAttribute("src"));
    }, 250);
  };

  const set = (which: "start" | "end", value: number, commit: boolean) => {
    if (which === "start") start = clamp(Math.round(value * 10) / 10, 0, end - MIN_SECS);
    else end = clamp(Math.round(value * 10) / 10, start + MIN_SECS, dur);
    store();
    paint();
    if (commit) {
      loadFrames();
      hooks.changed();
    }
  };

  const drag = (handle: HTMLElement, which: "start" | "end") => {
    const at = (e: PointerEvent) => {
      const r = track.getBoundingClientRect();
      return clamp((e.clientX - r.left) / r.width, 0, 1) * dur;
    };
    handle.addEventListener("pointerdown", (e) => {
      handle.setPointerCapture(e.pointerId);
      e.preventDefault();
    });
    handle.addEventListener("pointermove", (e) => {
      if (handle.hasPointerCapture(e.pointerId)) set(which, at(e), false);
    });
    handle.addEventListener("pointerup", (e) => {
      if (handle.hasPointerCapture(e.pointerId)) {
        handle.releasePointerCapture(e.pointerId);
        set(which, at(e), true);
      }
    });
    handle.addEventListener("keydown", (e) => {
      const step = e.shiftKey ? 10 : 1;
      if (e.key === "ArrowLeft") set(which, (which === "start" ? start : end) - step, true);
      else if (e.key === "ArrowRight") set(which, (which === "start" ? start : end) + step, true);
      else return;
      e.preventDefault();
    });
  };
  drag(hStart, "start");
  drag(hEnd, "end");

  const typed = (input: HTMLInputElement, which: "start" | "end") =>
    input.addEventListener("change", () => {
      const v = parsePrecise(input.value);
      const bad =
        v === null || v < 0 || v > dur + 0.05 || (which === "start" ? v > end - MIN_SECS : v < start + MIN_SECS);
      if (bad) {
        hooks.status(`Ungültige Zeit. Der Ausschnitt muss mindestens ${MIN_SECS} s lang sein und innerhalb von ${fmtPrecise(dur)} liegen.`, "error");
        input.value = fmtPrecise(which === "start" ? start : end);
        return;
      }
      set(which, v, true);
      input.value = fmtPrecise(which === "start" ? start : end);
    });
  typed(startInput, "start");
  typed(endInput, "end");

  paint();
  loadFrames();
  return h(
    "section",
    { class: "edit-section" },
    h("h3", {}, "Schnitt"),
    h("p", { class: "hint" }, "Nur den gewählten Ausschnitt ins Video übernehmen (Vorspann und Abspann abschneiden). Kapitel werden auf den Ausschnitt gekürzt."),
    track,
    h(
      "div",
      { class: "trim-times" },
      h("label", {}, "Start ", startInput),
      length,
      h("label", {}, "Ende ", endInput),
      h(
        "button",
        {
          class: "btn",
          type: "button",
          onclick: () => {
            start = 0;
            end = dur;
            store();
            paint();
            loadFrames();
            hooks.changed();
          },
        },
        "Gesamtes Video",
      ),
    ),
    h("div", { class: "trim-frames" }, h("figure", {}, startImg, h("figcaption", {}, "Start")), h("figure", {}, endImg, h("figcaption", {}, "Ende"))),
  );
}

// ---------------------------------------------------------------------------
// Crop inspector
// ---------------------------------------------------------------------------

type Edge = keyof Crop;

function renderCrop(t: EditTarget, hooks: EditHooks): HTMLElement {
  const v = t.meta.video;
  if (!v) return h("section", { class: "edit-section" }, h("h3", {}, "Bild"), h("div", { class: "empty" }, "Dieses Video hat keine Videospur."));
  const W = v.width;
  const H = v.height;
  const crop: Crop = { ...(t.crop ?? { top: 0, bottom: 0, left: 0, right: 0 }) };

  const img = h("img", { class: "crop-image", alt: "Bild mit Zuschnitt", draggable: false }) as HTMLImageElement;
  const masks = (["top", "bottom", "left", "right"] as const).map((e) => h("div", { class: `crop-mask ${e}` }));
  const box = h("div", { class: "crop-box" });
  const handles = (["top", "bottom", "left", "right"] as const).map((e) =>
    h("div", { class: `crop-edge ${e}`, "data-edge": e, role: "slider", tabIndex: 0, "aria-label": { top: "oberer", bottom: "unterer", left: "linker", right: "rechter" }[e] + " Rand", title: "Rand ziehen" }),
  );
  const stage = h("div", { class: "crop-stage" }, img, ...masks, box, ...handles);
  const fields = {} as Record<Edge, HTMLInputElement>;
  const labels: Record<Edge, string> = { top: "Oben", bottom: "Unten", left: "Links", right: "Rechts" };
  for (const e of ["top", "bottom", "left", "right"] as const) {
    fields[e] = h("input", {
      type: "number",
      min: "0",
      step: "2",
      class: "small",
      "aria-label": labels[e],
      onchange: (ev: Event) => setEdge(e, Number((ev.target as HTMLInputElement).value) || 0, true),
    }) as HTMLInputElement;
  }
  const info = h("p", { class: "hint crop-info" });
  const out = h("canvas", { class: "crop-output", width: 320, height: 180, "aria-label": "Vorschau des Ausgabebilds" }) as HTMLCanvasElement;

  const cropped = () => {
    const w = W - crop.left - crop.right;
    const hh = H - crop.top - crop.bottom;
    return { w, h: hh, dar: v.displayAspect * (w / W) / (hh / H) };
  };
  const store = () => {
    t.crop = crop.top || crop.bottom || crop.left || crop.right ? { ...crop } : null;
  };

  const drawOutput = () => {
    const ctx = out.getContext("2d");
    if (!ctx) return;
    const { dar } = cropped();
    const frame = hooks.targetAspect(dar);
    const cw = 320;
    const ch = Math.round(cw / frame);
    out.width = cw;
    out.height = ch;
    ctx.fillStyle = "#000";
    ctx.fillRect(0, 0, cw, ch);
    // The picture fills the frame in one direction and gets bars in the other.
    const pw = dar > frame ? cw : Math.round(ch * dar);
    const ph = dar > frame ? Math.round(cw / dar) : ch;
    const sw = img.naturalWidth || 1;
    const sh = img.naturalHeight || 1;
    if (img.complete && img.naturalWidth > 0) {
      ctx.drawImage(img, (crop.left / W) * sw, (crop.top / H) * sh, (cropped().w / W) * sw, (cropped().h / H) * sh, Math.round((cw - pw) / 2), Math.round((ch - ph) / 2), pw, ph);
    } else {
      ctx.fillStyle = "#334";
      ctx.fillRect(Math.round((cw - pw) / 2), Math.round((ch - ph) / 2), pw, ph);
    }
    const bars = dar > frame * 1.01 ? "oben und unten" : dar < frame / 1.01 ? "links und rechts" : null;
    info.textContent = `Bild ${cropped().w}×${cropped().h} (${(dar).toFixed(2).replace(".", ",")}:1) → Ausgabebild ${frame > 1.5 ? "16:9" : "4:3"}${bars ? `, schwarze Balken ${bars}` : ""}`;
  };

  const paint = () => {
    const p = (n: number, total: number) => `${(n / total) * 100}%`;
    box.style.cssText = `left:${p(crop.left, W)};top:${p(crop.top, H)};right:${p(crop.right, W)};bottom:${p(crop.bottom, H)}`;
    masks[0].style.cssText = `height:${p(crop.top, H)}`;
    masks[1].style.cssText = `height:${p(crop.bottom, H)}`;
    masks[2].style.cssText = `width:${p(crop.left, W)};top:${p(crop.top, H)};bottom:${p(crop.bottom, H)}`;
    masks[3].style.cssText = `width:${p(crop.right, W)};top:${p(crop.top, H)};bottom:${p(crop.bottom, H)}`;
    for (const e of ["top", "bottom", "left", "right"] as const) {
      if (document.activeElement !== fields[e]) fields[e].value = String(crop[e]);
    }
    drawOutput();
  };

  function setEdge(e: Edge, value: number, commit: boolean) {
    const vertical = e === "top" || e === "bottom";
    const total = vertical ? H : W;
    const other = vertical ? (e === "top" ? crop.bottom : crop.top) : e === "left" ? crop.right : crop.left;
    crop[e] = clamp(even(value), 0, Math.max(0, total - other - MIN_PIXELS));
    store();
    paint();
    if (commit) hooks.changed();
  }

  const dragEdge = (handle: HTMLElement) => {
    const e = handle.dataset.edge as Edge;
    const at = (ev: PointerEvent) => {
      const r = img.getBoundingClientRect();
      const fx = clamp((ev.clientX - r.left) / r.width, 0, 1);
      const fy = clamp((ev.clientY - r.top) / r.height, 0, 1);
      return { top: fy * H, bottom: (1 - fy) * H, left: fx * W, right: (1 - fx) * W }[e];
    };
    handle.addEventListener("pointerdown", (ev) => {
      handle.setPointerCapture(ev.pointerId);
      ev.preventDefault();
    });
    handle.addEventListener("pointermove", (ev) => {
      if (handle.hasPointerCapture(ev.pointerId)) setEdge(e, at(ev), false);
    });
    handle.addEventListener("pointerup", (ev) => {
      if (handle.hasPointerCapture(ev.pointerId)) {
        handle.releasePointerCapture(ev.pointerId);
        setEdge(e, at(ev), true);
      }
    });
    handle.addEventListener("keydown", (ev) => {
      const step = ev.shiftKey ? 20 : 2;
      if (ev.key === "ArrowUp" || ev.key === "ArrowLeft") setEdge(e, crop[e] + (e === "top" || e === "left" ? -step : step), true);
      else if (ev.key === "ArrowDown" || ev.key === "ArrowRight") setEdge(e, crop[e] + (e === "top" || e === "left" ? step : -step), true);
      else return;
      ev.preventDefault();
    });
  };
  handles.forEach(dragEdge);

  const midpoint = () => {
    const a = t.trimStart ?? 0;
    const b = t.trimEnd ?? t.meta.durationSecs;
    return a + (b - a) / 2;
  };
  const loadImage = () => {
    sourceFrame(t.meta.path, midpoint(), 640)
      .then((u) => {
        img.onload = () => drawOutput();
        img.src = u;
      })
      .catch((e) => hooks.status(`Vorschaubild: ${e}`, "error"));
  };

  const detect = h(
    "button",
    {
      class: "btn",
      type: "button",
      onclick: async (ev: Event) => {
        const b = ev.currentTarget as HTMLButtonElement;
        b.disabled = true;
        b.textContent = "Suche …";
        try {
          const c = await detectCrop(t.meta.path, t.trimStart, t.trimEnd);
          Object.assign(crop, c);
          for (const e of ["top", "bottom", "left", "right"] as const) crop[e] = even(crop[e]);
          store();
          paint();
          hooks.changed();
          const none = !crop.top && !crop.bottom && !crop.left && !crop.right;
          hooks.status(none ? "Keine schwarzen Ränder gefunden." : `Zuschnitt erkannt: oben ${crop.top}, unten ${crop.bottom}, links ${crop.left}, rechts ${crop.right} Pixel.`);
        } catch (e) {
          hooks.status(`Automatische Erkennung fehlgeschlagen: ${e}`, "error");
        } finally {
          b.disabled = false;
          b.textContent = "Automatisch erkennen";
        }
      },
    },
    "Automatisch erkennen",
  );
  const reset = h(
    "button",
    {
      class: "btn",
      type: "button",
      onclick: () => {
        crop.top = crop.bottom = crop.left = crop.right = 0;
        store();
        paint();
        hooks.changed();
      },
    },
    "Zurücksetzen",
  );

  paint();
  loadImage();
  return h(
    "section",
    { class: "edit-section" },
    h("h3", {}, "Bild (Zuschnitt)"),
    h("p", { class: "hint" }, "Schwarze Ränder abschneiden: Die Ränder im Bild ziehen, Pixelwerte eingeben oder automatisch erkennen lassen. Das Ergebnis wird mit schwarzen Balken in das Bildformat der Disc eingepasst."),
    h(
      "div",
      { class: "crop-layout" },
      stage,
      h(
        "div",
        { class: "crop-side" },
        h("div", { class: "crop-fields" }, ...(["top", "bottom", "left", "right"] as const).map((e) => h("label", {}, labels[e], fields[e]))),
        h("div", { class: "row" }, detect, reset),
        h("div", { class: "crop-output-box" }, out, info),
      ),
    ),
  );
}
