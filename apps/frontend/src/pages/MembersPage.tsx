import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { Link } from 'react-router'
import { deleteMember, listMembers, membersKey } from '../api/members'
import { invalidateLibraryData } from '../api/invalidate'
import { EmptyList } from '../components/EmptyList'
import { ErrorMessage } from '../components/ErrorMessage'
import { Pagination } from '../components/Pagination'
import { SearchBox } from '../components/SearchBox'
import type { Member } from '../generated/Member'
import type { MemberQuery } from '../generated/MemberQuery'
import { useListParams } from '../hooks/useListParams'

export function MembersPage() {
  const { page, get, update } = useListParams()
  const queryClient = useQueryClient()

  const q = get('q')
  const query: MemberQuery = { q, page }
  const members = useQuery({ queryKey: membersKey(query), queryFn: () => listMembers(query) })

  const remove = useMutation({
    mutationFn: deleteMember,
    onSuccess: async () => {
      await invalidateLibraryData(queryClient, 'members', 'member', 'loans', 'books', 'book')
    },
  })

  const handleDelete = (member: Member) => {
    if (window.confirm(`Delete member "${member.name}"? This cannot be undone.`)) remove.mutate(member.id)
  }

  return (
    <>
      <div className="toolbar">
        <h1>Members</h1>
        <Link role="button" to="/members/new">
          Add member
        </Link>
      </div>
      <SearchBox label="Search name or email" value={q} onSearch={(value) => update({ q: value }, { replace: true })} />
      <ErrorMessage error={remove.error} />
      {members.isPending && <p aria-busy="true">Loading…</p>}
      <ErrorMessage error={members.error} />
      {members.data &&
        (members.data.items.length === 0 ? (
          <EmptyList message="No members found." page={members.data.page} />
        ) : (
          <>
            <table>
              <thead>
                <tr>
                  <th>Name</th>
                  <th>Email</th>
                  <th>Active loans</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {members.data.items.map((member) => (
                  <tr key={member.id}>
                    <td>{member.name}</td>
                    <td>{member.email}</td>
                    <td>{member.active_loans}</td>
                    <td>
                      <div className="actions">
                        <Link role="button" className="secondary" to={`/members/${member.id}`}>
                          View
                        </Link>
                        <Link role="button" className="secondary" to={`/members/${member.id}/edit`}>
                          Edit
                        </Link>
                        <button type="button" className="contrast" onClick={() => handleDelete(member)}>
                          Delete
                        </button>
                      </div>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
            <Pagination
              page={members.data.page}
              perPage={members.data.per_page}
              total={members.data.total}
              onPageChange={(next) => update({ page: String(next) })}
            />
          </>
        ))}
    </>
  )
}
