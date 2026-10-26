// "Allgemeine Einstellungen": split modal with a navigation tree on the left and the settings
// page on the right, modelled on ConvertXToDVD's options dialog.
import { open } from "@tauri-apps/plugin-dialog";
import { defaultSettings, saveSettings, type AppSettings, type SettingsInfo } from "./api";
import { DEFAULT_LABEL, MAX_LABEL, sanitizeLabel } from "./label";

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

const LANGUAGES: [string, string][] = [
  ["", "Keine Vorgabe (Reihenfolge der Quelle)"],
  ["de", "Deutsch"],
  ["en", "Englisch"],
  ["fr", "Französisch"],
  ["es", "Spanisch"],
  ["it", "Italienisch"],
  ["nl", "Niederländisch"],
  ["pt", "Portugiesisch"],
  ["pl", "Polnisch"],
  ["cs", "Tschechisch"],
  ["hu", "Ungarisch"],
  ["ru", "Russisch"],
  ["uk", "Ukrainisch"],
  ["tr", "Türkisch"],
  ["el", "Griechisch"],
  ["sv", "Schwedisch"],
  ["da", "Dänisch"],
  ["no", "Norwegisch"],
  ["fi", "Finnisch"],
  ["ja", "Japanisch"],
  ["zh", "Chinesisch"],
  ["ko", "Koreanisch"],
  ["ar", "Arabisch"],
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

type PageId = "general" | "output" | "tv" | "encoding" | "video" | "menu" | "chapters" | "subtitles" | "audio" | "burn";

const TREE: { id: PageId; label: string; children?: { id: PageId; label: string }[] }[] = [
  { id: "general", label: "Allgemein" },
  {
    id: "output",
    label: "Ausgabe",
    children: [
      { id: "tv", label: "TV Format" },
      { id: "encoding", label: "Enkodierung" },
      { id: "video", label: "Videobearbeitung" },
    ],
  },
  { id: "menu", label: "Menü Optionen" },
  { id: "chapters", label: "Kapitel" },
  { id: "subtitles", label: "Untertitel" },
  { id: "audio", label: "Audio" },
  { id: "burn", label: "Brennen" },
];

/**
 * Opens the settings dialog on a copy of `current`. `onSaved` receives the stored settings;
 * "Abbrechen" changes nothing.
 */
export function openSettings(current: AppSettings, info: SettingsInfo, onSaved: (s: AppSettings) => void) {
  let draft: AppSettings = structuredClone(current);
  let page: PageId = "general";
  const previousFocus = document.activeElement as HTMLElement | null;

  const tree = el("nav", { class: "settings-tree", "aria-label": "Einstellungsbereiche" });
  const body = el("div", { class: "settings-page" });
  const error = el("div", { class: "settings-error", role: "alert" });

  // ---- field helpers (all bound to `draft`) ------------------------------------------------
  type BoolKey = { [K in keyof AppSettings]: AppSettings[K] extends boolean ? K : never }[keyof AppSettings];
  type StrKey = { [K in keyof AppSettings]: AppSettings[K] extends string ? K : never }[keyof AppSettings];
  type NumKey = { [K in keyof AppSettings]: AppSettings[K] extends number ? K : never }[keyof AppSettings];

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
          render();
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
            render();
          },
        },
        ...options.map(([value, text]) => {
          const unsupported = info.unsupported.includes(value);
          return el(
            "option",
            { value, selected: draft[key] === value, disabled: unsupported && draft[key] !== value },
            unsupported ? `${text} (noch nicht unterstützt)` : text,
          );
        }),
        // A stored value that is not in the list stays visible.
        options.some(([v]) => v === draft[key]) ? null : el("option", { value: draft[key], selected: true }, draft[key]),
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
              error.textContent = `${label}: erlaubt sind ${min} bis ${max} ${unit}.`;
              return;
            }
            error.textContent = "";
            (draft[key] as number) = v;
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
              error.textContent = `${label}: Format m:ss, zwischen ${fmtDuration(min)} und ${fmtDuration(max)}.`;
              return;
            }
            error.textContent = "";
            (draft[key] as number) = v;
            input.value = fmtDuration(v);
          },
        }),
        el("span", { class: "unit" }, "Min."),
      ),
      hint ? el("small", { class: "opt-hint" }, hint) : null,
    );

  const group = (title: string, ...items: Child[]) => el("fieldset", { class: "opt-group" }, el("legend", {}, title), ...items);
  const note = (text: string) => el("p", { class: "opt-note" }, text);

  // ---- pages --------------------------------------------------------------------------------
  const pages: Record<PageId, () => HTMLElement[]> = {
    general: () => [
      group(
        "Verarbeitung",
        number(
          "max_concurrent_encodes",
          "Gleichzeitige Encodings",
          1,
          64,
          "Prozesse",
          `Dieser Rechner hat ${info.cpuCount} logische Kerne; Standard ist die Hälfte (${Math.max(1, Math.floor(info.cpuCount / 2))}). Jeder ffmpeg-Prozess nutzt selbst mehrere Kerne – mehr parallele Encodings brauchen vor allem mehr Arbeitsspeicher und Festplattentempo.`,
        ),
        select("process_priority", "Prozesspriorität", [
          ["Normal", "Normal"],
          ["BelowNormal", "Niedriger als normal"],
          ["Idle", "Leerlauf (nur wenn der PC nichts anderes tut)"],
        ]),
      ),
    ],
    output: () => [
      note("Einstellungen für das erzeugte DVD-Video bzw. die Blu-ray. Unterpunkte: TV Format, Enkodierung und Videobearbeitung."),
      group("Aktuell", el("p", {}, `${draft.tv_standard} · ${draft.dvd_resolution} · Blu-ray ${draft.bd_resolution}${draft.bd_single_video ? " (ein Video)" : ""} · Seitenverhältnis ${draft.aspect_ratio_policy} · Profil ${draft.encoding_profile}${draft.fast_encode ? " · schnell" : ""}`)),
    ],
    tv: () => [
      group(
        "TV Format",
        select("tv_standard", "TV-Norm", [
          ["PAL", "PAL (720×576, 25 Bilder/s)"],
          ["NTSC", "NTSC (720×480, 29,97 Bilder/s)"],
        ]),
        select("dvd_resolution", "Auflösung", [
          ["Full D1", "Full D1 (720×576)"],
          ["Half D1", "Half D1 (352×576, NTSC 352×480)"],
          ["SIF", "SIF (352×288, NTSC 352×240)"],
        ]),
        note("Menüs und Titel werden in der gewählten Norm erzeugt (NTSC: 720×480, 29,97 Bilder/s). Half D1 und SIF gelten nur für die Titel – Menüs bleiben in voller Auflösung. Diese Titel sind immer 4:3; Breitbild-Videos bekommen schwarze Balken."),
      ),
      group(
        "Blu-ray",
        select("bd_resolution", "Blu-ray-Auflösung", [
          ["1080p", "1080p (1920×1080)"],
          ["720p", "720p (1280×720)"],
        ]),
        note("Blu-ray-Titel sind immer 16:9 mit quadratischen Pixeln (BT.709); 4:3-Videos bekommen seitliche Balken. Kinofilme (23,976/24 Bilder/s) behalten ihre Bildrate; sonst gilt die TV-Norm: 1080p mit 25 bzw. 29,97 Bildern/s (als 1080i markiert), 720p mit 50 bzw. 59,94 Bildern/s. Die Bildrate ist für die ganze Disc einheitlich."),
        check(
          "bd_single_video",
          "Nur ein Video pro Blu-ray (keine Playlist)",
          "Mehrere Videos werden sonst zu einer Playlist zusammengefasst, die nach dem Einlegen komplett abläuft. Ist die Option an, lässt sich die Blu-ray-Konvertierung nur mit einem Video starten.",
        ),
        check(
          "menu_animations",
          "Menü-Animationen (Intro)",
          "Spielt bei Menüs aus ConvertX-Vorlagen (DVD, Blu-ray, AVCHD) das Intro der Seiten (Ein-/Ausblenden, Zoomen) ab, bevor die Buttons erscheinen. Aus: die fertigen Seiten stehen sofort. Bei Blu-ray/AVCHD spielt das Weiterblättern innerhalb eines Menüs das Intro nicht erneut, bei DVD schon.",
        ),
        check(
          "bd_hw_encode",
          "Hardware-Encoding (NVIDIA NVENC) für Blu-ray",
          "Kodiert Blu-ray-Videos mit der NVIDIA-Grafikkarte statt mit x264 – deutlich schneller. Fällt automatisch auf x264 zurück, wenn NVENC nicht funktioniert (Treiber ≥ 610 nötig). 1080i-Bildraten (25 / 29,97 Bilder/s) bleiben bei x264.",
        ),
      ),
    ],
    encoding: () => [
      group(
        "Enkodierung",
        select(
          "encoding_profile",
          "Profil",
          [
            ["Auto", "Automatisch (Bitrate passend zur Disc)"],
            ["SP_Short", "SP – kurze Laufzeit, höchste Qualität (bis 8.500 kbps)"],
            ["MP_Medium", "MP – mittlere Laufzeit (bis 6.000 kbps)"],
            ["LP_Long", "LP – lange Laufzeit (bis 4.000 kbps)"],
          ],
          "„Automatisch“ verteilt den Platz der Disc auf alle Videos (Bitrate 1.000–8.500 kbps, Blu-ray 4.000–35.000 kbps). MP und LP begrenzen die Bitrate – kurze Projekte werden dann kleiner (Blu-ray: SP bis 35.000, MP bis 20.000, LP bis 12.000 kbps). Bei LP werden DVD-Titel unter 3.000 kbps automatisch in Half D1 kodiert.",
        ),
        number(
          "target_size_mb",
          "Benutzerdefinierte Zielgröße",
          100,
          20000,
          "MB",
          "Wird verwendet, wenn unten links als Medium „Benutzerdefiniert“ gewählt ist (nur DVD). DVD-5 = 4.300 MB, DVD-9 = 7.950 MB, BD-25 = 22.900 MB, BD-50 = 45.800 MB.",
        ),
      ),
      group(
        "Leistung",
        check(
          "hw_decode",
          "GPU-Decoding verwenden (CUDA / D3D11VA / QSV)",
          "Die Quellvideos werden auf der Grafikkarte dekodiert, wenn ein funktionierender Decoder gefunden wird – sonst automatisch auf der CPU. Entlastet die CPU bei vielen parallelen Encodes; bei 4K-Material ohne CUDA kann es langsamer sein.",
        ),
        check(
          "fast_encode",
          "Schnelles Encoding (Entwurf)",
          "Verzichtet auf Trellis-Quantisierung: etwa 40 % schneller, bei minimal geringerer Bildqualität. Für Testläufe und Entwürfe.",
        ),
      ),
    ],
    video: () => [
      group(
        "Videobearbeitung",
        select(
          "aspect_ratio_policy",
          "Seitenverhältnis der DVD",
          [
            ["Auto", "Automatisch (nach Quelle)"],
            ["4:3", "Immer 4:3"],
            ["16:9", "Immer 16:9"],
          ],
          "Abweichende Quellen bekommen schwarze Balken (Letterbox bzw. Pillarbox).",
        ),
        select(
          "scale_mode",
          "Video Skaliermethode",
          [
            ["letterbox", "Letterbox (Standard)"],
            ["panscan", "Pan & Scan (Zuschneiden)"],
            ["stretch", "Strecken"],
          ],
          "Letterbox: das Bild behält sein Seitenverhältnis, der Rest wird schwarz. Pan & Scan: das Bild füllt das ganze Bild aus, der Überstand wird mittig abgeschnitten. Strecken: das Bild wird ohne Rücksicht auf das Seitenverhältnis verzerrt.",
        ),
        select(
          "scale_filter",
          "Skalierungsfilter",
          [
            ["bilinear", "Bilinear (schnell)"],
            ["bicubic", "Bikubisch (Standard)"],
            ["lanczos", "Lanczos (hohe Qualität)"],
          ],
          "Algorithmus, mit dem das Bild vergrößert oder verkleinert wird.",
        ),
        select(
          "deinterlace",
          "Video-Deinterlacing",
          [
            ["auto", "Automatisch (bwdif)"],
            ["yadif", "Immer deinterlacen (yadif)"],
            ["off", "Nie deinterlacen"],
          ],
          "Automatisch: nur Videos, die als Halbbilder (interlaced) markiert sind. „Nie“ kann Kammartefakte erzeugen, wenn Halbbild-Material skaliert wird.",
        ),
      ),
    ],
    menu: () => [
      group(
        "Wiedergabe",
        check("auto_start_playback", "Wiedergabe automatisch starten", "Die DVD beginnt beim Einlegen sofort mit dem ersten Video."),
        check("play_sequentially", "Videos nacheinander abspielen", "Nach jedem Video folgt direkt das nächste, ohne Umweg über das Menü."),
        check(
          "loop_playback",
          "Endlosschleife",
          draft.play_sequentially ? "Nach dem letzten Video beginnt die DVD wieder beim ersten." : "Ein Video wird wiederholt; „Alle abspielen“ beginnt nach dem letzten wieder von vorn.",
        ),
        check(
          "return_to_title_menu_after_title",
          "Nach der Wiedergabe zum Titelmenü zurückkehren",
          draft.loop_playback
            ? "Ohne Wirkung, solange die Endlosschleife aktiv ist."
            : "Sonst geht es zurück in das eigene Menü des Videos (Kapitel/Einstellungen).",
          draft.loop_playback,
        ),
      ),
      group(
        "Menüstruktur",
        check("skip_root_menu", "Hauptmenü des Videos überspringen", "Kein Root-Menü pro Video (Kapitel, Einstellungen, Zurück, Wiedergabe) – das Video öffnet direkt sein Kapitelmenü."),
        check("no_title_menu_for_single_title", "Kein Titelmenü bei nur einem Video erstellen", "Die DVD startet dann direkt im Menü bzw. mit dem Video."),
      ),
      group(
        "Text",
        select("text_overflow_action", "Wenn ein Text zu lang ist", [
          ["reduce_font_size", "Schrift verkleinern (bis 50 %), dann kürzen"],
          ["truncate", "Mit „…“ kürzen, Schriftgröße beibehalten"],
        ]),
      ),
      group(
        "Menü-Ton (Motion Menu)",
        select(
          "menu_audio",
          "Ton aller Menüseiten",
          [
            ["title", "Hauptmenü stumm, Videomenüs mit eigenem Ton"],
            ["silence", "Alle Menüs stumm"],
            ["file", "Hauptmenü mit eigener Audiodatei, Videomenüs mit eigenem Ton"],
          ],
          "Das Menü eines Videos (Root, Kapitel, Einstellungen) spielt immer den Ton dieses Videos – Kapitelseiten ab ihrem ersten Kapitel.",
        ),
        draft.menu_audio === "file"
          ? el(
              "div",
              { class: "opt-row" },
              el("span", { class: "opt-label" }, "Audiodatei für das Hauptmenü"),
              el(
                "span",
                { class: "opt-input wide" },
                el("input", {
                  type: "text",
                  value: draft.menu_audio_file,
                  placeholder: "MP3, WAV, FLAC, AC3 …",
                  spellcheck: false,
                  oninput: (e: Event) => {
                    draft.menu_audio_file = (e.target as HTMLInputElement).value;
                  },
                }),
                el(
                  "button",
                  {
                    class: "btn icon",
                    title: "Datei wählen",
                    onclick: async () => {
                      const file = await open({
                        title: "Menü-Audio wählen",
                        filters: [{ name: "Audio", extensions: ["mp3", "wav", "flac", "ogg", "m4a", "aac", "ac3", "wma", "opus"] }],
                      });
                      if (typeof file === "string") {
                        draft.menu_audio_file = file;
                        render();
                      }
                    },
                  },
                  "…",
                ),
              ),
            )
          : null,
      ),
    ],
    chapters: () => {
      const off = !draft.auto_chapter_enabled;
      const example = draft.chapter_naming_template.includes("<c>") ? draft.chapter_naming_template.replace("<c>", "3") : "—";
      return [
        group(
          "Automatische Kapitel",
          check("auto_chapter_enabled", "Kapitel automatisch erzeugen", "Nur für Videos ohne eigene Kapitelmarken."),
          duration("auto_chapter_interval_seconds", "Ein Kapitel alle", 30, 3600, off),
          duration(
            "auto_chapter_min_video_length_seconds",
            "Nur bei Videos länger als",
            0,
            24 * 3600,
            off,
            "Kürzere Videos bekommen nur ein Kapitel.",
          ),
        ),
        group(
          "Kapitelnamen",
          el(
            "label",
            { class: "opt-row" },
            el("span", { class: "opt-label" }, "Vorlage"),
            el("input", {
              type: "text",
              value: draft.chapter_naming_template,
              maxLength: 60,
              oninput: (e: Event) => {
                draft.chapter_naming_template = (e.target as HTMLInputElement).value;
                const ex = body.querySelector(".chapter-example");
                if (ex) {
                  ex.textContent = draft.chapter_naming_template.includes("<c>")
                    ? `Beispiel: ${draft.chapter_naming_template.replace("<c>", "3")}`
                    : "Die Vorlage muss <c> für die Kapitelnummer enthalten.";
                }
              },
            }),
            el("small", { class: "opt-hint chapter-example" }, `Beispiel: ${example}`),
          ),
          note("<c> wird durch die Kapitelnummer ersetzt. Gilt für Kapitel ohne eigenen Titel. Bereits geladene Videos behalten ihre Kapitelliste."),
        ),
      ];
    },
    subtitles: () => [
      group(
        "Untertitel",
        select("default_subtitle_language", "Bevorzugte Sprache", LANGUAGES, "Untertitel in dieser Sprache werden zur ersten Untertitelspur der DVD."),
        check("force_first_subtitle", "Ersten Untertitel automatisch einblenden", "Sonst sind Untertitel beim Start ausgeschaltet."),
      ),
    ],
    audio: () => [
      group(
        "Audio",
        select("default_audio_language", "Bevorzugte Sprache", LANGUAGES, "Tonspuren in dieser Sprache werden zur ersten (Standard-)Tonspur der DVD."),
        select(
          "audio_mode",
          "Audio-Modus",
          [
            ["auto", "Automatisch (AC-3/DTS kopieren)"],
            ["ac3", "Immer AC-3"],
            ["lpcm", "LPCM (unkomprimiert)"],
          ],
          "Automatisch: passende AC-3-/DTS-Spuren werden verlustfrei übernommen (DVD: AC-3 bis 448 kbps, DTS-Kern; Blu-ray: AC-3 bis 640 kbps, DTS/DTS-HD; AVCHD: AC-3), alles andere wird zu AC-3. LPCM gibt es für Blu-ray (bis 8 Kanäle, 24 Bit) und AVCHD (Stereo, 16 Bit); auf der DVD wird stattdessen AC-3 geschrieben. Geschnittene Videos und normalisierte Spuren werden immer neu kodiert.",
        ),
        check(
          "loudnorm",
          "Lautstärke normalisieren (EBU R128)",
          "Gleicht die Lautstärke aller Tonspuren auf −23 LUFS an (zwei Durchläufe pro Spur: messen, dann anwenden). Die Spuren werden dabei neu kodiert.",
        ),
      ),
    ],
    burn: () => [
      group(
        "Brennen",
        select(
          "burn_speed",
          "Schreibgeschwindigkeit",
          [
            ["auto", "Automatisch (DVD-5/BD: maximal, DVD-9: 4x)"],
            ["max", "Maximal"],
            ["16", "16x"],
            ["12", "12x"],
            ["8", "8x"],
            ["6", "6x"],
            ["4", "4x"],
            ["2.4", "2,4x"],
          ],
          "Wird auf die nächste vom Laufwerk unterstützte Geschwindigkeit abgerundet; die Angabe gilt für das jeweilige Medium (bei Blu-ray: 1x = 36 Mbit/s, 4,5 MB/s). Im Brenndialog lässt sie sich pro Disc ändern.",
        ),
        check("verify_after_burn", "Disc nach dem Brennen überprüfen", "Liest die Disc vollständig zurück und vergleicht sie mit dem Image."),
        check("keep_iso", "ISO-Datei behalten", "Das Image (DVD: UDF 1.02, Blu-ray: UDF 2.50) bleibt neben dem Projektordner liegen."),
      ),
      group(
        "Nach dem Brennen",
        check(
          "delete_after_burn",
          "DVD-Ordner und Arbeitsdateien nach erfolgreichem Brennen löschen",
          "Löscht den erzeugten DVD-Ordner, das Image und den Arbeitsordner – nur wenn der Ordner ausschließlich VIDEO_TS/AUDIO_TS enthält.",
        ),
        check(
          "quiet_finish",
          "Explorer nicht öffnen / Laufwerk nicht auswerfen",
          "Ohne diese Option wird die Disc nach dem Brennen ausgeworfen bzw. der DVD-Ordner im Explorer geöffnet, wenn nicht gebrannt wird.",
        ),
        check(
          "auto_erase_rewritable",
          "Wiederbeschreibbare Discs ohne Nachfrage löschen",
          "DVD±RW mit alten Daten werden automatisch gelöscht/überschrieben; der automatische Brennstart (Countdown) gilt dann auch für sie.",
        ),
      ),
      group(
        "Projekt",
        el(
          "label",
          { class: "opt-row" },
          el("span", { class: "opt-label" }, "Standard-Projekt-/Volume-Name"),
          el(
            "span",
            { class: "opt-input" },
            el("input", {
              type: "text",
              class: "mono",
              value: draft.default_volume_label,
              placeholder: "aus Menütitel / erstem Video",
              maxlength: String(MAX_LABEL + 8),
              spellcheck: false,
              oninput: (e: Event) => {
                const input = e.target as HTMLInputElement;
                const clean = sanitizeLabel(input.value, false);
                if (clean !== input.value) {
                  input.value = clean;
                  input.setSelectionRange(clean.length, clean.length);
                }
                draft.default_volume_label = clean;
              },
              onblur: (e: Event) => {
                const input = e.target as HTMLInputElement;
                // Empty stays empty (= automatic name); junk becomes DVD_VIDEO.
                draft.default_volume_label = input.value.trim() ? sanitizeLabel(input.value) : "";
                input.value = draft.default_volume_label;
              },
            }),
          ),
          el(
            "small",
            { class: "opt-hint" },
            `Ordnername der DVD und Volume-Label der Disc. A–Z, 0–9 und _, höchstens ${MAX_LABEL} Zeichen; leer = automatisch (Fallback ${DEFAULT_LABEL}).`,
          ),
        ),
      ),
    ],
  };

  function renderTree() {
    tree.replaceChildren(
      ...TREE.map((node) =>
        el(
          "div",
          { class: "tree-node" },
          el("button", { class: `tree-item${page === node.id ? " active" : ""}`, onclick: () => go(node.id) }, node.label),
          node.children
            ? el(
                "div",
                { class: "tree-children" },
                ...node.children.map((c) =>
                  el("button", { class: `tree-item child${page === c.id ? " active" : ""}`, onclick: () => go(c.id) }, c.label),
                ),
              )
            : null,
        ),
      ),
    );
  }

  function render() {
    renderTree();
    const title = TREE.flatMap((n) => [n, ...(n.children ?? [])]).find((n) => n.id === page)?.label ?? "";
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
      el("header", { class: "settings-header" }, el("h2", { id: "settings-title" }, "Allgemeine Einstellungen")),
      el("div", { class: "settings-split" }, tree, body),
      el(
        "footer",
        { class: "settings-footer" },
        el(
          "button",
          {
            class: "btn",
            onclick: async () => {
              draft = await defaultSettings();
              error.textContent = "";
              render();
            },
          },
          "Standardwerte",
        ),
        el("span", { class: "settings-path", title: info.path }, info.path),
        error,
        okButton,
        el("button", { class: "btn", onclick: () => close() }, "Abbrechen"),
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
      error.textContent = `Speichern fehlgeschlagen: ${e}`;
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
