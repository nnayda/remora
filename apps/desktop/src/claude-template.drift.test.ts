import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { expect, it } from "vitest";
import { claudeMarkerTemplate } from "./config-editor-model";

it("template script matches the canonical contrib script", () => {
  const path = fileURLToPath(
    new URL(
      "../../../contrib/agent-hooks/claude-code/remora-notify.sh",
      import.meta.url,
    ),
  );
  const canonical = readFileSync(path, "utf8");
  // Tolerate a single trailing newline (the contrib file ends with one; the
  // embedded template may not) — see marker.rs's existing trim.
  const norm = (s: string) => s.replace(/\n$/, "");
  expect(norm(claudeMarkerTemplate().provision.content)).toBe(norm(canonical));
});
