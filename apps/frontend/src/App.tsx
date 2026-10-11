import { Navigate, type RouteObject } from 'react-router'
import { RequireAuth } from './auth/RequireAuth'
import { Layout } from './components/Layout'
import { BookFormPage } from './pages/BookFormPage'
import { BooksPage } from './pages/BooksPage'
import { LoansPage } from './pages/LoansPage'
import { LoginPage } from './pages/LoginPage'
import { MemberDetailPage } from './pages/MemberDetailPage'
import { MemberFormPage } from './pages/MemberFormPage'
import { MembersPage } from './pages/MembersPage'
import { NewLoanPage } from './pages/NewLoanPage'
import { NotFoundPage } from './pages/NotFoundPage'

/** Route table; `main.tsx` turns it into a browser router. */
export const routes: RouteObject[] = [
  {
    element: <Layout />,
    children: [
      { index: true, element: <Navigate to="/books" replace /> },
      { path: 'login', element: <LoginPage /> },
      { path: 'books', element: <BooksPage /> },
      {
        element: <RequireAuth />,
        children: [
          { path: 'books/new', element: <BookFormPage /> },
          { path: 'books/:id/edit', element: <BookFormPage /> },
          { path: 'members', element: <MembersPage /> },
          { path: 'members/new', element: <MemberFormPage /> },
          { path: 'members/:id', element: <MemberDetailPage /> },
          { path: 'members/:id/edit', element: <MemberFormPage /> },
          { path: 'loans', element: <LoansPage /> },
          { path: 'loans/new', element: <NewLoanPage /> },
        ],
      },
      { path: '*', element: <NotFoundPage /> },
    ],
  },
]
