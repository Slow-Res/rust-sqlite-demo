import { useState, type FormEvent } from 'react'
import { fieldErrorsOf, isGeneralError } from '../api/client'
import type { Member } from '../generated/Member'
import type { MemberInput } from '../generated/MemberInput'
import { ErrorMessage } from './ErrorMessage'
import { TextField } from './TextField'

interface MemberFormProps {
  initial?: Member
  error: unknown
  isSubmitting: boolean
  onSubmit: (input: MemberInput) => void
}

export function MemberForm({ initial, error, isSubmitting, onSubmit }: MemberFormProps) {
  const [name, setName] = useState(initial?.name ?? '')
  const [email, setEmail] = useState(initial?.email ?? '')
  const fields = fieldErrorsOf(error)

  const handleSubmit = (event: FormEvent) => {
    event.preventDefault()
    onSubmit({ name, email })
  }

  return (
    <form onSubmit={handleSubmit} noValidate>
      <TextField label="Name" name="name" value={name} onChange={(e) => setName(e.target.value)} error={fields.name} />
      <TextField
        label="Email"
        name="email"
        type="email"
        autoComplete="off"
        value={email}
        onChange={(e) => setEmail(e.target.value)}
        error={fields.email}
      />
      {isGeneralError(error) && <ErrorMessage error={error} />}
      <button type="submit" disabled={isSubmitting}>
        {initial ? 'Save changes' : 'Add member'}
      </button>
    </form>
  )
}
