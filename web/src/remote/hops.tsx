// Hop-latency panel: p50/p95 per hop over the last 10 s window (plan §4).
// Not routed yet; `desktop.$id.tsx` will mount it once the remote client exists.

import { Button } from '@ui/components/ui/button'
import { SettingsRow } from '@ui/settings/settings-row'
import { SettingsGroup, SettingsSection } from '@ui/settings/settings-section'
import { HOPS, WINDOW_MS, type HopReport } from './hop-stats'

const ms = (v: number | null) => (v === null ? '—' : v < 10 ? v.toFixed(1) : String(Math.round(v)))

export function HopPanel({ report }: { report: HopReport | undefined }) {
  const download = () => {
    if (!report) return
    const url = URL.createObjectURL(new Blob([JSON.stringify(report, null, 2)], { type: 'application/json' }))
    const a = document.createElement('a')
    a.href = url
    a.download = `veynode-hops-${new Date(report.at).toISOString()}.json`
    a.click()
    URL.revokeObjectURL(url)
  }
  return (
    <SettingsSection
      title="Latency by hop"
      actions={
        <Button size="sm" variant="secondary" onClick={download} disabled={!report}>
          Export JSON
        </Button>
      }
      description="Host ↔ gateway and gateway ↔ browser are half the measured round trip; cross-machine clocks are estimated from pings and only shown."
    >
      <SettingsGroup>
        {HOPS.map(({ id, label }) => {
          const hop = report?.hops[id]
          return (
            <SettingsRow
              key={id}
              label={label}
              value={hop ? `p50 ${ms(hop.p50)} ms · p95 ${ms(hop.p95)} ms · ${hop.n}` : `Collecting the first ${WINDOW_MS / 1000} s`}
            />
          )
        })}
      </SettingsGroup>
    </SettingsSection>
  )
}
