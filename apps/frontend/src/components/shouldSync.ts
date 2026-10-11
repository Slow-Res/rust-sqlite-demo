/**
 * Whether the input should adopt the committed (URL) value: never while the user is typing
 * (it would overwrite newer text), and not when the two only differ by surrounding whitespace.
 */
export function shouldSync(text: string, committed: string, typing: boolean): boolean {
  return !typing && text.trim() !== committed.trim()
}
