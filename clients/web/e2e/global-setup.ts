// Runs once before any worker starts: fail fast, with the fix in the message, when the web
// build or the `tm` binary is missing, instead of every worker failing the same way.
import { assertDistBuilt, DIST_DIR, resolveTmBin } from "./helpers/paths";

export default function globalSetup(): void {
  assertDistBuilt();
  const tm = resolveTmBin();
  console.log(`[e2e] tm: ${tm.path} (${tm.source})`);
  console.log(`[e2e] web: ${DIST_DIR}`);
}
