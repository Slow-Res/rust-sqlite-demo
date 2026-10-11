import { useState } from 'react'
import type { PickerOption } from '../components/Picker'

/**
 * Remembers the chosen option (id and label), so it stays selectable after the
 * search results change. `prefilledId` comes from the URL; its label is resolved
 * later by fetching the record, which `selected` accepts as `prefilled`.
 */
export function usePickerSelection(prefilledId: number | null) {
  const [id, setId] = useState(prefilledId)
  const [choice, setChoice] = useState<PickerOption | null>(null)

  return {
    id,
    /** The option to keep in the list regardless of the current search. */
    selected: (prefilled: PickerOption | undefined): PickerOption | undefined =>
      choice ?? (prefilled && prefilled.id === id ? prefilled : undefined),
    select: (nextId: number | null, options: PickerOption[]) => {
      setId(nextId)
      setChoice(options.find((option) => option.id === nextId) ?? null)
    },
  }
}
