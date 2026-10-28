import type { EventEmitter } from 'node:events'

export const API: number
export const RESTART: string
export class ApiError extends Error {
  code: 'invalid' | 'not_found' | 'state' | 'unsupported' | 'version' | 'failed'
}
export function socketPath(env?: Record<string, string | undefined>): string
export function call<T = any>(method: string, args?: object, options?: { timeoutMs?: number; path?: string }): Promise<T>
export interface Stream extends EventEmitter {
  stdin(data: Buffer): void
  closeStdin(): void
  control(message: unknown): void
  cancel(): void
}
export function stream(method: string, args?: object, options?: { path?: string }): Stream
