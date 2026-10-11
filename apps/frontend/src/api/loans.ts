import type { Loan } from '../generated/Loan'
import type { LoanQuery } from '../generated/LoanQuery'
import type { NewLoan } from '../generated/NewLoan'
import type { Page } from '../generated/Page'
import { request } from './client'

export const loansKey = (query: LoanQuery) => ['loans', query] as const

export const listLoans = (query: LoanQuery) => request<Page<Loan>>('GET', '/api/loans', { query })
export const createLoan = (input: NewLoan) => request<Loan>('POST', '/api/loans', { body: input })
export const returnLoan = (id: number) => request<Loan>('POST', `/api/loans/${id}/return`)
