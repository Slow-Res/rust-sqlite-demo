import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { render, screen } from '@testing-library/react'
import { MemoryRouter } from 'react-router'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { Layout } from './Layout'

afterEach(() => vi.unstubAllGlobals())

function renderLayout() {
  render(
    <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
      <MemoryRouter>
        <Layout />
      </MemoryRouter>
    </QueryClientProvider>,
  )
}

describe('Layout', () => {
  it('hides the auth area until the current user has resolved', async () => {
    let respond: (response: Response) => void = () => {}
    vi.stubGlobal('fetch', vi.fn(() => new Promise<Response>((resolve) => (respond = resolve))))
    renderLayout()

    expect(screen.queryByRole('link', { name: 'Log in' })).not.toBeInTheDocument()
    respond(Response.json({ error: { code: 'unauthorized', message: 'no' } }, { status: 401 }))
    expect(await screen.findByRole('link', { name: 'Log in' })).toBeInTheDocument()
  })

  it('shows the username once the user has resolved', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(Response.json({ id: 1, username: 'admin' })))
    renderLayout()

    expect(await screen.findByText('admin')).toBeInTheDocument()
    expect(screen.queryByRole('link', { name: 'Log in' })).not.toBeInTheDocument()
  })
})
