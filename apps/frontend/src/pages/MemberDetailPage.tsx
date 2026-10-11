import { useQuery } from '@tanstack/react-query'
import { Link, useParams } from 'react-router'
import { ApiRequestError } from '../api/client'
import { listLoans, loansKey } from '../api/loans'
import { getMember, memberKey } from '../api/members'
import { ErrorMessage } from '../components/ErrorMessage'
import { LoansTable } from '../components/LoansTable'
import { Pagination } from '../components/Pagination'
import type { LoanQuery } from '../generated/LoanQuery'
import { useListParams } from '../hooks/useListParams'
import { parsePositiveInt } from '../utils'
import { NotFoundPage } from './NotFoundPage'

export function MemberDetailPage() {
  const id = parsePositiveInt(useParams().id)
  return id === null ? <NotFoundPage /> : <MemberDetailView id={id} />
}

function MemberDetailView({ id }: { id: number }) {
  const { page, update } = useListParams()
  const member = useQuery({ queryKey: memberKey(id), queryFn: () => getMember(id) })
  const query: LoanQuery = { member_id: id, status: 'all', page }
  const loans = useQuery({ queryKey: loansKey(query), queryFn: () => listLoans(query) })

  if (member.isPending) return <p aria-busy="true">Loading…</p>
  if (member.error) {
    return member.error instanceof ApiRequestError && member.error.status === 404 ? (
      <NotFoundPage />
    ) : (
      <ErrorMessage error={member.error} />
    )
  }

  return (
    <>
      <div className="toolbar">
        <h1>{member.data.name}</h1>
        <div className="actions">
          <Link role="button" className="secondary" to={`/members/${id}/edit`}>
            Edit
          </Link>
          <Link role="button" to={`/loans/new?member_id=${id}`}>
            Lend a book
          </Link>
        </div>
      </div>
      <p>
        {member.data.email} · {member.data.active_loans} active loan(s)
      </p>
      <h2>Loans</h2>
      {loans.isPending && <p aria-busy="true">Loading…</p>}
      <ErrorMessage error={loans.error} />
      {loans.data && (
        <>
          <LoansTable loans={loans.data.items} showMember={false} page={loans.data.page} />
          <Pagination
            page={loans.data.page}
            perPage={loans.data.per_page}
            total={loans.data.total}
            onPageChange={(next) => update({ page: String(next) })}
          />
        </>
      )}
    </>
  )
}
