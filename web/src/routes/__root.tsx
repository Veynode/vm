/// <reference types="vite/client" />
import type { ReactNode } from 'react'
import { HeadContent, Link, Outlet, Scripts, createRootRoute } from '@tanstack/react-router'
import css from '../styles.css?url'

export const Route = createRootRoute({
  head: () => ({
    meta: [
      { charSet: 'utf-8' },
      { name: 'viewport', content: 'width=device-width, initial-scale=1' },
      { title: 'VeynodeVM' },
    ],
    links: [{ rel: 'stylesheet', href: css }],
  }),
  shellComponent: Shell,
  component: Layout,
})

function Shell({ children }: { children: ReactNode }) {
  return (
    <html lang="en">
      <head>
        <HeadContent />
      </head>
      <body>
        {children}
        <Scripts />
      </body>
    </html>
  )
}

const NAV = [
  { to: '/', label: 'Containers' },
  { to: '/images', label: 'Images' },
  { to: '/run', label: 'Run' },
  { to: '/engine', label: 'Engine' },
  { to: '/mcp', label: 'MCP' },
] as const

function Layout() {
  return (
    <div className="app">
      <header className="top">
        <Link to="/" className="brand">
          <span className="mark" aria-hidden>▣</span> VeynodeVM
        </Link>
        <nav>
          {NAV.map((n) => (
            <Link key={n.to} to={n.to} activeOptions={{ exact: n.to === '/' }} activeProps={{ className: 'active' }}>
              {n.label}
            </Link>
          ))}
        </nav>
      </header>
      <main>
        <Outlet />
      </main>
    </div>
  )
}
