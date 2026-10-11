interface ErrorMessageProps {
  error: unknown
}

/** Shows `ApiRequestError.message` (or any Error's message) as an alert. */
export function ErrorMessage({ error }: ErrorMessageProps) {
  if (error === null || error === undefined) return null
  const message = error instanceof Error ? error.message : 'Something went wrong.'
  return (
    <p role="alert" className="error">
      {message}
    </p>
  )
}
