import { describe, expect, it } from "vitest";
import { AVATAR_MAX_BYTES, checkAvatar } from "./avatar";

describe("checkAvatar", () => {
  it("accepts a small data:image URL", () => {
    expect(checkAvatar("data:image/png;base64,AAAA")).toEqual({ ok: true });
  });

  it("rejects a non-image URL", () => {
    const result = checkAvatar("http://evil/x.png");
    expect(result.ok).toBe(false);
  });

  it("rejects an oversize image at the server's 512 KB cap", () => {
    const huge = "data:image/png;base64," + "A".repeat(AVATAR_MAX_BYTES);
    const result = checkAvatar(huge);
    expect(result.ok).toBe(false);
  });
});
