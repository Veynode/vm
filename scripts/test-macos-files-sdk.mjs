#!/usr/bin/env node
// Actual opt-in macOS Guest files acceptance through the pinned official MCP SDK.
// Does not start/stop VMs or enable Guest permissions. Published test files remain.
import { open, readFile, lstat, realpath, access } from 'node:fs/promises';
import { constants } from 'node:fs';
import { createHash, randomBytes, randomUUID } from 'node:crypto';
import { isAbsolute, dirname } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';

const sdkRoot = new URL('../tests/mcp-client/node_modules/@modelcontextprotocol/sdk/', import.meta.url);
const sdkVersion = '1.32.1';
class CheckFailure extends Error { constructor(code) { super(code); this.code = code; } }
function check(value, code) { if (!value) throw new CheckFailure(code); }
const digest = bytes => createHash('sha256').update(bytes).digest('hex');
const object = value => value !== null && typeof value === 'object' && !Array.isArray(value);
async function sdk() {
  const installed = JSON.parse(await readFile(new URL('package.json', sdkRoot), 'utf8'));
  const declared = JSON.parse(await readFile(new URL('../tests/mcp-client/package.json', import.meta.url), 'utf8'));
  check(installed.name === '@modelcontextprotocol/sdk' && installed.version === sdkVersion
    && declared.dependencies['@modelcontextprotocol/sdk'] === sdkVersion, 'official_sdk_pin_mismatch');
  const { Client } = await import(new URL('dist/esm/client/index.js', sdkRoot));
  const { StdioClientTransport } = await import(new URL('dist/esm/client/stdio.js', sdkRoot));
  return { Client, StdioClientTransport };
}
function options(argv) {
  const args = {};
  for (let n = 0; n < argv.length; n += 2) {
    const key = argv[n];
    check(['--binary', '--state-dir', '--id', '--output', '--expected-binary-sha256'].includes(key)
      && args[key] === undefined && typeof argv[n + 1] === 'string' && !argv[n + 1].startsWith('--'), 'invalid_arguments');
    args[key] = argv[n + 1];
  }
  for (const key of ['--binary', '--state-dir', '--output']) check(isAbsolute(args[key] ?? ''), 'absolute_paths_required');
  check(/^[A-Za-z0-9_-]{1,64}$/.test(args['--id'] ?? ''), 'invalid_vm_id');
  if (args['--expected-binary-sha256']) check(/^[a-f0-9]{64}$/.test(args['--expected-binary-sha256']), 'invalid_binary_hash');
  return args;
}
async function main(argv) {
  if (argv.length === 1 && argv[0] === '--help') {
    console.log('node scripts/test-macos-files-sdk.mjs --binary /absolute/frozen/vm-control --state-dir /absolute/state --id VM_ID --output /absolute/new-report.json [--expected-binary-sha256 HEX]\n--check validates installed SDK imports without connecting or creating evidence.');
    return;
  }
  const { Client, StdioClientTransport } = await sdk();
  if (argv.length === 1 && argv[0] === '--check') {
    console.log(JSON.stringify({ checked: true, sdk: `@modelcontextprotocol/sdk ${sdkVersion}`, guest_connected: false })); return;
  }
  const args = options(argv);
  const binary = args['--binary'], state = args['--state-dir'], vm = args['--id'];
  const binaryMetadata = await lstat(binary), stateMetadata = await lstat(state);
  check(binaryMetadata.isFile() && !binaryMetadata.isSymbolicLink(), 'binary_must_be_regular_file');
  check(stateMetadata.isDirectory() && !stateMetadata.isSymbolicLink() && stateMetadata.uid === process.getuid()
    && (stateMetadata.mode & 0o777) === 0o700, 'state_must_be_private_owner_directory');
  await access(binary, constants.X_OK);
  const binaryHash = digest(await readFile(binary));
  check(!args['--expected-binary-sha256'] || args['--expected-binary-sha256'] === binaryHash, 'frozen_binary_hash_mismatch');
  const outputParent = await realpath(dirname(args['--output']));
  const parentMetadata = await lstat(outputParent);
  check(parentMetadata.isDirectory() && parentMetadata.uid === process.getuid(), 'evidence_parent_owner_mismatch');
  // Exclusive creation before connecting: never overwrite prior evidence.
  const evidence = await open(args['--output'], constants.O_CREAT | constants.O_EXCL | constants.O_WRONLY | constants.O_NOFOLLOW, 0o600);
  const report = { passed: false, sdk: { name: '@modelcontextprotocol/sdk', version: sdkVersion },
    transport: 'stdio', controller_sha256: binaryHash, vm_id: vm, started_at: new Date().toISOString(),
    scope: 'Actual macOS opt-in private-root same-UID files via official SDK; not command isolation or VM lifecycle acceptance',
    checks: [], clients: [], published_files_remain: true };
  const clients = [];
  let stage = 'sdk-connect';
  const record = (name, value = true) => report.checks.push({ name, evidence: value });
  async function connect(role) {
    const transport = new StdioClientTransport({ command: binary, args: ['mcp', '--state-dir', state, '--allow-files'], stderr: 'pipe' });
    const client = new Client({ name: 'veynode-macos-files-sdk-acceptance', version: '1.0.0' }, { capabilities: {} });
    const entry = { client, transport, pid: null, role, closed: false, closeEvent: false, stderrBytes: 0 };
    clients.push(entry);
    transport.stderr.on('data', chunk => { entry.stderrBytes += chunk.length; }); // Drain, never retain/log raw diagnostics.
    // Protocol.connect chains the existing callback; observe initialization
    // failure as well as normal closure without touching SDK-private children.
    transport.onclose = () => { entry.closeEvent = true; };
    await client.connect(transport, { timeout: 15000 });
    entry.pid = transport.pid;
    check(Number.isInteger(entry.pid) && entry.pid > 0, 'sdk_child_pid_missing');
    return entry;
  }
  async function tool(entry, name, fields = {}, denied = false) {
    let response;
    try { response = await entry.client.callTool({ name: `vm_${name}`, arguments: { id: vm, ...fields } }, undefined, { timeout: 15000 }); }
    catch { throw new CheckFailure('sdk_tool_rpc_failed'); }
    if (denied) { check(response.isError === true, 'expected_guest_denial'); return; }
    check(response.isError !== true && Array.isArray(response.content) && response.content.length === 1
      && response.content[0].type === 'text', 'unexpected_tool_result');
    let envelope;
    try { envelope = JSON.parse(response.content[0].text); } catch { throw new CheckFailure('invalid_tool_json'); }
    check(object(envelope) && object(envelope.result) && !Object.hasOwn(envelope, 'error'), 'invalid_guest_result');
    return envelope.result;
  }
  async function close(entry) {
    if (entry.closed) return;
    // SDK close() may escalate TERM/KILL after its own deadlines. A null SDK pid
    // is not exit proof: wait for its real close callback and test the saved PID.
    entry.pid ??= entry.transport.pid;
    await entry.client.close();
    const end = performance.now() + 5000;
    while (!entry.closeEvent && performance.now() < end) await delay(20);
    let gone = entry.pid === null && entry.closeEvent;
    if (entry.pid !== null) {
      try { process.kill(entry.pid, 0); } catch (error) { if (error.code === 'ESRCH') gone = true; }
    }
    check(gone && entry.closeEvent, 'sdk_child_cleanup_unconfirmed');
    entry.closed = true;
    report.clients.push({ role: entry.role, sdk_close_event: entry.closeEvent, child_no_longer_exists: gone,
      stderr_bytes_discarded: entry.stderrBytes, exit_code: 'not exposed by public SDK transport' });
  }
  try {
    const primary = await connect('files-primary'), other = await connect('files-other-owner');
    for (const entry of [primary, other]) {
      const listed = await entry.client.listTools({}, { timeout: 15000 });
      const names = listed.tools.map(value => value.name);
      check(['vm_guest_info', 'vm_file_upload_begin', 'vm_file_upload_write', 'vm_file_upload_commit', 'vm_file_download_begin', 'vm_file_download_read', 'vm_file_cancel'].every(name => names.includes(name)), 'required_tools_missing');
      check(!names.some(name => /^(vm_exec|vm_input|vm_acquire_control|vm_start|vm_shutdown|vm_force_stop|vm_create)/.test(name)), 'unexpected_non_files_permission');
    }
    record('official SDK initialized and negotiated file-only tool permissions');
    stage = 'guest-identity';
    const info = await tool(primary, 'guest_info');
    check(info.os === 'darwin' && info.arch === 'arm64' && typeof info.machine === 'string' && info.machine.startsWith('VirtualMac')
      && Number.isInteger(info.uid) && info.uid > 0 && info.capabilities?.files === true
      && info.capabilities.exec === false && info.capabilities.async_exec === false, 'guest_identity_or_capability_mismatch');
    check(info.file_policy?.uid === info.uid && info.file_policy.max_file_bytes === 8388608 && info.file_policy.chunk_bytes === 32768, 'guest_file_policy_mismatch');
    check(info.file_policy.active_transfers === 0 && info.file_policy.staged_upload_files === 0, 'fixture_not_idle');
    report.guest = { os: info.os, arch: info.arch, machine: info.machine, uid: info.uid, files: true, exec: false, async_exec: false, max_file_bytes: 8388608, chunk_bytes: 32768 };
    record('actual darwin VirtualMac files capability and same-UID policy');
    stage = 'roundtrip';
    const bytes = Buffer.concat([Buffer.from('Unicode payload: 你好 ✓ café\n'.repeat(1024), 'utf8'), Buffer.from([0, 255, 128, 0]), randomBytes(40001)]);
    check(bytes.length < 128 * 1024, 'fixture_payload_bound');
    const hash = digest(bytes), name = `sdk-${randomUUID()}.bin`;
    const upload = await tool(primary, 'file_upload_begin', { path: name, size: bytes.length, sha256: hash });
    check(/^[a-f0-9]{32}$/.test(upload.token ?? ''), 'invalid_transfer_token');
    await tool(other, 'file_upload_write', { token: upload.token, offset: 0, data: '' }, true);
    await tool(primary, 'file_download_begin', { path: name }, true);
    for (let offset = 0; offset < bytes.length; offset += 32768) {
      const part = bytes.subarray(offset, offset + 32768);
      const reply = await tool(primary, 'file_upload_write', { token: upload.token, offset, data: part.toString('base64') });
      check(reply.offset === offset + part.length, 'upload_offset_mismatch');
    }
    const committed = await tool(primary, 'file_upload_commit', { token: upload.token });
    check(committed.published === true && committed.size === bytes.length && committed.sha256 === hash, 'publication_mismatch');
    const download = await tool(primary, 'file_download_begin', { path: name });
    check(download.size === bytes.length && download.sha256 === hash && /^[a-f0-9]{32}$/.test(download.token ?? ''), 'download_metadata_mismatch');
    await tool(other, 'file_download_read', { token: download.token, offset: 0 }, true);
    const parts = []; let offset = 0;
    while (true) {
      const part = await tool(primary, 'file_download_read', { token: download.token, offset });
      check(typeof part.data === 'string' && part.data.length <= 43692, 'download_chunk_bound');
      const decoded = Buffer.from(part.data, 'base64');
      check(decoded.toString('base64') === part.data && decoded.length <= 32768 && decoded.length > 0, 'invalid_download_base64');
      offset += decoded.length;
      check(offset <= bytes.length && part.offset === offset && part.sha256 === hash && typeof part.eof === 'boolean', 'download_chunk_mismatch');
      parts.push(decoded);
      if (part.eof) break;
    }
    const downloaded = Buffer.concat(parts);
    check(offset === bytes.length && downloaded.equals(bytes) && digest(downloaded) === hash, 'roundtrip_bytes_or_hash_mismatch');
    record('atomic generated Unicode/binary roundtrip and foreign file-owner token denial', { bytes: bytes.length, sha256: hash, published_filename: name });
    stage = 'sdk-close-and-guest-cleanup';
    const unfinishedName = `sdk-partial-${randomUUID()}.bin`;
    const unfinished = await tool(primary, 'file_upload_begin', { path: unfinishedName, size: 8, sha256: digest(Buffer.alloc(8)) });
    await tool(primary, 'file_upload_write', { token: unfinished.token, offset: 0, data: Buffer.alloc(3).toString('base64') });
    const pendingInfo = await tool(other, 'guest_info');
    check(pendingInfo.file_policy.active_transfers === 1 && pendingInfo.file_policy.staged_upload_files === 1, 'partial_upload_not_observed');
    await close(primary);
    const end = performance.now() + 5000;
    while (true) {
      const current = await tool(other, 'guest_info');
      if (current.file_policy?.active_transfers === 0 && current.file_policy.staged_upload_files === 0) break;
      check(performance.now() < end, 'guest_eof_cleanup_unconfirmed'); await delay(50);
    }
    await tool(other, 'file_download_begin', { path: unfinishedName }, true);
    record('SDK close followed by observed Guest partial-transfer cleanup', { active_transfers: 0, staged_upload_files: 0, unfinished_file_not_published: true });
    await close(other);
    check(digest(await readFile(binary)) === binaryHash, 'controller_binary_changed_during_test');
    report.passed = true;
  } catch (error) {
    report.error = { stage, code: error instanceof CheckFailure ? error.code : 'acceptance_operation_failed' };
  } finally {
    for (const entry of clients) {
      try { await close(entry); } catch { report.passed = false; report.cleanup_unconfirmed = true; }
    }
    report.finished_at = new Date().toISOString();
    await evidence.writeFile(`${JSON.stringify(report, null, 2)}\n`); await evidence.sync(); await evidence.close();
  }
  console.log(JSON.stringify({ passed: report.passed, checks: report.checks.length, cleanup_unconfirmed: report.cleanup_unconfirmed === true }));
  if (!report.passed) process.exitCode = 1;
}
main(process.argv.slice(2)).catch(() => { console.error('SDK acceptance setup failed; no raw response or credential diagnostics emitted.'); process.exitCode = 1; });
