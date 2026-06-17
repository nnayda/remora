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
