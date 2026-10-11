/** Parses a route param as a positive integer; `null` when it is not one. */
export function parsePositiveInt(value: string | null | undefined): number | null {
  if (value === null || value === undefined || !/^[1-9]\d*$/.test(value)) return null
  const parsed = Number(value)
  return Number.isSafeInteger(parsed) ? parsed : null
}

/** Formats an ISO timestamp as `YYYY-MM-DD` in the viewer's local time. */
export function formatDate(iso: string): string {
  const date = new Date(iso)
  const month = String(date.getMonth() + 1).padStart(2, '0')
  const day = String(date.getDate()).padStart(2, '0')
  return `${date.getFullYear()}-${month}-${day}`
}

/**
 * Only same-site paths are valid post-login targets (prevents open redirects).
 * Browsers treat `\` as `/`, so `/\evil.com` would become the protocol-relative `//evil.com`;
 * control characters (tab, newline) are stripped by URL parsers and can hide a second slash.
 */
export function safeNextPath(next: string | null): string | null {
  if (next === null || !next.startsWith('/')) return null
  if (next[1] === '/') return null
  if (next.includes('\\')) return null
  for (let i = 0; i < next.length; i++) {
    const code = next.charCodeAt(i)
    if (code < 0x20 || code === 0x7f) return null
  }
  return next
}
