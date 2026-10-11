import { useQuery } from '@tanstack/react-query'
import { Link } from 'react-router'
import { listLoans, loansKey } from '../api/loans'
import { ErrorMessage } from '../components/ErrorMessage'
import { LoansTable } from '../components/LoansTable'
import { Pagination } from '../components/Pagination'
import type { LoanQuery } from '../generated/LoanQuery'
import type { LoanStatus } from '../generated/LoanStatus'
import { useListParams } from '../hooks/useListParams'

const TABS: { status: LoanStatus; label: string }[] = [
  { status: 'active', label: 'Active' },
  { status: 'overdue', label: 'Overdue' },
  { status: 'returned', label: 'Returned' },
  { status: 'all', label: 'All' },
]

function parseStatus(value: string): LoanStatus {
  return TABS.find((tab) => tab.status === value)?.status ?? 'active'
}

export function LoansPage() {
  const { page, get, update } = useListParams()
  const status = parseStatus(get('status'))
  const query: LoanQuery = { status, page }
  const loans = useQuery({ queryKey: loansKey(query), queryFn: () => listLoans(query) })

  return (
    <>
      <div className="toolbar">
        <h1>Loans</h1>
        <Link role="button" to="/loans/new">
          New loan
        </Link>
      </div>
      <nav aria-label="Loan status">
        <ul>
          {TABS.map((tab) => (
            <li key={tab.status}>
              <button
                type="button"
                className={tab.status === status ? undefined : 'secondary outline'}
                aria-current={tab.status === status ? 'page' : undefined}
                onClick={() => update({ status: tab.status === 'active' ? undefined : tab.status })}
              >
                {tab.label}
              </button>
            </li>
          ))}
        </ul>
      </nav>
      {loans.isPending && <p aria-busy="true">Loading…</p>}
      <ErrorMessage error={loans.error} />
      {loans.data && (
        <>
          <LoansTable loans={loans.data.items} page={loans.data.page} />
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
