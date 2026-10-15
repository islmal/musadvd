// DVD volume label rules, shared with the Rust side (`burner::label`).

export const DEFAULT_LABEL = "DVD_VIDEO";
export const MAX_LABEL = 32;

/**
 * Volume label rules (same as `burner::label::sanitize_volume_label`): upper case, spaces and
 * punctuation → `_`, other characters removed, `__` collapsed, max. 32 characters.
 * `fallback`: an empty (or underscore-only) result becomes DVD_VIDEO – off while typing.
 */
export function sanitizeLabel(input: string, fallback = true): string {
  let out = "";
  for (const ch of input) {
    const c = /^[a-z]$/.test(ch) ? ch.toUpperCase() : ch;
    let m: string | null = null;
    if (/^[A-Z0-9_]$/.test(c)) m = c;
    else if (/^\s$/u.test(c) || /^[!-/:-@[-`{-~]$/.test(c)) m = "_";
    if (m !== null && !(m === "_" && out.endsWith("_"))) out += m;
  }
  out = out.slice(0, MAX_LABEL);
  if (fallback && /^_*$/.test(out)) return DEFAULT_LABEL;
  return out;
}
