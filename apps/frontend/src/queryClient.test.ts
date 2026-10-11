import type { QueryClient } from '@tanstack/react-query'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import { meKey } from './api/auth'
import { ApiRequestError } from './api/client'
import { createQueryClient } from './queryClient'

const unauthorized = () => new ApiRequestError(401, 'unauthorized', 'Please log in')

function runMutation(client: QueryClient, error: Error, meta?: Record<string, unknown>) {
  return client
    .getMutationCache()
    .build(client, { mutationFn: () => Promise.reject(error), meta })
    .execute(undefined)
    .catch(() => undefined)
}

describe('createQueryClient 401 handling', () => {
  const onUnauthorized = vi.fn()
  let client: QueryClient

  beforeEach(() => {
    onUnauthorized.mockReset()
    client = createQueryClient(onUnauthorized)
    client.setQueryData(meKey, { id: 1, username: 'alice' })
  })

  it('clears the current user and redirects when a query gets 401', async () => {
    await client.fetchQuery({ queryKey: ['loans', {}], queryFn: () => Promise.reject(unauthorized()) }).catch(() => undefined)

    expect(onUnauthorized).toHaveBeenCalledTimes(1)
    expect(client.getQueryData(meKey)).toBeNull()
  })

  it('does the same for a mutation that gets 401', async () => {
    await runMutation(client, unauthorized())

    expect(onUnauthorized).toHaveBeenCalledTimes(1)
    expect(client.getQueryData(meKey)).toBeNull()
  })

  it('ignores 401 from the `me` query itself (no redirect loop)', async () => {
    await client.fetchQuery({ queryKey: meKey, queryFn: () => Promise.reject(unauthorized()) }).catch(() => undefined)

    expect(onUnauthorized).not.toHaveBeenCalled()
  })

  it('ignores 401 from mutations flagged skipUnauthorizedRedirect (login)', async () => {
    await runMutation(client, unauthorized(), { skipUnauthorizedRedirect: true })

    expect(onUnauthorized).not.toHaveBeenCalled()
    expect(client.getQueryData(meKey)).toEqual({ id: 1, username: 'alice' })
  })

  it('ignores other errors', async () => {
    await runMutation(client, new ApiRequestError(409, 'isbn_taken', 'ISBN already used'))
    await client.fetchQuery({ queryKey: ['books'], queryFn: () => Promise.reject(new Error('boom')) }).catch(() => undefined)

    expect(onUnauthorized).not.toHaveBeenCalled()
  })
})
