import { describe, expect, it } from 'vitest'
import { safeNextPath } from './utils'

describe('safeNextPath', () => {
  it.each([
    ['/members?page=2'],
    ['/%5Cevil.com'],
    ['/loans'],
  ])('allows %s', (next) => {
    expect(safeNextPath(next)).toBe(next)
  })

  it.each([
    [null],
    [''],
    ['members'],
    ['https://evil.com'],
    ['//evil.com'],
    ['/\\evil.com'],
    ['/\t/evil.com'],
    ['/a\nb'],
    ['/a\\b'],
    ['/a\x7fb'],
  ])('rejects %j', (next) => {
    expect(safeNextPath(next)).toBeNull()
  })
})
