import { useState } from 'react'

/**
 * Becomes true the first time `ready` is true and then stays true.
 *
 * Edit forms copy the record into their own state when they mount, so they must
 * mount with fresh data: a cached record shown before the background refetch lands
 * would never be replaced. Remounting the form on every data change (`key=`) or
 * hiding it while `isFetching` would fix that but throw away what the user has typed
 * if a refetch lands later. Waiting for the first fresh result, then never
 * unmounting, avoids both.
 */
export function useSettled(ready: boolean): boolean {
  const [settled, setSettled] = useState(false)
  // Setting state while rendering is React's supported way to derive state from props.
  if (ready && !settled) setSettled(true)
  return settled || ready
}
