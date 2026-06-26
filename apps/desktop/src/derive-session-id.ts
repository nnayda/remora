/**
 * TypeScript port of `remora_core::naming::derive_session_id` (ADR-0015, #124).
 *
 * Must produce byte-identical output to the Rust implementation. The cross-
 * language contract is enforced by the shared fixture at
 * crates/remora-protocol/tests/fixtures/derive-session-id-vectors.json.
 */

const MAX_ID_LEN = 64;
const SLUG = /^[a-z0-9-]+$/;

/**
 * FNV-1a 32-bit hash over the UTF-8 bytes of `input`, returned as an 8-digit
 * lowercase hex string. Mirrors Rust's `wrapping_mul` via `Math.imul(...) >>> 0`.
 */
function fnv1a32Hex(input: string): string {
  let h = 0x811c_9dc5;
  for (const byte of new TextEncoder().encode(input)) {
    h ^= byte;
    h = Math.imul(h, 0x0100_0193) >>> 0; // keep u32; mirrors wrapping_mul
  }
  return (h >>> 0).toString(16).padStart(8, "0");
}

/**
 * Derive the internal `session_id` slug from a worktree's branch name.
 *
 * - `remora/<slug>` where `<slug>` is a valid SessionId (`[a-z0-9-]+`, 1..=64)
 *   → returns `<slug>` (no hash, round-trip safe).
 * - Any other branch → slugify (lowercase; non-`[a-z0-9]` → `-`; collapse runs
 *   of `-`; trim; empty → `x`), append `-<fnv1a32hex>`, validate, return or null.
 *
 * Returns `null` when the result would exceed 64 chars or contain invalid chars
 * (mirrors Rust returning `None`).
 */
export function deriveSessionId(branch: string): string | null {
  // Fast path: remora/<slug> round-trips without a hash.
  if (branch.startsWith("remora/")) {
    const rest = branch.slice("remora/".length);
    if (rest.length >= 1 && rest.length <= MAX_ID_LEN && SLUG.test(rest)) {
      return rest;
    }
  }

  // Slugify: non-ASCII-alphanumeric → '-', then lowercase, collapse runs, trim,
  // fallback 'x'. The replace MUST precede toLowerCase to mirror Rust's per-char
  // `is_ascii_alphanumeric() ? to_ascii_lowercase() : '-'`: JS `toLowerCase()`
  // special-case-folds a few non-ASCII codepoints into ASCII (e.g. `İ` U+0130 →
  // `i`, KELVIN SIGN U+212A → `k`), so lowercasing first would diverge from Rust.
  const lowered = branch.replace(/[^A-Za-z0-9]/g, "-").toLowerCase();
  const parts = lowered.split("-").filter((s) => s.length > 0);
  const slug = parts.length > 0 ? parts.join("-") : "x";

  // FNV-1a/32 over the raw branch UTF-8 bytes (same as Rust's `branch.as_bytes()`).
  const hex = fnv1a32Hex(branch);
  const candidate = `${slug}-${hex}`;

  if (candidate.length <= MAX_ID_LEN && SLUG.test(candidate)) {
    return candidate;
  }
  return null;
}
