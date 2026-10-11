interface PaginationProps {
  page: number
  perPage: number
  total: number
  onPageChange: (page: number) => void
}

/** Prev / "Page X of Y" / Next; hidden when everything fits on one page. */
export function Pagination({ page, perPage, total, onPageChange }: PaginationProps) {
  if (total <= perPage) return null
  const pageCount = Math.ceil(total / perPage)
  return (
    <nav aria-label="Pagination" className="pagination">
      <button type="button" className="secondary outline" disabled={page <= 1} onClick={() => onPageChange(page - 1)}>
        Prev
      </button>
      <span>
        Page {page} of {pageCount}
      </span>
      <button
        type="button"
        className="secondary outline"
        disabled={page >= pageCount}
        onClick={() => onPageChange(page + 1)}
      >
        Next
      </button>
    </nav>
  )
}
