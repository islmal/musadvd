// Burn Studio settings dialog: seven tabs bound to `AppSettings.burn_studio`.
// Options the engine does not act on stay visible but disabled, with the reason (honest UI).
import { open } from "@tauri-apps/plugin-dialog";
import { saveSettings } from "../api";
import * as api from "./api";
import { el, modal } from "./ui";
import { t } from "../i18n";

const DIRECT = t("studio.settings.nurMitBackendSpti");
const NOT_YET = t("studio.settings.nochNichtUnterstuetzt");

interface Opt {
  /** Greyed out for good: shown with this reason. */
  off?: string;
  /** Works only with the direct write engine. */
  direct?: boolean;
  hint?: string;
}

type Tab = "general" | "read" | "build" | "write" | "verify" | "graph" | "io";
const TABS: [Tab, string][] = [
  ["general", t("studio.settings.allgemein")],
  ["read", t("studio.settings.lesen")],
  ["build", t("studio.settings.imageBauen")],
  ["write", t("studio.settings.schreiben")],
  ["verify", t("studio.settings.ueberpruefen")],
  ["graph", t("studio.settings.graph")],
  ["io", "E/A"],
];

const SPLIT: [string, string][] = [["none", t("studio.settings.nichtTeilen")], ["2gb", t("studio.settings.2GbTeile")], ["4gb", t("studio.settings.4GbTeileFat32")]];
const LAYOUT: [string, string][] = [["auto", t("studio.settings.automatisch")], ["none", t("studio.settings.keine")], ["dvd", ".dvd"], ["mds", ".mds"], ["cue", ".cue"]];

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
              error.textContent = t("settings.allowedRange", { label, min, max, unit: unit ?? "" }).replace(" .", ".");
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
    group(title, ...([["read", t("studio.settings.lesen")], ["write", t("studio.settings.schreiben")], ["verify", t("studio.settings.ueberpruefen")], ["erase", t("studio.settings.loeschen")]] as const).map(([k, t]) => check(`${path}.${k}`, t)));

  const preview = el("small", { class: "opt-hint" });
  const updatePreview = async () => {
    try {
      preview.textContent = t("studio.settings.beispiel", { api: await api.templatePreview(d.graph.template) });
    } catch {
      preview.textContent = "";
    }
  };

  const pages: Record<Tab, () => HTMLElement[]> = {
    general: () => [
      group(
        t("studio.settings.betrieb"),
        check("general.prevent_standby", t("studio.settings.standbyWaehrendDesVorgangs")),
        pick("general.on_complete", t("studio.settings.wennFertig"), [
          ["nothing", t("studio.settings.nichtsTun")],
          ["shutdown", t("studio.settings.herunterfahren60SFrist")],
          ["standby", t("studio.settings.standby")],
          ["hibernate", t("studio.settings.ruhezustand")],
          ["close", t("studio.settings.fensterSchliessen")],
        ]),
        check("general.md5_calculate", t("studio.settings.md5BerechnenLesenBauen")),
        check("general.md5_compare", t("studio.settings.md5Vergleichen"), { off: NOT_YET }),
      ),
      group(
        t("studio.settings.warnungen"),
        check("general.warnings.overburn", t("studio.settings.overburn"), { off: t("studio.settings.gespeichertDerzeitOhneWirkung") }),
        check("general.warnings.dvd_r_dl_layer_break", t("studio.settings.dvdRDlLayerwechsel"), { off: t("studio.settings.gespeichertDerzeitOhneWirkung") }),
        check("general.warnings.no_ifo_update", t("studio.settings.layerwechselOhneIfoAnpassung"), { off: t("studio.settings.gespeichertDerzeitOhneWirkung") }),
        check("general.warnings.seamless_layer_break", t("studio.settings.layerwechselInNahtloserZelle"), { off: t("studio.settings.gespeichertDerzeitOhneWirkung") }),
      ),
      group(
        t("studio.settings.layerwechselKandidaten"),
        check("general.lb_check_seamless", t("studio.settings.nichtNahtloseZellenBevorzugen")),
        check("general.lb_show_interleaved", t("studio.settings.verschachtelteZellenAnzeigen")),
      ),
    ],
    read: () => [
      group(
        t("studio.settings.image"),
        pick("read.split", t("studio.settings.imageTeilen"), SPLIT),
        pick("read.layout_file", t("studio.settings.layoutDatei"), LAYOUT, { hint: t("studio.settings.autoDvdBeiDual") }),
      ),
      group(
        t("studio.settings.fehlerbehandlung"),
        num("read.sw_retries", t("studio.settings.softwareWiederholungen"), 0, 100, "", {}),
        num("read.hw_retries", t("studio.settings.hardwareWiederholungen1Laufwerk"), -1, 100, "", { off: t("studio.settings.laufwerkseinstellung") + NOT_YET }),
        check("read.ignore_read_errors", t("studio.settings.lesefehlerIgnorieren"), { hint: t("studio.settings.unlesbareSektorenWerdenMit") }),
      ),
      group(
        t("studio.settings.sonstiges"),
        pick("read.priority", t("studio.settings.prioritaet"), [["normal", t("studio.settings.normal")], ["below", t("studio.settings.niedriger")], ["idle", t("studio.settings.leerlauf")]], { off: NOT_YET }),
        check("read.batch_eject_next", t("studio.settings.stapelAuswerfenUndNaechste"), { off: NOT_YET }),
      ),
    ],
    build: () => [
      group(
        t("studio.settings.image"),
        pick("build.fs", t("studio.settings.dateisystem"), [["iso_joliet_udf102", t("studio.settings.iso9660JolietUdf")], ["udf102", t("studio.settings.udf102")], ["udf250", t("studio.settings.udf250")]], {
          hint: t("studio.settings.dvdVideoImmerUdf"),
        }),
        num("build.start_lba", t("studio.settings.ersterDatensektor0Automatisch"), 0, 2_000_000, t("studio.settings.sektoren")),
        num("build.min_sectors", t("studio.settings.mindestgroesse"), 0, 2_000_000, t("studio.settings.sektoren")),
        check("build.dedupe", t("studio.settings.doppelteDateienTeilenSich")),
        pick("build.split", t("studio.settings.imageTeilen"), SPLIT),
        pick("build.layout_file", t("studio.settings.layoutDatei"), LAYOUT),
        pick("build.prompts", t("studio.settings.erkannteDvdBdOrdner"), [["ask", t("studio.settings.nachfragen")], ["always", t("studio.settings.immerAlsDisc")], ["never", t("studio.settings.immerAlsDaten")]]),
      ),
      group(
        t("studio.settings.dateifilter"),
        check("build.filter.skip_hidden", t("studio.settings.versteckteDateienUeberspringen")),
        check("build.filter.skip_system", t("studio.settings.systemdateienUeberspringen")),
        check("build.filter.skip_reparse", t("studio.settings.reparsePunkteVerknuepfungenUeberspringen")),
        check("build.filter.skip_in_use", t("studio.settings.dateienInBenutzungUeberspringen")),
        check("build.log_skipped", t("studio.settings.uebersprungeneDateienProtokollieren")),
      ),
      group(
        t("studio.settings.dvdVideo"),
        check("build.dvd.create_audio_ts", t("studio.settings.leerenAudioTsOrdner")),
        check("build.dvd.fix_vts_sectors", t("studio.settings.titelsetSektorenTtSrpt")),
        check("build.dvd.ifo_bup_ecc_padding", t("studio.settings.ifoBupInGetrennten"), { off: NOT_YET }),
        check("build.dvd.uppercase", t("studio.settings.namenInGrossbuchstaben"), { off: NOT_YET }),
        check("build.dvd.read_dvd_text", t("studio.settings.labelAusDvdText"), { off: NOT_YET }),
      ),
      group(
        t("studio.settings.bluRay"),
        check("build.bd.create_certificate", t("studio.settings.certificateOrdnerAnlegen")),
        check("build.bd.reset_cpi", t("studio.settings.kopierschutzFlagDerM2ts"), { hint: t("studio.settings.setztNurDieTp") }),
        check("build.bd.read_bdmt", t("studio.settings.labelAusBdmtEng")),
      ),
    ],
    write: () => [
      group(
        t("studio.settings.disc"),
        check("write.verify", t("studio.settings.nachDemSchreibenUeberpruefen")),
        num("write.copies", t("studio.settings.kopien"), 1, 99, "", { hint: t("studio.settings.nachJederKopieWird") }),
        check("write.delete_image_after", t("studio.settings.imageNachErfolgreichemBrennen"), { hint: t("studio.settings.betrifftNurImageSchreiben") }),
        check("write.test_mode", t("studio.settings.testmodusNurDvdR"), { direct: true }),
        pick("write.overburn", t("studio.settings.zuGrosseImages"), [["prompt", t("studio.settings.abbrechenFehlermeldung")], ["overburn", t("studio.settings.overburnVersuchen")], ["truncate", t("studio.settings.ablehnen")]], { direct: true }),
      ),
      group(
        t("studio.settings.layerwechsel"),
        pick("write.layer_break", t("studio.settings.layerwechsel"), [["auto", t("studio.settings.automatischAusDemImage")], ["user", t("studio.settings.eigenerWert")]]),
        num("write.user_l0_sectors", t("studio.settings.sektorenAufSchicht0"), 0, 4_000_000, t("studio.settings.sektoren"), {}),
      ),
      group(
        t("studio.settings.schreibparameter"),
        pick("write.mode", t("studio.settings.medium"), [["auto", t("studio.settings.automatisch")], ["dvd", "DVD"], ["bd", t("studio.settings.bluRay")]], { off: NOT_YET }),
        pick("write.write_type", t("studio.settings.schreibtypDvdRRw"), [["dao", t("studio.settings.discAtOnce")], ["incremental", t("studio.settings.inkrementell")]], { direct: true }),
        num("write.link_size", t("studio.settings.linkGroesse0Laufwerk"), 0, 255, "", { direct: true }),
        check("write.opc", t("studio.settings.opcLaserKalibrierung"), { direct: true }),
        check("write.dvdpr_reserve_track", t("studio.settings.dvdRSpurReservieren"), { direct: true }),
        check("write.dvdpr_min_radius", t("studio.settings.dvdRMinimalerRadius"), { off: NOT_YET }),
        check("write.dvdpr_dl_only_l0", t("studio.settings.dvdRDlNur"), { off: NOT_YET }),
        check("write.prefer_no_spare", t("studio.settings.bdReOhneSpare"), { direct: true }),
        check("write.lock_volume", t("studio.settings.volumeSperrenNurSpti"), { direct: true }),
        num("write.sw_retries", t("studio.settings.wiederholungenBeiBehebbarenFehlern"), 0, 100, "", { direct: true }),
        check("write.burn_proof", t("studio.settings.burnProof"), { off: NOT_YET }),
        check("write.fastwrite", t("studio.settings.fastwrite"), { off: NOT_YET }),
        check("write.bdr_verify_not_required", t("studio.settings.bdRVerifizierungNicht"), { off: NOT_YET }),
        check("write.cycle_tray_before_verify", t("studio.settings.schubladeVorDemUeberpruefen"), { off: NOT_YET }),
        check("write.auto_book_type", t("studio.settings.bookTypAutomatischAendern"), { off: t("studio.settings.nichtUnterstuetztHerstellerspezifisch") }),
      ),
    ],
    verify: () => [
      group(
        t("studio.settings.ueberpruefen"),
        check("verify.ignore_miscompares", t("studio.settings.abweichungenMeldenAberAls")),
        num("verify.sw_retries", t("studio.settings.softwareWiederholungen"), 0, 100),
        num("verify.hw_retries", t("studio.settings.hardwareWiederholungen1Laufwerk"), -1, 100, "", { off: t("studio.settings.laufwerkseinstellung") + NOT_YET }),
        check("verify.ignore_non_user_data", t("studio.settings.nichtNutzdatenIgnorieren"), { off: t("studio.settings.betrifftNur2352Byte") + NOT_YET }),
      ),
    ],
    graph: () => [
      group(
        t("studio.settings.messung"),
        check("graph.capture", t("studio.settings.graphDatenAufzeichnenUnd"), { hint: t("studio.settings.dieImgburnPruefsummenzeileAm") }),
        check("graph.capture_cpu", t("studio.settings.cpuLastAufzeichnen")),
        num("graph.sample_ms", t("studio.settings.abtastrate"), 50, 2000, "ms"),
      ),
      group(
        t("studio.settings.dateien"),
        text("graph.template", t("studio.settings.dateiname"), { hint: t("studio.settings.mModellFFirmware") }),
        el("div", {}, preview),
        text("graph.folder", t("studio.settings.ordnerLeerAppdataConvertx"), {}, true),
      ),
    ],
    io: () => [
      group(
        t("studio.settings.schreibBackend"),
        pick("io.backend", t("studio.settings.backend"), [["auto", t("studio.settings.automatischImapiSchreibtMmc")], ["imapi", t("studio.settings.nurImapiKeineMmc")], ["direct", t("studio.settings.sptiDirektEigeneSchreibschleife")]], {
          hint: t("studio.settings.sptiDirektWurdeNie"),
        }),
        pick("io.transport", t("studio.settings.mmcTransport"), [["imapi_ex", t("studio.settings.imapiOhneAdministratorrechte")], ["spti", t("studio.settings.sptiBenoetigtAdministratorrechte")]]),
        num("io.ring_mib", t("studio.settings.ringpuffer"), 20, 512, t("studio.settings.mib")),
        num("io.recovery_main_low", t("studio.settings.bufferRecoveryPauseUnter"), 0, 100, "%"),
        num("io.recovery_main_high", t("studio.settings.bufferRecoveryWeiterAb"), 0, 100, "%"),
        num("io.recovery_device_low", t("studio.settings.laufwerkspufferPauseUnter0"), 0, 100, "%", { direct: true }),
        pick("io.transfer_kib", t("studio.settings.transfergroesse"), [["0", t("studio.settings.automatisch")], ["32", t("studio.settings.32Kib")], ["64", t("studio.settings.64Kib")]], { direct: true }),
        check("io.buffered_io_read", t("studio.settings.gepufferteEABeim"), { off: NOT_YET }),
        check("io.buffered_io_write", t("studio.settings.gepufferteEABeim2"), { off: NOT_YET }),
        pick("io.device_sort", t("studio.settings.laufwerkeSortierenNach"), [["letter", t("studio.settings.laufwerksbuchstabe")], ["name", t("studio.settings.name")]]),
      ),
      perOp("io.lock_tray", t("studio.settings.schubladeSperrenWaehrend")),
      perOp("io.eject_after", t("studio.settings.auswerfenNach")),
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
    t("studio.settings.brennstudioEinstellungen"),
    wrap,
    [
      { label: t("studio.settings.abbrechen") },
      {
        label: t("studio.settings.speichern"),
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
