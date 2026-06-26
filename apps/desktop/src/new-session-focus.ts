/** DOM id of the session-name input in the New Session dialog, shared by the
 * field and the on-open focus effect so the latter can target it directly. */
export const NAME_INPUT_ID = "new-session-name";

/**
 * Where initial focus should land when the New Session dialog opens.
 *
 * Opened from a project "+" the project is already chosen (it's implied by
 * which "+" was clicked), so focus jumps straight to the session-name field —
 * you can name and create keyboard-only. Opened from the global "+ New session"
 * no project is implied, so focus leads with the body's first focusable (the
 * project picker) instead.
 *
 * `initialProjectId` is the prop the dialog receives: a non-empty value marks a
 * per-project open, "" / undefined the global one.
 */
export function shouldFocusNameField(
  initialProjectId: string | undefined,
): boolean {
  return (initialProjectId ?? "") !== "";
}
