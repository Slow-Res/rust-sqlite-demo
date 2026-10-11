import type { CurrentUser } from '../generated/CurrentUser'
import type { LoginRequest } from '../generated/LoginRequest'
import { ApiRequestError, request } from './client'

export const meKey = ['me'] as const

export const login = (credentials: LoginRequest) =>
  request<CurrentUser>('POST', '/api/auth/login', { body: credentials })

export const logout = () => request<void>('POST', '/api/auth/logout')

/** Resolves `null` (instead of throwing) when nobody is logged in. */
export async function fetchCurrentUser(): Promise<CurrentUser | null> {
  try {
    return await request<CurrentUser>('GET', '/api/auth/me')
  } catch (error) {
    if (error instanceof ApiRequestError && error.status === 401) return null
    throw error
  }
}
