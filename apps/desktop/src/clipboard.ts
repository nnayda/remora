import { writeText } from "@tauri-apps/plugin-clipboard-manager";

/** A function that writes `text` to the host system clipboard. */
export type ClipboardWriter = (text: string) => Promise<void>;

/** The one seam through which the terminal reaches the OS clipboard. Wraps the
 * Tauri clipboard-manager plugin so callers (and tests) depend on a single small
 * function instead of the plugin surface. */
export const writeClipboard: ClipboardWriter = (text) => writeText(text);
