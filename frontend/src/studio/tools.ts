// "Werkzeuge" modals: volume label, layer break inspector entry, layout files, drive info,
// auto speed table.
import { open } from "@tauri-apps/plugin-dialog";
import * as api from "./api";
import { openLayerBreak } from "./layerbreak";
import { el, fmtSectors, modal } from "./ui";
import { fmtNumber, t } from "../i18n";

const IMAGE_FILTER = [{ name: t("studio.tools.discImages"), extensions: ["iso"] }];

async function pickImage(): Promise<string | null> {
  const p = await open({ multiple: false, filters: IMAGE_FILTER });
  return typeof p === "string" ? p : null;
}

/** A path row: read-only text with a "…" button. */
function pathRow(initial: string, pick: () => Promise<string | null>, onChange: (p: string) => void) {
  const input = el("input", { type: "text", readOnly: true, value: initial, placeholder: t("studio.tools.dateiWaehlen") });
  const row = el(
    "span",
    { class: "opt-input wide" },
    input,
    el(
      "button",
      {
        class: "btn icon",
        onclick: async () => {
          const p = await pick();
          if (p) {
            input.value = p;
            onChange(p);
          }
        },
      },
      "…",
    ),
  );
  return row;
}

export function openLabelTool() {
  let iso = "";
  const label = el("input", { type: "text", maxLength: 32, spellcheck: false });
  const msg = el("p", { class: "hint" });
  const body = el(
    "div",
    { class: "opt-group" },
    el("p", { class: "hint" }, t("studio.tools.aendertDieBezeichnungDirekt")),
    el(
      "label",
      { class: "opt-row" },
      el("span", { class: "opt-label" }, t("studio.tools.image")),
      pathRow(
        "",
        pickImage,
        async (p) => {
          iso = p;
          try {
            label.value = await api.labelRead(p);
            msg.textContent = "";
          } catch (e) {
            msg.textContent = String(e);
          }
        },
      ),
    ),
    el("label", { class: "opt-row" }, el("span", { class: "opt-label" }, t("studio.tools.volumeLabel")), label),
    msg,
  );
  modal(t("studio.tools.volumeLabelAendern"), body, [
    { label: t("studio.tools.abbrechen") },
    {
      label: t("studio.tools.aendern"),
      primary: true,
      onClick: async () => {
        if (!iso) {
          msg.textContent = t("studio.tools.bitteZuerstEinImage");
          return false;
        }
        try {
          const [old, now] = await api.labelPatch(iso, label.value);
          msg.textContent = `${old} → ${now}`;
        } catch (e) {
          msg.textContent = String(e);
          return false;
        }
      },
    },
  ]);
}

export async function openIfoTool() {
  const p = await open({ multiple: false, filters: IMAGE_FILTER, title: t("studio.tools.dvdVideoImageIso") });
  if (typeof p === "string") return void openLayerBreak(p);
  const dir = await open({ directory: true, title: t("studio.tools.ordnerMitVideoTs") });
  if (typeof dir === "string") void openLayerBreak(dir);
}

export function openSidecarTool() {
  let image = "";
  let layerBreak: number | null = null;
  const lb = el("input", { type: "number", min: "1", placeholder: t("studio.tools.sektorenAufSchicht0") });
  lb.addEventListener("change", () => (layerBreak = lb.value ? Number(lb.value) : null));
  const msg = el("p", { class: "hint" });
  const write = async (kind: "dvd" | "cue" | "mds") => {
    if (!image) return void (msg.textContent = t("studio.tools.bitteZuerstEinImage"));
    try {
      msg.textContent = t("studio.tools.geschrieben", { api: await api.sidecarWrite(kind, image, layerBreak) });
    } catch (e) {
      msg.textContent = String(e);
    }
  };
  const body = el(
    "div",
    { class: "opt-group" },
    el("p", { class: "hint" }, t("studio.tools.schreibtEineLayoutDatei")),
    el("label", { class: "opt-row" }, el("span", { class: "opt-label" }, t("studio.tools.image")), pathRow("", pickImage, (p) => (image = p))),
    el("label", { class: "opt-row" }, el("span", { class: "opt-label" }, t("studio.tools.layerwechsel")), lb, el("small", { class: "opt-hint" }, t("studio.tools.leerLassenBeiEiner"))),
    el(
      "div",
      { class: "opt-input" },
      ...(["dvd", "cue", "mds"] as const).map((k) => el("button", { class: "btn", onclick: () => void write(k) }, t("studio.tools.writeExt", { k }))),
    ),
    msg,
  );
  modal(t("studio.tools.layoutDateienSchreiben"), body);
}

export function renderDriveInfo(v: api.DriveView, into: HTMLElement) {
  const rows: [string, string][] = [];
  const m = v.media;
  if (m) {
    rows.push([t("studio.tools.disc"), m.present ? `${m.physical} (${m.blank ? t("common.blank") : t("common.written")}${m.writable ? "" : ", " + t("common.notWritable")})` : t("studio.tools.keineDisc")]);
    if (m.present) rows.push([t("studio.tools.capacity"), t("studio.tools.capacityValue", { total: fmtSectors(m.total_sectors), free: fmtSectors(m.free_sectors) })]);
  } else if (v.mediaError) rows.push([t("studio.tools.disc"), v.mediaError]);
  const d = v.details;
  if (d) {
    rows.push([t("studio.tools.drive"), `${d.vendor} ${d.model} ${d.firmware}`.trim()]);
    rows.push([t("studio.tools.currentProfile"), `${d.current_profile_name || "—"}${d.ready ? "" : ` (${t("common.notReady")})`}`]);
    if (d.book_type) rows.push([t("studio.tools.bookTyp"), d.book_type]);
    if (d.media_id) rows.push([t("studio.tools.mediaId"), d.media_id]);
    if (d.disc_status) rows.push([t("studio.tools.discStatus"), d.disc_status]);
    if (d.layer_break) rows.push([t("studio.tools.layerBreak"), t("studio.tools.sektor", { d: fmtNumber(d.layer_break) })]);
    if (d.write_speeds_kbps.length) rows.push([t("studio.tools.schreibgeschwindigkeiten"), d.write_speeds_kbps.map((k) => `${k} kB/s`).join(", ")]);
    rows.push([t("studio.tools.unterstuetzteProfile"), d.profiles.map((p) => p.name).join(", ")]);
    d.notes.forEach((n) => rows.push([t("studio.tools.hinweis"), n]));
    rows.push([t("studio.tools.changeBookType"), t("studio.tools.nichtUnterstuetztHerstellerspezifisch")]);
  } else if (v.detailsError) rows.push([t("studio.tools.mmcAbfrage"), v.detailsError]);
  into.replaceChildren(
    el("table", { class: "studio-table" }, el("tbody", {}, ...rows.map(([k, val]) => el("tr", {}, el("th", {}, k), el("td", {}, val))))),
  );
}

export async function openDriveInfo(driveId: string | null) {
  const body = el("div", {}, el("p", { class: "hint" }, t("studio.tools.wirdAbgefragt")));
  modal(t("studio.tools.laufwerksUndDiscInfo"), body, undefined, true);
  if (!driveId) return void (body.textContent = t("studio.tools.keinLaufwerkGewaehlt"));
  try {
    renderDriveInfo(await api.driveInfo(driveId), body);
  } catch (e) {
    body.textContent = String(e);
  }
}

/** Auto speed rules: media ID (part), drive (part, empty = any) → speed; first match wins. */
export function openAutoSpeed(settings: api.StudioSettings, save: () => void) {
  const rules = settings.auto_speed.map((r) => ({ ...r }));
  const list = el("div", { class: "opt-group" });
  const render = () => {
    list.replaceChildren(
      el("p", { class: "hint" }, t("studio.tools.greiftBeiGeschwindigkeitAuto")),
      ...rules.map((r, i) =>
        el(
          "div",
          { class: "speed-rule" },
          el("input", { type: "text", value: r.media_id, placeholder: t("studio.tools.mediaIdZB"), onchange: (e: Event) => (r.media_id = (e.target as HTMLInputElement).value) }),
          el("input", { type: "text", value: r.drive, placeholder: t("studio.tools.laufwerkLeerAlle"), onchange: (e: Event) => (r.drive = (e.target as HTMLInputElement).value) }),
          el("input", { type: "text", value: r.speed, class: "short", placeholder: "max / 8", onchange: (e: Event) => (r.speed = (e.target as HTMLInputElement).value) }),
          el("button", { class: "btn icon", title: t("studio.tools.zeileEntfernen"), onclick: () => (rules.splice(i, 1), render()) }, "✕"),
        ),
      ),
      el("button", { class: "btn", onclick: () => (rules.push({ media_id: "", drive: "", speed: "max" }), render()) }, t("studio.tools.regel")),
    );
  };
  render();
  modal(t("studio.tools.autoGeschwindigkeit"), list, [
    { label: t("studio.tools.abbrechen") },
    {
      label: t("studio.tools.speichern"),
      primary: true,
      onClick: () => {
        settings.auto_speed = rules.filter((r) => r.media_id.trim());
        save();
      },
    },
  ]);
}
