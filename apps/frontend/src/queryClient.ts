import { MutationCache, QueryCache, QueryClient } from '@tanstack/react-query'
import { meKey } from './api/auth'
import { ApiRequestError } from './api/client'

/** Mutation `meta` flag for requests whose 401 is an expected outcome (e.g. login). */
export interface UnauthorizedMeta extends Record<string, unknown> {
  skipUnauthorizedRedirect?: boolean
}

/**
 * Builds the app's QueryClient. Any 401 from the API clears the cached user
 * and calls `onUnauthorized` (which navigates to the login page).
 */
export function createQueryClient(onUnauthorized: () => void): QueryClient {
  const handle = (client: QueryClient, error: unknown, skip: boolean) => {
    if (skip || !(error instanceof ApiRequestError) || error.status !== 401) return
    client.setQueryData(meKey, null)
    onUnauthorized()
  }

  const client: QueryClient = new QueryClient({
    queryCache: new QueryCache({
      // `me` maps 401 to `null` itself, so it never reaches here as an error.
      onError: (error, query) => handle(client, error, query.queryKey[0] === meKey[0]),
    }),
    mutationCache: new MutationCache({
      onError: (error, _variables, _context, mutation) =>
        handle(client, error, (mutation.meta as UnauthorizedMeta | undefined)?.skipUnauthorizedRedirect === true),
    }),
    defaultOptions: { queries: { retry: false, refetchOnWindowFocus: false } },
  })
  return client
}
