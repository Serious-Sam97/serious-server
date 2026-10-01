import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { createBrowserRouter, RouterProvider } from 'react-router'
import './index.css'
import Layout, { Home } from './components/Layout'
import Users from './pages/Users'
import Setup from './pages/Setup'
import Login from './pages/Login'
import Dashboard from './pages/Dashboard'
import Projects, { ProjectsIndex } from './pages/Projects'
import ProjectDetail from './pages/ProjectDetail'
import TerminalPage from './pages/TerminalPage'
import Files from './pages/Files'
import Audit from './pages/Audit'
import Fleet from './pages/Fleet'

const queryClient = new QueryClient({
  defaultOptions: {
    queries: { retry: 1, refetchOnWindowFocus: false },
  },
})

/** Pages that act on one machine: served at `/…` for the home server and at
 *  `/n/<node>/…` for a droplet (the API calls go through the fleet tunnel). */
const nodePages = [
  { path: 'system', element: <Dashboard /> },
  {
    path: 'projects',
    element: <Projects />,
    children: [
      { index: true, element: <ProjectsIndex /> },
      { path: ':name', element: <ProjectDetail /> },
    ],
  },
  { path: 'terminal', element: <TerminalPage /> },
  { path: 'files', element: <Files /> },
  { path: 'audit', element: <Audit /> },
]

const router = createBrowserRouter([
  { path: '/setup', element: <Setup /> },
  { path: '/login', element: <Login /> },
  {
    path: '/',
    element: <Layout />,
    children: [
      { index: true, element: <Home /> },
      { path: 'fleet', element: <Fleet /> },
      { path: 'users', element: <Users /> },
      ...nodePages,
      { path: 'n/:node', children: nodePages },
    ],
  },
])

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <QueryClientProvider client={queryClient}>
      <RouterProvider router={router} />
    </QueryClientProvider>
  </StrictMode>,
)
