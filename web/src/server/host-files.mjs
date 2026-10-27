// Server-only checks against the host's file system for paths typed in the
// browser (an installer ISO, a seed image, an SSH key). The files are never
// read here: the CLI opens them itself; only their kind is checked, so their
// contents never reach the browser or a log.
import { readFile, stat, statfs } from 'node:fs/promises'
import { homedir } from 'node:os'
import { dirname, join } from 'node:path'

/** `path` (already shaped by machine-input.mjs) exists and is a regular file. */
export async function regularFile(path, label) {
  let info
  try {
    info = await stat(path)
  } catch {
    throw new Error(`${label} not found: ${path}`)
  }
  if (!info.isFile()) throw new Error(`${label} is not a regular file: ${path}`)
  return path
}

/** VEYNODEVM_HOME as the CLI resolves it (vm_control::home::root). */
export function veynodeHome(env = process.env) {
  if (env.VEYNODEVM_HOME) return env.VEYNODEVM_HOME
  return join(env.HOME || homedir(), 'Library/Application Support/VeynodeVM')
}

/** Free bytes on the volume holding `path` (its nearest existing parent before first use). */
export async function freeBytes(path) {
  for (let dir = path; ; dir = dirname(dir)) {
    try {
      const s = await statfs(dir)
      return s.bavail * s.bsize
    } catch (e) {
      if (dir === dirname(dir)) throw e
    }
  }
}

/**
 * What `machine ssh-key` left in `<machine>/ssh/`: the account name (not a
 * secret) and whether a key file is there. The key itself is not opened.
 */
export async function sshLogin(machineDir) {
  const user = await readFile(join(machineDir, 'ssh/user'), 'utf8').then((s) => s.trim(), () => null)
  const key = await stat(join(machineDir, 'ssh/id_ed25519')).then((s) => s.isFile(), () => false)
  return { user: user || null, key }
}
