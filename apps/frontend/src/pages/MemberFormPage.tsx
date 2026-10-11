import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { useNavigate, useParams } from 'react-router'
import { ApiRequestError } from '../api/client'
import { memberKey, createMember, getMember, updateMember } from '../api/members'
import { invalidateLibraryData } from '../api/invalidate'
import { MemberForm } from '../components/MemberForm'
import { ErrorMessage } from '../components/ErrorMessage'
import type { MemberInput } from '../generated/MemberInput'
import { useSettled } from '../hooks/useSettled'
import { parsePositiveInt } from '../utils'
import { NotFoundPage } from './NotFoundPage'

/** `/members/new` (no id param) and `/members/:id/edit`. */
export function MemberFormPage() {
  const { id: rawId } = useParams()
  if (rawId === undefined) return <MemberFormView />
  const id = parsePositiveInt(rawId)
  return id === null ? <NotFoundPage /> : <MemberFormView id={id} />
}

function MemberFormView({ id }: { id?: number }) {
  const navigate = useNavigate()
  const queryClient = useQueryClient()
  const existing = useQuery({
    queryKey: memberKey(id ?? 0),
    queryFn: () => getMember(id ?? 0),
    enabled: id !== undefined,
  })

  const save = useMutation({
    mutationFn: (input: MemberInput) => (id === undefined ? createMember(input) : updateMember(id, input)),
    onSuccess: async () => {
      await invalidateLibraryData(queryClient, 'members', 'member', 'loans')
      void navigate('/members')
    },
  })

  // The form copies `existing.data` once on mount, so wait for a fresh (not merely cached) record.
  const ready = useSettled(id === undefined || (existing.isSuccess && !existing.isFetching))

  if (id !== undefined) {
    if (existing.error) {
      const notFound = existing.error instanceof ApiRequestError && existing.error.status === 404
      return notFound ? <NotFoundPage /> : <ErrorMessage error={existing.error} />
    }
    if (!ready) return <p aria-busy="true">Loading…</p>
  }

  return (
    <>
      <h1>{id === undefined ? 'Add member' : 'Edit member'}</h1>
      <MemberForm
        initial={existing.data}
        error={save.error}
        isSubmitting={save.isPending}
        onSubmit={(input) => save.mutate(input)}
      />
    </>
  )
}
