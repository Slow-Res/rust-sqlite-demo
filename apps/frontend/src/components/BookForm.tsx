import { useState, type FormEvent } from 'react'
import { fieldErrorsOf, isGeneralError } from '../api/client'
import type { Book } from '../generated/Book'
import type { BookInput } from '../generated/BookInput'
import { ErrorMessage } from './ErrorMessage'
import { TextField } from './TextField'

interface BookFormProps {
  initial?: Book
  error: unknown
  isSubmitting: boolean
  onSubmit: (input: BookInput) => void
}

/** Converts blank optional text to `null`; otherwise the number (NaN is rejected by the server). */
function optionalNumber(text: string): number | null {
  return text.trim() === '' ? null : Number(text)
}

export function BookForm({ initial, error, isSubmitting, onSubmit }: BookFormProps) {
  const [title, setTitle] = useState(initial?.title ?? '')
  const [author, setAuthor] = useState(initial?.author ?? '')
  const [isbn, setIsbn] = useState(initial?.isbn ?? '')
  const [year, setYear] = useState(initial?.published_year?.toString() ?? '')
  const [copies, setCopies] = useState(initial?.total_copies.toString() ?? '1')
  const fields = fieldErrorsOf(error)

  const handleSubmit = (event: FormEvent) => {
    event.preventDefault()
    onSubmit({
      title,
      author,
      isbn: isbn.trim() === '' ? null : isbn,
      published_year: optionalNumber(year),
      total_copies: Number(copies),
    })
  }

  return (
    <form onSubmit={handleSubmit} noValidate>
      <TextField label="Title" name="title" value={title} onChange={(e) => setTitle(e.target.value)} error={fields.title} />
      <TextField label="Author" name="author" value={author} onChange={(e) => setAuthor(e.target.value)} error={fields.author} />
      <TextField
        label="ISBN"
        name="isbn"
        value={isbn}
        onChange={(e) => setIsbn(e.target.value)}
        error={fields.isbn}
        hint="Optional. ISBN-10 or ISBN-13; spaces and hyphens are ignored."
      />
      <TextField
        label="Published year"
        name="published_year"
        type="number"
        value={year}
        onChange={(e) => setYear(e.target.value)}
        error={fields.published_year}
        hint="Optional."
      />
      <TextField
        label="Total copies"
        name="total_copies"
        type="number"
        min={1}
        max={1000}
        value={copies}
        onChange={(e) => setCopies(e.target.value)}
        error={fields.total_copies}
        hint="Between 1 and 1000."
      />
      {isGeneralError(error) && <ErrorMessage error={error} />}
      <button type="submit" disabled={isSubmitting}>
        {initial ? 'Save changes' : 'Add book'}
      </button>
    </form>
  )
}
