// One real `tm serve` in a scratch project: a fresh `git` repo, its own `TM_HOME`, `tm init`,
// then `tm serve` on an ephemeral loopback port serving the built web client at `/app/`.
//
// Nothing here touches the checkout the tests run from: the project, its `.tm/` state and
// `TM_HOME` all live in `mktemp`-style directories that are removed on stop (set
// `E2E_KEEP_TMP=1` to keep them for a post-mortem).
import { type ChildProcess, execFile, spawn } from "node:child_process";
import { createWriteStream, mkdtempSync, realpathSync, rmSync, type WriteStream } from "node:fs";
import os from "node:os";
import path from "node:path";
import { promisify } from "node:util";
import { DIST_DIR, resolveTmBin } from "./paths";

const execFileAsync = promisify(execFile);

const SERVING_LINE = /Serving the API at (http:\/\/[^\s]+)/;

export interface TmServer {
  /** The API origin, e.g. `http://127.0.0.1:53412` (no trailing slash). */
  apiURL: string;
  /** The web client, e.g. `http://127.0.0.1:53412/app/` (with the trailing slash). */
  appURL: string;
  /** The scratch git repo `tm serve` runs in (the project root). */
  repoDir: string;
  /** The scratch `TM_HOME`. */
  tmHome: string;
  /** Where the server's stdout and stderr are written. */
  logPath: string;
  /** The environment every `tm` call for this project runs with. */
  env: NodeJS.ProcessEnv;
  /** The `tm` binary in use. */
  tmBin: string;
  /** Run another `tm` command against this project (same cwd and env as the server). */
  runTm(args: string[]): Promise<{ stdout: string; stderr: string }>;
  /** Stop the server (SIGINT, then SIGKILL) and remove the scratch directories. */
  stop(): Promise<void>;
}

export interface StartTmServerOptions {
  /** Where to write the server's output. Defaults to a file in the scratch `TM_HOME`. */
  logPath?: string;
  /** Pass `--no-workers`: tickets are never worked, so nothing escalates on its own. */
  noWorkers?: boolean;
  /** How long to wait for the "Serving the API at" line and `/health`. */
  startTimeoutMs?: number;
}

// Every server this process started, so a crashed worker doesn't orphan one.
const live = new Set<ChildProcess>();
let exitHookInstalled = false;
function installExitHook(): void {
  if (exitHookInstalled) return;
  exitHookInstalled = true;
  process.on("exit", () => {
    for (const child of live) {
      try {
        child.kill("SIGKILL");
      } catch {
        // Already gone.
      }
    }
  });
}

function scratchDir(prefix: string): string {
  // realpath: on macOS os.tmpdir() is under /var, a symlink to /private/var, and tm reports the
  // resolved form.
  return realpathSync(mkdtempSync(path.join(os.tmpdir(), prefix)));
}

export async function startTmServer(options: StartTmServerOptions = {}): Promise<TmServer> {
  installExitHook();
  const tmBin = resolveTmBin().path;
  const repoDir = scratchDir("tm-e2e-repo-");
  const tmHome = scratchDir("tm-e2e-home-");
  const logPath = options.logPath ?? path.join(tmHome, "serve.log");
  const startTimeoutMs = options.startTimeoutMs ?? 30_000;

  const env: NodeJS.ProcessEnv = {
    ...process.env,
    TM_HOME: tmHome,
    TM_NOTIFY: "0",
    TM_TEST_MOCK_PROVIDER: "1",
  };
  delete env.TM_WEB_DIR;

  const cleanupDirs = () => {
    if (process.env.E2E_KEEP_TMP === "1") return;
    rmSync(repoDir, { recursive: true, force: true });
    rmSync(tmHome, { recursive: true, force: true });
  };

  const runTm = async (args: string[]) => {
    const { stdout, stderr } = await execFileAsync(tmBin, args, { cwd: repoDir, env });
    return { stdout, stderr };
  };

  let child: ChildProcess | undefined;
  let log: WriteStream | undefined;
  try {
    const git = (args: string[]) => execFileAsync("git", args, { cwd: repoDir, env });
    await git(["init", "-q"]);
    await git([
      "-c",
      "user.name=tm-e2e",
      "-c",
      "user.email=tm-e2e@localhost",
      "-c",
      "commit.gpgsign=false",
      "commit",
      "-q",
      "--allow-empty",
      "-m",
      "init",
    ]);
    await runTm(["init"]);

    log = createWriteStream(logPath);
    const serveArgs = ["serve", "--addr", "127.0.0.1:0", "--web-dir", DIST_DIR];
    if (options.noWorkers) serveArgs.push("--no-workers");
    child = spawn(tmBin, serveArgs, { cwd: repoDir, env, stdio: ["ignore", "pipe", "pipe"] });
    live.add(child);
    const started = child;
    started.once("exit", () => live.delete(started));

    const apiURL = await waitForServingLine(started, log, startTimeoutMs, logPath);
    await waitForHealth(apiURL, startTimeoutMs, logPath);
    const appURL = `${apiURL}/app/`;
    const app = await fetch(appURL);
    if (!app.ok || !(app.headers.get("content-type") ?? "").includes("text/html")) {
      throw new Error(`${appURL} answered ${app.status}, not the web client (see ${logPath})`);
    }

    const openLog = log;
    return {
      apiURL,
      appURL,
      repoDir,
      tmHome,
      logPath,
      env,
      tmBin,
      runTm,
      stop: async () => {
        await stopChild(started);
        openLog.end();
        cleanupDirs();
      },
    };
  } catch (error) {
    if (child) await stopChild(child);
    log?.end();
    cleanupDirs();
    throw error;
  }
}

function waitForServingLine(
  child: ChildProcess,
  log: WriteStream,
  timeoutMs: number,
  logPath: string,
): Promise<string> {
  return new Promise((resolve, reject) => {
    let seen = "";
    let settled = false;
    const finish = (fn: () => void) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      fn();
    };
    const onData = (chunk: Buffer) => {
      log.write(chunk);
      if (settled) return;
      seen += chunk.toString("utf8");
      const match = SERVING_LINE.exec(seen);
      if (match) finish(() => resolve(match[1].replace(/\/$/, "")));
    };
    // `tm` prints its notes on one stream or the other depending on the renderer; watch both.
    child.stdout?.on("data", onData);
    child.stderr?.on("data", onData);
    child.once("exit", (code, signal) =>
      finish(() =>
        reject(
          new Error(`tm serve exited (${signal ?? code}) before serving:\n${seen}\n(log: ${logPath})`),
        ),
      ),
    );
    const timer = setTimeout(
      () =>
        finish(() =>
          reject(new Error(`tm serve didn't print its address in ${timeoutMs}ms:\n${seen}`)),
        ),
      timeoutMs,
    );
  });
}

async function waitForHealth(apiURL: string, timeoutMs: number, logPath: string): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  let last: unknown;
  while (Date.now() < deadline) {
    try {
      const res = await fetch(`${apiURL}/health`);
      if (res.ok) return;
      last = `HTTP ${res.status}`;
    } catch (error) {
      last = error;
    }
    await new Promise((r) => setTimeout(r, 100));
  }
  throw new Error(`${apiURL}/health never answered 200 (last: ${String(last)}; log: ${logPath})`);
}

async function stopChild(child: ChildProcess): Promise<void> {
  if (child.exitCode !== null || child.signalCode !== null) return;
  const exited = new Promise<void>((resolve) => child.once("exit", () => resolve()));
  const within = (ms: number) =>
    Promise.race([exited.then(() => true), new Promise<boolean>((r) => setTimeout(() => r(false), ms))]);
  // `tm serve` shuts down gracefully on ctrl-c only, and graceful shutdown waits for open SSE
  // streams (a still-open page holds one), so SIGKILL if it hasn't gone shortly.
  child.kill("SIGINT");
  if (await within(1_500)) return;
  child.kill("SIGKILL");
  await within(5_000);
}
