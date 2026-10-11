import type { Book } from '../generated/Book'
import type { BookInput } from '../generated/BookInput'
import type { BookQuery } from '../generated/BookQuery'
import type { Page } from '../generated/Page'
import { request } from './client'

export const booksKey = (query: BookQuery) => ['books', query] as const
export const bookKey = (id: number) => ['book', id] as const

export const listBooks = (query: BookQuery) => request<Page<Book>>('GET', '/api/books', { query })
export const getBook = (id: number) => request<Book>('GET', `/api/books/${id}`)
export const createBook = (input: BookInput) => request<Book>('POST', '/api/books', { body: input })
export const updateBook = (id: number, input: BookInput) =>
  request<Book>('PUT', `/api/books/${id}`, { body: input })
export const deleteBook = (id: number) => request<void>('DELETE', `/api/books/${id}`)
