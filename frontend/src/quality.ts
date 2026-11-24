// Pure quality rating for the disc quality bar (no DOM, no i18n: callers translate the returned keys).
export type Rating = "excellent" | "good" | "fair" | "low";
export type Medium = "dvd" | "bluray" | "avchd";
export type DvdRes = "full" | "half" | "sif";
export interface Frame { w: number; h: number; fps: number }

export function rate(percent: number): Rating {
  return percent >= 85 ? "excellent" : percent >= 65 ? "good" : percent >= 40 ? "fair" : "low";
}

/** Maps the `dvd_resolution` setting ("Full D1" | "Half D1" | "SIF") to a DvdRes. */
export function dvdResOf(setting: string | null | undefined): DvdRes {
  return setting === "Half D1" ? "half" : setting === "SIF" ? "sif" : "full";
}

/** `bdRes` is the `bd_resolution` setting ("1080p" | "720p"); "720" and "BD720" are accepted too. */
export function frameOf(medium: Medium, dvdRes: DvdRes, tv: "PAL" | "NTSC", bdRes?: string): Frame {
  if (medium !== "dvd") return bdRes?.includes("720") ? { w: 1280, h: 720, fps: 23.976 } : { w: 1920, h: 1080, fps: 23.976 };
  const pal = tv === "PAL";
  const w = dvdRes === "full" ? 720 : 352;
  const h = dvdRes === "sif" ? (pal ? 288 : 240) : pal ? 576 : 480;
  return { w, h, fps: pal ? 25 : 29.97 };
}

export function bitsPerPixel(videoKbps: number, f: Frame): number {
  return (videoKbps * 1000) / (f.w * f.h * f.fps);
}

/** Locale key of a tip for a poor rating, else null. */
export function qualityHint(r: Rating, medium: Medium, dvdRes: DvdRes, discIsDual: boolean): string | null {
  if (r !== "fair" && r !== "low") return null;
  if (medium !== "dvd") return "quality.tipBiggerDisc";
  if (dvdRes === "full") return discIsDual ? "quality.tipHalfD1Only" : "quality.tipHalfD1";
  if (dvdRes === "half") return "quality.tipSif";
  return r === "low" ? "quality.tipFewerTitles" : "quality.tipLowerAudio";
}

export const ratingKey = (r: Rating) => `quality.${r}`;
