import { createFileRoute } from '@tanstack/react-router'
import { ContainersError, ContainersPage, loadContainers } from '../containers-list'

/**
 * The places' list: a phone shows the places alone here (core's Settings at
 * its root); a wider screen shows them with Containers beside them.
 */
export const Route = createFileRoute('/')({
  loader: loadContainers,
  component: () => <ContainersPage reading={Route.useLoaderData()} />,
  errorComponent: ContainersError,
})
