import { describe, expect, it } from "vitest";
import { confirmKind, eraseDefault } from "./burnConfirm";

const disc = (rewritable: boolean, blank: boolean, present = true) => ({ present, writable: true, rewritable, blank });

describe("burnConfirm", () => {
  it("pre-ticks erase only for rewritable discs with data", () => {
    expect(eraseDefault(disc(true, false))).toBe(true);
    expect(eraseDefault(disc(true, true))).toBe(false);
    expect(eraseDefault(disc(false, false))).toBe(false);
    expect(eraseDefault(disc(true, false, false))).toBe(false);
    expect(eraseDefault({ ...disc(true, false), writable: false })).toBe(false);
    expect(eraseDefault(null)).toBe(false);
  });
  it("picks the confirmation", () => {
    expect(confirmKind(disc(true, false), true)).toBe("erase");
    expect(confirmKind(disc(true, false), false)).toBe("none");
    expect(confirmKind(disc(true, true), true)).toBe("none");
    expect(confirmKind(disc(false, true), false)).toBe("permanent");
    expect(confirmKind(disc(false, false), false)).toBe("permanent");
    expect(confirmKind(null, true)).toBe("none");
  });
});
