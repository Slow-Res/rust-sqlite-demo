import { Link } from 'react-router'
import type { Loan } from '../generated/Loan'
import { useReturnLoan } from '../hooks/useReturnLoan'
import { formatDate } from '../utils'
import { EmptyList } from './EmptyList'
import { ErrorMessage } from './ErrorMessage'

interface LoansTableProps {
  loans: Loan[]
  /** Hide the member column when the table already belongs to one member. */
  showMember?: boolean
  /** The page being shown, so an out-of-range page can link back to page 1. */
  page?: number
}

function statusLabel(loan: Loan): string {
  if (loan.returned_at !== null) return `Returned ${formatDate(loan.returned_at)}`
  return loan.is_overdue ? 'Overdue' : 'Active'
}

export function LoansTable({ loans, showMember = true, page = 1 }: LoansTableProps) {
  const returnMutation = useReturnLoan()

  if (loans.length === 0) return <EmptyList message="No loans found." page={page} />

  return (
    <>
      <ErrorMessage error={returnMutation.error} />
      <table>
        <thead>
          <tr>
            <th>Book</th>
            {showMember && <th>Member</th>}
            <th>Borrowed</th>
            <th>Due</th>
            <th>Status</th>
            <th />
          </tr>
        </thead>
        <tbody>
          {loans.map((loan) => (
            <tr key={loan.id} className={loan.is_overdue ? 'overdue' : undefined}>
              <td>{loan.book_title}</td>
              {showMember && (
                <td>
                  <Link to={`/members/${loan.member_id}`}>{loan.member_name}</Link>
                </td>
              )}
              <td>{formatDate(loan.borrowed_at)}</td>
              <td>{formatDate(loan.due_at)}</td>
              <td>{statusLabel(loan)}</td>
              <td>
                {loan.returned_at === null && (
                  <button
                    type="button"
                    disabled={returnMutation.isPending}
                    onClick={() => returnMutation.mutate(loan.id)}
                  >
                    Return
                  </button>
                )}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </>
  )
}
