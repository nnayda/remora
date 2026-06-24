/** Longest id the protocol accepts (mirror of remora-protocol MAX_ID_LEN, ADR-0004). */
const MAX_ID_LEN = 64;
const SLUG = /^[a-z0-9-]+$/;

/**
 * Mirror of the ADR-0004 id grammar (`[a-z0-9-]`, 1–64 chars). The Rust bridge
 * (`ProjectId::new`) is the authority — this only gates the dialog's submit
 * button for instant feedback; a mismatch still surfaces as a bridge
 * `invalidId` error.
 */
export function isValidSlug(value: string): boolean {
  return value.length > 0 && value.length <= MAX_ID_LEN && SLUG.test(value);
}

/**
 * Canonicalize typed id input toward the slug grammar by lowercasing, so an
 * autocapitalized first letter (mobile/touch keyboards, macOS autocaps) no
 * longer gets rejected on every session/entity creation (#80). Apply at the
 * input `onChange` so the field visibly shows the lowercased form as the user
 * types. Only case is touched — other out-of-grammar characters are left for
 * `isValidSlug` to flag, preserving that feedback. Case is a canonicalization
 * choice, not a parsing requirement (ADR-0004), so the strict grammar and the
 * Rust bridge stay the authority.
 */
export function normalizeSlugInput(value: string): string {
  return value.toLowerCase();
}
