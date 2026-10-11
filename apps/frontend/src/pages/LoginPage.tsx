import { useMutation, useQueryClient } from '@tanstack/react-query'
import { useState, type FormEvent } from 'react'
import { useNavigate, useSearchParams } from 'react-router'
import { login, meKey } from '../api/auth'
import { fieldErrorsOf, isGeneralError } from '../api/client'
import { ErrorMessage } from '../components/ErrorMessage'
import { FieldError } from '../components/FieldError'
import { safeNextPath } from '../utils'

export function LoginPage() {
  const [username, setUsername] = useState('')
  const [password, setPassword] = useState('')
  const [searchParams] = useSearchParams()
  const navigate = useNavigate()
  const queryClient = useQueryClient()

  const mutation = useMutation({
    mutationFn: login,
    // A 401 here just means "wrong credentials"; do not bounce to /login again.
    meta: { skipUnauthorizedRedirect: true },
    onSuccess: (user) => {
      queryClient.setQueryData(meKey, user)
      void navigate(safeNextPath(searchParams.get('next')) ?? '/loans', { replace: true })
    },
  })
  const fields = fieldErrorsOf(mutation.error)

  const handleSubmit = (event: FormEvent) => {
    event.preventDefault()
    mutation.mutate({ username, password })
  }

  return (
    <article>
      <h1>Log in</h1>
      <form onSubmit={handleSubmit} noValidate>
        <label>
          Username
          <input
            name="username"
            autoComplete="username"
            value={username}
            onChange={(event) => setUsername(event.target.value)}
            aria-invalid={fields.username ? true : undefined}
            aria-describedby={fields.username ? 'username-error' : undefined}
          />
          <FieldError id="username-error" message={fields.username} />
        </label>
        <label>
          Password
          <input
            name="password"
            type="password"
            autoComplete="current-password"
            value={password}
            onChange={(event) => setPassword(event.target.value)}
            aria-invalid={fields.password ? true : undefined}
            aria-describedby={fields.password ? 'password-error' : undefined}
          />
          <FieldError id="password-error" message={fields.password} />
        </label>
        {isGeneralError(mutation.error) && <ErrorMessage error={mutation.error} />}
        <button type="submit" disabled={mutation.isPending}>
          Log in
        </button>
      </form>
    </article>
  )
}
