import assert from 'node:assert/strict'
import { test } from 'node:test'
import { daemonLabel, hardwareChanges, LABEL, runRequest } from '../src/requests.mjs'

test('container hardware changes are container.update changes', () => {
  assert.deepEqual(
    hardwareChanges({ cpus: 4, memory_mib: 4096, disk_gib: 40, gpus: '', desktop: true, desktop_size: '1920x1080', ports: [], dns: ['1.1.1.1'], mtu: 0, new_mac: true }, false),
    { cpus: 4, memoryMiB: 4096, diskGiB: 40, gpus: 'none', desktop: true, desktopSize: [1920, 1080], ports: [], dns: ['1.1.1.1'], mtu: 0, newMac: true },
  )
  assert.deepEqual(hardwareChanges({ gpus: 'venus', ports: ['127.0.0.1:8080:80/tcp'], mtu: 1400 }, false), { gpus: 'venus', ports: ['127.0.0.1:8080:80/tcp'], mtu: 1400 })
  // Machine-only fields are not container changes.
  assert.throws(() => hardwareChanges({ audio: true }, false), /nothing to change/)
  assert.throws(() => hardwareChanges({ gpus: 'neptune' }, false), /invalid GPU/)
  assert.throws(() => hardwareChanges({ mtu: 100 }, false), /invalid MTU/)
  assert.throws(() => hardwareChanges({ ports: ['80'] }, false), /invalid port/)
})

test('machine hardware changes are machine.update arguments', () => {
  assert.deepEqual(hardwareChanges({ cpus: 8, gpus: '', audio: false, ports: ['2222:22'] }, true), { cpus: 8, gpu: null, audio: false, ports: [{ host: 2222, guest: 22 }] })
  assert.deepEqual(hardwareChanges({ gpus: 'neptune', ports: [] }, true), { gpu: 'neptune', ports: [] })
  // Container-only fields are not machine changes.
  assert.throws(() => hardwareChanges({ desktop: true, dns: [] }, true), /nothing to change/)
  assert.throws(() => hardwareChanges({ ports: ['127.0.0.1:1:2'] }, true), /invalid port/)
  assert.throws(() => hardwareChanges({}, true), /nothing to change/)
})

test('runs are detached, quiet container.run calls', () => {
  assert.deepEqual(runRequest({ image: 'alpine', command: [], ports: [], volumes: [], env: [] }), {
    image: 'alpine',
    command: [],
    detach: true,
    quiet: true,
    ports: [],
    volumes: [],
    env: [],
    desktop: false,
  })
  const full = runRequest({
    image: 'ubuntu',
    command: ['sleep', 'infinity'],
    name: 'web',
    ports: ['8080:80'],
    volumes: ['/Users/me/src:/src:ro'],
    env: ['A=b'],
    cpus: 2,
    memory: '2g',
    disk_size: 40,
    gpus: 'virgl',
    desktop: true,
    desktop_size: '1280x800',
  })
  assert.deepEqual(full, {
    image: 'ubuntu',
    command: ['sleep', 'infinity'],
    detach: true,
    quiet: true,
    ports: ['8080:80'],
    volumes: ['/Users/me/src:/src:ro'],
    env: ['A=b'],
    desktop: true,
    name: 'web',
    cpus: 2,
    memoryMiB: 2048,
    diskGiB: 40,
    gpus: 'virgl',
    desktopSize: [1280, 800],
  })
  // A size without a desktop is not sent.
  assert.equal(runRequest({ image: 'a', command: [], ports: [], volumes: [], env: [], desktop_size: '1280x800' }).desktopSize, undefined)
})

test('daemon labels match the CLI', () => {
  const home = '/Users/me/Library/Application Support/VeynodeVM'
  assert.equal(daemonLabel(home, home), LABEL)
  // Vectors from crates/cli/src/daemon.rs label_for.
  assert.equal(daemonLabel('/tmp/a', home), 'dev.veynode.veynodevm.6cc13bddf2746ce7')
  assert.equal(daemonLabel('/Users/me/VeynodeVM test', home), 'dev.veynode.veynodevm.4bb0cfcc426d06e5')
})
