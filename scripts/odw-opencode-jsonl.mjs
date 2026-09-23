// Open Dynamic Workflow 0.6.1's OpenCode adapter recognizes top-level event.text
// but not OpenCode's actual JSONL event.part.text. Stream the CLI's events and
// print ONLY the final assistant text so schema validation sees the real result,
// not an earlier JSON object in tool output. No credentials are read or logged.
import { spawn } from "node:child_process";
import { createInterface } from "node:readline";

const child = spawn("opencode", ["run", "--format", "json", ...process.argv.slice(2)], {
  stdio: ["ignore", "pipe", "inherit"],
  env: process.env,
});

let finalText = "";
let fallbackText = "";
let parseErrors = 0;
const lines = createInterface({ input: child.stdout, crlfDelay: Infinity });
for await (const line of lines) {
  if (!line.trim()) continue;
  let event;
  try {
    event = JSON.parse(line);
  } catch {
    parseErrors++;
    continue;
  }
  if (event.type !== "text" || typeof event.part?.text !== "string") continue;
  fallbackText += event.part.text;
  if (event.part.metadata?.openai?.phase === "final_answer") {
    finalText += event.part.text;
  }
}

const exitCode = await new Promise((resolve) => {
  child.once("error", (error) => {
    console.error(`OpenCode could not start: ${error.message}`);
    resolve(1);
  });
  child.once("close", (code) => resolve(code ?? 1));
});
if (exitCode !== 0) process.exit(exitCode);
const answer = finalText || fallbackText;
if (!answer.trim()) {
  console.error(`OpenCode emitted no assistant text (${parseErrors} malformed event lines).`);
  process.exit(1);
}
process.stdout.write(`${answer.trim()}\n`);
