import assert from 'node:assert/strict'
import { mkdtemp, mkdir, rm, symlink, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { test } from 'node:test'
import { actionArgs, createArgs, createInput, diskSize, displaySize, hostPath, machineName, macosImage, memory, port, sshUser, ISO_EXTENSIONS } from '../src/machine-input.mjs'
import { freeBytes, regularFile, veynodeHome } from '../src/server/host-files.mjs'

test('machine names: no options, paths or shell syntax', () => {
  for (const bad of ['', '-rf', '--iso', '.hidden', '_x', 'a/b', '../x', 'a b', 'a;rm', '$(id)', 'x'.repeat(65), 'ä', 'a\nb', null, 3, ['a']]) {
    assert.throws(() => machineName(bad), /invalid machine name/, String(bad))
  }
  for (const good of ['ubuntu', 'win11', 'Fedora-44.x', 'a', 'x'.repeat(64)]) assert.equal(machineName(good), good)
})

test('ISO paths: absolute, normalised, an expected extension', () => {
  const bad = [
    'installer.iso', // relative
    '~/x.iso',
    '-x.iso',
    '--iso=/x.iso',
    '/tmp/../etc/x.iso',
    '/tmp/./x.iso',
    '/tmp/x.iso\0',
    '/tmp/x\n.iso',
    '/tmp/x.dmg',
    '/tmp/x.iso.sh',
    '/etc/passwd',
    '',
    `/${'a'.repeat(1030)}.iso`,
    42,
    undefined,
  ]
  for (const path of bad) assert.throws(() => hostPath(path, ISO_EXTENSIONS, 'ISO path'), /invalid ISO path/, String(path))
  assert.equal(hostPath('/Users/me/Downloads/Win11_ARM64.ISO', ISO_EXTENSIONS, 'ISO path'), '/Users/me/Downloads/Win11_ARM64.ISO')
  assert.equal(hostPath('/Volumes/My Disk/ubuntu 24.04.iso', ISO_EXTENSIONS, 'ISO path'), '/Volumes/My Disk/ubuntu 24.04.iso')
})

test('ISO paths on the host: must exist and be a regular file', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'vvm-web-'))
  try {
    const file = join(dir, 'a.iso')
    await writeFile(file, 'x')
    await mkdir(join(dir, 'dir.iso'))
    await symlink(file, join(dir, 'link.iso'))
    assert.equal(await regularFile(file, 'ISO'), file)
    assert.equal(await regularFile(join(dir, 'link.iso'), 'ISO'), join(dir, 'link.iso'))
    await assert.rejects(regularFile(join(dir, 'missing.iso'), 'ISO'), /not found/)
    await assert.rejects(regularFile(join(dir, 'dir.iso'), 'ISO'), /not a regular file/)
    await assert.rejects(regularFile('/dev/null', 'ISO'), /not a regular file/)
  } finally {
    await rm(dir, { recursive: true })
  }
})

test('ports: HOST:GUEST TCP within 1..65535', () => {
  for (const bad of ['', '22', ':22', '2222:', '0:22', '2222:0', '65536:22', '2222:70000', '127.0.0.1:2222:22', '2222:22/udp', '-p', '2222:22;x', ' 2222:22', 2222, null]) {
    assert.throws(() => port(bad), /invalid port/, String(bad))
  }
  for (const good of ['2222:22', '1:65535', '3389:3389']) assert.equal(port(good), good)
})

test('memory: digits with an optional m/g, 512 MiB through 128 GiB', () => {
  for (const bad of ['', '4', '511m', '4 g', '4gb', '4.5g', '-4g', '129g', '4k', 'g', '1e3', '0x10g', 4, null]) {
    assert.throws(() => memory(bad), /invalid memory/, String(bad))
  }
  for (const good of ['512', '512m', '4g', '4G', '8192M', '128g']) assert.equal(memory(good), good)
})

test('disk sizes: 4 GiB through 4 TiB', () => {
  for (const bad of ['', '3G', '5T', '40GB', '40 G', '-40G', '1.5T', 'G']) assert.throws(() => diskSize(bad), /invalid disk size/, bad)
  for (const good of ['4G', '64G', '64g', '64', '1T', '4096']) assert.equal(diskSize(good), good)
})

test('create: options that need QEMU are refused on Apple Virtualization', () => {
  assert.throws(() => createInput({ name: 'u', backend: 'apple', ports: ['2222:22'] }), /QEMU/)
  assert.throws(() => createInput({ name: 'u', diskBus: 'nvme' }), /QEMU/)
  assert.throws(() => createInput({ name: 'u', monitor: true }), /QEMU/)
  assert.throws(() => createInput({ name: 'w', os: 'windows', backend: 'apple' }), /Apple Virtualization/)
  assert.throws(() => createInput({ name: 'w', os: 'windows', gpus: 'virgl' }), /invalid gpu/)
  assert.throws(() => createInput({ name: 'u', gpus: 'neptune' }), /invalid gpu/)
  assert.throws(() => createInput({ name: 'u', os: 'macos' }), /invalid os/)
  assert.throws(() => createInput({ name: 'u', cpus: 0 }), /invalid cpus/)
  assert.throws(() => createInput({ name: 'u', cpus: '4; rm' }), /invalid cpus/)
  assert.throws(() => createInput({ name: 'u', attach: ['/x/seed.sh'] }), /invalid attached image/)
  assert.throws(() => createInput({ name: 'u', attach: 'not-a-list' }), /invalid attached images/)
  assert.throws(() => createInput({ name: 'u', ports: new Array(17).fill('1:1'), backend: 'qemu' }), /invalid ports/)
})

test('create: a checked form becomes one argv element per value', () => {
  const spec = createInput({
    os: 'windows',
    name: 'win11',
    iso: '/Users/me/Win 11.iso',
    diskSize: '64G',
    memory: '8g',
    cpus: '6',
    gpus: 'neptune',
    diskBus: 'nvme',
    ports: ['2222:22'],
    attach: ['/Users/me/virtio-win.iso', '/Users/me/autounattend.img'],
  })
  assert.deepEqual(createArgs(spec), [
    'machine', 'create', 'win11', '--os', 'windows', '--iso', '/Users/me/Win 11.iso', '--disk-size', '64G', '--memory', '8g', '--cpus', '6',
    '--gpus', 'neptune', '--disk-bus', 'nvme', '--publish', '2222:22', '--attach', '/Users/me/virtio-win.iso', '--attach', '/Users/me/autounattend.img',
  ])
  assert.deepEqual(createArgs(createInput({ name: 'ubuntu' })), ['machine', 'create', 'ubuntu', '--os', 'linux'])
})

test('actions map to machine subcommands only', () => {
  assert.deepEqual(actionArgs({ action: 'kill', name: 'u' }), ['machine', 'stop', '--force', 'u'])
  assert.deepEqual(actionArgs({ action: 'rm', name: 'u' }), ['machine', 'rm', 'u'])
  assert.deepEqual(actionArgs({ action: 'eject', name: 'u' }), ['machine', 'eject', 'u'])
  for (const action of ['ssh', 'create', 'exec', '', undefined]) assert.throws(() => actionArgs({ action, name: 'u' }), /invalid action/)
  assert.throws(() => actionArgs({ action: 'start', name: '-f' }), /invalid machine name/)
})

test('display sizes are WIDTHxHEIGHT within the CLI bounds', () => {
  for (const bad of ['', '1920', '1920x', 'x1080', '1920X1080', '1920x1080 ', '639x480', '640x479', '3841x2160', '3840x2161', '-1920x1080', '1920x1080;id', 1920, null]) {
    assert.throws(() => displaySize(bad), /invalid display size/, String(bad))
  }
  for (const good of ['640x480', '1920x1080', '2048x1152', '3840x2160']) assert.equal(displaySize(good), good)
})

test('ssh-key user and macOS image references', () => {
  for (const bad of ['', 'a b', 'a;b', 'DOMAIN\\user', '-o']) assert.throws(() => sshUser(bad), /invalid user/, bad)
  assert.equal(sshUser('veynode'), 'veynode')
  for (const bad of ['alpine', 'ghcr.io/cirruslabs/ubuntu', 'ghcr.io/cirruslabs/macos-tahoe-vanilla;id', 'docker.io/cirruslabs/macos-x', '-ghcr.io/cirruslabs/macos-x']) {
    assert.throws(() => macosImage(bad), /invalid macOS image/, bad)
  }
  assert.equal(macosImage('ghcr.io/cirruslabs/macos-tahoe-vanilla:latest'), 'ghcr.io/cirruslabs/macos-tahoe-vanilla:latest')
})

test('the home and its free space follow VEYNODEVM_HOME', async () => {
  assert.equal(veynodeHome({ VEYNODEVM_HOME: '/tmp/vvm' }), '/tmp/vvm')
  assert.equal(veynodeHome({ HOME: '/Users/me' }), '/Users/me/Library/Application Support/VeynodeVM')
  // A home that does not exist yet reports its volume's free space.
  assert.ok((await freeBytes(join(tmpdir(), 'vvm-missing', 'deeper'))) > 0)
})
