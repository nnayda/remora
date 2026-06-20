/** Best-effort human message from an unknown thrown value — a typed
 * `BridgeError` (every arm carries `message`) or anything else. Shared by the
 * settings forms to render a rejected mutation inline. */
export function formErrorMessage(error: unknown): string {
  if (typeof error === "object" && error !== null && "message" in error) {
    // Only trust a non-empty string; a null/empty/non-string message would
    // otherwise surface as "null"/"undefined"/"" in the alert.
    const message = (error as { message: unknown }).message;
    if (typeof message === "string" && message.trim() !== "") return message;
  }
  return "Something went wrong.";
}
