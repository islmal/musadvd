/// <reference types="vite/client" />
import type { IconName } from "./layout";
import audio from "../../assets/icons/audio.svg?raw";
import back from "../../assets/icons/back.svg?raw";
import home from "../../assets/icons/home.svg?raw";
import next from "../../assets/icons/next.svg?raw";
import play from "../../assets/icons/play.svg?raw";
import prev from "../../assets/icons/prev.svg?raw";
import subtitles from "../../assets/icons/subtitles.svg?raw";

const SOURCES: Record<IconName, string> = { audio, back, home, next, play, prev, subtitles };

/** The built-in symbol as SVG text with every `currentColor` replaced by `color`. */
export function iconSvg(name: IconName, color: string): string {
  return SOURCES[name].replaceAll("currentColor", color);
}

/** `iconSvg` as a `data:` URL, for `<img>` and canvas drawing. */
export function iconDataUrl(name: IconName, color: string): string {
  return `data:image/svg+xml;charset=utf-8,${encodeURIComponent(iconSvg(name, color))}`;
}
