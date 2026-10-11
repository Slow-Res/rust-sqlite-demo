import type { QueryClient } from '@tanstack/react-query'

/** Invalidates every cached query whose key starts with one of `keys` (e.g. `'books'`, `'member'`). */
export async function invalidateLibraryData(queryClient: QueryClient, ...keys: string[]): Promise<void> {
  await Promise.all(keys.map((key) => queryClient.invalidateQueries({ queryKey: [key] })))
}
