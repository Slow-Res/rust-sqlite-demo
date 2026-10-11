import { useMutation, useQueryClient } from '@tanstack/react-query'
import { returnLoan } from '../api/loans'
import { invalidateLibraryData } from '../api/invalidate'

/** Returning a loan changes availability and member loan counts too. */
export function useReturnLoan() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: returnLoan,
    onSuccess: async () => {
      await invalidateLibraryData(queryClient, 'loans', 'books', 'book', 'members', 'member')
    },
  })
}
