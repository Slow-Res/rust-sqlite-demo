import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { Link } from 'react-router'
import { booksKey, deleteBook, listBooks } from '../api/books'
import { invalidateLibraryData } from '../api/invalidate'
import { useCurrentUser } from '../auth/useCurrentUser'
import { EmptyList } from '../components/EmptyList'
import { ErrorMessage } from '../components/ErrorMessage'
import { Pagination } from '../components/Pagination'
import { SearchBox } from '../components/SearchBox'
import type { Availability } from '../generated/Availability'
import type { Book } from '../generated/Book'
import type { BookQuery } from '../generated/BookQuery'
import { useListParams } from '../hooks/useListParams'

const AVAILABILITIES: { value: Availability; label: string }[] = [
  { value: 'all', label: 'All' },
  { value: 'available', label: 'Available' },
  { value: 'on_loan', label: 'On loan' },
]

function parseAvailability(value: string): Availability {
  return AVAILABILITIES.find((option) => option.value === value)?.value ?? 'all'
}

export function BooksPage() {
  const { user } = useCurrentUser()
  const { page, get, update } = useListParams()
  const queryClient = useQueryClient()

  const q = get('q')
  const availability = parseAvailability(get('availability'))
  const query: BookQuery = { q, availability, page }
  const books = useQuery({ queryKey: booksKey(query), queryFn: () => listBooks(query) })

  const remove = useMutation({
    mutationFn: deleteBook,
    onSuccess: async () => {
      await invalidateLibraryData(queryClient, 'books', 'book', 'loans')
    },
  })

  const handleDelete = (book: Book) => {
    if (window.confirm(`Delete "${book.title}"? This cannot be undone.`)) remove.mutate(book.id)
  }

  return (
    <>
      <div className="toolbar">
        <h1>Books</h1>
        {user && (
          <Link role="button" to="/books/new">
            Add book
          </Link>
        )}
      </div>
      <div className="toolbar">
        <SearchBox label="Search title, author or ISBN" value={q} onSearch={(value) => update({ q: value }, { replace: true })} />
        <select
          aria-label="Availability"
          value={availability}
          onChange={(event) => update({ availability: event.target.value === 'all' ? undefined : event.target.value })}
        >
          {AVAILABILITIES.map((option) => (
            <option key={option.value} value={option.value}>
              {option.label}
            </option>
          ))}
        </select>
      </div>
      <ErrorMessage error={remove.error} />
      {books.isPending && <p aria-busy="true">Loading…</p>}
      <ErrorMessage error={books.error} />
      {books.data &&
        (books.data.items.length === 0 ? (
          <EmptyList message="No books found." page={books.data.page} />
        ) : (
          <>
            <table>
              <thead>
                <tr>
                  <th>Title</th>
                  <th>Author</th>
                  <th>ISBN</th>
                  <th>Year</th>
                  <th>Available</th>
                  {user && <th />}
                </tr>
              </thead>
              <tbody>
                {books.data.items.map((book) => (
                  <tr key={book.id}>
                    <td>{book.title}</td>
                    <td>{book.author}</td>
                    <td>{book.isbn ?? ''}</td>
                    <td>{book.published_year ?? ''}</td>
                    <td>
                      {book.available_copies} / {book.total_copies}
                    </td>
                    {user && (
                      <td>
                        <div className="actions">
                        <Link role="button" className="secondary" to={`/books/${book.id}/edit`}>
                          Edit
                        </Link>
                        <Link role="button" className="secondary" to={`/loans/new?book_id=${book.id}`}>
                          Lend
                        </Link>
                        <button type="button" className="contrast" onClick={() => handleDelete(book)}>
                          Delete
                        </button>
                        </div>
                      </td>
                    )}
                  </tr>
                ))}
              </tbody>
            </table>
            <Pagination
              page={books.data.page}
              perPage={books.data.per_page}
              total={books.data.total}
              onPageChange={(next) => update({ page: String(next) })}
            />
          </>
        ))}
    </>
  )
}
