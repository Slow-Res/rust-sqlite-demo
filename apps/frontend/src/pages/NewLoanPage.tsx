import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { useState, type FormEvent } from 'react'
import { useNavigate, useSearchParams } from 'react-router'
import { bookKey, booksKey, getBook, listBooks } from '../api/books'
import { fieldErrorsOf, isGeneralError } from '../api/client'
import { createLoan } from '../api/loans'
import { getMember, listMembers, memberKey, membersKey } from '../api/members'
import { invalidateLibraryData } from '../api/invalidate'
import { ErrorMessage } from '../components/ErrorMessage'
import { Picker, type PickerOption } from '../components/Picker'
import type { BookQuery } from '../generated/BookQuery'
import type { MemberQuery } from '../generated/MemberQuery'
import { usePickerSelection } from '../hooks/usePickerSelection'
import { parsePositiveInt } from '../utils'

/** Search results, plus the pre-selected record when it is not among them. */
function withSelected(options: PickerOption[], selected: PickerOption | undefined): PickerOption[] {
  if (!selected || options.some((option) => option.id === selected.id)) return options
  return [selected, ...options]
}

export function NewLoanPage() {
  const [searchParams] = useSearchParams()
  const navigate = useNavigate()
  const queryClient = useQueryClient()

  const bookSelection = usePickerSelection(parsePositiveInt(searchParams.get('book_id')))
  const memberSelection = usePickerSelection(parsePositiveInt(searchParams.get('member_id')))
  const bookId = bookSelection.id
  const memberId = memberSelection.id
  const [bookSearch, setBookSearch] = useState('')
  const [memberSearch, setMemberSearch] = useState('')

  const bookQuery: BookQuery = { q: bookSearch, availability: 'available' }
  const memberQuery: MemberQuery = { q: memberSearch }
  const books = useQuery({ queryKey: booksKey(bookQuery), queryFn: () => listBooks(bookQuery) })
  const members = useQuery({ queryKey: membersKey(memberQuery), queryFn: () => listMembers(memberQuery) })

  // Prefilled ids may not appear in the first page of search results, so fetch them directly.
  const prefilledBook = useQuery({
    queryKey: bookKey(bookId ?? 0),
    queryFn: () => getBook(bookId ?? 0),
    enabled: bookId !== null,
  })
  const prefilledMember = useQuery({
    queryKey: memberKey(memberId ?? 0),
    queryFn: () => getMember(memberId ?? 0),
    enabled: memberId !== null,
  })

  const bookOptions = withSelected(
    (books.data?.items ?? []).map((book) => ({ id: book.id, label: `${book.title} — ${book.author}` })),
    bookSelection.selected(
      prefilledBook.data && { id: prefilledBook.data.id, label: `${prefilledBook.data.title} — ${prefilledBook.data.author}` },
    ),
  )
  const memberOptions = withSelected(
    (members.data?.items ?? []).map((member) => ({ id: member.id, label: `${member.name} (${member.email})` })),
    memberSelection.selected(
      prefilledMember.data && {
        id: prefilledMember.data.id,
        label: `${prefilledMember.data.name} (${prefilledMember.data.email})`,
      },
    ),
  )

  const lend = useMutation({
    mutationFn: createLoan,
    onSuccess: async () => {
      await invalidateLibraryData(queryClient, 'loans', 'books', 'book', 'members', 'member')
      void navigate('/loans')
    },
  })
  const fields = fieldErrorsOf(lend.error)

  const handleSubmit = (event: FormEvent) => {
    event.preventDefault()
    if (bookId === null || memberId === null) return
    lend.mutate({ book_id: bookId, member_id: memberId })
  }

  return (
    <>
      <h1>New loan</h1>
      <form onSubmit={handleSubmit} noValidate>
        <Picker
          label="Book"
          search={bookSearch}
          onSearch={setBookSearch}
          options={bookOptions}
          selectedId={bookId}
          onSelect={(id) => bookSelection.select(id, bookOptions)}
          isLoading={books.isFetching}
          shown={books.data?.items.length}
          total={books.data?.total}
          error={fields.book_id}
        />
        <Picker
          label="Member"
          search={memberSearch}
          onSearch={setMemberSearch}
          options={memberOptions}
          selectedId={memberId}
          onSelect={(id) => memberSelection.select(id, memberOptions)}
          isLoading={members.isFetching}
          shown={members.data?.items.length}
          total={members.data?.total}
          error={fields.member_id}
        />
        <ErrorMessage error={books.error ?? members.error} />
        {isGeneralError(lend.error) && <ErrorMessage error={lend.error} />}
        <button type="submit" disabled={lend.isPending || bookId === null || memberId === null}>
          Lend book
        </button>
      </form>
    </>
  )
}
