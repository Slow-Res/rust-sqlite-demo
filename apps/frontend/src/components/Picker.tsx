import { useId } from 'react'
import { SearchBox } from './SearchBox'

export interface PickerOption {
  id: number
  label: string
}

interface PickerProps {
  label: string
  search: string
  onSearch: (value: string) => void
  options: PickerOption[]
  selectedId: number | null
  onSelect: (id: number | null) => void
  isLoading: boolean
  /** Search results returned vs. matching in total; a hint shows when results are cut off. */
  shown?: number
  total?: number
  error?: string
}

/** A search box plus a select of the matching options (used on the new-loan form). */
export function Picker({
  label,
  search,
  onSearch,
  options,
  selectedId,
  onSelect,
  isLoading,
  shown,
  total,
  error,
}: PickerProps) {
  const errorId = useId()
  return (
    <fieldset>
      <legend>{label}</legend>
      <SearchBox label={`Search ${label.toLowerCase()}s`} value={search} onSearch={onSearch} />
      <select
        aria-label={label}
        aria-busy={isLoading}
        aria-invalid={error ? true : undefined}
        aria-describedby={error ? errorId : undefined}
        value={selectedId ?? ''}
        onChange={(event) => onSelect(event.target.value === '' ? null : Number(event.target.value))}
      >
        <option value="">Select a {label.toLowerCase()}…</option>
        {options.map((option) => (
          <option key={option.id} value={option.id}>
            {option.label}
          </option>
        ))}
      </select>
      {shown !== undefined && total !== undefined && total > shown ? (
        <small aria-live="polite">
          Showing the first {shown} of {total} — type to narrow the search
        </small>
      ) : null}
      {error ? (
        <small id={errorId} className="error">
          {error}
        </small>
      ) : null}
    </fieldset>
  )
}
