// Pre-flight check before a conversion starts: warnings from data the GUI already has. Hard
// blockers (strict copy failures, merge mismatches, stale layouts) are handled by the Convert button.
// Copy failures that only show while muxing (VBV overflow, IDR distance) cannot be predicted here;
// the build reports them as warnings/errors.
import { t as tr } from "./i18n";
import { modal } from "./studio/ui";

export interface PreflightInput {
  /** Disc fill of the size estimate (percent) and the bitrate clamp of the calculation. */
  fillPercent: number | null;
  clamp: "none" | "min" | "max" | null;
  titles: {
    label: string;
    /** Blu-ray or DVD stream copy was requested (project or title level) and the verdict says it will be encoded. */
    copyFallback: string | null;
    /** Blu-ray stream copy is still on while a forced deinterlace is set. */
    copyWithDeinterlace: boolean;
    quality: "good" | "medium" | "poor" | null;
  }[];
}

export function collectWarnings(p: PreflightInput): string[] {
  const out: string[] = [];
  if (p.fillPercent !== null && p.fillPercent > 99) out.push(tr("preflight.discFull", { pct: Math.round(p.fillPercent) }));
  if (p.clamp === "min") out.push(tr("preflight.minBitrate"));
  for (const t of p.titles) {
    if (t.copyFallback) out.push(tr("preflight.copyFallback", { title: t.label, reason: t.copyFallback }));
    if (t.copyWithDeinterlace) out.push(tr("preflight.copyDeinterlace", { title: t.label }));
    if (t.quality === "poor") out.push(tr("preflight.poorQuality", { title: t.label }));
  }
  return out;
}

/** Resolves true to go on; no dialog when there is nothing to warn about. */
export function confirmPreflight(p: PreflightInput): Promise<boolean> {
  const warnings = collectWarnings(p);
  if (warnings.length === 0) return Promise.resolve(true);
  const body = document.createElement("div");
  const ul = Object.assign(document.createElement("ul"), { className: "preflight-list" });
  for (const w of warnings) ul.append(Object.assign(document.createElement("li"), { textContent: w }));
  body.append(Object.assign(document.createElement("p"), { className: "hint", textContent: tr("preflight.intro") }), ul);
  return new Promise((resolve) => {
    modal(tr("preflight.title"), body, [
      { label: tr("preflight.cancel"), onClick: () => resolve(false) },
      { label: tr("preflight.convertAnyway"), primary: true, onClick: () => resolve(true) },
    ]);
  });
}
