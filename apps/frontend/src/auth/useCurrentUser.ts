import { useQuery } from '@tanstack/react-query'
import { fetchCurrentUser, meKey } from '../api/auth'

/** The logged-in librarian (`user: null` when logged out) and whether we are still finding out. */
export function useCurrentUser() {
  const query = useQuery({ queryKey: meKey, queryFn: fetchCurrentUser, staleTime: Infinity })
  return { user: query.data ?? null, isLoading: query.isPending, error: query.error }
}
