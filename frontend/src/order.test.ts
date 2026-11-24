import { describe, expect, it } from "vitest";
import { reorderByLanguages } from "./order";

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
