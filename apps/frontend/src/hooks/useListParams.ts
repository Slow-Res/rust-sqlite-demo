import { useSearchParams } from 'react-router'
import { parsePositiveInt } from '../utils'

/**
 * List state (`page`, search and filters) lives in the URL so refresh and the
 * back button work. Changing anything but the page resets to page 1.
 */
export function useListParams() {
  const [searchParams, setSearchParams] = useSearchParams()

  const page = parsePositiveInt(searchParams.get('page')) ?? 1
  const get = (key: string) => searchParams.get(key) ?? ''

  const update = (patch: Record<string, string | undefined>, options: { replace?: boolean } = {}) => {
    setSearchParams(
      (previous) => {
        const next = new URLSearchParams(previous)
        for (const [key, value] of Object.entries(patch)) {
          if (value === undefined || value === '') next.delete(key)
          else next.set(key, value)
        }
        if (!('page' in patch)) next.delete('page')
        return next
      },
      { replace: options.replace },
    )
  }

  return { page, get, update }
}
