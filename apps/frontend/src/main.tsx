import '@picocss/pico/css/pico.min.css'
import './app.css'
import { QueryClientProvider } from '@tanstack/react-query'
import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { createBrowserRouter, RouterProvider } from 'react-router'
import { routes } from './App'
import { createQueryClient } from './queryClient'

const router = createBrowserRouter(routes)

const queryClient = createQueryClient(() => {
  const { pathname, search } = router.state.location
  if (pathname === '/login') return
  void router.navigate(`/login?next=${encodeURIComponent(pathname + search)}`, { replace: true })
})

const root = document.getElementById('root')
if (root === null) throw new Error('Missing #root element')

createRoot(root).render(
  <StrictMode>
    <QueryClientProvider client={queryClient}>
      <RouterProvider router={router} />
    </QueryClientProvider>
  </StrictMode>,
)
