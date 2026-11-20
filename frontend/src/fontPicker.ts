import "./fontPicker.css";
import { listSystemFonts } from "./api";
import { FONTS } from "./layout";
import { t } from "./i18n";

const RECENT_KEY = "musadvd-recent-fonts";
const MAX_RECENT = 5;
const MAX_VISIBLE = 300;

let systemCache: Promise<string[]> | null = null;
/** Installed families, de-duplicated and sorted; loaded once for all pickers. */
const systemFamilies = () =>
  (systemCache ??= listSystemFonts().then((l) => [...new Set(l.map((f) => f.family))].sort((a, b) => a.localeCompare(b))));

function readRecent(): string[] {
  try {
    const v = JSON.parse(localStorage.getItem(RECENT_KEY) ?? "[]");
    return Array.isArray(v) ? v.filter((x) => typeof x === "string").slice(0, MAX_RECENT) : [];
  } catch {
    return [];
  }
}

function pushRecent(family: string) {
  try {
    const next = [family, ...readRecent().filter((f) => f !== family)].slice(0, MAX_RECENT);
    localStorage.setItem(RECENT_KEY, JSON.stringify(next));
  } catch {
    /* storage unavailable: recents are a convenience only */
  }
}

const css = (family: string) => `"${family.replace(/["\\]/g, "")}", sans-serif`;

let uid = 0;

export function fontPicker(opts: { value: string; onChange: (family: string) => void; defaultOption?: string }): HTMLElement & { value: string } {
  const id = `fp${++uid}`;
  let value = opts.value;
  let system: string[] = [];
  let active = -1;
  let options: { family: string; el: HTMLElement }[] = [];

  const root = document.createElement("div") as unknown as HTMLElement & { value: string };
  root.className = "fp";
  const btn = document.createElement("button");
  btn.type = "button";
  btn.className = "fp-btn";
  btn.setAttribute("aria-haspopup", "listbox");
  btn.setAttribute("aria-expanded", "false");
  const label = document.createElement("span");
  btn.append(label);
  root.append(btn);
  Object.defineProperty(root, "value", { get: () => value, set: (v: string) => ((value = v), paintButton()) });

  function paintButton() {
    label.textContent = value || opts.defaultOption || "";
    label.style.fontFamily = value ? css(value) : "";
  }
  paintButton();

  let pop: HTMLElement | null = null;
  let search: HTMLInputElement;
  let list: HTMLElement;

  const close = (refocus: boolean) => {
    if (!pop) return;
    pop.remove();
    pop = null;
    document.removeEventListener("pointerdown", outside, true);
    window.removeEventListener("resize", onResize);
    btn.setAttribute("aria-expanded", "false");
    if (refocus) btn.focus();
  };
  const outside = (e: Event) => {
    if (pop && !pop.contains(e.target as Node) && !btn.contains(e.target as Node)) close(false);
  };
  const onResize = () => close(false);

  const pick = (family: string) => {
    value = family;
    if (family) pushRecent(family);
    paintButton();
    close(true);
    opts.onChange(family);
  };

  function setActive(i: number) {
    if (options.length === 0) return;
    active = Math.max(0, Math.min(options.length - 1, i));
    options.forEach((o, k) => o.el.classList.toggle("active", k === active));
    const el = options[active].el;
    el.scrollIntoView({ block: "nearest" });
    search.setAttribute("aria-activedescendant", el.id);
  }

  function render() {
    const q = search.value.trim().toLowerCase();
    const match = (f: string) => !q || f.toLowerCase().includes(q);
    list.replaceChildren();
    options = [];
    const add = (family: string, text = family) => {
      const el = document.createElement("div");
      el.id = `${id}-o${options.length}`;
      el.className = "fp-opt";
      el.setAttribute("role", "option");
      el.setAttribute("aria-selected", String(family === value));
      el.textContent = text;
      if (family) el.style.fontFamily = css(family);
      el.addEventListener("pointerdown", (e) => e.preventDefault()); // keep focus in the search box
      el.addEventListener("click", () => pick(family));
      el.addEventListener("pointermove", () => options.findIndex((o) => o.el === el) !== active && setActive(options.findIndex((o) => o.el === el)));
      options.push({ family, el });
      list.append(el);
    };
    const group = (key: string) => {
      const g = document.createElement("div");
      g.className = "fp-group";
      g.setAttribute("role", "presentation");
      g.textContent = t(key);
      list.append(g);
    };

    if (opts.defaultOption !== undefined && match(opts.defaultOption)) add("", opts.defaultOption);
    const shown = new Set<string>();
    const section = (key: string, families: string[]) => {
      const fs = families.filter((f) => !shown.has(f) && match(f));
      if (fs.length === 0) return;
      group(key);
      fs.forEach((f) => (shown.add(f), add(f)));
    };
    // A current value that is in no list (e.g. from another machine) stays selectable.
    if (value && !FONTS.includes(value) && !system.includes(value) && match(value)) add(value);
    section("fontPicker.recent", readRecent());
    shown.clear(); // recents repeat below in their home list
    section("fontPicker.recommended", FONTS);

    const rest = system.filter((f) => !FONTS.includes(f) && match(f));
    if (rest.length) {
      const d = document.createElement("div");
      d.className = "fp-divider";
      d.setAttribute("role", "separator");
      list.append(d);
      group("fontPicker.system");
      rest.slice(0, MAX_VISIBLE).forEach((f) => add(f));
      if (rest.length > MAX_VISIBLE) {
        const m = document.createElement("div");
        m.className = "fp-hint";
        m.textContent = t("fontPicker.more", { n: rest.length - MAX_VISIBLE });
        list.append(m);
      }
    }
    if (options.length === 0) {
      const m = document.createElement("div");
      m.className = "fp-hint";
      m.textContent = t("fontPicker.none");
      list.append(m);
      search.removeAttribute("aria-activedescendant");
      return;
    }
    const sel = options.findIndex((o) => o.family === value);
    setActive(q ? 0 : sel >= 0 ? sel : 0);
  }

  function place() {
    const r = btn.getBoundingClientRect();
    const p = pop!;
    p.style.width = `${Math.max(r.width, 220)}px`;
    p.style.left = `${Math.max(4, Math.min(r.left, window.innerWidth - p.offsetWidth - 4))}px`;
    const below = window.innerHeight - r.bottom - 8;
    const above = r.top - 8;
    const flip = below < Math.min(p.offsetHeight, 320) && above > below;
    p.style.maxHeight = `${Math.min(320, flip ? above : below)}px`;
    if (flip) {
      p.style.top = "";
      p.style.bottom = `${window.innerHeight - r.top + 2}px`;
    } else {
      p.style.bottom = "";
      p.style.top = `${r.bottom + 2}px`;
    }
  }

  function open() {
    if (pop) return;
    // Appended to <body>, not the picker: a wrapping <label> would re-click the button for clicks in the popup.
    pop = document.createElement("div");
    pop.className = "fp-pop";
    search = document.createElement("input");
    search.type = "text";
    search.className = "fp-search";
    search.placeholder = t("fontPicker.search");
    search.setAttribute("role", "combobox");
    search.setAttribute("aria-expanded", "true");
    search.setAttribute("aria-controls", `${id}-list`);
    search.setAttribute("aria-label", t("fontPicker.search"));
    list = document.createElement("div");
    list.id = `${id}-list`;
    list.className = "fp-list";
    list.setAttribute("role", "listbox");
    pop.append(search, list);
    document.body.append(pop);
    btn.setAttribute("aria-expanded", "true");
    render();
    place();
    search.focus();
    document.addEventListener("pointerdown", outside, true);
    window.addEventListener("resize", onResize);
    void systemFamilies().then((l) => {
      if (!pop) return;
      system = l;
      render();
      place();
    });
    search.addEventListener("input", () => (render(), place()));
    search.addEventListener("keydown", (e) => {
      const k = e.key;
      if (k === "ArrowDown") setActive(active + 1);
      else if (k === "ArrowUp") setActive(active - 1);
      else if (k === "Home") setActive(0);
      else if (k === "End") setActive(options.length - 1);
      else if (k === "Enter") {
        if (active >= 0 && options[active]) pick(options[active].family);
      } else if (k === "Escape") close(true);
      else if (k === "Tab") close(false);
      else return;
      if (k !== "Tab") e.preventDefault();
      e.stopPropagation();
    });
  }

  btn.addEventListener("click", () => (pop ? close(false) : open()));
  btn.addEventListener("keydown", (e) => {
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      open();
    }
  });
  return root;
}
