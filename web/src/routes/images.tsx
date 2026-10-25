import { useState } from 'react'
import { Link, createFileRoute, useRouter } from '@tanstack/react-router'
import { listImages, pullImage, removeImage } from '../api'
import { age, size, useAction } from '../ui'

export const Route = createFileRoute('/images')({
  loader: () => listImages(),
  component: Images,
})

function Images() {
  const images = Route.useLoaderData()
  const router = useRouter()
  const [name, setName] = useState('')
  const pull = useAction(async () => {
    await pullImage({ data: name.trim() })
    setName('')
    await router.invalidate()
  })
  const remove = useAction(async (reference: string) => {
    await removeImage({ data: reference })
    await router.invalidate()
  })
  return (
    <>
      <div className="spread">
        <h1>Images</h1>
        <form
          className="row"
          onSubmit={(e) => {
            e.preventDefault()
            if (name.trim()) pull.run()
          }}
        >
          <input className="mono" placeholder="alpine:3.20" value={name} onChange={(e) => setName(e.target.value)} style={{ width: 260 }} />
          <button className="primary" disabled={pull.pending}>{pull.pending ? 'Pulling…' : 'Pull'}</button>
        </form>
      </div>
      {(pull.error || remove.error) && <p className="error">{pull.error || remove.error}</p>}
      {images.length === 0 ? (
        <div className="panel empty">No images. Pull a linux/arm64 image from any OCI registry.</div>
      ) : (
        <div className="table-wrap">
          <table>
            <thead>
              <tr>
                <th>Reference</th>
                <th>Image ID</th>
                <th>Default command</th>
                <th>Size</th>
                <th>Pulled</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {images.map((i) => (
                <tr key={i.reference}>
                  <td className="mono">{i.reference}</td>
                  <td className="mono muted">{i.config_digest.slice(7, 19)}</td>
                  <td className="mono wrap">{[...i.config.entrypoint, ...i.config.cmd].join(' ') || '—'}</td>
                  <td>{size(i.size)}</td>
                  <td className="muted">{age(i.created_at)}</td>
                  <td>
                    <div className="row">
                      <Link to="/run" search={{ image: i.reference }} className="button">Run</Link>
                      <button
                        className="danger"
                        disabled={remove.pending}
                        onClick={() => confirm(`Remove ${i.reference}?`) && remove.run(i.reference)}
                      >
                        Remove
                      </button>
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </>
  )
}
