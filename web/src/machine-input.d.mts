export interface MachineSpec {
  os: 'linux' | 'windows'
  name: string
  iso?: string
  diskSize?: string
  memory?: string
  cpus?: number
  gpus?: 'virgl' | 'venus' | 'neptune'
  backend?: 'apple' | 'qemu'
  diskBus?: 'virtio' | 'nvme'
  monitor?: boolean
  ports: string[]
  attach: string[]
}
export type MachineAction = 'start' | 'stop' | 'kill' | 'eject' | 'rm'

export const NAME: RegExp
export const MACOS_IMAGE: RegExp
export const ISO_EXTENSIONS: string[]
export const ATTACH_EXTENSIONS: string[]
export const MACHINE_ACTIONS: MachineAction[]
export function machineName(input: unknown): string
export function hostPath(input: unknown, extensions: string[], label: string): string
export function memory(input: unknown): string
export function diskSize(input: unknown): string
export function cpus(input: unknown): number
export function port(input: unknown): string
export function sshUser(input: unknown): string
export function displaySize(input: unknown): string
export function macosImage(input: unknown): string
export function createInput(input: unknown): MachineSpec
export function createArgs(spec: MachineSpec): string[]
export function actionArgs(input: unknown): string[]
