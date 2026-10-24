// "Werkzeuge" modals: volume label, layer break inspector entry, layout files, drive info,
// auto speed table.
import { open } from "@tauri-apps/plugin-dialog";
import * as api from "./api";
import { openLayerBreak } from "./layerbreak";
import { el, fmtSectors, modal } from "./ui";

const IMAGE_FILTER = [{ name: "Disc-Images", extensions: ["iso"] }];

async function pickImage(): Promise<string | null> {
  const p = await open({ multiple: false, filters: IMAGE_FILTER });
  return typeof p === "string" ? p : null;
}

/** A path row: read-only text with a "…" button. */
function pathRow(initial: string, pick: () => Promise<string | null>, onChange: (p: string) => void) {
  const input = el("input", { type: "text", readOnly: true, value: initial, placeholder: "Datei wählen …" });
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
    el("p", { class: "hint" }, "Ändert die Bezeichnung direkt im Image (ISO 9660, Joliet und UDF; Prüfsummen werden neu berechnet). Erlaubt sind A–Z, 0–9 und _."),
    el(
      "label",
      { class: "opt-row" },
      el("span", { class: "opt-label" }, "Image"),
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
    el("label", { class: "opt-row" }, el("span", { class: "opt-label" }, "Volume-Label"), label),
    msg,
  );
  modal("Volume-Label ändern", body, [
    { label: "Abbrechen" },
    {
      label: "Ändern",
      primary: true,
      onClick: async () => {
        if (!iso) {
          msg.textContent = "Bitte zuerst ein Image wählen.";
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
  const p = await open({ multiple: false, filters: IMAGE_FILTER, title: "DVD-Video-Image (.iso) wählen – oder „Abbrechen“ und Ordner wählen" });
  if (typeof p === "string") return void openLayerBreak(p);
  const dir = await open({ directory: true, title: "Ordner mit VIDEO_TS wählen" });
  if (typeof dir === "string") void openLayerBreak(dir);
}

export function openSidecarTool() {
  let image = "";
  let layerBreak: number | null = null;
  const lb = el("input", { type: "number", min: "1", placeholder: "Sektoren auf Schicht 0" });
  lb.addEventListener("change", () => (layerBreak = lb.value ? Number(lb.value) : null));
  const msg = el("p", { class: "hint" });
  const write = async (kind: "dvd" | "cue" | "mds") => {
    if (!image) return void (msg.textContent = "Bitte zuerst ein Image wählen.");
    try {
      msg.textContent = `Geschrieben: ${await api.sidecarWrite(kind, image, layerBreak)}`;
    } catch (e) {
      msg.textContent = String(e);
    }
  };
  const body = el(
    "div",
    { class: "opt-group" },
    el("p", { class: "hint" }, "Schreibt eine Layout-Datei neben das Image: .dvd (nur Dual Layer), .cue (eine Datenspur) oder .mds (Alcohol/Daemon Tools – Kompatibilität nicht geprüft)."),
    el("label", { class: "opt-row" }, el("span", { class: "opt-label" }, "Image"), pathRow("", pickImage, (p) => (image = p))),
    el("label", { class: "opt-row" }, el("span", { class: "opt-label" }, "Layerwechsel"), lb, el("small", { class: "opt-hint" }, "Leer lassen bei einer Schicht.")),
    el(
      "div",
      { class: "opt-input" },
      ...(["dvd", "cue", "mds"] as const).map((k) => el("button", { class: "btn", onclick: () => void write(k) }, `.${k} schreiben`)),
    ),
    msg,
  );
  modal("Layout-Dateien schreiben", body);
}

export function renderDriveInfo(v: api.DriveView, into: HTMLElement) {
  const rows: [string, string][] = [];
  const m = v.media;
  if (m) {
    rows.push(["Disc", m.present ? `${m.physical} (${m.blank ? "leer" : "beschrieben"}${m.writable ? "" : ", nicht beschreibbar"})` : "keine Disc"]);
    if (m.present) rows.push(["Kapazität", `${fmtSectors(m.total_sectors)}, frei ${fmtSectors(m.free_sectors)}`]);
  } else if (v.mediaError) rows.push(["Disc", v.mediaError]);
  const d = v.details;
  if (d) {
    rows.push(["Laufwerk", `${d.vendor} ${d.model} ${d.firmware}`.trim()]);
    rows.push(["Aktuelles Profil", `${d.current_profile_name || "—"}${d.ready ? "" : " (nicht bereit)"}`]);
    if (d.book_type) rows.push(["Book-Typ", d.book_type]);
    if (d.media_id) rows.push(["Media-ID", d.media_id]);
    if (d.disc_status) rows.push(["Disc-Status", d.disc_status]);
    if (d.layer_break) rows.push(["Layerwechsel", `Sektor ${d.layer_break.toLocaleString("de-DE")}`]);
    if (d.write_speeds_kbps.length) rows.push(["Schreibgeschwindigkeiten", d.write_speeds_kbps.map((k) => `${k} kB/s`).join(", ")]);
    rows.push(["Unterstützte Profile", d.profiles.map((p) => p.name).join(", ")]);
    d.notes.forEach((n) => rows.push(["Hinweis", n]));
    rows.push(["Book-Typ ändern", "nicht unterstützt (herstellerspezifisch)"]);
  } else if (v.detailsError) rows.push(["MMC-Abfrage", v.detailsError]);
  into.replaceChildren(
    el("table", { class: "studio-table" }, el("tbody", {}, ...rows.map(([k, val]) => el("tr", {}, el("th", {}, k), el("td", {}, val))))),
  );
}

export async function openDriveInfo(driveId: string | null) {
  const body = el("div", {}, el("p", { class: "hint" }, "Wird abgefragt …"));
  modal("Laufwerks- und Disc-Info", body, undefined, true);
  if (!driveId) return void (body.textContent = "Kein Laufwerk gewählt.");
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
      el("p", { class: "hint" }, "Greift bei Geschwindigkeit „Auto“: erste passende Zeile gewinnt (Media-ID enthält den Text, Laufwerk enthält den Text oder ist leer). Die Geschwindigkeit wird auf die nächste unterstützte gerundet."),
      ...rules.map((r, i) =>
        el(
          "div",
          { class: "speed-rule" },
          el("input", { type: "text", value: r.media_id, placeholder: "Media-ID, z. B. CMC MAG", onchange: (e: Event) => (r.media_id = (e.target as HTMLInputElement).value) }),
          el("input", { type: "text", value: r.drive, placeholder: "Laufwerk (leer = alle)", onchange: (e: Event) => (r.drive = (e.target as HTMLInputElement).value) }),
          el("input", { type: "text", value: r.speed, class: "short", placeholder: "max / 8", onchange: (e: Event) => (r.speed = (e.target as HTMLInputElement).value) }),
          el("button", { class: "btn icon", title: "Zeile entfernen", onclick: () => (rules.splice(i, 1), render()) }, "✕"),
        ),
      ),
      el("button", { class: "btn", onclick: () => (rules.push({ media_id: "", drive: "", speed: "max" }), render()) }, "＋ Regel"),
    );
  };
  render();
  modal("Auto-Geschwindigkeit", list, [
    { label: "Abbrechen" },
    {
      label: "Speichern",
      primary: true,
      onClick: () => {
        settings.auto_speed = rules.filter((r) => r.media_id.trim());
        save();
      },
    },
  ]);
}
