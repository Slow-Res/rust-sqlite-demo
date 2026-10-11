import { render, screen } from '@testing-library/react'
import { MemoryRouter } from 'react-router'
import { describe, expect, it } from 'vitest'
import { EmptyList } from './EmptyList'

function renderAt(url: string, page: number) {
  render(
    <MemoryRouter initialEntries={[url]}>
      <EmptyList message="No books found." page={page} />
    </MemoryRouter>,
  )
}

describe('EmptyList', () => {
  it('shows only the message on page 1', () => {
    renderAt('/books', 1)
    expect(screen.getByText('No books found.')).toBeInTheDocument()
    expect(screen.queryByRole('link')).not.toBeInTheDocument()
  })

  it('links back to page 1, keeping the other filters, when out of range', () => {
    renderAt('/books?q=dune&page=99', 99)
    expect(screen.getByRole('link', { name: 'Go to page 1' })).toHaveAttribute('href', '/books?q=dune')
  })
})
