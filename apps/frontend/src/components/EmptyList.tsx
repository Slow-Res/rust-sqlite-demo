import { Link, useSearchParams } from 'react-router'

interface EmptyListProps {
  message: string
  /** The page that came back empty; beyond page 1 it means the page is out of range. */
  page: number
}

/** "Nothing found" message; on an out-of-range page it also offers a way back to page 1. */
export function EmptyList({ message, page }: EmptyListProps) {
  const [searchParams] = useSearchParams()
  if (page <= 1) return <p>{message}</p>

  const firstPage = new URLSearchParams(searchParams)
  firstPage.delete('page')
  return (
    <p>
      {message} <Link to={{ search: firstPage.toString() }}>Go to page 1</Link>
    </p>
  )
}
