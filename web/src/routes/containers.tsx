import { createFileRoute } from '@tanstack/react-router'
import { ContainersError, ContainersPage, loadContainers } from '../containers-list'

export const Route = createFileRoute('/containers')({
  loader: loadContainers,
  component: () => <ContainersPage reading={Route.useLoaderData()} />,
  errorComponent: ContainersError,
})
