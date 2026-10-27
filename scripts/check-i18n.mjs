// Fails the build on i18n drift. Zero dependencies.
//  1. locale files have the same keys   3. same {placeholders} per key
//  2. every literal t("…") key exists   4. no hard-coded umlauts/ß in .ts code (comments ignored)
//  5. every data-i18n* key in the HTML exists
import { readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("../frontend/", import.meta.url));
const src = join(root, "src");
const errors = [];

const locales = {};
for (const f of readdirSync(join(src, "locales")).filter((f) => f.endsWith(".json"))) {
  locales[f.slice(0, -5)] = JSON.parse(readFileSync(join(src, "locales", f), "utf8"));
}
const codes = Object.keys(locales);
if (!locales.en) errors.push("locales/en.json missing (master language)");
const master = locales.en ?? {};

const holders = (s) => [...new Set([...s.matchAll(/\{(\w+)\}/g)].map((m) => m[1]))].sort().join(",");
for (const code of codes) {
  if (!locales[code]["_meta.name"]) errors.push(`${code}.json: _meta.name missing`);
  for (const [k, v] of Object.entries(locales[code])) if (v === "") errors.push(`${code}.json: empty value for ${k}`);
  for (const k of Object.keys(master)) {
    if (!(k in locales[code])) errors.push(`${code}.json: missing key ${k}`);
    else if (holders(master[k]) !== holders(locales[code][k])) errors.push(`${code}.json: placeholders of ${k} differ from en.json`);
  }
  for (const k of Object.keys(locales[code])) if (!(k in master)) errors.push(`${code}.json: extra key ${k} (not in en.json)`);
}

const files = (dir) =>
  readdirSync(dir, { withFileTypes: true }).flatMap((e) => (e.isDirectory() ? (e.name === "locales" ? [] : files(join(dir, e.name))) : [join(dir, e.name)]));
const ts = files(src).filter((f) => f.endsWith(".ts"));
const html = readdirSync(root).filter((f) => f.endsWith(".html")).map((f) => join(root, f));

const stripComments = (s) => s.replace(/\/\*[\s\S]*?\*\//g, "").replace(/(^|[\s;,({[])\/\/.*$/gm, "$1");

for (const f of ts) {
  const code = stripComments(readFileSync(f, "utf8"));
  const name = f.slice(root.length);
  for (const m of code.matchAll(/\b(?:t|tr)\(\s*(["'])([\w.-]+)\1/g)) {
    if (!(m[2] in master)) errors.push(`${name}: unknown key ${m[2]}`);
  }
  code.split("\n").forEach((line, i) => {
    if (/[äöüÄÖÜß]/.test(line)) errors.push(`${name}:${i + 1}: hard-coded umlaut: ${line.trim().slice(0, 80)}`);
  });
}
for (const f of html) {
  const text = readFileSync(f, "utf8");
  for (const m of text.matchAll(/data-i18n(?:-title|-placeholder)?="([\w.-]+)"/g)) {
    if (!(m[1] in master)) errors.push(`${f.slice(root.length)}: unknown data-i18n key ${m[1]}`);
  }
}

if (errors.length) {
  console.error(`check-i18n: ${errors.length} problem(s)\n  ` + errors.join("\n  "));
  process.exit(1);
}
console.log(`check-i18n: ok (${Object.keys(master).length} keys, ${codes.length} languages)`);
