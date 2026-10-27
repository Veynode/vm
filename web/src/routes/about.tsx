import { createFileRoute } from '@tanstack/react-router'
import { Container, Cpu, FileText, Folder, Globe, Info, MemoryStick, Server, SquareTerminal, Zap } from '@ui/icons'
import { SettingsRow } from '@ui/settings/settings-row'
import { SettingsGroup, SettingsSection } from '@ui/settings/settings-section'
import { daemonStatus, engineStatus, systemInfo } from '../api'
import { CodeBlock, PageBody, PanelLoadError, StatusTag, message, usePoll } from '../ui'

export const Route = createFileRoute('/about')({
  // The Docker engine is a micro-VM under the daemon: while the daemon is down
  // it is not read, and it is stopped (a worker stops its VM when the daemon goes).
  loader: async () => {
    const [daemon, info] = await Promise.all([daemonStatus(), systemInfo()])
    return { daemon, info, docker: daemon.running ? await engineStatus() : null }
  },
  component: About,
  errorComponent: ({ error, reset }) => <PanelLoadError title="Couldn't read VeynodeVM's state." detail={message(error)} onRetry={reset} />,
})

const mib = (value: number) => (value % 1024 === 0 ? `${value / 1024} GiB` : `${value} MiB`)

/**
 * What VeynodeVM is running with: its version and CLI, the VM engine (the
 * control daemon everything runs under, turned on and off from Containers
 * and VMs) and the Docker engine (one shared micro-VM running dockerd, started
 * from the CLI or by `run --shared`).
 */
function About() {
  const { daemon, info, docker } = Route.useLoaderData()
  usePoll(4000)
  const exists = !!docker && docker.state !== 'absent'
  return (
    <PageBody label="About">
      <SettingsGroup>
        <SettingsRow icon={Info} label="Version" value={info.version ?? '—'} />
        <SettingsRow icon={SquareTerminal} label="CLI" value={<span className="font-mono" title={info.cli}>{info.cli}</span>} />
      </SettingsGroup>
      <SettingsSection title="VM engine" description={`The control daemon every container and VM runs on; launchd (${daemon.label}) starts it at login.`}>
        <SettingsGroup>
          <SettingsRow icon={Server} label="State" state={<StatusTag state={daemon.running ? 'running' : 'stopped'} status={daemon.running ? 'Running' : 'Not running'} />} />
          {daemon.running && (
            <>
              <SettingsRow
                icon={MemoryStick}
                label="VM memory"
                value={`${daemon.assigned_memory_mib ? mib(daemon.assigned_memory_mib) : 'None'} of ${mib(daemon.memory_budget_mib ?? 0)} assigned`}
              />
              <SettingsRow icon={Zap} label="QEMU/GPU runtime" value={daemon.qemu_registered ? 'Registered' : 'Not registered'} />
            </>
          )}
          {daemon.state_dir && <SettingsRow icon={Folder} label="State directory" value={<span className="font-mono" title={daemon.state_dir}>{daemon.state_dir}</span>} />}
          <SettingsRow icon={FileText} label="Log" value={<span className="font-mono" title={daemon.log}>{daemon.log}</span>} />
        </SettingsGroup>
      </SettingsSection>
      <SettingsSection title="Docker engine" description="A shared micro-VM running dockerd, for the docker CLI, docker build, compose and run --shared. It stops with the VM engine.">
        <SettingsGroup>
          <SettingsRow icon={Container} label="State" state={docker ? <StatusTag state={docker.state} status={docker.status} /> : <StatusTag state="stopped" status="Stopped" />} />
          {exists && (
            <>
              <SettingsRow icon={Globe} label="Endpoint" value={<span className="font-mono">{docker.endpoint}</span>} />
              <SettingsRow icon={Zap} label="Backend" value={docker.backend === 'qemu-hvf' ? 'QEMU/HVF' : 'Apple Virtualization'} />
              <SettingsRow icon={Cpu} label="GPU (CDI)" value={docker.gpu ?? 'None'} />
            </>
          )}
        </SettingsGroup>
        <CodeBlock>{`veynodevm engine start          # add --gpus for GPU containers
veynodevm engine stop

docker --context ${docker?.context ?? 'veynodevm'} ps
docker --context veynodevm compose up -d
eval "$(veynodevm engine env)"   # point this shell at it
docker run --device veynodevm.dev/gpu=all …`}</CodeBlock>
      </SettingsSection>
    </PageBody>
  )
}
