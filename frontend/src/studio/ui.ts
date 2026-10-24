// Small DOM helpers shared by the Burn Studio views.

type Child = Node | string | null | undefined | false;

export function el<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  props: Record<string, unknown> = {},
  ...children: Child[]
): HTMLElementTagNameMap[K] {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(props)) {
    if (v === undefined || v === null || v === false) continue;
    if (k === "class") e.className = String(v);
    else if (k.startsWith("on") && typeof v === "function") e.addEventListener(k.slice(2), v as EventListener);
    else if (k in e && typeof v !== "string") (e as unknown as Record<string, unknown>)[k] = v;
    else e.setAttribute(k, v === true ? "" : String(v));
  }
  for (const c of children) if (c !== null && c !== undefined && c !== false) e.append(c);
  return e;
}

export function fmtBytes(b: number): string {
  if (b >= 1e9) return `${(b / 1e9).toFixed(2)} GB`;
  if (b >= 1e6) return `${(b / 1e6).toFixed(1)} MB`;
  if (b >= 1e3) return `${(b / 1e3).toFixed(0)} kB`;
  return `${b} B`;
}

export function fmtTime(secs: number | null | undefined): string {
  if (secs === null || secs === undefined || !Number.isFinite(secs) || secs < 0) return "—";
  const s = Math.round(secs);
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const p = (n: number) => String(n).padStart(2, "0");
  return h > 0 ? `${h}:${p(m)}:${p(s % 60)}` : `${m}:${p(s % 60)}`;
}

/** Sectors → "2.295.104 Sektoren (4,70 GB)". */
export function fmtSectors(n: number): string {
  return `${n.toLocaleString("de-DE")} Sektoren (${fmtBytes(n * 2048)})`;
}

export interface ModalButton {
  label: string;
  primary?: boolean;
  /** Return `false` to keep the modal open. */
  onClick?: () => boolean | void | Promise<boolean | void>;
}

/** A modal over the whole window; buttons close it unless their handler returns `false`. */
export function modal(
  title: string,
  body: HTMLElement,
  buttons: ModalButton[] = [{ label: "Schließen", primary: true }],
  wide = false,
): () => void {
  const overlay = el("div", { class: "overlay" });
  const close = () => overlay.remove();
  const footer = el("div", { class: "settings-footer" }, el("span", { class: "settings-path" }));
  for (const b of buttons) {
    footer.append(
      el(
        "button",
        {
          class: `btn${b.primary ? " primary" : ""}`,
          onclick: async () => {
            if ((await b.onClick?.()) !== false) close();
          },
        },
        b.label,
      ),
    );
  }
  overlay.append(
    el(
      "div",
      { class: `settings-dialog studio-modal${wide ? " wide" : ""}` },
      el("div", { class: "settings-header" }, el("h2", {}, title)),
      el("div", { class: "settings-page" }, body),
      footer,
    ),
  );
  document.body.append(overlay);
  return close;
}

export function field(label: string, control: HTMLElement, hint?: string): HTMLElement {
  return el("label", { class: "opt-row" }, el("span", { class: "opt-label" }, label), control, hint ? el("small", { class: "opt-hint" }, hint) : null);
}
