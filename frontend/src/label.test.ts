import { describe, expect, it } from "vitest";
import { MAX_UDF_LABEL, sanitizeLabel, sanitizeUdfLabel } from "./label";

describe("volume labels", () => {
  it("ISO label follows the main burner rules", () => {
    expect(sanitizeLabel("Urlaub 2025")).toBe("URLAUB_2025");
    expect(sanitizeLabel("", false)).toBe("");
  });

  it("UDF label keeps Latin-1 free text up to 30 characters", () => {
    expect(sanitizeUdfLabel("  Urlaub 2025 – Teil 1 ")).toBe("Urlaub 2025  Teil 1");
    expect(sanitizeUdfLabel("\u00c4rger & \u00d6l")).toBe("\u00c4rger & \u00d6l");
    expect(sanitizeUdfLabel("Война")).toBe("");
    expect(sanitizeUdfLabel("a\tb")).toBe("ab");
    expect(sanitizeUdfLabel("x".repeat(40))).toHaveLength(MAX_UDF_LABEL);
    expect(sanitizeUdfLabel("Mein Film ", false)).toBe("Mein Film ");
  });
});
