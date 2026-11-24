// Pure disc decisions used by main.ts (no imports, so vitest can run them directly).

const RATES = { PAL: [25, 50], NTSC: [23.976, 29.97, 59.94] } as const;

/** Does a source frame rate belong to the TV standard `tv`? */
export const fpsMatches = (fps: number, tv: "PAL" | "NTSC") => RATES[tv].some((x) => Math.abs(fps - x) < 0.05);

/** The other TV standard when more than half of the sources run at its frame rates, else null. */
export function tvMismatch(fpsList: number[], tv: "PAL" | "NTSC"): "PAL" | "NTSC" | null {
  const other = tv === "PAL" ? "NTSC" : "PAL";
  return fpsList.filter((f) => fpsMatches(f, other)).length * 2 > fpsList.length ? other : null;
}

/** DVD menu aspect: Half D1 / SIF / Blu-ray are 4:3 here; Full D1 follows `choice`, auto = 16:9 when most titles are 16:9. */
export function autoMenuAspect(
  aspects: (string | null | undefined)[],
  dvdRes: string | undefined,
  choice: "auto" | "4:3" | "16:9",
  isBd: boolean,
): "4:3" | "16:9" {
  if (isBd || dvdRes !== "Full D1") return "4:3";
  if (choice !== "auto") return choice;
  return aspects.filter((a) => a === "16:9").length * 2 > aspects.length ? "16:9" : "4:3";
}

const AC3_DVD = [96, 128, 160, 192, 224, 256, 384, 448];
const AC3_BD = [192, 224, 256, 320, 384, 448, 512, 576, 640];

/** Mirror of the engine's `snap_ac3_kbps`: nearest rate of the medium's set, ties go to the lower rate; 0 stays 0 (auto). */
export function snapAc3Kbps(kbps: number, medium: "dvd" | "bd"): number {
  if (kbps <= 0) return 0;
  const set = medium === "dvd" ? AC3_DVD : AC3_BD;
  return set.reduce((best, s) => (Math.abs(s - kbps) < Math.abs(best - kbps) ? s : best), set[0]);
}

/** Mirror of `ac3_settings`: the AC-3 rate of an encoded track (0 = automatic: 448 for 5+ source channels, else 192). */
export const ac3EncodeKbps = (channels: number, want: number, medium: "dvd" | "bd") =>
  want <= 0 ? (channels >= 5 ? 448 : 192) : snapAc3Kbps(want, medium);

/**
 * Locale key of the "does not fit" advice for `disc`: a bigger medium while there is one, else
 * what is left on the largest disc (audio bitrate, smaller DVD frame size, a second disc).
 */
export function overflowHintKey(disc: string, dvdRes: string): string {
  switch (disc) {
    case "dvd5":
      return "main.dvd9WaehlenOder";
    case "bd25":
      return "main.bd50WaehlenOder";
    case "avchd5":
      return "main.avchdDvd9Waehlen";
    case "bd50":
    case "avchd9":
      return "main.videosKuerzenOderTonspuren";
    default:
      // DVD-9 or a custom size: no bigger DVD to suggest.
      return dvdRes === "SIF" ? "main.overflowSifFull" : "main.overflowDvd9Full";
  }
}
