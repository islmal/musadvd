// Layer break modal: the cells of a DVD-Video image or folder that make a legal layer break.
import * as api from "./api";
import { el, fmtSectors, modal } from "./ui";
import { t } from "../i18n";

/**
 * Lists the candidates of `source` (an `.iso` or a folder with `VIDEO_TS`). With `onPick` a row can
 * be chosen ("Apply"); without it the dialog only inspects.
 */
export async function openLayerBreak(source: string, onPick?: (c: api.Candidate) => void) {
  let showAll = false;
  let picked: api.Candidate | null = null;
  const body = el("div");
  const table = el("div", { class: "lb-table" });
  const note = el("p", { class: "hint" });
  const load = async () => {
    table.replaceChildren(el("p", { class: "hint" }, t("studio.layerbreak.wirdGelesen")));
    picked = null;
    try {
      const list = await api.ifoCandidates(source, showAll);
      note.textContent = list.length
        ? t("studio.layerbreak.moeglicheLayerwechselNahtloseZellen", { length: list.length })
        : t("studio.layerbreak.keineMoeglichenLayerwechselGefunden");
      const rows = list.map((c) => {
        const tr = el(
          "tr",
          {
            class: onPick ? "pickable" : "",
            onclick: () => {
              picked = c;
              table.querySelectorAll("tr.sel").forEach((r) => r.classList.remove("sel"));
              tr.classList.add("sel");
            },
          },
          ...[c.vts, c.ttn, c.pgc, c.cell, c.start_lba, c.l0_sectors, c.l1_sectors, `${c.split_percent.toFixed(1)} %`, c.padding_sectors].map((v) => el("td", {}, String(v))),
          el("td", {}, [c.seamless ? "nahtlos" : "", c.interleaved ? "verschachtelt" : ""].filter(Boolean).join(", ") || "—"),
        );
        return tr;
      });
      table.replaceChildren(
        el(
          "table",
          { class: "studio-table" },
          el("thead", {}, el("tr", {}, ...["VTS", "TTN", "PGC", t("studio.layerbreak.zelle"), t("studio.layerbreak.start"), t("studio.layerbreak.schicht0"), t("studio.layerbreak.schicht1"), t("studio.layerbreak.teilung"), t("studio.layerbreak.fuellsektoren"), t("studio.layerbreak.flags")].map((t) => el("th", {}, t)))),
          el("tbody", {}, ...rows),
        ),
      );
    } catch (e) {
      table.replaceChildren(el("p", { class: "layout-errors" }, String(e)));
      note.textContent = "";
    }
  };
  body.append(
    el("p", {}, el("strong", {}, t("studio.layerbreak.quelle")), source),
    el(
      "label",
      { class: "opt-check" },
      el("input", {
        type: "checkbox",
        onchange: (e: Event) => {
          showAll = (e.target as HTMLInputElement).checked;
          void load();
        },
      }),
      el("span", {}, t("studio.layerbreak.verschachtelteZellenAnzeigen"), el("small", {}, t("studio.layerbreak.interleavedBloeckeMultiAngle"))),
    ),
    note,
    table,
  );
  const buttons: Parameters<typeof modal>[2] = onPick
    ? [
        { label: t("studio.layerbreak.abbrechen") },
        {
          label: t("studio.layerbreak.uebernehmen"),
          primary: true,
          onClick: () => {
            if (!picked) return false;
            onPick(picked);
          },
        },
      ]
    : [{ label: t("studio.layerbreak.schliessen"), primary: true }];
  modal(t("studio.layerbreak.layerwechselIfoInspektor"), body, buttons, true);
  await load();
}

export const describeBreak = (c: api.Candidate) => t("studio.layerbreak.schicht0Schicht1", { fmtSectors: fmtSectors(c.l0_sectors), fmtSectors2: fmtSectors(c.l1_sectors) });
