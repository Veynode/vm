import { useEffect, useRef, useState } from 'react'
import { useRouter } from '@tanstack/react-router'
import { daemonAction, type DaemonStatus } from './api'
import { ConfirmDialog, PendingSwitch, useToastAction } from './ui'

/**
 * Where a switch is going while its engine starts or stops. It is held past
 * the action until the page has read the engines again: `router.invalidate()`
 * settles before the new reading is drawn, and letting go then would draw the
 * old state for a moment, the switch going back and over again.
 */
function useGoing(reading: unknown, actual: boolean) {
  const [going, setGoing] = useState<boolean | null>(null)
  // The reading drawn when the action finished; the next one settles it.
  const [finishedAt, setFinishedAt] = useState<unknown>(null)
  const latest = useRef(reading)
  latest.current = reading
  useEffect(() => {
    if (going !== null && finishedAt !== null && (reading !== finishedAt || actual === going)) {
      setGoing(null)
      setFinishedAt(null)
    }
  }, [reading, actual, going, finishedAt])
  return {
    going,
    start: (to: boolean) => {
      setFinishedAt(null)
      setGoing(to)
    },
    /** Done: wait for a new reading; failed: back to what it is now. */
    finish: (ok: boolean) => (ok ? setFinishedAt(latest.current) : setGoing(null)),
  }
}

/**
 * The VM engine's switch in a page's title band (Containers, VMs): what
 * everything on the page runs on, turned on or off where it is missed.
 * `reading` is the page's loader data, which the switch waits on after an
 * action. It can't be turned off under running VMs.
 */
export function EngineSwitch({ daemon, reading }: { daemon: DaemonStatus | null; reading: unknown }) {
  const router = useRouter()
  const running = !!daemon?.running
  const going = useGoing(reading, running)
  const [stopping, setStopping] = useState(false)
  const action = useToastAction(async (act: 'start' | 'stop') => {
    going.start(act === 'start')
    let ok = false
    try {
      await daemonAction({ data: act })
      await router.invalidate()
      ok = true
    } finally {
      going.finish(ok)
    }
  })
  if (!daemon) return null
  const busy = running && (daemon.assigned_memory_mib ?? 0) > 0
  return (
    <>
      {/* No label: its tooltip names it, and the page is what runs on it. */}
      <span className="flex h-8 shrink-0 items-center" title={busy && going.going === null ? 'VM engine: stop the containers and VMs on it to turn it off' : `VM engine: ${running ? 'on' : 'off'}`}>
        <PendingSwitch
          aria-label="VM engine"
          pending={going.going !== null}
          checked={going.going ?? running}
          disabled={going.going === null && busy}
          onCheckedChange={(on) => (on ? action.run('start') : setStopping(true))}
        />
      </span>
      <ConfirmDialog
        open={stopping}
        title="Stop the VM engine?"
        confirmLabel="Stop"
        busy={action.pending}
        // Closed at once: the switch shows the stopping.
        onConfirm={() => {
          setStopping(false)
          void action.run('stop')
        }}
        close={() => setStopping(false)}
      >
        Containers and VMs can't be started until it is turned on again, here or at your next login.
      </ConfirmDialog>
    </>
  )
}
