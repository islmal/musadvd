import { localeData } from "./api";
// UI strings live in locales/<code>.json (flat, dotted keys, `{name}` placeholders, `_meta.name` =
// display name). Add a language by dropping a JSON file into locales/.
// Catalogs are read from the locale files at runtime (Tauri command), nothing is bundled.
let master: Record<string, string> = {};
let cat: Record<string, string> = {};

// English is the master catalog (every key exists there); missing translations fall back to it.
const MASTER = "en";
// Language used until the saved one is loaded and when it is unknown.
const DEFAULT = "en";
let current = DEFAULT;

/** Translates `key` (current language, then German, then the key itself). */
export function t(key: string, vars?: Record<string, unknown>): string {
  const raw = cat[key] ?? master[key] ?? key;
  return vars ? raw.replace(/\{(\w+)\}/g, (m, k: string) => (k in vars ? String(vars[k]) : m)) : raw;
}

export function currentLanguage(): string {
  return current;
}

/** Number with the decimal separator of the current language. */
export function fmtNumber(n: number, digits?: number): string {
  const opts = digits === undefined ? undefined : { minimumFractionDigits: digits, maximumFractionDigits: digits };
  return n.toLocaleString(current, opts);
}

/** Translates `data-i18n` (text), `data-i18n-title`, `data-i18n-placeholder` below `root`. */
export function applyStatic(root: ParentNode = document): void {
  root.querySelectorAll<HTMLElement>("[data-i18n]").forEach((e) => (e.textContent = t(e.dataset.i18n!)));
  root.querySelectorAll<HTMLElement>("[data-i18n-title]").forEach((e) => (e.title = t(e.dataset.i18nTitle!)));
  root.querySelectorAll<HTMLInputElement>("[data-i18n-placeholder]").forEach((e) => (e.placeholder = t(e.dataset.i18nPlaceholder!)));
}

/** Loads `code` plus the English master; an unreadable file falls back to German, then English only. */
export async function loadLanguage(code: string): Promise<void> {
  for (const lang of [code, DEFAULT, MASTER]) {
    try {
      const [m, c] = await Promise.all([localeData(MASTER), localeData(lang)]);
      [master, cat] = [m, c];
      current = lang;
      break;
    } catch {
      // try the next fallback
    }
  }
  document.documentElement.lang = current;
}

/** Loads the saved language. Call (and await) before the first render of every window. */
export async function initI18n(getSettings: () => Promise<{ ui_language: string }>): Promise<void> {
  try {
    await loadLanguage((await getSettings()).ui_language);
  } catch {
    await loadLanguage(DEFAULT);
  }
  applyStatic();
}
