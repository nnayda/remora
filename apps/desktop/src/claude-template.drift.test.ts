import { expect, it } from "vitest";
import canonical from "../../../contrib/agent-hooks/claude-code/remora-notify.sh?raw";
import { claudeMarkerTemplate } from "./config-editor-model";

it("template script matches the canonical contrib script", () => {
  // Tolerate a single trailing newline (the contrib file ends with one; the
  // embedded template may not) — see marker.rs's existing trim.
  const norm = (s: string) => s.replace(/\n$/, "");
  expect(norm(claudeMarkerTemplate().provision.content)).toBe(norm(canonical));
});
