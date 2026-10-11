import { useEffect, useRef, useState } from 'react'
import { useDebounce } from '../hooks/useDebounce'
import { shouldSync } from './shouldSync'

const DEBOUNCE_MS = 300

interface SearchBoxProps {
  label: string
  /** The committed (URL) value. */
  value: string
  onSearch: (value: string) => void
}

/** Text input that reports changes via `onSearch` 300 ms after typing stops. */
export function SearchBox({ label, value, onSearch }: SearchBoxProps) {
  const [text, setText] = useState(value)
  const debounced = useDebounce(text, DEBOUNCE_MS)

  const typing = text !== debounced

  // Follow external changes (back/forward navigation) by adjusting state while rendering.
  const [seenValue, setSeenValue] = useState(value)
  if (value !== seenValue) {
    setSeenValue(value)
    if (shouldSync(text, value, typing)) setText(value)
  }

  // A settled keystroke triggers a search; later changes to `value` or `onSearch` must not.
  const latest = useRef({ value, onSearch })
  useEffect(() => {
    latest.current = { value, onSearch }
  })
  useEffect(() => {
    if (debounced !== latest.current.value) latest.current.onSearch(debounced)
  }, [debounced])

  return (
    <input
      type="search"
      aria-label={label}
      placeholder={label}
      value={text}
      onChange={(event) => setText(event.target.value)}
    />
  )
}
