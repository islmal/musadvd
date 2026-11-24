import "./landing.css";
import { currentLanguage, t } from "./i18n";
import type { RecentProject } from "./recent";
import { el } from "./studio/ui";

export interface LandingHandlers {
  onNew(): void;
  onOpen(): void;
  onStudio(): void;
  onOpenRecent(path: string): void;
  onForgetRecent(path: string): void;
}

// 24x24 stroke icons (currentColor).
const ICONS = {
  new: '<path d="M12 5v14M5 12h14"/>',
  open: '<path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/>',
  studio: '<circle cx="12" cy="12" r="9"/><circle cx="12" cy="12" r="2.5"/>',
};

function icon(name: keyof typeof ICONS): Element {
  const box = document.createElement("div");
  box.innerHTML = `<svg viewBox="0 0 24 24" width="32" height="32" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${ICONS[name]}</svg>`;
  return box.firstElementChild!;
}

function card(kind: keyof typeof ICONS, title: string, desc: string, onClick: () => void): HTMLElement {
  return el("button", { class: "landing-card", type: "button", onclick: onClick }, icon(kind), el("span", { class: "landing-card-title" }, title), el("span", { class: "landing-card-desc" }, desc));
}

function recentRow(r: RecentProject, h: LandingHandlers): HTMLElement {
  const date = new Intl.DateTimeFormat(currentLanguage(), { dateStyle: "medium", timeStyle: "short" }).format(new Date(r.modified));
  return el(
    "li",
    { class: "landing-recent-item" },
    el(
      "button",
      { class: "landing-recent-open", type: "button", title: r.path, onclick: () => h.onOpenRecent(r.path) },
      el("span", { class: "landing-recent-name" }, r.name),
      el("span", { class: "landing-badge" }, r.medium),
      el("span", { class: "landing-recent-path" }, r.path),
      el("span", { class: "landing-recent-date" }, date),
    ),
    el("button", { class: "landing-forget", type: "button", title: t("landing.forget"), "aria-label": t("landing.forget"), onclick: () => h.onForgetRecent(r.path) }, String.fromCharCode(215)),
  );
}

/** Replaces `root`'s children with the start screen. */
export function renderLanding(root: HTMLElement, recent: RecentProject[], h: LandingHandlers): void {
  root.replaceChildren(
    el(
      "div",
      { class: "landing" },
      el("header", { class: "landing-head" }, el("img", { class: "landing-logo", src: "/logo.png", alt: "" }), el("h1", {}, "MusaDVD")),
      el("div", { class: "landing-cards" }, card("new", t("landing.new"), t("landing.newDesc"), h.onNew), card("open", t("landing.open"), t("landing.openDesc"), h.onOpen), card("studio", t("landing.studio"), t("landing.studioDesc"), h.onStudio)),
      el(
        "section",
        { class: "landing-recent" },
        el("h2", {}, t("landing.recent")),
        recent.length ? el("ul", { class: "landing-recent-list" }, ...recent.map((r) => recentRow(r, h))) : el("p", { class: "landing-empty" }, t("landing.empty")),
      ),
    ),
  );
}
