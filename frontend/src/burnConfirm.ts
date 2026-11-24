// Pure decisions for the erase checkbox and the "Burn now?" confirmation (no DOM, no i18n).
import type { MediaInfo } from "./api";

type Disc = Pick<MediaInfo, "present" | "writable" | "rewritable" | "blank">;

/** A rewritable disc that holds data: the erase checkbox starts ticked (the confirmation still asks). */
export function eraseDefault(m: Disc | null | undefined): boolean {
  return !!m && m.present && m.writable && m.rewritable && !m.blank;
}

/**
 * Which confirmation precedes the burn: "erase" (existing contents are overwritten), "permanent"
 * (write-once medium) or "none" (blank rewritable disc: nothing is lost, nothing is permanent).
 */
export function confirmKind(m: Disc | null | undefined, erase: boolean): "erase" | "permanent" | "none" {
  if (!m || !m.present) return "none";
  if (!m.rewritable) return "permanent";
  return erase && !m.blank ? "erase" : "none";
}
