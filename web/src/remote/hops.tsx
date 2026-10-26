// Hop-latency panel: p50/p95 per hop over the last 10 s window (plan §4).
// Not routed yet; `desktop.$id.tsx` will mount it once the remote client exists.

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
    <div className="panel">
      <div className="spread">
        <strong>Latency by hop</strong>
        <button type="button" onClick={download} disabled={!report}>
          Export JSON
        </button>
      </div>
      {report ? (
        <table>
          <thead>
            <tr>
              <th>Hop</th>
              <th>p50 ms</th>
              <th>p95 ms</th>
              <th>Samples</th>
            </tr>
          </thead>
          <tbody>
            {HOPS.map(({ id, label }) => {
              const hop = report.hops[id]
              return (
                <tr key={id}>
                  <td>{label}</td>
                  <td className="mono">{ms(hop.p50)}</td>
                  <td className="mono">{ms(hop.p95)}</td>
                  <td className="mono muted">{hop.n}</td>
                </tr>
              )
            })}
          </tbody>
        </table>
      ) : (
        <p className="muted">Collecting the first {WINDOW_MS / 1000} s window…</p>
      )}
      <p className="muted">Host ↔ gateway and gateway ↔ browser are half the measured round trip; cross-machine clocks are estimated from pings and only shown.</p>
    </div>
  )
}
