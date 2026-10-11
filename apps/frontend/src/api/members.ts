import type { Member } from '../generated/Member'
import type { MemberInput } from '../generated/MemberInput'
import type { MemberQuery } from '../generated/MemberQuery'
import type { Page } from '../generated/Page'
import { request } from './client'

export const membersKey = (query: MemberQuery) => ['members', query] as const
export const memberKey = (id: number) => ['member', id] as const

export const listMembers = (query: MemberQuery) =>
  request<Page<Member>>('GET', '/api/members', { query })
export const getMember = (id: number) => request<Member>('GET', `/api/members/${id}`)
export const createMember = (input: MemberInput) =>
  request<Member>('POST', '/api/members', { body: input })
export const updateMember = (id: number, input: MemberInput) =>
  request<Member>('PUT', `/api/members/${id}`, { body: input })
export const deleteMember = (id: number) => request<void>('DELETE', `/api/members/${id}`)
