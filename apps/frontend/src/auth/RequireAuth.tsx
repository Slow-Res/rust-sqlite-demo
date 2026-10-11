import { Navigate, Outlet, useLocation } from 'react-router'
import { ErrorMessage } from '../components/ErrorMessage'
import { useCurrentUser } from './useCurrentUser'

/** Route wrapper: renders the nested routes only for a logged-in librarian. */
export function RequireAuth() {
  const { user, isLoading, error } = useCurrentUser()
  const location = useLocation()

  if (isLoading) return <p aria-busy="true">Loading…</p>
  if (error) return <ErrorMessage error={error} />
  if (user === null) {
    const next = encodeURIComponent(location.pathname + location.search)
    return <Navigate to={`/login?next=${next}`} replace />
  }
  return <Outlet />
}
