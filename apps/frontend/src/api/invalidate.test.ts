import { QueryClient } from '@tanstack/react-query'
import { describe, expect, it, vi } from 'vitest'
import { invalidateLibraryData } from './invalidate'

describe('invalidateLibraryData', () => {
  it('invalidates each given key prefix and nothing else', async () => {
    const queryClient = new QueryClient()
    const spy = vi.spyOn(queryClient, 'invalidateQueries')

    await invalidateLibraryData(queryClient, 'books', 'loans')

    expect(spy.mock.calls.map(([filters]) => filters)).toEqual([{ queryKey: ['books'] }, { queryKey: ['loans'] }])
  })
})
