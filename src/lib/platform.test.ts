import { describe, expect, it } from "vitest";

import { isMobile } from "./platform";

describe("isMobile", () => {
  it("detects an Android WebView user agent", () => {
    expect(
      isMobile(
        "Mozilla/5.0 (Linux; Android 13; Pixel 7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Mobile Safari/537.36",
      ),
    ).toBe(true);
  });

  it("does not treat a desktop Windows WebView2 user agent as mobile", () => {
    expect(
      isMobile(
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36 Edg/124.0.0.0",
      ),
    ).toBe(false);
  });

  it("treats an empty or missing user agent as desktop", () => {
    expect(isMobile("")).toBe(false);
  });
});
