// Layer break modal: the cells of a DVD-Video image or folder that make a legal layer break.
import * as api from "./api";
import { el, fmtSectors, modal } from "./ui";

/**
 * Lists the candidates of `source` (an `.iso` or a folder with `VIDEO_TS`). With `onPick` a row can
 * be chosen ("Übernehmen"); without it the dialog only inspects.
 */
export async function openLayerBreak(source: string, onPick?: (c: api.Candidate) => void) {
  let showAll = false;
  let picked: api.Candidate | null = null;
  const body = el("div");
  const table = el("div", { class: "lb-table" });
  const note = el("p", { class: "hint" });
  const load = async () => {
    table.replaceChildren(el("p", { class: "hint" }, "Wird gelesen …"));
    picked = null;
    try {
      const list = await api.ifoCandidates(source, showAll);
      note.textContent = list.length
        ? `${list.length} mögliche Layerwechsel. Nahtlose Zellen stehen hinten (Wechsel dort kann kurz ruckeln); „Füllsektoren“ werden vor das Titleset eingefügt, um die Grenze auf 16 Sektoren auszurichten.`
        : "Keine möglichen Layerwechsel gefunden (passt auf eine Schicht, oder keine Zelle erfüllt die Regeln).";
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
          el("thead", {}, el("tr", {}, ...["VTS", "TTN", "PGC", "Zelle", "Start", "Schicht 0", "Schicht 1", "Teilung", "Füllsektoren", "Flags"].map((t) => el("th", {}, t)))),
          el("tbody", {}, ...rows),
        ),
      );
    } catch (e) {
      table.replaceChildren(el("p", { class: "layout-errors" }, String(e)));
      note.textContent = "";
    }
  };
  body.append(
    el("p", {}, el("strong", {}, "Quelle: "), source),
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
      el("span", {}, "Verschachtelte Zellen anzeigen", el("small", {}, "Interleaved-Blöcke (Multi-Angle) sind sonst ausgeblendet.")),
    ),
    note,
    table,
  );
  const buttons: Parameters<typeof modal>[2] = onPick
    ? [
        { label: "Abbrechen" },
        {
          label: "Übernehmen",
          primary: true,
          onClick: () => {
            if (!picked) return false;
            onPick(picked);
          },
        },
      ]
    : [{ label: "Schließen", primary: true }];
  modal("Layerwechsel (IFO-Inspektor)", body, buttons, true);
  await load();
}

export const describeBreak = (c: api.Candidate) => `Schicht 0: ${fmtSectors(c.l0_sectors)}, Schicht 1: ${fmtSectors(c.l1_sectors)}`;
