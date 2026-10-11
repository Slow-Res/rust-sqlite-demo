import type { ApiErrorBody } from '../generated/ApiErrorBody'

/** Thrown for every non-2xx response; mirrors the backend's `ApiError` envelope. */
export class ApiRequestError extends Error {
  readonly status: number
  readonly code: string
  /** Per-field validation messages (only present for 422 responses). */
  readonly fields: Record<string, string>

  constructor(status: number, code: string, message: string, fields: Record<string, string> = {}) {
    super(message)
    this.name = 'ApiRequestError'
    this.status = status
    this.code = code
    this.fields = fields
  }
}

export type QueryParams = Record<string, string | number | boolean | null | undefined>

export interface RequestOptions {
  query?: QueryParams
  body?: unknown
}

/** Serializes query params, skipping `undefined`, `null` and empty strings. */
export function buildQueryString(query: QueryParams = {}): string {
  const params = new URLSearchParams()
  for (const [key, value] of Object.entries(query)) {
    if (value === undefined || value === null || value === '') continue
    params.set(key, String(value))
  }
  const text = params.toString()
  return text === '' ? '' : `?${text}`
}

function isApiErrorBody(value: unknown): value is ApiErrorBody {
  if (typeof value !== 'object' || value === null) return false
  const candidate = value as Record<string, unknown>
  return typeof candidate.code === 'string' && typeof candidate.message === 'string'
}

async function parseError(response: Response): Promise<ApiRequestError> {
  const fallback = `Request failed (${response.status})`
  try {
    const payload: unknown = await response.json()
    const body =
      typeof payload === 'object' && payload !== null ? (payload as Record<string, unknown>).error : undefined
    if (isApiErrorBody(body)) {
      return new ApiRequestError(response.status, body.code, body.message, body.fields)
    }
  } catch {
    // Body was not JSON; fall through to the generic error.
  }
  return new ApiRequestError(response.status, 'unknown', fallback)
}

/** JSON `fetch` wrapper. Resolves with the parsed body (`undefined` for 204). */
export async function request<T>(
  method: string,
  path: string,
  { query, body }: RequestOptions = {},
): Promise<T> {
  const response = await fetch(`${path}${buildQueryString(query)}`, {
    method,
    credentials: 'same-origin',
    headers: {
      Accept: 'application/json',
      ...(body !== undefined ? { 'Content-Type': 'application/json' } : {}),
    },
    body: body !== undefined ? JSON.stringify(body) : undefined,
  })
  if (!response.ok) throw await parseError(response)
  if (response.status === 204) return undefined as T
  return (await response.json()) as T
}

/** Field-level validation messages carried by an error (empty for other errors). */
export function fieldErrorsOf(error: unknown): Record<string, string> {
  return error instanceof ApiRequestError ? error.fields : {}
}

/** True when the error has no field messages and should be shown as a general message. */
export function isGeneralError(error: unknown): boolean {
  return error !== null && error !== undefined && Object.keys(fieldErrorsOf(error)).length === 0
}
