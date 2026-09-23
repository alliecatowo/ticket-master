// Where things are: the web client's root, its built `dist/`, and the `tm` binary to serve it.
//
// `package.json` has `"type": "module"`, so there is no `__dirname`; every path is derived from
// this file's own URL instead of `process.cwd()`, so the harness works no matter where
// `playwright test` was started from.
import { execFileSync } from "node:child_process";
import { existsSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

/** `clients/web/`, absolute. */
export const WEB_ROOT = fileURLToPath(new URL("../../", import.meta.url));

/** `clients/web/dist/`, absolute: what `pnpm build` writes and `tm serve --web-dir` serves. */
export const DIST_DIR = path.join(WEB_ROOT, "dist");

/** Throws with the fix in the message when `pnpm build` hasn't been run. */
export function assertDistBuilt(): void {
  if (!existsSync(path.join(DIST_DIR, "index.html"))) {
    throw new Error(
      `No web build at ${DIST_DIR}. Run \`pnpm -C clients/web build\` first ` +
        "(`pnpm -C clients/web e2e` does this for you).",
    );
  }
}

export interface ResolvedTmBin {
  path: string;
  /** Which rule found it, for the log line. */
  source: string;
}

/**
 * Find the `tm` binary, in order:
 * 1. `$TM_BIN`, when set (must exist).
 * 2. This checkout's `target/debug/tm` (a worktree that has built its own binary).
 * 3. The primary checkout's `target/debug/tm`, found through `git rev-parse --git-common-dir`,
 *    so a worktree without its own `target/` still finds the shared build.
 * 4. `tm` on `$PATH`.
 */
export function resolveTmBin(): ResolvedTmBin {
  const fromEnv = process.env.TM_BIN;
  if (fromEnv) {
    if (!existsSync(fromEnv)) throw new Error(`TM_BIN=${fromEnv} does not exist`);
    return { path: fromEnv, source: "$TM_BIN" };
  }

  const local = path.resolve(WEB_ROOT, "../../target/debug/tm");
  if (existsSync(local)) return { path: local, source: "this checkout's target/debug" };

  try {
    const commonDir = execFileSync(
      "git",
      ["rev-parse", "--path-format=absolute", "--git-common-dir"],
      { cwd: WEB_ROOT, encoding: "utf8", stdio: ["ignore", "pipe", "ignore"] },
    ).trim();
    const primary = path.resolve(commonDir, "../target/debug/tm");
    if (existsSync(primary)) return { path: primary, source: "the primary checkout's target/debug" };
  } catch {
    // Not a git checkout; fall through to $PATH.
  }

  try {
    const onPath = execFileSync("which", ["tm"], {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "ignore"],
    }).trim();
    if (onPath) return { path: onPath, source: "$PATH" };
  } catch {
    // `which` exits non-zero when nothing is found.
  }

  throw new Error(
    "No tm binary found. Build one with `mise run build`, or set TM_BIN to its path.",
  );
}
