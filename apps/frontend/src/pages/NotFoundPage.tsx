import { Link } from 'react-router'

export function NotFoundPage() {
  return (
    <>
      <h1>Not found</h1>
      <p>The page or record you asked for does not exist.</p>
      <Link to="/books">Back to books</Link>
    </>
  )
}
