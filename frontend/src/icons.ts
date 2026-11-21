/// <reference types="vite/client" />
import type { IconName } from "./layout";
import audio from "../../assets/icons/audio.svg?raw";
import back from "../../assets/icons/back.svg?raw";
import home from "../../assets/icons/home.svg?raw";
import next from "../../assets/icons/next.svg?raw";
import play from "../../assets/icons/play.svg?raw";
import prev from "../../assets/icons/prev.svg?raw";
import subtitles from "../../assets/icons/subtitles.svg?raw";
import arrowLeft from "../../assets/icons/arrowLeft.svg?raw";
import arrowRight from "../../assets/icons/arrowRight.svg?raw";
import chapters from "../../assets/icons/chapters.svg?raw";
import settings from "../../assets/icons/settings.svg?raw";
import star from "../../assets/icons/star.svg?raw";
import framePill from "../../assets/icons/framePill.svg?raw";
import frameRect from "../../assets/icons/frameRect.svg?raw";
import pointer from "../../assets/icons/pointer.svg?raw";
import bullet from "../../assets/icons/bullet.svg?raw";
import underline from "../../assets/icons/underline.svg?raw";
import brackets from "../../assets/icons/brackets.svg?raw";

const SOURCES: Record<IconName, string> = {
  audio, back, home, next, play, prev, subtitles, arrowLeft, arrowRight, chapters, settings, star, framePill, frameRect, pointer, bullet, underline, brackets,
};

/** Icons that fill the whole element box instead of being contain-fitted. */
export const STRETCH_ICONS: IconName[] = ["framePill", "frameRect", "underline", "brackets"];

const f3 = (v: number) => v.toFixed(3);

/** Generated SVG of a stretch icon for a w x h box. Mirrors `icon_svg_sized` in src/icons.rs; keep in sync. */
function stretchSvg(name: IconName, w: number, h: number): string {
  const m = Math.min(w, h);
  const sw = Math.min(Math.max(m * 0.08, 1.5), 12, m / 2);
  const hs = sw / 2;
  const iw = w - sw;
  const ih = h - sw;
  let body: string;
  if (name === "framePill") {
    body = `<rect x="${f3(hs)}" y="${f3(hs)}" width="${f3(iw)}" height="${f3(ih)}" rx="${f3(ih / 2)}" fill="none" stroke="currentColor" stroke-width="${f3(sw)}"/>`;
  } else if (name === "frameRect") {
    body = `<rect x="${f3(hs)}" y="${f3(hs)}" width="${f3(iw)}" height="${f3(ih)}" rx="${f3(hs)}" fill="none" stroke="currentColor" stroke-width="${f3(sw)}"/>`;
  } else if (name === "underline") {
    body = `<rect x="0" y="${f3(h - sw)}" width="${f3(w)}" height="${f3(sw)}" rx="${f3(hs)}" fill="currentColor"/>`;
  } else {
    const a = m * 0.3;
    const r = w - hs;
    const b = h - hs;
    body = `<path d="M${f3(hs + a)} ${f3(hs)}H${f3(hs)}V${f3(b)}H${f3(hs + a)}M${f3(r - a)} ${f3(hs)}H${f3(r)}V${f3(b)}H${f3(r - a)}" fill="none" stroke="currentColor" stroke-width="${f3(sw)}"/>`;
  }
  return `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 ${f3(w)} ${f3(h)}" width="${f3(w)}" height="${f3(h)}">${body}</svg>`;
}

/** The built-in symbol as SVG text with every `currentColor` replaced by `color`. Stretch icons with
 *  `w`/`h` (canvas units) get geometry generated for that box; everything else is the 24x24 source. */
export function iconSvg(name: IconName, color: string, w?: number, h?: number): string {
  const svg =
    STRETCH_ICONS.includes(name) && w !== undefined && h !== undefined && w > 0 && h > 0 ? stretchSvg(name, w, h) : SOURCES[name];
  return svg.replaceAll("currentColor", color);
}

/** `iconSvg` as a `data:` URL, for `<img>` and canvas drawing. */
export function iconDataUrl(name: IconName, color: string, w?: number, h?: number): string {
  return `data:image/svg+xml;charset=utf-8,${encodeURIComponent(iconSvg(name, color, w, h))}`;
}
