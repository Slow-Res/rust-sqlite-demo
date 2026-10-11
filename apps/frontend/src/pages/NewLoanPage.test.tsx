import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { MemoryRouter } from 'react-router'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { NewLoanPage } from './NewLoanPage'

afterEach(() => vi.unstubAllGlobals())

const book = {
  id: 7,
  title: 'Dune',
  author: 'Frank Herbert',
  isbn: null,
  published_year: null,
  total_copies: 2,
  available_copies: 2,
  created_at: '',
  updated_at: '',
}
const member = { id: 3, name: 'Ada', email: 'ada@example.com', active_loans: 0, created_at: '', updated_at: '' }

const otherBook = { ...book, id: 8, title: 'Zzz Tales' }

const page = (items: unknown[], total = items.length) => ({ items, page: 1, per_page: 20, total })

/** The empty search returns Dune; any search text returns a different book. */
function stubApi(bookTotal?: number) {
  vi.stubGlobal(
    'fetch',
    vi.fn((url: string) => {
      const params = new URL(url, 'http://x').searchParams
      if (url.startsWith('/api/books')) return Promise.resolve(Response.json(page(params.get('q') ? [otherBook] : [book], bookTotal)))
      return Promise.resolve(Response.json(page([member])))
    }),
  )
}

function renderPage() {
  render(
    <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
      <MemoryRouter initialEntries={['/loans/new']}>
        <NewLoanPage />
      </MemoryRouter>
    </QueryClientProvider>,
  )
}

async function waitForOption(name: string, label: RegExp) {
  const select = await screen.findByRole('combobox', { name })
  await screen.findByRole('option', { name: label })
  return select
}
const bookSelect = () => waitForOption('Book', /Dune/)
const memberSelect = () => waitForOption('Member', /Ada/)

describe('NewLoanPage pickers', () => {
  it('disables submit until both a book and a member are chosen', async () => {
    stubApi()
    renderPage()
    const submit = screen.getByRole('button', { name: 'Lend book' })
    expect(submit).toBeDisabled()

    await userEvent.selectOptions(await bookSelect(), '7')
    expect(submit).toBeDisabled()
    await userEvent.selectOptions(await memberSelect(), '3')
    expect(submit).toBeEnabled()
  })

  it('keeps the selected book when the search no longer matches it', async () => {
    stubApi()
    renderPage()
    const select = await bookSelect()
    await userEvent.selectOptions(select, '7')

    await userEvent.type(screen.getByRole('searchbox', { name: 'Search books' }), 'zzz')

    // The new results (a different book) must have rendered before we assert.
    await screen.findByRole('option', { name: /Zzz Tales/ })
    expect(select).toHaveValue('7')
    expect(screen.getByRole('option', { name: /Dune/ })).toBeInTheDocument()
  })

  it('hints when the book results are truncated', async () => {
    stubApi(25)
    renderPage()
    await bookSelect()
    expect(await screen.findByText('Showing the first 1 of 25 — type to narrow the search')).toBeInTheDocument()
  })

  it('shows no hint when everything fits', async () => {
    stubApi()
    renderPage()
    await bookSelect()
    expect(screen.queryByText(/type to narrow the search/)).not.toBeInTheDocument()
  })
})
