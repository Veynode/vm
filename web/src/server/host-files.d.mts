export function regularFile(path: string, label: string): Promise<string>
export function veynodeHome(env?: Record<string, string | undefined>): string
export function freeBytes(path: string): Promise<number>
export function sshLogin(machineDir: string): Promise<{ user: string | null; key: boolean }>
