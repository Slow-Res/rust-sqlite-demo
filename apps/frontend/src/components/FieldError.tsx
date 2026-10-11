interface FieldErrorProps {
  /** Id referenced by the input's `aria-describedby`. */
  id: string
  message: string | undefined
}

export function FieldError({ id, message }: FieldErrorProps) {
  if (!message) return null
  return (
    <small id={id} className="error">
      {message}
    </small>
  )
}
