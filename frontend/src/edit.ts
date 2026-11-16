// Tab "Schnitt & Bild": timeline trimmer (start/end of the used section) and crop inspector
// (black bars cut away, with automatic detection and a preview of the output frame). The trimmer has a
// live player (ffmpeg fMP4 stream from `preview.rs`) to set start, end and chapters at the playhead.
import { detectCrop, previewStop, previewStream, sourceFrame, type Crop, type VideoMetadata } from "./api";
import { fmtNumber, t as tr } from "./i18n";

/** The part of a project title this tab edits. */
export interface EditTarget {
  meta: VideoMetadata;
  /** Used section in seconds of the source (null = file start / end). */
  trimStart: number | null;
  trimEnd: number | null;
  crop: Crop | null;
  /** Chapter starts in source seconds (first is 0); edited in place by "Kapitel hier setzen". */
  chapters: number[];
}

export interface EditHooks {
  /** A value changed: refresh the title list and the size estimate (the tab itself is kept). */
  changed(): void;
  /** Frame aspect (width / height) the picture of display aspect `dar` ends up in on the disc. */
  targetAspect(dar: number): number;
  status(text: string, kind?: "info" | "error"): void;
  /** ffprobe index of the audio track the player plays (null = silent). */
  audioIndex(): number | null;
  /** Turn the title into one title per chapter segment. */
  split(): void;
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

/** Active player of the open tab; `pos` = logical source time of the playhead. */
let player: { video: HTMLVideoElement; pos(): number } | null = null;

/** Stops playback and the preview ffmpeg (call before the tab is torn down). */
export async function disposePlayer() {
  const p = player;
  player = null;
  if (p) {
    p.video.pause();
    p.video.removeAttribute("src");
    p.video.load();
  }
  try {
    await previewStop();
  } catch {
    // nothing left to stop
  }
}

/** Segments the chapters cut the used section [start, end] into; each starts at `chapter` (a source chapter time). */
export function chapterSegments(chapters: number[], start: number, end: number): { start: number; end: number; chapter: number }[] {
  const cs = [...chapters].sort((a, b) => a - b);
  const starts = [start, ...cs.filter((c) => c > start + 0.5 && c < end - 1)];
  const segs = starts.map((s, i) => ({
    start: s,
    end: starts[i + 1] ?? end,
    chapter: i === 0 ? Math.max(0, ...cs.filter((c) => c <= start + 0.5)) : s,
  }));
  const out: typeof segs = [];
  segs.forEach((s, i) => {
    if (s.end - s.start < 1) {
      if (out.length) out[out.length - 1].end = s.end;
      else if (segs[i + 1]) segs[i + 1] = { ...segs[i + 1], start: s.start, chapter: s.chapter };
    } else out.push(s);
  });
  return out;
}

{
  const ranges = (c: number[], a: number, b: number) => chapterSegments(c, a, b).map((s) => `${s.start}-${s.end}`).join(" ");
  console.assert(ranges([], 0, 100) === "0-100");
  console.assert(ranges([0, 60, 120], 0, 180) === "0-60 60-120 120-180");
  console.assert(ranges([0, 0.3, 60], 0, 120) === "0-60 60-120");
  console.assert(ranges([0, 60, 120], 50, 130) === "50-60 60-120 120-130");
  console.assert(ranges([0, 60, 60.6], 0, 180) === "0-60.6 60.6-180");
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

  const startInput = h("input", { type: "text", class: "mono time", "aria-label": tr("edit.start") }) as HTMLInputElement;
  const endInput = h("input", { type: "text", class: "mono time", "aria-label": tr("edit.ende") }) as HTMLInputElement;
  const length = h("span", { class: "hint" });
  const range = h("div", { class: "trim-range" });
  const hStart = h("div", { class: "trim-handle", role: "slider", tabIndex: 0, "aria-label": tr("edit.startpunkt"), title: tr("edit.startpunktZiehen") });
  const hEnd = h("div", { class: "trim-handle", role: "slider", tabIndex: 0, "aria-label": tr("edit.endpunkt"), title: tr("edit.endpunktZiehen") });
  const playhead = h("div", { class: "trim-playhead" });
  const track = h("div", { class: "trim-track" }, range, playhead, hStart, hEnd);
  const video = h("video", { playsinline: "", preload: "auto", "aria-label": tr("edit.vorschau") }) as HTMLVideoElement;
  const timeText = h("span", { class: "mono" });
  const playBtn = h("button", { class: "btn", type: "button" }, tr("edit.wiedergabe")) as HTMLButtonElement;
  const splitBtn = h("button", { class: "btn", type: "button", onclick: () => hooks.split() }, tr("edit.inSeparateTitelAufteilen")) as HTMLButtonElement;
  const splitHint = h("span", { class: "hint" });

  // Output timestamps of a stream start at 0 (`-ss` before `-i`): logical time = base + currentTime.
  let base = 0;
  let seq = 0;
  let streamUrl: Promise<string> | null = null;
  let streamCrop = "";
  const pos = () => base + (video.currentTime || 0);
  const paintPlayhead = () => {
    playhead.style.left = `${(clamp(pos(), 0, dur) / dur) * 100}%`;
    timeText.textContent = `${fmtPrecise(pos())} / ${fmtPrecise(dur)}`;
    playBtn.textContent = video.paused ? tr("edit.wiedergabe") : tr("edit.pause");
  };
  const seek = async (secs: number, play: boolean) => {
    const mine = ++seq;
    const at = clamp(secs, 0, Math.max(0, dur - 0.1));
    try {
      // The stream bakes the crop in: a changed crop needs a new token.
      const cropKey = JSON.stringify(t.crop);
      if (cropKey !== streamCrop) streamUrl = null;
      streamCrop = cropKey;
      streamUrl ??= previewStream(t.meta.path, t.crop);
      const url = await streamUrl;
      await previewStop();
      if (mine !== seq) return;
      const a = hooks.audioIndex();
      base = at;
      video.src = `${url}?t=${at.toFixed(3)}${a !== null ? `&a=${a}` : ""}`;
      if (play) void video.play().catch(() => {});
      paintPlayhead();
    } catch (e) {
      streamUrl = null;
      hooks.status(tr("edit.vorschau2", { e }), "error");
    }
  };
  player = { video, pos };
  for (const ev of ["timeupdate", "play", "pause", "seeked"]) video.addEventListener(ev, paintPlayhead);
  video.addEventListener("timeupdate", () => {
    if (pos() >= dur) video.pause();
  });
  playBtn.addEventListener("click", () => {
    if (!video.paused) video.pause();
    else if (!video.src || video.ended || pos() >= dur - 0.2) void seek(pos() >= dur - 0.2 ? start : pos(), true);
    else void video.play().catch(() => {});
  });
  track.addEventListener("pointerdown", (e) => {
    if (e.target instanceof HTMLElement && e.target.classList.contains("trim-handle")) return;
    const r = track.getBoundingClientRect();
    void seek(clamp((e.clientX - r.left) / r.width, 0, 1) * dur, !video.paused);
  });

  const paintMarks = () => {
    track.querySelectorAll(".trim-chapter").forEach((m) => m.remove());
    for (const c of t.chapters) {
      if (c > 0.05 && c < dur) track.append(h("div", { class: "trim-chapter", style: `left:${(c / dur) * 100}%`, title: fmtPrecise(c) }));
    }
  };
  const refreshSplit = () => {
    const n = chapterSegments(t.chapters, start, end).length;
    splitBtn.disabled = n < 2;
    splitHint.textContent = n < 2 ? tr("edit.mindestensZweiKapitelIm") : tr("edit.ergibtTitel", { n });
  };

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
    length.textContent = tr("edit.laengeVon", { fmtPrecise: fmtPrecise(end - start), fmtPrecise2: fmtPrecise(dur) });
    refreshSplit();
  };

  /** `show`: a committed handle/typed change shows that frame (paused) in the player. */
  const set = (which: "start" | "end", value: number, commit: boolean, show = true) => {
    if (which === "start") start = clamp(Math.round(value * 10) / 10, 0, end - MIN_SECS);
    else end = clamp(Math.round(value * 10) / 10, start + MIN_SECS, dur);
    store();
    paint();
    if (commit) {
      if (show) void seek(which === "start" ? start : Math.max(start, end - 0.5), false);
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
        hooks.status(tr("edit.ungueltigeZeitDerAusschnitt", { MIN_SECS, fmtPrecise: fmtPrecise(dur) }), "error");
        input.value = fmtPrecise(which === "start" ? start : end);
        return;
      }
      set(which, v, true);
      input.value = fmtPrecise(which === "start" ? start : end);
    });
  typed(startInput, "start");
  typed(endInput, "end");

  const addChapter = () => {
    const c = Math.round(pos() * 10) / 10;
    const bad = c <= 0 || c >= dur - 1 || t.chapters.some((x) => Math.abs(x - c) < 0.5) || c < start || c > end;
    if (bad) {
      hooks.status(tr("edit.hierKannKeinKapitel"), "error");
      return;
    }
    t.chapters.push(c);
    t.chapters.sort((x, y) => x - y);
    paintMarks();
    refreshSplit();
    hooks.changed();
    hooks.status(tr("edit.kapitelBeiGesetzt", { fmtPrecise: fmtPrecise(c) }));
  };
  const btn = (label: string, onclick: () => void, title?: string) => h("button", { class: "btn", type: "button", onclick, title }, label);

  paint();
  paintMarks();
  paintPlayhead();
  void seek(start, false);
  return h(
    "section",
    { class: "edit-section" },
    h("h3", {}, tr("edit.schnitt")),
    h("p", { class: "hint" }, tr("edit.nurDenGewaehltenAusschnitt")),
    h(
      "div",
      { class: "edit-player" },
      video,
      h(
        "div",
        { class: "player-controls" },
        btn("◀◀", () => void seek(Math.max(0, pos() - 1.0), false), tr("edit.eineSekundeZurueck")),
        btn("◀", () => void seek(Math.max(0, pos() - 0.1), false), tr("edit.zehntelSekundeZurueck")),
        playBtn,
        btn("▶", () => void seek(Math.min(dur, pos() + 0.1), false), tr("edit.zehntelSekundeVor")),
        btn("▶▶", () => void seek(Math.min(dur, pos() + 1.0), false), tr("edit.eineSekundeVor")),
        timeText,
        btn(tr("edit.startHierSetzen"), () => set("start", pos(), true, false)),
        btn(tr("edit.endeHierSetzen"), () => set("end", pos(), true, false)),
        btn(tr("edit.kapitelHierSetzen"), addChapter),
      ),
    ),
    track,
    h(
      "div",
      { class: "trim-times" },
      h("label", {}, tr("edit.start2"), startInput),
      length,
      h("label", {}, tr("edit.ende2"), endInput),
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
            hooks.changed();
          },
        },
        tr("edit.gesamtesVideo"),
      ),
    ),
    h("div", { class: "row" }, splitBtn, splitHint),
  );
}

// ---------------------------------------------------------------------------
// Crop inspector
// ---------------------------------------------------------------------------

type Edge = keyof Crop;

function renderCrop(t: EditTarget, hooks: EditHooks): HTMLElement {
  const v = t.meta.video;
  if (!v) return h("section", { class: "edit-section" }, h("h3", {}, tr("edit.bild")), h("div", { class: "empty" }, tr("edit.diesesVideoHatKeine")));
  const W = v.width;
  const H = v.height;
  const crop: Crop = { ...(t.crop ?? { top: 0, bottom: 0, left: 0, right: 0 }) };

  const img = h("img", { class: "crop-image", alt: tr("edit.bildMitZuschnitt"), draggable: false }) as HTMLImageElement;
  const masks = (["top", "bottom", "left", "right"] as const).map((e) => h("div", { class: `crop-mask ${e}` }));
  const box = h("div", { class: "crop-box" });
  const handles = (["top", "bottom", "left", "right"] as const).map((e) =>
    h("div", { class: `crop-edge ${e}`, "data-edge": e, role: "slider", tabIndex: 0, "aria-label": { top: "oberer", bottom: "unterer", left: "linker", right: "rechter" }[e] + tr("edit.rand"), title: tr("edit.randZiehen") }),
  );
  const stage = h("div", { class: "crop-stage" }, img, ...masks, box, ...handles);
  const fields = {} as Record<Edge, HTMLInputElement>;
  const labels: Record<Edge, string> = { top: tr("edit.oben"), bottom: tr("edit.unten"), left: tr("edit.links"), right: tr("edit.rechts") };
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
  const out = h("canvas", { class: "crop-output", width: 320, height: 180, "aria-label": tr("edit.vorschauDesAusgabebilds") }) as HTMLCanvasElement;

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
    const bars = dar > frame * 1.01 ? tr("edit.barsTopBottom") : dar < frame / 1.01 ? tr("edit.barsLeftRight") : null;
    info.textContent = tr("edit.bild1Ausgabebild", { cropped: cropped().w, cropped2: cropped().h, dar: fmtNumber(dar, 2), frame: frame > 1.5 ? "16:9" : "4:3", bars: bars ? tr("edit.schwarzeBalken", { bars }) : "" });
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
  const loadImage = (at: number) => {
    sourceFrame(t.meta.path, at, 640)
      .then((u) => {
        img.onload = () => drawOutput();
        img.src = u;
      })
      .catch((e) => hooks.status(tr("edit.vorschaubild", { e }), "error"));
  };

  const detect = h(
    "button",
    {
      class: "btn",
      type: "button",
      onclick: async (ev: Event) => {
        const b = ev.currentTarget as HTMLButtonElement;
        b.disabled = true;
        b.textContent = tr("edit.suche");
        try {
          const c = await detectCrop(t.meta.path, t.trimStart, t.trimEnd);
          Object.assign(crop, c);
          for (const e of ["top", "bottom", "left", "right"] as const) crop[e] = even(crop[e]);
          store();
          paint();
          hooks.changed();
          const none = !crop.top && !crop.bottom && !crop.left && !crop.right;
          hooks.status(none ? tr("edit.keineSchwarzenRaenderGefunden") : tr("edit.zuschnittErkanntObenUnten", { top: crop.top, bottom: crop.bottom, left: crop.left, right: crop.right }));
        } catch (e) {
          hooks.status(tr("edit.automatischeErkennungFehlgeschlagen", { e }), "error");
        } finally {
          b.disabled = false;
          b.textContent = tr("edit.automatischErkennen");
        }
      },
    },
    tr("edit.automatischErkennen"),
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
    tr("edit.zuruecksetzen"),
  );

  paint();
  loadImage(midpoint());
  return h(
    "section",
    { class: "edit-section" },
    h("h3", {}, tr("edit.bildZuschnitt")),
    h("p", { class: "hint" }, tr("edit.schwarzeRaenderAbschneidenDie")),
    h(
      "div",
      { class: "crop-layout" },
      stage,
      h(
        "div",
        { class: "crop-side" },
        h("div", { class: "crop-fields" }, ...(["top", "bottom", "left", "right"] as const).map((e) => h("label", {}, labels[e], fields[e]))),
        h("div", { class: "row" }, detect, reset, h("button", { class: "btn", type: "button", onclick: () => loadImage(player?.pos() ?? midpoint()) }, tr("edit.bildVonAbspielposition"))),
        h("div", { class: "crop-output-box" }, out, info),
      ),
    ),
  );
}
