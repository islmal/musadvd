// "Auf DVD brennen": drive, disc check, volume label, speed, 20 s auto-start countdown and the
// burn progress (image → formatting → writing → finalizing → verification).
import {
  cancelBurn,
  discNeeds,
  formatRemaining,
  listDrives,
  openFolder,
  onBurnDone,
  onBurnProgress,
  probeMedia,
  startBurn,
  type AppSettings,
  type BurnDone,
  type BurnPhase,
  type BurnProgress,
  type DiscNeeds,
  type DriveInfo,
  type MediaView,
  type WriteSpeed,
} from "./api";
import { DEFAULT_LABEL, MAX_LABEL, sanitizeLabel } from "./label";
import { fmtNumber, t } from "./i18n";

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

const COUNTDOWN_SECS = 20;
const MEDIA_POLL_MS = 4000;

const speedKey = (s: WriteSpeed) => (s === "max" ? "max" : String(s.specific));
const gb = (sectors: number) => fmtNumber((sectors * 2048) / 1e9, 2);

const phases = (): { id: BurnPhase; label: string }[] => [
  { id: "preparing", label: t("burn.image") },
  { id: "formatting", label: t("burn.formatieren") },
  { id: "writing", label: t("burn.schreiben") },
  { id: "finalizing", label: t("burn.abschliessen") },
  { id: "verifying", label: t("burn.ueberpruefen") },
];

/** Share of the total bar per phase: [start, end] in %. */
function phaseRange(phase: BurnPhase, verify: boolean): [number, number] {
  const writeEnd = verify ? 80 : 94;
  switch (phase) {
    case "preparing":
      return [0, 15];
    case "formatting":
      return [15, 17];
    case "writing":
      return [17, writeEnd];
    case "finalizing":
      return [writeEnd, verify ? 85 : 100];
    case "verifying":
      return [85, 100];
    case "done":
      return [100, 100];
  }
}

let open = false;

/** The DVD's project folder (`…/<Projekt>` of `…/<Projekt>/VIDEO_TS`). */
function projectFolder(videoTs: string): string {
  return videoTs.replace(/[\\/]+$/, "").replace(/[\\/][^\\/]*$/, "");
}

/**
 * Opens the burn dialog for a finished DVD folder. Defaults come from the global settings;
 * `afterConversion`: closing without burning shows the DVD folder in Explorer (unless the
 * "quiet" setting is on).
 */
export async function openBurner(
  videoTs: string,
  projectName: string | null,
  settings: AppSettings | null,
  afterConversion: boolean,
  onClosed?: () => void,
) {
  if (open) return;
  open = true;
  const quiet = settings?.quiet_finish ?? false;
  const autoErase = settings?.auto_erase_rewritable ?? false;
  const speedPref = settings?.burn_speed ?? "auto";

  let drives: DriveInfo[] = [];
  let driveId = "";
  let needs: DiscNeeds | null = null;
  let media: MediaView | null = null;
  let speed: WriteSpeed | null = null; // null = default of the disc
  let label = settings?.default_volume_label || sanitizeLabel(projectName ?? "");
  let verify = settings?.verify_after_burn ?? true;
  let keepIso = settings?.keep_iso ?? false;
  let eject = !quiet;
  let deleteAfter = settings?.delete_after_burn ?? false;
  let eraseConfirmed = false;
  let burning = false;
  let finished: BurnDone | null = null;
  let countdown: number | null = null; // seconds left
  let countdownCancelled = false;
  let tick: number | undefined;
  let poll: number | undefined;
  let probing = false;
  let cancelArmed = false;
  let total = 0;
  let current: BurnProgress | null = null;
  const error = el("div", { class: "settings-error", role: "alert" });

  const card = el("div", { class: "overlay-card burn-card", role: "dialog", "aria-modal": "true", "aria-labelledby": "burn-title" });
  const root = el("div", { class: "overlay burn-overlay" }, card);
  document.body.append(root);

  const unlisten = [
    await onBurnProgress((p) => {
      current = p;
      const [a, b] = phaseRange(p.phase, verify);
      total = Math.max(total, a + (b - a) * p.fraction);
      render();
    }),
    await onBurnDone((d) => {
      burning = false;
      finished = d;
      if (d.ok) total = 100;
      render();
    }),
  ];

  function close() {
    if (burning) return;
    stopCountdown();
    window.clearInterval(poll);
    unlisten.forEach((u) => u());
    root.remove();
    open = false;
    // Not burnt: show the finished DVD folder instead.
    if (afterConversion && !quiet && !finished?.ok) void openFolder(projectFolder(videoTs)).catch(() => undefined);
    onClosed?.();
  }

  // ---- countdown ------------------------------------------------------------------------------
  function stopCountdown() {
    window.clearInterval(tick);
    tick = undefined;
    countdown = null;
  }

  /** Any user interaction ends the automatic start for good. */
  function interact() {
    if (countdownCancelled || burning) return;
    countdownCancelled = true;
    if (countdown !== null) {
      stopCountdown();
      render();
    }
  }

  function maybeStartCountdown() {
    if (countdownCancelled || burning || finished || countdown !== null || !media?.autoOk) return;
    countdown = COUNTDOWN_SECS;
    tick = window.setInterval(async () => {
      if (countdown === null) return;
      countdown -= 1;
      if (countdown <= 0) {
        stopCountdown();
        countdownCancelled = true;
        // Re-check the disc right before starting without the user.
        await refreshMedia();
        if (media?.autoOk) void burn();
        else render();
        return;
      }
      render();
    }, 1000);
  }

  // ---- data -------------------------------------------------------------------------------------
  async function refreshDrives() {
    try {
      drives = await listDrives();
      if (!drives.some((d) => d.id === driveId)) driveId = drives[0]?.id ?? "";
      error.textContent = drives.length === 0 ? t("burn.keinDvdBluRay") : "";
    } catch (e) {
      error.textContent = t("burn.laufwerke", { e });
    }
    await refreshMedia();
  }

  async function refreshMedia() {
    if (!driveId || probing || burning) {
      if (!driveId) media = null;
      render();
      return;
    }
    probing = true;
    const before = JSON.stringify(media);
    try {
      const id = driveId;
      const m = await probeMedia(id, videoTs, speedPref, autoErase);
      if (id === driveId) media = m;
      if (speed !== null && !m.speeds.some((s) => speedKey(s.value) === speedKey(speed!))) speed = null;
    } catch (e) {
      media = null;
      error.textContent = String(e);
    } finally {
      probing = false;
    }
    maybeStartCountdown();
    // The disc went away or became unusable while counting down.
    if (countdown !== null && !media?.autoOk) stopCountdown();
    // Unchanged disc: no re-render (it would close an open drop-down list).
    if (JSON.stringify(media) !== before || countdown !== null) render();
  }

  /** Old content on a rewritable disc needs an explicit OK unless erasing is automatic. */
  const eraseOk = () => !media?.needsErase || autoErase || eraseConfirmed;

  async function burn() {
    if (burning || !media || media.problem || !driveId || !eraseOk()) return;
    stopCountdown();
    countdownCancelled = true;
    window.clearInterval(poll);
    label = sanitizeLabel(label);
    burning = true;
    finished = null;
    total = 0;
    current = null;
    error.textContent = "";
    render();
    try {
      await startBurn({
        driveId,
        videoTs,
        label,
        speed: speed ?? media.defaultSpeed,
        verify,
        keepIso,
        eject,
        erase: media.needsErase && (autoErase || eraseConfirmed),
        deleteAfter,
      });
    } catch (e) {
      burning = false;
      current = null;
      error.textContent = String(e);
      render();
    }
  }

  async function requestCancel() {
    if (!cancelArmed) {
      // Second click within 5 s confirms: an interrupted write-once disc is usually unusable.
      cancelArmed = true;
      render();
      window.setTimeout(() => {
        cancelArmed = false;
        render();
      }, 5000);
      return;
    }
    cancelArmed = false;
    await cancelBurn().catch(() => false);
    if (current) current = { ...current, message: t("burn.wirdAbgebrochen") };
    render();
  }

  // ---- rendering ----------------------------------------------------------------------------------
  function mediaLine() {
    if (!driveId) return el("div", { class: "media-state bad" }, t("burn.keinLaufwerk"));
    if (!media) return el("div", { class: "media-state" }, probing ? t("burn.discWirdGeprueft") : "—");
    const m = media.media;
    const kind = { dvd9: "DVD-9", dvd5: "DVD-5", bd25: "BD-25", bd50: "BD-50", unknown: "" }[m.media_type];
    const text = m.present
      ? `${m.physical}${kind ? ` (${kind})` : ""} · ${m.blank ? t("common.blank") : t("common.written")} · ${gb(
          m.blank ? m.free_sectors : m.total_sectors,
        )} GB`
      : t("burn.keineDiscEingelegt");
    const cls = media.problem ? "bad" : media.warning ? "warn" : "good";
    return el(
      "div",
      { class: "burn-media" },
      el("div", { class: `media-state ${cls}` }, text),
      media.problem && m.present ? el("div", { class: "media-note bad" }, media.problem) : null,
      media.warning ? el("div", { class: "media-note warn" }, media.warning) : null,
    );
  }

  function form() {
    const disabled = burning;
    return el(
      "div",
      { class: "burn-form" },
      el(
        "label",
        { class: "opt-row" },
        el("span", { class: "opt-label" }, t("burn.laufwerk")),
        el(
          "div",
          { class: "burn-drive" },
          el(
            "select",
            {
              disabled: disabled || drives.length === 0,
              onchange: (e: Event) => {
                driveId = (e.target as HTMLSelectElement).value;
                media = null;
                speed = null;
                void refreshMedia();
              },
            },
            ...drives.map((d) => el("option", { value: d.id, selected: d.id === driveId }, d.name)),
            drives.length === 0 ? el("option", {}, t("burn.keinBrennerGefunden")) : null,
          ),
          el("button", { class: "btn", type: "button", disabled, onclick: () => void refreshDrives() }, t("burn.aktualisieren")),
        ),
      ),
      el("div", { class: "opt-row" }, el("span", { class: "opt-label" }, t("burn.medium")), mediaLine()),
      el(
        "label",
        { class: "opt-row" },
        el("span", { class: "opt-label" }, t("burn.volumeLabel")),
        el(
          "div",
          {},
          el("input", {
            type: "text",
            class: "burn-label",
            value: label,
            maxlength: String(MAX_LABEL + 8),
            placeholder: DEFAULT_LABEL,
            spellcheck: false,
            disabled,
            oninput: (e: Event) => {
              const input = e.target as HTMLInputElement;
              const clean = sanitizeLabel(input.value, false);
              if (clean !== input.value) {
                input.value = clean;
                input.setSelectionRange(clean.length, clean.length);
              }
              label = clean;
            },
            onblur: (e: Event) => {
              label = sanitizeLabel((e.target as HTMLInputElement).value);
              (e.target as HTMLInputElement).value = label;
            },
          }),
          el("small", { class: "hint" }, t("burn.az09UndHoechstens")),
        ),
      ),
      el(
        "label",
        { class: "opt-row" },
        el("span", { class: "opt-label" }, t("burn.geschwindigkeit")),
        el(
          "select",
          {
            disabled: disabled || !media?.media.present,
            onchange: (e: Event) => {
              const v = (e.target as HTMLSelectElement).value;
              const opt = media?.speeds.find((s) => speedKey(s.value) === v);
              speed = opt ? opt.value : null;
              render();
            },
          },
          ...(media?.speeds ?? [{ value: "max" as WriteSpeed, label: t("burn.maximal") }]).map((s) => {
            const isDefault = media && speedKey(s.value) === speedKey(media.defaultSpeed);
            const selected = speedKey(speed ?? media?.defaultSpeed ?? "max") === speedKey(s.value);
            return el("option", { value: speedKey(s.value), selected }, isDefault ? t("burn.recommended", { label: s.label }) : s.label);
          }),
        ),
      ),
      el(
        "div",
        { class: "burn-checks" },
        check(t("burn.nachDemBrennenUeberpruefen"), verify, (v) => ((verify = v), render()), disabled),
        check(t("burn.isoDateiBehalten"), keepIso, (v) => (keepIso = v), disabled || deleteAfter),
        check(t("burn.discDanachAuswerfen"), eject, (v) => (eject = v), disabled),
        check(t("burn.ordnerDanachLoeschen", { discKind: discKind() }), deleteAfter, (v) => ((deleteAfter = v), render()), disabled),
        media?.needsErase && !autoErase
          ? check(t("burn.inhaltDerDiscLoeschen"), eraseConfirmed, (v) => ((eraseConfirmed = v), render()), disabled)
          : null,
      ),
      estimateLine(),
    );
  }

  /** Duration of image + burn at the selected speed (before starting). */
  function estimateLine() {
    if (burning || finished || !media || media.problem) return null;
    const key = speedKey(speed ?? media.defaultSpeed);
    const opt = media.speeds.find((s) => speedKey(s.value) === key);
    if (!opt) return null;
    const secs = verify ? opt.secsVerify : opt.secs;
    return el(
      "div",
      { class: "media-note" },
      t("burn.geschaetzteDauerIsoBrennen", { formatRemaining: formatRemaining(secs), label: opt.label, verify: verify ? t("burn.ueberpruefung") : "" }),
    );
  }

  function check(text: string, value: boolean, set: (v: boolean) => unknown, disabled: boolean) {
    return el(
      "label",
      { class: "opt-check" },
      el("input", { type: "checkbox", checked: value, disabled, onchange: (e: Event) => set((e.target as HTMLInputElement).checked) }),
      el("span", {}, text),
    );
  }

  function progressView() {
    if (!current && !finished) return null;
    const phase = current?.phase ?? "done";
    const all = phases();
    const idx = all.findIndex((p) => p.id === phase);
    const shown = verify ? all : all.filter((p) => p.id !== "verifying");
    return el(
      "div",
      { class: "burn-progress" },
      el(
        "ol",
        { class: "phases burn-phases", style: `grid-template-columns: repeat(${shown.length}, 1fr)` },
        ...shown.map((p) => {
          const i = all.indexOf(p);
          const cls = finished?.ok || phase === "done" || i < idx ? "done" : i === idx && !finished ? "active" : "";
          return el("li", { class: cls }, p.label);
        }),
      ),
      el(
        "div",
        { class: "progress-block" },
        el("div", { class: "progress-label" }, el("span", {}, finished ? finished.message : current?.message ?? t("burn.wirdGestartet")), el("span", {}, `${Math.floor(total)} %`)),
        el("div", { class: "progress" }, el("div", { class: "progress-value", style: `width: ${total}%` })),
      ),
      current && !finished
        ? el(
            "div",
            { class: "eta-line" },
            t("burn.geschaetzteRestzeitPhase", { formatRemaining: formatRemaining(current.eta.remainingSecs ?? 0), phaseNumber: current.eta.phaseNumber, phaseCount: current.eta.phaseCount, phaseLabel: current.eta.phaseLabel, current: current.eta.detail ? ` – ${current.eta.detail}` : "" }),
          )
        : null,
      finished?.iso ? el("div", { class: "media-note" }, t("burn.isoDatei", { iso: finished.iso })) : null,
    );
  }

  /** "Blu-ray" or "DVD": known from the folder name until the disc check has run. */
  function discKind() {
    const bluray = needs?.bluray ?? /BDMV$/i.test(videoTs.replace(/[\\/]+$/, ""));
    if (!bluray) return "DVD";
    // A BDMV that fits a DVD goes on DVD media as AVCHD.
    const onDvd = !!media && media.media.present && !["bd25", "bd50"].includes(media.media.media_type);
    return needs?.fitsDvd && onDvd ? t("burn.avchdDisc") : t("burn.bluRay");
  }

  function render() {
    const kind = discKind();
    const title = finished
      ? finished.ok
        ? t("burn.burned", { kind })
        : finished.cancelled
          ? t("burn.brennenAbgebrochen")
          : t("burn.brennenFehlgeschlagen")
      : burning
        ? current
          ? `[${current.eta.phaseNumber}/${current.eta.phaseCount}] ${current.eta.phaseLabel}`
          : t("burn.burning", { kind })
        : t("burn.aufBrennen", { kind });
    const info = needs
      ? t("burn.inhaltGb", { needs: fmtNumber(needs.sizeGb, 2), needs2: needs.bluray ? (needs.dualLayer ? t("burn.bd50DualLayer") : t("burn.passtAufEineBd", { needs: needs.fitsDvd ? t("burn.oderAlsAvchdAuf", { needs: needs.sectors > 2295104 ? "DVD-9" : "DVD-5" }) : "" })) : needs.dualLayer ? t("burn.dvd9DoubleLayer") : t("burn.passtAufEineDvd") })
      : t("burn.inhaltWirdGeprueft");
    const canBurn = !burning && !finished && !!media && !media.problem && !!driveId && eraseOk();
    const countdownBar =
      countdown !== null
        ? el(
            "div",
            { class: "burn-countdown", role: "status" },
            el("span", {}, t("burn.brennenStartetAutomatischIn", { countdown })),
            el(
              "button",
              {
                class: "btn",
                type: "button",
                onclick: () => {
                  countdownCancelled = true;
                  stopCountdown();
                  render();
                },
              },
              t("burn.timerAbbrechen"),
            ),
            el("div", { class: "countdown-bar", style: `width: ${(countdown / COUNTDOWN_SECS) * 100}%` }),
          )
        : null;
    const focused = document.activeElement;
    const focusIndex = focused && card.contains(focused) ? [...card.querySelectorAll("input,select,button")].indexOf(focused) : -1;
    const children: Child[] = [
      el("h2", { id: "burn-title" }, title),
      el("div", { class: "burn-info" }, info, ` · ${videoTs}`),
      form(),
      countdownBar,
      progressView(),
      error,
      el(
        "div",
        { class: "overlay-actions" },
        burning
          ? el(
              "button",
              { class: "btn danger", type: "button", onclick: () => void requestCancel() },
              cancelArmed ? t("burn.wirklichAbbrechenRohlingWird") : t("burn.abbrechen"),
            )
          : el("button", { class: "btn", type: "button", onclick: close }, t("burn.schliessen")),
        !burning && !finished
          ? el("button", { class: "btn primary", type: "button", disabled: !canBurn, onclick: () => void burn() }, t("burn.brennen"))
          : null,
        finished && !finished.ok && !finished.cancelled
          ? el(
              "button",
              {
                class: "btn primary",
                type: "button",
                onclick: () => {
                  finished = null;
                  current = null;
                  void refreshMedia();
                },
              },
              t("burn.erneutVersuchen"),
            )
          : null,
      ),
    ];
    card.replaceChildren(...children.filter((c): c is Node | string => !!c));
    // Keep keyboard focus across re-renders (the countdown re-renders every second).
    if (focusIndex >= 0) (card.querySelectorAll("input,select,button")[focusIndex] as HTMLElement | undefined)?.focus();
  }

  // Interaction anywhere in the dialog stops the countdown (the timer's own button included).
  for (const type of ["pointerdown", "keydown", "wheel"]) card.addEventListener(type, interact, { capture: true });
  root.addEventListener("keydown", (e) => {
    if ((e as KeyboardEvent).key === "Escape") close();
  });

  render();
  try {
    needs = await discNeeds(videoTs);
  } catch (e) {
    error.textContent = String(e);
  }
  await refreshDrives();
  poll = window.setInterval(() => {
    if (!burning && !finished) void refreshMedia();
  }, MEDIA_POLL_MS);
}
