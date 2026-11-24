// Settings UI. App-level options live in the split "General settings" dialog (navigation tree on
// the left, page on the right, modelled on ConvertXToDVD's options dialog); options that describe
// the disc being made live in the main window's "Project settings" tab, are applied on every
// change and are saved with the project (PROJECT_SETTING_KEYS).
import { open } from "@tauri-apps/plugin-dialog";
import { defaultSettings, saveSettings, type AppSettings, type SettingsInfo } from "./api";
import { DEFAULT_LABEL, MAX_LABEL, sanitizeLabel } from "./label";
import { t } from "./i18n";

type Child = Node | string | null | undefined | false;

function el<K extends keyof HTMLElementTagNameMap>(
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

/** Language choices for the audio/subtitle defaults (names in the UI language). */
const languages = (): [string, string][] => [
  ["", t("settings.noLanguagePreference")],
  ...["de", "en", "fr", "es", "it", "nl", "pt", "pl", "cs", "hu", "ru", "uk", "tr", "el", "sv", "da", "no", "fi", "ja", "zh", "ko", "ar"].map((c): [string, string] => [c, t(`lang.${c}`)]),
];

/** `m:ss` / `h:mm:ss` for a number of seconds. */
function fmtDuration(secs: number): string {
  const h = Math.floor(secs / 3600);
  const m = Math.floor((secs % 3600) / 60);
  const s = secs % 60;
  return h > 0 ? `${h}:${String(m).padStart(2, "0")}:${String(s).padStart(2, "0")}` : `${m}:${String(s).padStart(2, "0")}`;
}

function parseDuration(text: string): number | null {
  const parts = text.trim().split(":");
  if (parts.length === 0 || parts.length > 3 || parts.some((p) => !/^\d+$/.test(p))) return null;
  return parts.reduce((acc, p) => acc * 60 + Number(p), 0);
}

/** Settings that describe the disc being made: edited in the Project settings tab and saved in the project file. */
export const PROJECT_SETTING_KEYS = [
  "tv_standard",
  "dvd_resolution",
  "dvd_video",
  "bd_resolution",
  "bd_single_video",
  "menu_animations",
  "encoding_profile",
  "target_size_mb",
  "aspect_ratio_policy",
  "scale_mode",
  "scale_filter",
  "deinterlace",
  "auto_start_playback",
  "play_sequentially",
  "loop_playback",
  "return_to_title_menu_after_title",
  "skip_root_menu",
  "no_title_menu_for_single_title",
  "text_overflow_action",
  "menu_audio",
  "menu_audio_file",
  "auto_chapter_enabled",
  "auto_chapter_interval_seconds",
  "auto_chapter_min_video_length_seconds",
  "chapter_naming_template",
  "default_subtitle_language",
  "force_first_subtitle",
  "default_audio_language",
  "audio_mode",
  "audio_bitrate_kbps",
  "loudnorm",
  "default_volume_label",
] as const satisfies readonly (keyof AppSettings)[];

type BoolKey = { [K in keyof AppSettings]: AppSettings[K] extends boolean ? K : never }[keyof AppSettings];
type StrKey = { [K in keyof AppSettings]: AppSettings[K] extends string ? K : never }[keyof AppSettings];
type NumKey = { [K in keyof AppSettings]: AppSettings[K] extends number ? K : never }[keyof AppSettings];

/**
 * Field builders bound to `draft` (mutated in place). `changed(rerender)` runs after every edit;
 * `rerender` is false for typing in a text field (the form must keep its focus).
 */
function formKit(draft: AppSettings, error: HTMLElement, changed: (rerender: boolean) => void) {
  const check = (key: BoolKey, label: string, hint?: string, disabled = false) =>
    el(
      "label",
      { class: `opt-check${disabled ? " disabled" : ""}` },
      el("input", {
        type: "checkbox",
        checked: draft[key],
        disabled,
        onchange: (e: Event) => {
          (draft[key] as boolean) = (e.target as HTMLInputElement).checked;
          changed(true);
        },
      }),
      el("span", {}, label, hint ? el("small", {}, hint) : null),
    );

  const select = (key: StrKey, label: string, options: [string, string][], hint?: string) =>
    el(
      "label",
      { class: "opt-row" },
      el("span", { class: "opt-label" }, label),
      el(
        "select",
        {
          onchange: (e: Event) => {
            (draft[key] as string) = (e.target as HTMLSelectElement).value;
            changed(true);
          },
        },
        ...options.map(([value, text]) => el("option", { value, selected: draft[key] === value }, text)),
        // A stored value that is not in the list stays visible.
        options.some(([v]) => v === draft[key]) ? null : el("option", { value: draft[key], selected: true }, draft[key]),
      ),
      hint ? el("small", { class: "opt-hint" }, hint) : null,
    );

  const numSelect = (key: NumKey, label: string, options: [number, string][], hint?: string) =>
    el(
      "label",
      { class: "opt-row" },
      el("span", { class: "opt-label" }, label),
      el(
        "select",
        {
          onchange: (e: Event) => {
            (draft[key] as number) = Number((e.target as HTMLSelectElement).value);
            changed(true);
          },
        },
        ...options.map(([value, text]) => el("option", { value: String(value), selected: draft[key] === value }, text)),
        // A stored value that is not in the list stays visible.
        options.some(([v]) => v === draft[key]) ? null : el("option", { value: String(draft[key]), selected: true }, `${draft[key]} kbps`),
      ),
      hint ? el("small", { class: "opt-hint" }, hint) : null,
    );

  const number = (key: NumKey, label: string, min: number, max: number, unit: string, hint?: string) =>
    el(
      "label",
      { class: "opt-row" },
      el("span", { class: "opt-label" }, label),
      el(
        "span",
        { class: "opt-input" },
        el("input", {
          type: "number",
          min: String(min),
          max: String(max),
          step: "1",
          value: String(draft[key]),
          onchange: (e: Event) => {
            const input = e.target as HTMLInputElement;
            const v = Math.round(Number(input.value));
            if (!Number.isFinite(v) || v < min || v > max) {
              input.value = String(draft[key]);
              error.textContent = t("settings.allowedRange", { label, min, max, unit });
              return;
            }
            error.textContent = "";
            (draft[key] as number) = v;
            changed(true);
          },
        }),
        el("span", { class: "unit" }, unit),
      ),
      hint ? el("small", { class: "opt-hint" }, hint) : null,
    );

  const duration = (key: NumKey, label: string, min: number, max: number, disabled: boolean, hint?: string) =>
    el(
      "label",
      { class: `opt-row${disabled ? " disabled" : ""}` },
      el("span", { class: "opt-label" }, label),
      el(
        "span",
        { class: "opt-input" },
        el("input", {
          type: "text",
          class: "mono",
          value: fmtDuration(draft[key] as number),
          disabled,
          onchange: (e: Event) => {
            const input = e.target as HTMLInputElement;
            const v = parseDuration(input.value);
            if (v === null || v < min || v > max) {
              input.value = fmtDuration(draft[key] as number);
              error.textContent = t("settings.formatMSsZwischen", { label, fmtDuration: fmtDuration(min), fmtDuration2: fmtDuration(max) });
              return;
            }
            error.textContent = "";
            (draft[key] as number) = v;
            input.value = fmtDuration(v);
            changed(true);
          },
        }),
        el("span", { class: "unit" }, t("settings.min")),
      ),
      hint ? el("small", { class: "opt-hint" }, hint) : null,
    );

  /** Text field with a "…" picker button; typing applies on change (blur/Enter). */
  const pathField = (key: StrKey, label: string, placeholder: string, pickTitle: string, pick: () => Promise<unknown>) =>
    el(
      "div",
      { class: "opt-row" },
      el("span", { class: "opt-label" }, label),
      el(
        "span",
        { class: "opt-input wide" },
        el("input", {
          type: "text",
          value: draft[key],
          placeholder,
          spellcheck: false,
          onchange: (e: Event) => {
            (draft[key] as string) = (e.target as HTMLInputElement).value;
            changed(false);
          },
        }),
        el(
          "button",
          {
            class: "btn icon",
            title: pickTitle,
            onclick: async () => {
              const picked = await pick();
              if (typeof picked === "string") {
                (draft[key] as string) = picked;
                changed(true);
              }
            },
          },
          "…",
        ),
      ),
    );

  const group = (title: string, ...items: Child[]) => el("fieldset", { class: "opt-group" }, el("legend", {}, title), ...items);
  const note = (text: string) => el("p", { class: "opt-note" }, text);
  return { check, select, numSelect, number, duration, pathField, group, note };
}

type Kit = ReturnType<typeof formKit>;

// ---- project settings ---------------------------------------------------------------------------

/** Project sections: heading plus groups. */
function projectSections(k: Kit, draft: AppSettings, changed: (rerender: boolean) => void): [string, HTMLElement[]][] {
  const { check, select, numSelect, number, duration, pathField, group, note } = k;
  const chapterOff = !draft.auto_chapter_enabled;
  const example = draft.chapter_naming_template.includes("<c>")
    ? t("settings.beispiel", { draft: draft.chapter_naming_template.replace("<c>", "3") })
    : t("settings.dieVorlageMussC");
  return [
    [
      t("settings.ausgabe"),
      [
        group(
          t("settings.enkodierung"),
          select(
            "encoding_profile",
            t("settings.profil"),
            [
              ["Auto", t("settings.automatischBitratePassendZur")],
              ["SP_Short", t("settings.spKurzeLaufzeitHoechste")],
              ["MP_Medium", t("settings.mpMittlereLaufzeitBis")],
              ["LP_Long", t("settings.lpLangeLaufzeitBis")],
            ],
            t("settings.automatischVerteiltDenPlatz"),
          ),
          number("target_size_mb", t("settings.benutzerdefinierteZielgroesse"), 100, 20000, "MB", t("settings.wirdVerwendetWennUnten")),
        ),
        group(
          t("settings.bluRay"),
          select("bd_resolution", t("settings.bluRayAufloesung"), [
            ["1080p", "1080p (1920×1080)"],
            ["720p", "720p (1280×720)"],
          ]),
          note(t("settings.bluRayTitelSind")),
          check("bd_single_video", t("settings.nurEinVideoPro"), t("settings.mehrereVideosWerdenSonst")),
        ),
        group(
          t("settings.projekt"),
          el(
            "label",
            { class: "opt-row" },
            el("span", { class: "opt-label" }, t("settings.standardProjektVolumeName")),
            el(
              "span",
              { class: "opt-input" },
              el("input", {
                type: "text",
                class: "mono",
                value: draft.default_volume_label,
                placeholder: t("settings.ausMenuetitelErstemVideo"),
                maxlength: String(MAX_LABEL + 8),
                spellcheck: false,
                oninput: (e: Event) => {
                  const input = e.target as HTMLInputElement;
                  const clean = sanitizeLabel(input.value, false);
                  if (clean !== input.value) {
                    input.value = clean;
                    input.setSelectionRange(clean.length, clean.length);
                  }
                },
                onchange: (e: Event) => {
                  const input = e.target as HTMLInputElement;
                  // Empty stays empty (= automatic name); junk becomes DVD_VIDEO.
                  draft.default_volume_label = input.value.trim() ? sanitizeLabel(input.value) : "";
                  input.value = draft.default_volume_label;
                  changed(false);
                },
              }),
            ),
            el("small", { class: "opt-hint" }, t("settings.ordnernameDerDvdUnd", { MAX_LABEL, DEFAULT_LABEL })),
          ),
        ),
      ],
    ],
    [
      t("settings.videobearbeitung"),
      [
        group(
          t("settings.videobearbeitung"),
          select(
            "aspect_ratio_policy",
            t("settings.seitenverhaeltnisDerDvd"),
            [
              ["Auto", t("settings.automatischNachQuelle")],
              ["4:3", t("settings.immer43")],
              ["16:9", t("settings.immer169")],
            ],
            t("settings.abweichendeQuellenBekommenSchwarze"),
          ),
          select(
            "scale_mode",
            t("settings.videoSkaliermethode"),
            [
              ["letterbox", t("settings.letterboxStandard")],
              ["panscan", t("settings.panScanZuschneiden")],
              ["stretch", t("settings.strecken")],
            ],
            t("settings.letterboxDasBildBehaelt"),
          ),
          select(
            "scale_filter",
            t("settings.skalierungsfilter"),
            [
              ["bilinear", t("settings.bilinearSchnell")],
              ["bicubic", t("settings.bikubischStandard")],
              ["lanczos", t("settings.lanczosHoheQualitaet")],
            ],
            t("settings.algorithmusMitDemDas"),
          ),
          select(
            "deinterlace",
            t("settings.videoDeinterlacing"),
            [
              ["auto", t("settings.automatischBwdif")],
              ["yadif", t("settings.immerDeinterlacenYadif")],
              ["off", t("settings.nieDeinterlacen")],
            ],
            t("settings.automatischNurVideosDie"),
          ),
        ),
      ],
    ],
    [
      t("settings.audio"),
      [
        group(
          t("settings.audio"),
          select("default_audio_language", t("settings.bevorzugteSprache"), languages(), t("settings.tonspurenInDieserSprache")),
          select(
            "audio_mode",
            t("settings.audioModus"),
            [
              ["auto", t("settings.automatischAc3Dts")],
              ["ac3", t("settings.immerAc3")],
              ["lpcm", t("settings.lpcmUnkomprimiert")],
            ],
            t("settings.automatischPassendeAc3"),
          ),
          numSelect(
            "audio_bitrate_kbps",
            t("settings.audioBitrate"),
            [[0, t("settings.audioBitrateAuto")], ...[96, 128, 160, 192, 224, 256, 320, 384, 448, 512, 576, 640].map((n): [number, string] => [n, `${n} kbps`])],
            t("settings.audioBitrateHint"),
          ),
          check("loudnorm", t("settings.lautstaerkeNormalisierenEbuR128"), t("settings.gleichtDieLautstaerkeAller")),
        ),
      ],
    ],
    [
      t("settings.untertitel"),
      [
        group(
          t("settings.untertitel"),
          select("default_subtitle_language", t("settings.bevorzugteSprache"), languages(), t("settings.untertitelInDieserSprache")),
          check("force_first_subtitle", t("settings.erstenUntertitelAutomatischEinblenden"), t("settings.sonstSindUntertitelBeim")),
        ),
      ],
    ],
    [
      t("settings.menueOptionen"),
      [
        group(
          t("settings.wiedergabe"),
          check("auto_start_playback", t("settings.wiedergabeAutomatischStarten"), t("settings.dieDvdBeginntBeim")),
          check("play_sequentially", t("settings.videosNacheinanderAbspielen"), t("settings.nachJedemVideoFolgt")),
          check(
            "loop_playback",
            t("settings.endlosschleife"),
            draft.play_sequentially ? t("settings.nachDemLetztenVideo") : t("settings.einVideoWirdWiederholt"),
          ),
          check(
            "return_to_title_menu_after_title",
            t("settings.nachDerWiedergabeZum"),
            draft.loop_playback ? t("settings.ohneWirkungSolangeDie") : t("settings.sonstGehtEsZurueck"),
            draft.loop_playback,
          ),
        ),
        group(
          t("settings.menuestruktur"),
          check("skip_root_menu", t("settings.hauptmenueDesVideosUeberspringen"), t("settings.keinRootMenuePro")),
          check("no_title_menu_for_single_title", t("settings.keinTitelmenueBeiNur"), t("settings.dieDvdStartetDann")),
          check("menu_animations", t("settings.menueAnimationenIntro"), t("settings.spieltBeiMenuesAus")),
        ),
        group(
          t("settings.text"),
          select("text_overflow_action", t("settings.wennEinTextZu"), [
            ["reduce_font_size", t("settings.schriftVerkleinernBis50")],
            ["truncate", t("settings.mitKuerzenSchriftgroesseBeibehalten")],
          ]),
        ),
        group(
          t("settings.menueTonMotionMenu"),
          select(
            "menu_audio",
            t("settings.tonAllerMenueseiten"),
            [
              ["title", t("settings.hauptmenueStummVideomenuesMit")],
              ["silence", t("settings.alleMenuesStumm")],
              ["file", t("settings.hauptmenueMitEigenerAudiodatei")],
            ],
            t("settings.dasMenueEinesVideos"),
          ),
          draft.menu_audio === "file"
            ? pathField("menu_audio_file", t("settings.audiodateiFuerDasHauptmenue"), t("settings.mp3WavFlacAc3"), t("settings.dateiWaehlen"), () =>
                open({
                  title: t("settings.menueAudioWaehlen"),
                  filters: [{ name: t("settings.audio"), extensions: ["mp3", "wav", "flac", "ogg", "m4a", "aac", "ac3", "wma", "opus"] }],
                }),
              )
            : null,
        ),
      ],
    ],
    [
      t("settings.kapitel"),
      [
        group(
          t("settings.automatischeKapitel"),
          check("auto_chapter_enabled", t("settings.kapitelAutomatischErzeugen"), t("settings.nurFuerVideosOhne")),
          duration("auto_chapter_interval_seconds", t("settings.einKapitelAlle"), 30, 3600, chapterOff),
          duration("auto_chapter_min_video_length_seconds", t("settings.nurBeiVideosLaenger"), 0, 24 * 3600, chapterOff, t("settings.kuerzereVideosBekommenNur")),
        ),
        group(
          t("settings.kapitelnamen"),
          el(
            "label",
            { class: "opt-row" },
            el("span", { class: "opt-label" }, t("settings.vorlage")),
            el("input", {
              type: "text",
              value: draft.chapter_naming_template,
              maxLength: 60,
              onchange: (e: Event) => {
                draft.chapter_naming_template = (e.target as HTMLInputElement).value;
                changed(true);
              },
            }),
            el("small", { class: "opt-hint" }, example),
          ),
          note(t("settings.cWirdDurchDie")),
        ),
      ],
    ],
  ];
}

/**
 * The main window's Project settings tab, editing a copy of `current`. Every change is stored right
 * away and handed to `onApplied` (the caller re-renders, which rebuilds this form).
 */
export function renderProjectSettings(current: AppSettings, onApplied: (s: AppSettings) => void | Promise<void>): HTMLElement {
  const draft: AppSettings = structuredClone(current);
  const error = el("div", { class: "settings-error", role: "alert" });
  let saving = Promise.resolve();
  const changed = () => {
    // Serialised so a quick second change never overtakes the first one.
    saving = saving.then(async () => {
      try {
        await onApplied(await saveSettings(draft));
      } catch (e) {
        error.textContent = t("settings.speichernFehlgeschlagen", { e });
      }
    });
  };
  const sections = projectSections(formKit(draft, error, changed), draft, changed);
  return el(
    "div",
    { class: "project-settings" },
    el("p", { class: "hint" }, t("settings.projectSettingsHint")),
    error,
    ...sections.flatMap(([title, groups]) => [el("h3", {}, title), ...groups]),
  );
}

// ---- general settings dialog ---------------------------------------------------------------------

type PageId = "general" | "burn";

const treeNodes = (): { id: PageId; label: string }[] => [
  { id: "general", label: t("settings.allgemein") },
  { id: "burn", label: t("settings.brennen") },
];

/**
 * Opens the settings dialog (app-level options only) on a copy of `current`. `onSaved` receives
 * the stored settings; "Cancel" changes nothing.
 */
export function openSettings(current: AppSettings, info: SettingsInfo, uiLanguages: { code: string; name: string }[], onSaved: (s: AppSettings) => void) {
  const draft: AppSettings = structuredClone(current);
  let page: PageId = "general";
  const previousFocus = document.activeElement as HTMLElement | null;

  const tree = el("nav", { class: "settings-tree", "aria-label": t("settings.einstellungsbereiche") });
  const body = el("div", { class: "settings-page" });
  const error = el("div", { class: "settings-error", role: "alert" });
  const { check, select, number, pathField, group, note } = formKit(draft, error, (rerender) => {
    if (rerender) render();
  });

  const pages: Record<PageId, () => HTMLElement[]> = {
    general: () => [
      group(
        t("settings.language"),
        select("ui_language", t("settings.uiLanguage"), uiLanguages.map((l): [string, string] => [l.code, l.name]), t("settings.uiLanguageHint")),
      ),
      group(
        t("settings.verarbeitung"),
        number(
          "max_concurrent_encodes",
          t("settings.gleichzeitigeEncodings"),
          1,
          64,
          t("settings.prozesse"),
          t("settings.dieserRechnerHatLogische", { cpuCount: info.cpuCount, max: Math.max(1, Math.floor(info.cpuCount / 2)) }),
        ),
        select("process_priority", t("settings.prozessprioritaet"), [
          ["Normal", t("settings.normal")],
          ["BelowNormal", t("settings.niedrigerAlsNormal")],
          ["Idle", t("settings.leerlaufNurWennDer")],
        ]),
      ),
      group(
        t("settings.leistung"),
        check("hw_decode", t("settings.gpuDecodingVerwendenCuda"), t("settings.dieQuellvideosWerdenAuf")),
        select(
          "hw_accel_mode",
          t("settings.hw_accel_mode"),
          [
            ["none", t("settings.hw_accel.none")],
            ["nvidia", t("settings.hw_accel.nvidia")],
            ["intel", t("settings.hw_accel.intel")],
            ["amd", t("settings.hw_accel.amd")],
          ],
          t("settings.hw_accel_mode_hint"),
        ),
        check("fast_encode", t("settings.schnellesEncodingEntwurf"), t("settings.verzichtetAufTrellisQuantisierung")),
      ),
      group(
        t("settings.menueOptionen"),
        select(
          "track_label_mode",
          t("settings.spurnamenInMenues"),
          [
            ["smart", t("settings.intelligentSpracheTitel")],
            ["language", t("settings.nurSprache")],
            ["generic", t("settings.generischTonspur1")],
          ],
          t("settings.intelligentSpurenGleicherSprache"),
        ),
        pathField("template_dir", t("settings.template_dir"), t("settings.template_dir_auto"), t("settings.template_dir_pick"), () =>
          open({ title: t("settings.template_dir_pick"), directory: true }),
        ),
        el("small", { class: "opt-hint" }, t("settings.template_dir_hint")),
      ),
      note(t("settings.projectSettingsMoved")),
    ],
    burn: () => [
      group(
        t("settings.brennen"),
        select(
          "burn_speed",
          t("settings.schreibgeschwindigkeit"),
          [
            ["auto", t("settings.automatischDvd5Bd")],
            ["max", t("settings.maximal")],
            ["16", "16x"],
            ["12", "12x"],
            ["8", "8x"],
            ["6", "6x"],
            ["4", "4x"],
            ["2.4", "2,4x"],
          ],
          t("settings.wirdAufDieNaechste"),
        ),
        check("verify_after_burn", t("settings.discNachDemBrennen"), t("settings.liestDieDiscVollstaendig")),
        check("keep_iso", t("settings.isoDateiBehalten"), t("settings.dasImageDvdUdf")),
      ),
      group(
        t("settings.nachDemBrennen"),
        check("delete_after_burn", t("settings.dvdOrdnerUndArbeitsdateien"), t("settings.loeschtDenErzeugtenDvd")),
        check("quiet_finish", t("settings.explorerNichtOeffnenLaufwerk"), t("settings.ohneDieseOptionWird")),
        check("auto_erase_rewritable", t("settings.wiederbeschreibbareDiscsOhneNachfrage"), t("settings.dvdrwMitAltenDaten")),
      ),
    ],
  };

  function render() {
    tree.replaceChildren(
      ...treeNodes().map((node) =>
        el("div", { class: "tree-node" }, el("button", { class: `tree-item${page === node.id ? " active" : ""}`, onclick: () => go(node.id) }, node.label)),
      ),
    );
    const title = treeNodes().find((n) => n.id === page)?.label ?? "";
    body.replaceChildren(el("h3", {}, title), ...pages[page]());
  }

  function go(id: PageId) {
    page = id;
    error.textContent = "";
    render();
  }

  // ---- dialog ---------------------------------------------------------------------------------
  const okButton = el("button", { class: "btn primary" }, "OK");
  const dialog = el(
    "div",
    { class: "overlay settings-overlay" },
    el(
      "div",
      { class: "settings-dialog", role: "dialog", "aria-modal": "true", "aria-labelledby": "settings-title" },
      el("header", { class: "settings-header" }, el("h2", { id: "settings-title" }, t("settings.allgemeineEinstellungen"))),
      el("div", { class: "settings-split" }, tree, body),
      el(
        "footer",
        { class: "settings-footer" },
        el(
          "button",
          {
            class: "btn",
            onclick: async () => {
              // Defaults for the app-level options only; the project keeps its values.
              const defaults = await defaultSettings();
              const draftRec = draft as unknown as Record<string, unknown>;
              for (const [key, value] of Object.entries(defaults)) {
                if (!(PROJECT_SETTING_KEYS as readonly string[]).includes(key)) draftRec[key] = value;
              }
              error.textContent = "";
              render();
            },
          },
          t("settings.standardwerte"),
        ),
        el("span", { class: "settings-path", title: info.path }, info.path),
        error,
        okButton,
        el("button", { class: "btn", onclick: () => close() }, t("settings.abbrechen")),
      ),
    ),
  );

  okButton.addEventListener("click", async () => {
    okButton.disabled = true;
    try {
      const saved = await saveSettings(draft);
      close();
      onSaved(saved);
    } catch (e) {
      error.textContent = t("settings.speichernFehlgeschlagen", { e });
      okButton.disabled = false;
    }
  });

  const onKey = (e: KeyboardEvent) => {
    if (e.key === "Escape") close();
  };
  function close() {
    document.removeEventListener("keydown", onKey);
    dialog.remove();
    previousFocus?.focus();
  }

  document.addEventListener("keydown", onKey);
  document.body.append(dialog);
  render();
  (tree.querySelector(".tree-item") as HTMLElement | null)?.focus();
}
