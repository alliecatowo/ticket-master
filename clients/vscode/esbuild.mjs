// Bundles the extension entry point into a single CommonJS file VS Code can
// require() directly. @ticketmaster/client is ESM-only ("type": "module",
// import-only exports), while the extension's runtime is CommonJS, so the
// SDK has to be bundled in rather than left as a runtime require().
import * as esbuild from "esbuild";

await esbuild.build({
  entryPoints: ["src/extension.ts"],
  bundle: true,
  outfile: "out/extension.js",
  format: "cjs",
  platform: "node",
  target: "node18",
  sourcemap: true,
  external: ["vscode"],
});
