import { render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { describe, expect, it, vi } from 'vitest'
import { Pagination } from './Pagination'

describe('Pagination', () => {
  it('renders nothing when everything fits on one page', () => {
    const { container } = render(<Pagination page={1} perPage={20} total={20} onPageChange={vi.fn()} />)
    expect(container).toBeEmptyDOMElement()
  })

  it('shows the current page and page count', () => {
    render(<Pagination page={2} perPage={20} total={45} onPageChange={vi.fn()} />)
    expect(screen.getByText('Page 2 of 3')).toBeInTheDocument()
  })

  it('disables Prev on the first page and Next on the last', () => {
    const { rerender } = render(<Pagination page={1} perPage={10} total={25} onPageChange={vi.fn()} />)
    expect(screen.getByRole('button', { name: 'Prev' })).toBeDisabled()
    expect(screen.getByRole('button', { name: 'Next' })).toBeEnabled()

    rerender(<Pagination page={3} perPage={10} total={25} onPageChange={vi.fn()} />)
    expect(screen.getByRole('button', { name: 'Next' })).toBeDisabled()
  })

  it('reports the requested page', async () => {
    const onPageChange = vi.fn()
    render(<Pagination page={2} perPage={10} total={30} onPageChange={onPageChange} />)

    await userEvent.click(screen.getByRole('button', { name: 'Next' }))
    await userEvent.click(screen.getByRole('button', { name: 'Prev' }))

    expect(onPageChange).toHaveBeenNthCalledWith(1, 3)
    expect(onPageChange).toHaveBeenNthCalledWith(2, 1)
  })
})
