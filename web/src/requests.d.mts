export function hardwareChanges(input: unknown, machine: boolean): Record<string, unknown>
export function runRequest(input: {
  image: string
  command: string[]
  name?: string
  ports: string[]
  volumes: string[]
  env: string[]
  cpus?: number
  memory?: string
  gpus?: string
  desktop?: boolean
  desktop_size?: string
  disk_size?: number
}): Record<string, unknown>
export const LABEL: string
export function daemonLabel(root: string, defaultRoot: string): string
