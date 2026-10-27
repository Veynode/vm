/// <reference types="vite/client" />
import { useState, type ReactNode } from 'react'
import { HeadContent, Outlet, ScriptOnce, Scripts, createRootRoute, useLocation } from '@tanstack/react-router'
import { TooltipProvider } from '@ui/components/ui/tooltip'
import { Box, Info, Layers, MonitorCog, Plug } from '@ui/icons'
import { SettingsPage, type SettingsPageGroup } from '@ui/settings/settings-page'
import css from '../styles.css?url'
import { SIDEBAR_SCRIPT, SidebarFrame } from '../sidebar'
import { SubPageContext, ToastProvider, type SubPage } from '../ui'
import { useGo } from '../page-nav'

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
    <html lang="en" suppressHydrationWarning>
      <head>
        {/* Before the first paint, run once by the server's page and gone before
            it hydrates: core's theme switches on `.dark`, following the system,
            and the sidebar takes its saved width (sidebar.tsx). */}
        <ScriptOnce>{THEME}</ScriptOnce>
        <ScriptOnce>{SIDEBAR_SCRIPT}</ScriptOnce>
        <HeadContent />
      </head>
      <body>
        <TooltipProvider>{children}</TooltipProvider>
        <Scripts />
      </body>
    </html>
  )
}

const THEME = `(() => {
  const dark = matchMedia('(prefers-color-scheme: dark)')
  const apply = () => document.documentElement.classList.toggle('dark', dark.matches)
  apply()
  dark.addEventListener('change', apply)
})()`

/** The places, one run of rows; a page under one (a container, Run) marks its place's row. */
const PLACES: SettingsPageGroup[] = [
  {
    id: 'places',
    items: [
      { id: '/containers', label: 'Containers', icon: Box },
      { id: '/machines', label: 'VMs', icon: MonitorCog },
      { id: '/images', label: 'Images', icon: Layers },
      { id: '/mcp', label: 'MCP', icon: Plug },
      { id: '/about', label: 'About', icon: Info },
    ],
  },
]

function placeOf(path: string) {
  const top = PLACES[0].items.find((item) => path === item.id || path.startsWith(item.id + '/'))
  // `/` is the places' list; beside it a wider screen shows Containers.
  return top?.id ?? '/containers'
}

/**
 * Core's Settings shell (design-system/settings.md "Shell"): the places on
 * the floor, the page a pane with its 56px title band; below 600px the list
 * and the page take turns.
 */
function Layout() {
  const path = useLocation({ select: (location) => location.pathname })
  const go = useGo()
  // As core's Settings: on a phone the address says which shows, the places
  // (`/`) or a page; going between them moves as a phone's pages move.
  const view = path === '/' ? 'list' : 'page'
  const [sub, setSub] = useState<SubPage | null>(null)
  return (
    <ToastProvider>
      <SubPageContext value={setSub}>
        <SettingsPage
          title="VeynodeVM"
          label="VeynodeVM"
          backLabel="Back"
          groups={PLACES}
          // `/` shows Containers beside the places, named as it.
          page={path === '/' ? '/containers' : path}
          row={placeOf(path)}
          view={view}
          onPageChange={(page) => go({ to: page as '/containers' })}
          onList={() => go({ to: '/', replace: true })}
          heading={sub?.heading}
          back={sub?.back}
          listFrame={SidebarFrame}
        >
          <Outlet />
        </SettingsPage>
      </SubPageContext>
    </ToastProvider>
  )
}
