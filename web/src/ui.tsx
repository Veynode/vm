import { useEffect, useState } from 'react'
import { useRouter } from '@tanstack/react-router'

/** Re-run route loaders every `ms` while the page is visible. */
export function usePoll(ms: number) {
  const router = useRouter()
  useEffect(() => {
    const timer = setInterval(() => {
      if (document.visibilityState === 'visible') router.invalidate()
    }, ms)
    return () => clearInterval(timer)
  }, [router, ms])
}

/** Wrap an async action with pending and error state. */
export function useAction<A extends unknown[], R>(fn: (...args: A) => Promise<R>) {
  const [pending, setPending] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const run = async (...args: A): Promise<R | undefined> => {
    setPending(true)
    setError(null)
    try {
      return await fn(...args)
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
      return undefined
    } finally {
      setPending(false)
    }
  }
  return { run, pending, error }
}

export function StatusBadge({ state, status }: { state: string; status: string }) {
  const up = !['stopped', 'crashed', 'missing', 'unknown', 'absent'].includes(state)
  const bad = state === 'crashed' || status.startsWith('Failed') || /Exited \((?!0\))/.test(status)
  return <span className={`badge ${bad ? 'bad' : up ? 'up' : 'down'}`}>{status}</span>
}

export function age(seconds: number) {
  const d = Math.max(0, Date.now() / 1000 - seconds)
  if (d < 60) return `${Math.floor(d)}s ago`
  if (d < 3600) return `${Math.floor(d / 60)}m ago`
  if (d < 86400) return `${Math.floor(d / 3600)}h ago`
  return `${Math.floor(d / 86400)}d ago`
}

export function size(bytes: number) {
  if (bytes >= 1 << 30) return `${(bytes / (1 << 30)).toFixed(2)} GB`
  if (bytes >= 1 << 20) return `${(bytes / (1 << 20)).toFixed(1)} MB`
  return `${(bytes / 1024).toFixed(1)} kB`
}

/** Split a command line into argv, honouring single and double quotes. */
export function words(line: string): string[] {
  const out: string[] = []
  let current = ''
  let quote: string | null = null
  let started = false
  for (const ch of line) {
    if (quote) {
      if (ch === quote) quote = null
      else current += ch
    } else if (ch === '"' || ch === "'") {
      quote = ch
      started = true
    } else if (/\s/.test(ch)) {
      if (started || current) out.push(current)
      current = ''
      started = false
    } else {
      current += ch
    }
  }
  if (quote) throw new Error('unterminated quote')
  if (started || current) out.push(current)
  return out
}

export function lines(text: string): string[] {
  return text.split('\n').map((s) => s.trim()).filter(Boolean)
}
