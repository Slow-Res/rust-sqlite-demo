import { fireEvent, render, screen } from '@testing-library/react'
import { MemoryRouter, useLocation } from 'react-router'
import { describe, expect, it } from 'vitest'
import { useListParams } from './useListParams'

function Probe() {
  const { page, get, update } = useListParams()
  const location = useLocation()
  return (
    <>
      <output data-testid="state">{`page=${page} q=${get('q')}`}</output>
      <output data-testid="search">{location.search}</output>
      <button onClick={() => update({ page: '3' })}>go to page 3</button>
      <button onClick={() => update({ q: 'dune' })}>search</button>
      <button onClick={() => update({ q: '' })}>clear search</button>
    </>
  )
}

const renderAt = (path: string) =>
  render(
    <MemoryRouter initialEntries={[path]}>
      <Probe />
    </MemoryRouter>,
  )

describe('useListParams', () => {
  it('reads page and filters from the URL, defaulting and sanitising the page', () => {
    renderAt('/books?q=x&page=2')
    expect(screen.getByTestId('state')).toHaveTextContent('page=2 q=x')
  })

  it.each(['0', '-1', 'abc', '1.5'])('falls back to page 1 for page=%s', (page) => {
    renderAt(`/books?page=${page}`)
    expect(screen.getByTestId('state')).toHaveTextContent('page=1')
  })

  it('keeps other params when only the page changes', () => {
    renderAt('/books?q=x')
    fireEvent.click(screen.getByText('go to page 3'))
    expect(screen.getByTestId('search')).toHaveTextContent('?q=x&page=3')
  })

  it('resets to page 1 when search or filters change', () => {
    renderAt('/books?page=4')
    fireEvent.click(screen.getByText('search'))
    expect(screen.getByTestId('search')).toHaveTextContent('?q=dune')
  })

  it('removes a param when its value is empty', () => {
    renderAt('/books?q=dune&page=2')
    fireEvent.click(screen.getByText('clear search'))
    expect(screen.getByTestId('search')).toBeEmptyDOMElement()
  })
})
