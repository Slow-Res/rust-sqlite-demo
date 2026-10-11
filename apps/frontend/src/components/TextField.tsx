import { useId, type InputHTMLAttributes } from 'react'
import { FieldError } from './FieldError'

interface TextFieldProps extends Omit<InputHTMLAttributes<HTMLInputElement>, 'id' | 'aria-invalid'> {
  label: string
  /** Field-level message from `ApiError.error.fields`. */
  error?: string
  hint?: string
}

/** A labelled input with its field error wired up via `aria-invalid` / `aria-describedby`. */
export function TextField({ label, error, hint, ...inputProps }: TextFieldProps) {
  const id = useId()
  const errorId = `${id}-error`
  const hintId = `${id}-hint`
  const describedBy = [error ? errorId : null, hint ? hintId : null].filter(Boolean).join(' ')
  return (
    <div>
      <label htmlFor={id}>{label}</label>
      <input
        {...inputProps}
        id={id}
        aria-invalid={error ? true : undefined}
        aria-describedby={describedBy || undefined}
      />
      <FieldError id={errorId} message={error} />
      {hint && <small id={hintId}>{hint}</small>}
    </div>
  )
}
