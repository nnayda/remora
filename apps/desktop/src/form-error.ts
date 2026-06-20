/** Best-effort human message from an unknown thrown value — a typed
 * `BridgeError` (every arm carries `message`) or anything else. Shared by the
 * settings forms to render a rejected mutation inline. */
export function formErrorMessage(error: unknown): string {
  if (typeof error === "object" && error !== null && "message" in error) {
    return String((error as { message: unknown }).message);
  }
  return "Something went wrong.";
}
