import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { render, screen } from '@testing-library/react'
import { MemoryRouter, Route, Routes, useLocation } from 'react-router'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { RequireAuth } from './RequireAuth'

afterEach(() => vi.unstubAllGlobals())

function LoginProbe() {
  const location = useLocation()
  return <p>login page {location.search}</p>
}

function renderAt(path: string) {
  render(
    <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
      <MemoryRouter initialEntries={[path]}>
        <Routes>
          <Route path="/login" element={<LoginProbe />} />
          <Route element={<RequireAuth />}>
            <Route path="/loans" element={<p>secret loans</p>} />
          </Route>
        </Routes>
      </MemoryRouter>
    </QueryClientProvider>,
  )
}

describe('RequireAuth', () => {
  it('redirects to /login with the original path (and query) as ?next=', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn().mockResolvedValue(Response.json({ error: { code: 'unauthorized', message: 'no' } }, { status: 401 })),
    )
    renderAt('/loans?status=overdue')

    const probe = await screen.findByText(/login page/)
    expect(probe).toHaveTextContent('?next=%2Floans%3Fstatus%3Doverdue')
    expect(screen.queryByText('secret loans')).not.toBeInTheDocument()
  })

  it('renders the protected route for a logged-in user', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(Response.json({ id: 1, username: 'alice' })))
    renderAt('/loans')

    expect(await screen.findByText('secret loans')).toBeInTheDocument()
  })
})
