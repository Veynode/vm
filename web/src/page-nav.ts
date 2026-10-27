import { useRouter, type NavigateOptions } from '@tanstack/react-router'
import { pageTransition } from '@console/lib/page-transition'

/**
 * How deep an address is, as a phone stacks its pages: the places' list
 * (`/`), a place (Containers, VMs, Images, MCP, About), a container or VM,
 * then its terminal or desktop.
 */
export function pageDepth(pathname: string) {
  const parts = pathname.split('/').filter(Boolean)
  if (parts.length === 0) return 0
  if (parts[0] === 'terminal' || parts[0] === 'desktop') return 3
  return Math.min(parts.length, 2)
}

/** The motion from one address to another: deeper is a push, shallower a pop. */
export function pageMotion(from: string, to: string) {
  const before = pageDepth(from)
  const after = pageDepth(to)
  if (before === after) return undefined
  return after > before ? ('push' as const) : ('pop' as const)
}

/**
 * Going to another page as a phone goes there (core's `goAsPage`): one
 * deeper comes in from the right over the page it leaves, one up goes back
 * the same way reversed; wider screens and reduced motion just change page.
 */
export function useGo() {
  const router = useRouter()
  return (options: NavigateOptions) => {
    const to = router.buildLocation(options as any).pathname
    const motion = pageMotion(router.state.location.pathname, to)
    pageTransition(
      motion,
      () =>
        new Promise<void>((done) => {
          const finish = () => {
            off()
            clearTimeout(timer)
            done()
          }
          const off = router.subscribe('onRendered', finish)
          const timer = setTimeout(finish, 500)
          void router.navigate(options as any).catch(finish)
        }),
    )
  }
}
