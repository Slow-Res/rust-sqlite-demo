import { afterEach, describe, expect, it, vi } from 'vitest'
import { ApiRequestError, buildQueryString, request } from './client'

function mockFetch(response: Response) {
  const fetchMock = vi.fn().mockResolvedValue(response)
  vi.stubGlobal('fetch', fetchMock)
  return fetchMock
}

afterEach(() => vi.unstubAllGlobals())

describe('buildQueryString', () => {
  it('skips undefined, null and empty values', () => {
    expect(buildQueryString({ q: 'dune', page: 2, availability: undefined, status: '', x: null })).toBe(
      '?q=dune&page=2',
    )
  })

  it('returns an empty string when there is nothing to send', () => {
    expect(buildQueryString({ q: '' })).toBe('')
    expect(buildQueryString()).toBe('')
  })

  it('encodes special characters', () => {
    expect(buildQueryString({ q: 'a&b c' })).toBe('?q=a%26b+c')
  })
})

describe('request', () => {
  it('sends same-origin JSON requests and parses the response', async () => {
    const fetchMock = mockFetch(Response.json({ id: 1 }))
    const result = await request<{ id: number }>('POST', '/api/books', { query: { page: 1 }, body: { title: 'x' } })

    expect(result).toEqual({ id: 1 })
    const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit]
    expect(url).toBe('/api/books?page=1')
    expect(init.method).toBe('POST')
    expect(init.credentials).toBe('same-origin')
    expect(init.body).toBe('{"title":"x"}')
    expect(init.headers).toMatchObject({ 'Content-Type': 'application/json' })
  })

  it('resolves undefined for 204', async () => {
    mockFetch(new Response(null, { status: 204 }))
    await expect(request<void>('DELETE', '/api/books/1')).resolves.toBeUndefined()
  })

  it('throws ApiRequestError with code, message and fields', async () => {
    mockFetch(
      Response.json(
        { error: { code: 'validation_error', message: 'Some fields are invalid', fields: { title: 'must not be empty' } } },
        { status: 422 },
      ),
    )
    const error = await request('POST', '/api/books').catch((e: unknown) => e)

    expect(error).toBeInstanceOf(ApiRequestError)
    expect(error).toMatchObject({
      status: 422,
      code: 'validation_error',
      message: 'Some fields are invalid',
      fields: { title: 'must not be empty' },
    })
  })

  it('falls back to a generic error when the body is not JSON', async () => {
    mockFetch(new Response('<html>Bad gateway</html>', { status: 502 }))
    const error = await request('GET', '/api/books').catch((e: unknown) => e)

    expect(error).toBeInstanceOf(ApiRequestError)
    expect(error).toMatchObject({ status: 502, code: 'unknown', message: 'Request failed (502)', fields: {} })
  })
})
