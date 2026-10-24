// Burn Studio settings dialog: seven tabs bound to `AppSettings.burn_studio`.
// Options the engine does not act on stay visible but disabled, with the reason (honest UI).
import { open } from "@tauri-apps/plugin-dialog";
import { saveSettings } from "../api";
import * as api from "./api";
import { el, modal } from "./ui";

const DIRECT = "nur mit Backend „SPTI Direkt“";
const NOT_YET = "noch nicht unterstützt";

interface Opt {
  /** Greyed out for good: shown with this reason. */
  off?: string;
  /** Works only with the direct write engine. */
  direct?: boolean;
  hint?: string;
}

type Tab = "general" | "read" | "build" | "write" | "verify" | "graph" | "io";
const TABS: [Tab, string][] = [
  ["general", "Allgemein"],
  ["read", "Lesen"],
  ["build", "Image bauen"],
  ["write", "Schreiben"],
  ["verify", "Überprüfen"],
  ["graph", "Graph"],
  ["io", "E/A"],
];

const SPLIT: [string, string][] = [["none", "Nicht teilen"], ["2gb", "2 GB-Teile"], ["4gb", "4 GB-Teile (FAT32)"]];
const LAYOUT: [string, string][] = [["auto", "Automatisch"], ["none", "Keine"], ["dvd", ".dvd"], ["mds", ".mds"], ["cue", ".cue"]];

function walk(obj: unknown, path: string): { parent: Record<string, unknown>; key: string } {
  const keys = path.split(".");
  let o = obj as Record<string, unknown>;
  for (const k of keys.slice(0, -1)) o = o[k] as Record<string, unknown>;
  return { parent: o, key: keys[keys.length - 1] };
}

export function openStudioSettings(current: api.FullSettings, onSaved: (s: api.FullSettings) => void) {
  const full: api.FullSettings = structuredClone(current);
  const d = full.burn_studio;
  let tab: Tab = "general";
  const body = el("div");
  const error = el("div", { class: "settings-error", role: "alert" });
  const direct = () => d.io.backend === "direct";

  const get = <T,>(path: string) => {
    const { parent, key } = walk(d, path);
    return parent[key] as T;
  };
  const set = (path: string, v: unknown) => {
    const { parent, key } = walk(d, path);
    parent[key] = v;
  };
  const disabled = (o: Opt) => !!o.off || (!!o.direct && !direct());
  const hintOf = (o: Opt) => [o.hint, o.off, o.direct && !direct() ? DIRECT : ""].filter(Boolean).join(" – ");

  const check = (path: string, label: string, o: Opt = {}) =>
    el(
      "label",
      { class: `opt-check${disabled(o) ? " disabled" : ""}` },
      el("input", { type: "checkbox", checked: get<boolean>(path), disabled: disabled(o), onchange: (e: Event) => set(path, (e.target as HTMLInputElement).checked) }),
      el("span", {}, label, hintOf(o) ? el("small", {}, hintOf(o)) : null),
    );

  const num = (path: string, label: string, min: number, max: number, unit = "", o: Opt = {}) =>
    el(
      "label",
      { class: `opt-row${disabled(o) ? " disabled" : ""}` },
      el("span", { class: "opt-label" }, label),
      el(
        "span",
        { class: "opt-input" },
        el("input", {
          type: "number",
          min: String(min),
          max: String(max),
          value: String(get<number>(path)),
          disabled: disabled(o),
          onchange: (e: Event) => {
            const input = e.target as HTMLInputElement;
            const v = Math.round(Number(input.value));
            if (!Number.isFinite(v) || v < min || v > max) {
              input.value = String(get<number>(path));
              error.textContent = `${label}: erlaubt sind ${min} bis ${max}${unit ? " " + unit : ""}.`;
              return;
            }
            error.textContent = "";
            set(path, v);
          },
        }),
        el("span", { class: "unit" }, unit),
      ),
      hintOf(o) ? el("small", { class: "opt-hint" }, hintOf(o)) : null,
    );

  const pick = (path: string, label: string, options: [string, string][], o: Opt = {}) =>
    el(
      "label",
      { class: `opt-row${disabled(o) ? " disabled" : ""}` },
      el("span", { class: "opt-label" }, label),
      el(
        "select",
        { disabled: disabled(o), onchange: (e: Event) => (set(path, (e.target as HTMLSelectElement).value), render()) },
        ...options.map(([v, t]) => el("option", { value: v, selected: String(get<unknown>(path)) === v }, t)),
      ),
      hintOf(o) ? el("small", { class: "opt-hint" }, hintOf(o)) : null,
    );

  const text = (path: string, label: string, o: Opt = {}, browse = false) => {
    const input = el("input", { type: "text", value: get<string>(path), spellcheck: false, disabled: disabled(o), oninput: (e: Event) => set(path, (e.target as HTMLInputElement).value) });
    return el(
      "label",
      { class: "opt-row" },
      el("span", { class: "opt-label" }, label),
      el(
        "span",
        { class: "opt-input wide" },
        input,
        browse
          ? el(
              "button",
              {
                class: "btn icon",
                onclick: async (e: Event) => {
                  e.preventDefault();
                  const p = await open({ directory: true });
                  if (typeof p === "string") {
                    input.value = p;
                    set(path, p);
                  }
                },
              },
              "…",
            )
          : null,
      ),
      hintOf(o) ? el("small", { class: "opt-hint" }, hintOf(o)) : null,
    );
  };

  const group = (title: string, ...rows: HTMLElement[]) => el("fieldset", { class: "opt-group" }, el("legend", {}, title), ...rows);

  const perOp = (path: "io.lock_tray" | "io.eject_after", title: string) =>
    group(title, ...([["read", "Lesen"], ["write", "Schreiben"], ["verify", "Überprüfen"], ["erase", "Löschen"]] as const).map(([k, t]) => check(`${path}.${k}`, t)));

  const preview = el("small", { class: "opt-hint" });
  const updatePreview = async () => {
    try {
      preview.textContent = `Beispiel: ${await api.templatePreview(d.graph.template)}`;
    } catch {
      preview.textContent = "";
    }
  };

  const pages: Record<Tab, () => HTMLElement[]> = {
    general: () => [
      group(
        "Betrieb",
        check("general.prevent_standby", "Standby während des Vorgangs verhindern"),
        pick("general.on_complete", "Wenn fertig", [
          ["nothing", "Nichts tun"],
          ["shutdown", "Herunterfahren (60 s Frist, „shutdown /a“ bricht ab)"],
          ["standby", "Standby"],
          ["hibernate", "Ruhezustand"],
          ["close", "Fenster schließen"],
        ]),
        check("general.md5_calculate", "MD5 berechnen (Lesen, Bauen, Überprüfen)"),
        check("general.md5_compare", "MD5 vergleichen", { off: NOT_YET }),
      ),
      group(
        "Warnungen",
        check("general.warnings.overburn", "Overburn", { off: "gespeichert, derzeit ohne Wirkung" }),
        check("general.warnings.dvd_r_dl_layer_break", "DVD-R DL: Layerwechsel", { off: "gespeichert, derzeit ohne Wirkung" }),
        check("general.warnings.no_ifo_update", "Layerwechsel ohne IFO-Anpassung", { off: "gespeichert, derzeit ohne Wirkung" }),
        check("general.warnings.seamless_layer_break", "Layerwechsel in nahtloser Zelle", { off: "gespeichert, derzeit ohne Wirkung" }),
      ),
      group(
        "Layerwechsel-Kandidaten",
        check("general.lb_check_seamless", "Nicht nahtlose Zellen bevorzugen"),
        check("general.lb_show_interleaved", "Verschachtelte Zellen anzeigen"),
      ),
    ],
    read: () => [
      group(
        "Image",
        pick("read.split", "Image teilen", SPLIT),
        pick("read.layout_file", "Layout-Datei", LAYOUT, { hint: "Auto: .dvd bei Dual Layer, sonst keine." }),
      ),
      group(
        "Fehlerbehandlung",
        num("read.sw_retries", "Software-Wiederholungen", 0, 100, "", {}),
        num("read.hw_retries", "Hardware-Wiederholungen (-1 = Laufwerk)", -1, 100, "", { off: "Laufwerkseinstellung, " + NOT_YET }),
        check("read.ignore_read_errors", "Lesefehler ignorieren", { hint: "Unlesbare Sektoren werden mit Nullen gefüllt und gelistet." }),
      ),
      group(
        "Sonstiges",
        pick("read.priority", "Priorität", [["normal", "Normal"], ["below", "Niedriger"], ["idle", "Leerlauf"]], { off: NOT_YET }),
        check("read.batch_eject_next", "Stapel: auswerfen und nächste Disc", { off: NOT_YET }),
      ),
    ],
    build: () => [
      group(
        "Image",
        pick("build.fs", "Dateisystem", [["iso_joliet_udf102", "ISO 9660 + Joliet + UDF 1.02"], ["udf102", "UDF 1.02"], ["udf250", "UDF 2.50"]], {
          hint: "DVD-Video: immer UDF-1.02-Brücke; Blu-ray: immer UDF 2.50.",
        }),
        num("build.start_lba", "Erster Datensektor (0 = automatisch)", 0, 2_000_000, "Sektoren"),
        num("build.min_sectors", "Mindestgröße", 0, 2_000_000, "Sektoren"),
        check("build.dedupe", "Doppelte Dateien teilen sich einen Bereich"),
        pick("build.split", "Image teilen", SPLIT),
        pick("build.layout_file", "Layout-Datei", LAYOUT),
        pick("build.prompts", "Erkannte DVD/BD-Ordner", [["ask", "Nachfragen"], ["always", "Immer als Disc"], ["never", "Immer als Daten"]]),
      ),
      group(
        "Dateifilter",
        check("build.filter.skip_hidden", "Versteckte Dateien überspringen"),
        check("build.filter.skip_system", "Systemdateien überspringen"),
        check("build.filter.skip_reparse", "Reparse-Punkte (Verknüpfungen) überspringen"),
        check("build.filter.skip_in_use", "Dateien in Benutzung überspringen"),
        check("build.log_skipped", "Übersprungene Dateien protokollieren"),
      ),
      group(
        "DVD-Video",
        check("build.dvd.create_audio_ts", "Leeren AUDIO_TS-Ordner anlegen"),
        check("build.dvd.fix_vts_sectors", "Titelset-Sektoren (TT_SRPT) an das Layout anpassen"),
        check("build.dvd.ifo_bup_ecc_padding", "IFO/BUP in getrennten ECC-Blöcken", { off: NOT_YET }),
        check("build.dvd.uppercase", "Namen in Großbuchstaben", { off: NOT_YET }),
        check("build.dvd.read_dvd_text", "Label aus DVD-Text lesen", { off: NOT_YET }),
      ),
      group(
        "Blu-ray",
        check("build.bd.create_certificate", "CERTIFICATE-Ordner anlegen"),
        check("build.bd.reset_cpi", "Kopierschutz-Flag der M2TS-Pakete zurücksetzen", { hint: "Setzt nur die TP_extra_header-Bits; das Flag in der .clpi bleibt unberührt." }),
        check("build.bd.read_bdmt", "Label aus bdmt_eng.xml lesen"),
      ),
    ],
    write: () => [
      group(
        "Disc",
        check("write.verify", "Nach dem Schreiben überprüfen"),
        num("write.copies", "Kopien", 1, 99, "", { hint: "Nach jeder Kopie wird ausgeworfen und auf eine leere Disc gewartet." }),
        check("write.delete_image_after", "Image nach erfolgreichem Brennen löschen", { hint: "Betrifft nur „Image schreiben“." }),
        check("write.test_mode", "Testmodus (nur DVD-R/-RW, schreibt nichts)", { direct: true }),
        pick("write.overburn", "Zu große Images", [["prompt", "Abbrechen (Fehlermeldung)"], ["overburn", "Overburn versuchen"], ["truncate", "Ablehnen"]], { direct: true }),
      ),
      group(
        "Layerwechsel",
        pick("write.layer_break", "Layerwechsel", [["auto", "Automatisch (aus dem Image)"], ["user", "Eigener Wert"]]),
        num("write.user_l0_sectors", "Sektoren auf Schicht 0", 0, 4_000_000, "Sektoren", {}),
      ),
      group(
        "Schreibparameter",
        pick("write.mode", "Medium", [["auto", "Automatisch"], ["dvd", "DVD"], ["bd", "Blu-ray"]], { off: NOT_YET }),
        pick("write.write_type", "Schreibtyp (DVD-R/-RW)", [["dao", "Disc-at-once"], ["incremental", "Inkrementell"]], { direct: true }),
        num("write.link_size", "Link-Größe (0 = Laufwerk)", 0, 255, "", { direct: true }),
        check("write.opc", "OPC (Laser-Kalibrierung)", { direct: true }),
        check("write.dvdpr_reserve_track", "DVD+R: Spur reservieren", { direct: true }),
        check("write.dvdpr_min_radius", "DVD+R: minimaler Radius", { off: NOT_YET }),
        check("write.dvdpr_dl_only_l0", "DVD+R DL: nur Schicht 0", { off: NOT_YET }),
        check("write.prefer_no_spare", "BD-RE: ohne Spare-Bereiche formatieren", { direct: true }),
        check("write.lock_volume", "Volume sperren (nur SPTI-Transport)", { direct: true }),
        num("write.sw_retries", "Wiederholungen bei behebbaren Fehlern", 0, 100, "", { direct: true }),
        check("write.burn_proof", "BURN-Proof", { off: NOT_YET }),
        check("write.fastwrite", "FastWrite", { off: NOT_YET }),
        check("write.bdr_verify_not_required", "BD-R: Verifizierung nicht erforderlich", { off: NOT_YET }),
        check("write.cycle_tray_before_verify", "Schublade vor dem Überprüfen aus-/einfahren", { off: NOT_YET }),
        check("write.auto_book_type", "Book-Typ automatisch ändern", { off: "nicht unterstützt (herstellerspezifisch)" }),
      ),
    ],
    verify: () => [
      group(
        "Überprüfen",
        check("verify.ignore_miscompares", "Abweichungen melden, aber als bestanden werten"),
        num("verify.sw_retries", "Software-Wiederholungen", 0, 100),
        num("verify.hw_retries", "Hardware-Wiederholungen (-1 = Laufwerk)", -1, 100, "", { off: "Laufwerkseinstellung, " + NOT_YET }),
        check("verify.ignore_non_user_data", "Nicht-Nutzdaten ignorieren", { off: "betrifft nur 2352-Byte-Quellen, " + NOT_YET }),
      ),
    ],
    graph: () => [
      group(
        "Messung",
        check("graph.capture", "Graph-Daten aufzeichnen und als .ibg speichern", { hint: "Die ImgBurn-Prüfsummenzeile am Dateiende wird nicht geschrieben." }),
        check("graph.capture_cpu", "CPU-Last aufzeichnen"),
        num("graph.sample_ms", "Abtastrate", 50, 2000, "ms"),
      ),
      group(
        "Dateien",
        text("graph.template", "Dateiname", { hint: "%M Modell, %F Firmware, %D Datum lang, %d dd-mm-yyyy, %B yyyy-mm-dd, %T HH-MM, %I Media-ID, %S Geschwindigkeit, %V Label" }),
        el("div", {}, preview),
        text("graph.folder", "Ordner (leer = %APPDATA%\\convertx-rust\\graph)", {}, true),
      ),
    ],
    io: () => [
      group(
        "Schreib-Backend",
        pick("io.backend", "Backend", [["auto", "Automatisch (IMAPI schreibt, MMC fragt ab)"], ["imapi", "Nur IMAPI (keine MMC-Abfragen)"], ["direct", "SPTI Direkt (eigene Schreibschleife, ungetestet)"]], {
          hint: "„SPTI Direkt“ wurde nie an echter Hardware erprobt.",
        }),
        pick("io.transport", "MMC-Transport", [["imapi_ex", "IMAPI (ohne Administratorrechte)"], ["spti", "SPTI (benötigt Administratorrechte)"]]),
        num("io.ring_mib", "Ringpuffer", 20, 512, "MiB"),
        num("io.recovery_main_low", "Buffer Recovery: Pause unter", 0, 100, "%"),
        num("io.recovery_main_high", "Buffer Recovery: weiter ab", 0, 100, "%"),
        num("io.recovery_device_low", "Laufwerkspuffer: Pause unter (0 = aus)", 0, 100, "%", { direct: true }),
        pick("io.transfer_kib", "Transfergröße", [["0", "Automatisch"], ["32", "32 KiB"], ["64", "64 KiB"]], { direct: true }),
        check("io.buffered_io_read", "Gepufferte E/A beim Lesen", { off: NOT_YET }),
        check("io.buffered_io_write", "Gepufferte E/A beim Schreiben", { off: NOT_YET }),
        pick("io.device_sort", "Laufwerke sortieren nach", [["letter", "Laufwerksbuchstabe"], ["name", "Name"]]),
      ),
      perOp("io.lock_tray", "Schublade sperren während"),
      perOp("io.eject_after", "Auswerfen nach"),
    ],
  };

  const nav = el("nav", { class: "settings-tree" });
  const render = () => {
    nav.replaceChildren(...TABS.map(([id, label]) => el("button", { class: `tree-item${id === tab ? " active" : ""}`, onclick: () => ((tab = id), render()) }, label)));
    body.replaceChildren(el("h3", {}, TABS.find(([id]) => id === tab)![1]), ...pages[tab]());
    // The selects hand back strings; transfer size is a number in the file.
    if (tab === "graph") void updatePreview();
  };
  render();
  const wrap = el("div", { class: "settings-split studio-settings" }, nav, el("div", { class: "settings-page" }, body, error));
  modal(
    "Brennstudio – Einstellungen",
    wrap,
    [
      { label: "Abbrechen" },
      {
        label: "Speichern",
        primary: true,
        onClick: async () => {
          d.io.transfer_kib = Number(d.io.transfer_kib);
          try {
            onSaved((await saveSettings(full)) as api.FullSettings);
          } catch (e) {
            error.textContent = String(e);
            return false;
          }
        },
      },
    ],
    true,
  );
}
