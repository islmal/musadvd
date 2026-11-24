import { describe, expect, it } from "vitest";
import { autoSplitPoints, chapterOverflows, dropExternalKind, mixSubs, moveItem, reorderByLanguages, unmixSubs } from "./order";

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

describe("chapterOverflows", () => {
  const ones = (n: number) => Array<number>(n).fill(1);
  it("accepts 151 one-chapter videos merged into 2-4 titles", () => {
    expect(chapterOverflows([ones(76), ones(75)], 99)).toEqual([]);
    expect(chapterOverflows([ones(50), ones(50), ones(51)], 99)).toEqual([]);
    expect(chapterOverflows([ones(38), ones(38), ones(38), ones(37)], 99)).toEqual([]);
  });
  it("flags one title of 151 merged videos and names the video to split at", () => {
    expect(chapterOverflows([ones(151)], 99)).toEqual([{ group: 0, chapters: 151, splitAt: 99 }]);
  });
  it("counts every member's chapters, not only one per video", () => {
    expect(chapterOverflows([[1], [40, 40, 30]], 99)).toEqual([{ group: 1, chapters: 110, splitAt: 2 }]);
    expect(chapterOverflows([[99]], 99)).toEqual([]);
  });
});

describe("autoSplitPoints", () => {
  const ones = (n: number) => Array<number>(n).fill(1);
  it("splits 151 one-chapter videos into titles of at most 99", () => {
    expect(autoSplitPoints([ones(151)], 99)).toEqual([[0, 99]]);
    expect(chapterOverflows([ones(99), ones(52)], 99)).toEqual([]);
  });
  it("splits 7-chapter episodes after 14 episodes each", () => {
    expect(autoSplitPoints([Array<number>(30).fill(7)], 99)).toEqual([[0, 14], [0, 28]]);
  });
  it("leaves fitting groups alone and only splits the ones over the limit", () => {
    expect(autoSplitPoints([ones(50), [60, 30, 20]], 99)).toEqual([[1, 2]]);
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
