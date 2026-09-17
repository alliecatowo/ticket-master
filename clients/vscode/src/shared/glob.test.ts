import { describe, expect, it } from "vitest";
import { matchesAny, matchesPath, normalizePath } from "./glob";

describe("normalizePath", () => {
  it("converts backslashes to forward slashes", () => {
    expect(normalizePath("src\\auth\\login.rs")).toBe("src/auth/login.rs");
  });

  it("strips a leading ./", () => {
    expect(normalizePath("./src/auth/login.rs")).toBe("src/auth/login.rs");
  });
});

describe("matchesPath", () => {
  it("matches a literal path exactly", () => {
    expect(matchesPath("src/auth/login.rs", "src/auth/login.rs")).toBe(true);
    expect(matchesPath("src/auth/login.rs", "src/auth/logout.rs")).toBe(false);
  });

  it("matches ** across any number of segments", () => {
    expect(matchesPath("src/auth/**", "src/auth/login.rs")).toBe(true);
    expect(matchesPath("src/auth/**", "src/auth/sub/login.rs")).toBe(true);
    expect(matchesPath("src/auth/**", "src/auth")).toBe(false);
    expect(matchesPath("src/**", "src/auth/sub/login.rs")).toBe(true);
    expect(matchesPath("src/other/**", "src/auth/login.rs")).toBe(false);
  });

  it("matches * within a single segment only", () => {
    expect(matchesPath("src/auth/*.rs", "src/auth/login.rs")).toBe(true);
    expect(matchesPath("src/auth/*.rs", "src/auth/sub/login.rs")).toBe(false);
  });

  it("normalizes both pattern and path before comparing", () => {
    expect(matchesPath("src/auth/**", "src\\auth\\login.rs")).toBe(true);
    expect(matchesPath("./src/auth/**", "src/auth/login.rs")).toBe(true);
  });
});

describe("matchesAny", () => {
  it("returns true when at least one pattern matches", () => {
    expect(
      matchesAny(["docs/**", "src/auth/**"], "src/auth/login.rs"),
    ).toBe(true);
  });

  it("returns false when no pattern matches", () => {
    expect(
      matchesAny(["docs/**", "src/auth/**"], "src/web/index.ts"),
    ).toBe(false);
  });

  it("returns false for an empty pattern list", () => {
    expect(matchesAny([], "src/auth/login.rs")).toBe(false);
  });
});
