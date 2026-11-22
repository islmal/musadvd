// DVD volume label rules, shared with the Rust side (`burner::label`).

export const DEFAULT_LABEL = "DVD_VIDEO";
export const MAX_LABEL = 32;
/** UDF volume identifier (UDF 1.02 bridge and UDF 2.50): 30 Latin-1 characters. */
export const MAX_UDF_LABEL = 30;

/** Free-text UDF label (same as `burner::label::sanitize_udf_label`): control characters and
 * characters outside Latin-1 removed, at most 30. `trim`: off while typing so spaces can be typed. */
export function sanitizeUdfLabel(input: string, trim = true): string {
  let out = [...input].filter((c) => c.codePointAt(0)! < 256 && !/\p{Cc}/u.test(c)).join("");
  if (trim) out = out.trimStart();
  out = [...out].slice(0, MAX_UDF_LABEL).join("");
  return trim ? out.trimEnd() : out;
}

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
