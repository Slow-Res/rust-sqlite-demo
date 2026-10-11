import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { act, render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { createMemoryRouter, RouterProvider } from 'react-router'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { bookKey } from '../api/books'
import { BookFormPage } from './BookFormPage'

afterEach(() => vi.unstubAllGlobals())

const book = (title: string) => ({
  id: 7,
  title,
  author: 'Frank Herbert',
  isbn: null,
  published_year: null,
  total_copies: 2,
  available_copies: 2,
  created_at: '',
  updated_at: '',
})

function renderEdit(queryClient: QueryClient) {
  const router = createMemoryRouter([{ path: '/books/:id/edit', element: <BookFormPage /> }], {
    initialEntries: ['/books/7/edit'],
  })
  render(
    <QueryClientProvider client={queryClient}>
      <RouterProvider router={router} />
    </QueryClientProvider>,
  )
}

describe('BookFormPage (edit)', () => {
  it('waits for fresh data instead of editing a stale cached record', async () => {
    vi.stubGlobal('fetch', vi.fn(() => Promise.resolve(Response.json(book('New title')))))
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false, staleTime: 0 } } })
    queryClient.setQueryData(bookKey(7), book('Old title'))
    renderEdit(queryClient)

    // The cached record is not offered for editing; the fresh one is.
    expect(screen.queryByDisplayValue('Old title')).not.toBeInTheDocument()
    expect(await screen.findByDisplayValue('New title')).toBeInTheDocument()
  })

  it('keeps what the user typed when a later refetch lands', async () => {
    vi.stubGlobal('fetch', vi.fn(() => Promise.resolve(Response.json(book('Dune')))))
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } })
    renderEdit(queryClient)

    const title = await screen.findByLabelText('Title')
    await userEvent.type(title, ' (typed)')
    await act(() => queryClient.invalidateQueries({ queryKey: bookKey(7) }))

    expect(screen.getByLabelText('Title')).toHaveValue('Dune (typed)')
  })
})
