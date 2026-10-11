import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { render, screen } from '@testing-library/react'
import { MemoryRouter } from 'react-router'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { LoansPage } from './LoansPage'

afterEach(() => vi.unstubAllGlobals())

function renderAt(url: string, body: unknown) {
  vi.stubGlobal('fetch', vi.fn().mockResolvedValue(Response.json(body)))
  render(
    <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
      <MemoryRouter initialEntries={[url]}>
        <LoansPage />
      </MemoryRouter>
    </QueryClientProvider>,
  )
}

describe('LoansPage', () => {
  it('marks only the active status tab with aria-current', async () => {
    renderAt('/loans?status=overdue', { items: [], page: 1, per_page: 20, total: 0 })
    await screen.findByText('No loans found.')

    expect(screen.getByRole('button', { name: 'Overdue' })).toHaveAttribute('aria-current', 'page')
    for (const name of ['Active', 'Returned', 'All']) {
      expect(screen.getByRole('button', { name })).not.toHaveAttribute('aria-current')
    }
  })

  it('offers a way back to page 1 when the page is out of range', async () => {
    renderAt('/loans?page=99', { items: [], page: 99, per_page: 20, total: 3 })

    const link = await screen.findByRole('link', { name: 'Go to page 1' })
    expect(link).toHaveAttribute('href', '/loans')
  })
})
