import { useState } from 'react'
import { createFileRoute, useRouter } from '@tanstack/react-router'
import { IconButton } from '@ui/components/ui/icon-button'
import { Input } from '@ui/components/ui/input'
import { Download, Layers, Play, Trash2 } from '@ui/icons'
import { Field } from '@ui/settings/field'
import { ListRow } from '@ui/settings/list-row'
import { RowMenu } from '@ui/settings/row-menu'
import { SettingsGroup } from '@ui/settings/settings-section'
import { ImageIcon } from '../container-icon'
import { RunDialog } from '../run-dialog'
import { listImages, pullImage, removeImage } from '../api'
import { ConfirmDialog, EmptyState, FormDialog, PageActions, PageBody, PanelLoadError, age, message, size, useToast, useToastAction } from '../ui'

export const Route = createFileRoute('/images')({
  loader: () => listImages(),
  component: Images,
  errorComponent: ({ error, reset }) => <PanelLoadError title="Couldn't load the images." detail={message(error)} onRetry={reset} />,
})

/** The pulled images, one row each: its size and age, Run, and Remove in its ··· menu. */
function Images() {
  const images = Route.useLoaderData()
  const router = useRouter()
  const [pulling, setPulling] = useState(false)
  const [removing, setRemoving] = useState<string | null>(null)
  const [runImage, setRunImage] = useState<string | null>(null)
  const remove = useToastAction(async (reference: string) => {
    await removeImage({ data: reference })
    await router.invalidate()
  })
  return (
    <PageBody label="Images">
      <PageActions>
        <IconButton variant="ghost" icon={Download} aria-label="Pull" title="Pull an image" onClick={() => setPulling(true)} />
      </PageActions>
      {images.length === 0 ? (
        <EmptyState icon={Layers} title="No images" />
      ) : (
        <SettingsGroup>
          {images.map((i) => (
            <ListRow
              key={i.reference}
              icon={<ImageIcon reference={i.reference} />}
              title={i.reference}
              meta={`${size(i.size)} · ${age(i.created_at)}`}
              metaWhole
              actions={
                <>
                  <IconButton size="sm" variant="ghost" icon={Play} aria-label={`Run ${i.reference}`} title="Run" onClick={() => setRunImage(i.reference)} />
                  <RowMenu label={`More for ${i.reference}`} items={[{ label: 'Remove', icon: Trash2, danger: true, onSelect: () => setRemoving(i.reference) }]} />
                </>
              }
            />
          ))}
        </SettingsGroup>
      )}
      <PullDialog open={pulling} close={() => setPulling(false)} />
      <RunDialog open={runImage !== null} close={() => setRunImage(null)} image={runImage ?? undefined} />
      <ConfirmDialog
        open={removing !== null}
        title={`Remove ${removing ?? ''}?`}
        confirmLabel="Remove"
        busy={remove.pending}
        onConfirm={async () => {
          if (removing) await remove.run(removing)
          setRemoving(null)
        }}
        close={() => setRemoving(null)}
      >
        Containers already made from it keep running.
      </ConfirmDialog>
    </PageBody>
  )
}

/** Pulling opens a sheet: the reference, then Cancel and Pull. A failed pull is a toast and the sheet stays. */
function PullDialog({ open, close }: { open: boolean; close: () => void }) {
  const router = useRouter()
  const toast = useToast()
  const [name, setName] = useState('')
  const pull = useToastAction(async () => {
    await pullImage({ data: name.trim() })
    await router.invalidate()
    toast(`Pulled ${name.trim()}`)
    setName('')
    close()
  })
  return (
    <FormDialog open={open} close={close} title="Pull an image" busy={pull.pending} submitLabel="Pull" canSubmit={!!name.trim()} onSubmit={() => pull.run()}>
      <Field label="Image">
        <Input className="font-mono" placeholder="alpine:3.20" value={name} onChange={(e) => setName(e.target.value)} autoFocus />
      </Field>
    </FormDialog>
  )
}
