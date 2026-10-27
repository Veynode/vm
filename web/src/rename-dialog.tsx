import { useState } from 'react'
import { Input } from '@ui/components/ui/input'
import { Field } from '@ui/settings/field'
import { renameContainer, renameMachine, type ContainerRow } from './api'
import { FormDialog, useToastAction } from './ui'

/**
 * Renaming a container (`veynodevm rename`, as `docker rename`): only its
 * record changes, the guest keeps the hostname it booted with. `renamed`
 * gets the new name once it is saved.
 */
export function RenameDialog({
  container,
  close,
  renamed,
}: {
  container: Pick<ContainerRow, 'id' | 'name'> | null
  close: () => void
  renamed: (name: string) => unknown
}) {
  return (
    <NameDialog
      target={container && { key: container.id, name: container.name }}
      save={async (name) => (await renameContainer({ data: { id: container!.id, name } })).name}
      close={close}
      renamed={renamed}
    />
  )
}

/**
 * Renaming a stopped VM (`veynodevm machine rename`): its directory and its
 * VM id move to the new name.
 */
export function RenameMachineDialog({ machine, close, renamed }: { machine: { name: string } | null; close: () => void; renamed: (name: string) => unknown }) {
  return (
    <NameDialog
      target={machine && { key: machine.name, name: machine.name }}
      save={async (name) => (await renameMachine({ data: { name: machine!.name, new_name: name } })).name}
      close={close}
      renamed={renamed}
    />
  )
}

function NameDialog({
  target,
  save: write,
  close,
  renamed,
}: {
  target: { key: string; name: string } | null
  save: (name: string) => Promise<string>
  close: () => void
  renamed: (name: string) => unknown
}) {
  const [name, setName] = useState('')
  // Each opening starts from the current name.
  const [openedFor, setOpenedFor] = useState<string | null>(null)
  if ((target?.key ?? null) !== openedFor) {
    setOpenedFor(target?.key ?? null)
    setName(target?.name ?? '')
  }
  const next = name.trim()
  const save = useToastAction(async () => {
    if (!target) return
    const saved = await write(next)
    close()
    await renamed(saved)
  })
  return (
    <FormDialog
      open={target !== null}
      close={close}
      title={`Rename ${target?.name ?? ''}`}
      busy={save.pending}
      submitLabel="Rename"
      canSubmit={/^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$/.test(next) && next !== target?.name}
      onSubmit={() => save.run()}
    >
      <Field label="Name">
        <Input value={name} onChange={(e) => setName(e.target.value)} autoFocus />
      </Field>
    </FormDialog>
  )
}
