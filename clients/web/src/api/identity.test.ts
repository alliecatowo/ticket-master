import { describe, expect, it } from "vitest";
import {
  actorFor,
  DEFAULT_HANDLE,
  HANDLE_STORAGE_KEY,
  loadHandle,
  MAX_HANDLE_LENGTH,
  normalizeHandle,
  saveHandle,
  type HandleStorage,
} from "./identity";

function memory(initial: Record<string, string> = {}): HandleStorage & { data: Record<string, string> } {
  const data = { ...initial };
  return {
    data,
    getItem: (k) => (k in data ? data[k] : null),
    setItem: (k, v) => {
      data[k] = v;
    },
  };
}

describe("identity", () => {
  it("defaults to human:web", () => {
    expect(loadHandle(memory())).toBe(DEFAULT_HANDLE);
    expect(actorFor(loadHandle(memory()))).toBe("human:web");
  });

  it("normalizes what people type", () => {
    expect(normalizeHandle("  Allie ")).toBe("Allie");
    expect(normalizeHandle("human:allie")).toBe("allie");
    expect(normalizeHandle("Human: Sam  Jones")).toBe("Sam-Jones");
    expect(normalizeHandle("human:human:x")).toBe("x");
    expect(normalizeHandle("a\u0007b")).toBe("a-b");
    expect(normalizeHandle("   ")).toBe(DEFAULT_HANDLE);
    expect(normalizeHandle("human:")).toBe(DEFAULT_HANDLE);
    expect([...normalizeHandle("x".repeat(100))]).toHaveLength(MAX_HANDLE_LENGTH);
  });

  it("always produces a human actor, so accept/reject/retry are never refused as non-human", () => {
    for (const input of ["", "system", "agent:claude/a1", "human:", "  "]) {
      expect(actorFor(input)).toMatch(/^human:.+/);
    }
  });

  it("saves the normalized handle and reads it back (including an older human:-prefixed value)", () => {
    const storage = memory();
    expect(saveHandle(" human:allie ", storage)).toBe("allie");
    expect(storage.data[HANDLE_STORAGE_KEY]).toBe("allie");
    expect(loadHandle(storage)).toBe("allie");
    expect(loadHandle(memory({ [HANDLE_STORAGE_KEY]: "human:web" }))).toBe("web");
  });

  it("survives storage that throws", () => {
    const broken: HandleStorage = {
      getItem: () => {
        throw new Error("denied");
      },
      setItem: () => {
        throw new Error("denied");
      },
    };
    expect(loadHandle(broken)).toBe(DEFAULT_HANDLE);
    expect(saveHandle("sam", broken)).toBe("sam");
  });
});
