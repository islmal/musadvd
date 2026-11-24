import { describe, expect, it } from "vitest";
import { dropExternalKind, mixSubs, moveItem, reorderByLanguages, unmixSubs } from "./order";

const tr = [
  { l: "eng", n: 1 },
  { l: "ger", n: 2 },
  { l: "fre", n: 3 },
  { l: "eng", n: 4 },
];
const byLang = (x: { l: string }) => x.l;

describe("reorderByLanguages", () => {
  it("puts matching tracks first in preference order", () => {
    expect(reorderByLanguages(tr, byLang, ["ger", "eng"]).map((x) => x.n)).toEqual([2, 1, 4, 3]);
  });
  it("keeps unmatched tracks in their relative order at the end", () => {
    expect(reorderByLanguages(tr, byLang, ["fre"]).map((x) => x.n)).toEqual([3, 1, 2, 4]);
  });
  it("supports slot mode via position keys", () => {
    expect(reorderByLanguages(["a", "b", "c"], (_x, i) => String(i), ["2", "0"])).toEqual(["c", "a", "b"]);
  });
  it("leaves the list unchanged for empty prefs", () => {
    expect(reorderByLanguages(tr, byLang, [])).toEqual(tr);
  });
});

describe("mixSubs", () => {
  const flat = (m: { item: string }[]) => m.map((x) => x.item);
  it("defaults to embedded first, then external", () => {
    expect(flat(mixSubs(["e1", "e2"], ["x1"], undefined))).toEqual(["e1", "e2", "x1"]);
  });
  it("interleaves by the kind pattern and appends uncovered items", () => {
    expect(flat(mixSubs(["e1", "e2"], ["x1", "x2"], "xs"))).toEqual(["x1", "e1", "e2", "x2"]);
  });
  it("skips pattern entries beyond a list's length", () => {
    expect(flat(mixSubs(["e1"], [], "xxs"))).toEqual(["e1"]);
  });
  it("round-trips through unmixSubs after a move", () => {
    const m = mixSubs(["e1"], ["x1"], undefined);
    moveItem(m, 1, 0);
    expect(unmixSubs(m)).toEqual({ emb: ["e1"], ext: ["x1"], kinds: "xs" });
  });
  it("drops the removed external's slot", () => {
    expect(dropExternalKind("xsx", 0)).toBe("sx");
    expect(dropExternalKind("xsx", 1)).toBe("xs");
    expect(dropExternalKind(undefined, 0)).toBeUndefined();
  });
});
