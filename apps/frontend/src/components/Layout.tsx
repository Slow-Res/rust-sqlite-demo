import { useMutation, useQueryClient } from '@tanstack/react-query'
import { Link, NavLink, Outlet, useNavigate } from 'react-router'
import { logout, meKey } from '../api/auth'
import { useCurrentUser } from '../auth/useCurrentUser'

/** App shell: top navigation plus the routed page. */
export function Layout() {
  const { user, isLoading } = useCurrentUser()
  const queryClient = useQueryClient()
  const navigate = useNavigate()

  const logoutMutation = useMutation({
    mutationFn: logout,
    meta: { skipUnauthorizedRedirect: true },
    onSettled: () => {
      queryClient.clear()
      queryClient.setQueryData(meKey, null)
      void navigate('/books')
    },
  })

  return (
    <>
      <header className="container">
        <nav>
          <ul>
            <li>
              <Link to="/" className="brand">
                <strong>Library</strong>
              </Link>
            </li>
          </ul>
          <ul>
            <li>
              <NavLink to="/books">Books</NavLink>
            </li>
            {user && (
              <>
                <li>
                  <NavLink to="/members">Members</NavLink>
                </li>
                <li>
                  <NavLink to="/loans">Loans</NavLink>
                </li>
              </>
            )}
          </ul>
          <ul>
            {isLoading ? null : user ? (
              <>
                <li>{user.username}</li>
                <li>
                  <button
                    type="button"
                    className="secondary outline"
                    disabled={logoutMutation.isPending}
                    onClick={() => logoutMutation.mutate()}
                  >
                    Log out
                  </button>
                </li>
              </>
            ) : (
              <li>
                <NavLink to="/login">Log in</NavLink>
              </li>
            )}
          </ul>
        </nav>
      </header>
      <main className="container">
        <Outlet />
      </main>
    </>
  )
}
