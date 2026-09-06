// Mock fixture: enough JavaScript to judge the highlighter.
import { readFile } from "node:fs/promises";

const FIXTURE_DIR = "/tmp/opengrok-mock-fixtures";
const retries = 3;

/**
 * Read a fixture and describe it. Template literal, async/await,
 * optional chaining and a regex all appear on purpose.
 */
export async function describe(name) {
  const path = `${FIXTURE_DIR}/${name}`;
  const bytes = await readFile(path);
  const kind = /\.(png|mp4|pdf)$/.exec(name)?.[1] ?? "text";
  return { name, size: bytes.length, kind, ok: bytes.length > 0 };
}

for (let i = 0; i < retries; i++) {
  if (i === retries - 1) console.log("done", i);
}
