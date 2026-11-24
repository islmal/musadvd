import { describe, expect, it } from "vitest";
import { loadRecent, pushRecent, removeRecent, RECENT_KEY, type RecentProject } from "./recent";

const fake = () => {
  const m = new Map<string, string>();
  return { getItem: (k: string) => m.get(k) ?? null, setItem: (k: string, v: string) => void m.set(k, v) };
};
const p = (path: string, modified = 1): RecentProject => ({ name: path, path, medium: "dvd", modified });

describe("recent", () => {
  it("tolerates bad JSON", () => {
    const s = fake();
    s.setItem(RECENT_KEY, "{nope");
    expect(loadRecent(s)).toEqual([]);
    s.setItem(RECENT_KEY, '{"a":1}');
    expect(loadRecent(s)).toEqual([]);
  });
  it("dedupes case-insensitively, newest first", () => {
    const s = fake();
    pushRecent(p("C:\\a.mdvd"), s);
    pushRecent(p("C:\\b.mdvd"), s);
    const l = pushRecent(p("c:\\A.mdvd", 2), s);
    expect(l.map((x) => x.path)).toEqual(["c:\\A.mdvd", "C:\\b.mdvd"]);
    expect(loadRecent(s)).toEqual(l);
  });
  it("caps at 10 and removes", () => {
    const s = fake();
    for (let i = 0; i < 12; i++) pushRecent(p(`C:\\${i}`), s);
    expect(loadRecent(s)).toHaveLength(10);
    expect(removeRecent("c:\\11", s)[0].path).toBe("C:\\10");
  });
});
