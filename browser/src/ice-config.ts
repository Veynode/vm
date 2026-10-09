import { DeviceIdentity, SCOPES, verifyGrant, verifySignature, identityId, randomNonce, sha256, unhex, type Challenge, type SignedGrant, type Scope } from './session-auth.js';
/** Signed ICE bootstrap on the current loopback HTTP diagnostic authenticates
 * integrity; it does not encrypt the returned temporary credentials. A future
 * remote bootstrap needs a confidential authenticated transport.
 * ICE diagnostics deliberately exclude addresses, URLs, ICE ufrags and TURN
 * credentials. Selecting relay policy alone is not evidence of a relay path. */
export interface SelectedIcePath {
  localCandidateType: 'host' | 'srflx' | 'prflx' | 'relay';
  remoteCandidateType: 'host' | 'srflx' | 'prflx' | 'relay';
  protocol: 'udp' | 'tcp';
  relayProtocol?: 'udp' | 'tcp' | 'tls';
  roundTripTimeSeconds?: number;
  relayed: boolean;
}
const candidateTypes = new Set(['host', 'srflx', 'prflx', 'relay']);
/** Only transport.selectedCandidatePairId proves selection. A nominated or
 * succeeded pair elsewhere in the report may be an obsolete path. */
export function selectedIcePath(report: RTCStatsReport): SelectedIcePath | null {
  const selected = new Set<string>();
  report.forEach(stat => {
    if (stat.type === 'transport' && stat.dtlsState === 'connected' && typeof stat.selectedCandidatePairId === 'string') selected.add(stat.selectedCandidatePairId);
  });
  if (selected.size !== 1) return null;
  const pair = report.get([...selected][0]!);
  if (pair?.type !== 'candidate-pair' || pair.state !== 'succeeded' || pair.nominated !== true) return null;
  const local = report.get(pair.localCandidateId), remote = report.get(pair.remoteCandidateId);
  if (local?.type !== 'local-candidate' || remote?.type !== 'remote-candidate' || !candidateTypes.has(local.candidateType) || !candidateTypes.has(remote.candidateType)) return null;
  if (!['udp', 'tcp'].includes(local.protocol) || remote.protocol !== local.protocol) return null;
  const result: SelectedIcePath = {
    localCandidateType: local.candidateType, remoteCandidateType: remote.candidateType,
    protocol: local.protocol, relayed: local.candidateType === 'relay' || remote.candidateType === 'relay',
  };
  if (local.candidateType === 'relay' && ['udp', 'tcp', 'tls'].includes(local.relayProtocol)) result.relayProtocol = local.relayProtocol;
  if (typeof pair.currentRoundTripTime === 'number' && Number.isFinite(pair.currentRoundTripTime) && pair.currentRoundTripTime >= 0) result.roundTripTimeSeconds = pair.currentRoundTripTime;
  return result;
}

export interface IceServer { urls: string[]; username?: string; credential?: string }
export interface IceConfiguration { version: 1; transport_policy: 'all' | 'relay'; issued_at: number; expires_at: number; ice_servers: IceServer[] }
export class IceConfigurationError extends Error { constructor(message: string) { super(message); this.name = 'IceConfigurationError'; } }
function ensure(value: unknown, message: string): asserts value { if (!value) throw new IceConfigurationError(message); }
function exact(value: unknown, required: string[], optional: string[] = []): asserts value is Record<string, unknown> {
  ensure(value !== null && typeof value === 'object' && !Array.isArray(value), 'Invalid ICE object');
  const keys = Object.keys(value); ensure(required.every(key => keys.includes(key)) && keys.every(key => required.includes(key) || optional.includes(key)), 'Unknown or missing ICE fields');
}
function strictHex(value: unknown, bytes: number) { unhex(value, bytes); ensure(typeof value === 'string' && !/[^0-9a-f]/.test(value), 'Invalid ICE hex'); }
function integer(value: unknown, minimum = 0): asserts value is number { ensure(typeof value === 'number' && Number.isSafeInteger(value) && value >= minimum, 'Invalid ICE integer'); }
/** The first relay validation slice permits only explicit IPv4 loopback UDP.
 * Other hosts/protocols require a separately reviewed deployment path. */
function configuration(value: unknown, now?: number): IceConfiguration {
  exact(value, ['version', 'transport_policy', 'issued_at', 'expires_at', 'ice_servers']);
  ensure(value.version === 1 && (value.transport_policy === 'all' || value.transport_policy === 'relay'), 'Unsupported ICE configuration');
  integer(value.issued_at); integer(value.expires_at);
  ensure(value.expires_at > value.issued_at && value.expires_at - value.issued_at <= 600, 'ICE credentials expired or lifetime outside bounds');
  if (now !== undefined) ensure(value.issued_at <= now && value.expires_at - now >= 210, 'ICE credentials expired or insufficient remaining lifetime');
  ensure(Array.isArray(value.ice_servers) && value.ice_servers.length <= 4, 'Invalid ICE server count');
  let urls = 0, turns = 0;
  const seen = new Set<string>();
  for (const server of value.ice_servers) {
    exact(server, ['urls'], ['username', 'credential']);
    ensure(Array.isArray(server.urls) && server.urls.length > 0 && server.urls.length <= 8, 'Invalid ICE URLs');
    let isTurn: boolean | undefined;
    for (const url of server.urls) {
      ensure(typeof url === 'string' && url.length <= 128, 'Invalid ICE URL');
      const match = /^(stun|turn):127\.0\.0\.1:([1-9][0-9]{0,4})(\?transport=udp)?$/.exec(url);
      ensure(match && match[0].length === url.length && Number(match[2]) <= 65535 && ((match[1] === 'turn') === (match[3] !== undefined)), 'Only explicit loopback UDP ICE URLs are supported');
      const turn = match[1] === 'turn'; ensure(isTurn === undefined || isTurn === turn, 'Mixed STUN and TURN server entry'); isTurn = turn;
      ensure(!seen.has(url), 'Duplicate ICE URL'); seen.add(url); urls++; if (turn) turns++;
    }
    if (isTurn) {
      ensure(typeof server.username === 'string' && server.username.length >= 1 && server.username.length <= 256 && !/[^\x21-\x7e]/.test(server.username), 'Invalid temporary TURN username');
      ensure(typeof server.credential === 'string' && server.credential.length >= 1 && server.credential.length <= 1024 && !/[^\x21-\x7e]/.test(server.credential), 'Invalid temporary TURN credential');
    } else ensure((server.username === undefined || server.username === '') && (server.credential === undefined || server.credential === ''), 'STUN must not include credentials');
  }
  ensure(urls <= 8 && (value.transport_policy !== 'relay' || turns > 0), 'Invalid ICE relay or URL count');
  return structuredClone(value) as unknown as IceConfiguration;
}
export function validateIceConfiguration(value: unknown, now: number): IceConfiguration { integer(now); return configuration(value, now); }
export function icePeerConfiguration(config: IceConfiguration, now = Math.floor(Date.now() / 1000)): RTCConfiguration {
  const valid = validateIceConfiguration(config, now);
  return { iceServers: valid.ice_servers.map(server => ({ ...server, urls: [...server.urls] })), iceTransportPolicy: valid.transport_policy, iceCandidatePoolSize: 0 };
}
export function iceConfigurationSummary(config: IceConfiguration) {
  return { policy: config.transport_policy, serverCount: config.ice_servers.length, urlCount: config.ice_servers.reduce((n, server) => n + server.urls.length, 0), expiresAt: config.expires_at, scope: 'loopback-udp-only' };
}

export interface IceRequest { challenge: Challenge; request_nonce: string }
export interface SignedIceRequest { request: IceRequest; signature: string }
export interface SignedIceConfiguration { request: IceRequest; config: IceConfiguration; signature: string }
const encoder = new TextEncoder();
class Canon {
  private chunks: Uint8Array<ArrayBuffer>[];
  constructor(domain: string) { this.chunks = [encoder.encode(`Veynode/${domain}/v1\0`)]; }
  text(value: string) { const bytes = encoder.encode(value), size = new Uint8Array(4); new DataView(size.buffer).setUint32(0, bytes.length); this.chunks.push(size, bytes); }
  int(value: number) { integer(value); const bytes = new Uint8Array(8); new DataView(bytes.buffer).setBigUint64(0, BigInt(value)); this.chunks.push(bytes); }
  count(value: number) { const size = new Uint8Array(4); new DataView(size.buffer).setUint32(0, value); this.chunks.push(size); }
  strings(values: string[]) { this.count(values.length); values.forEach(value => this.text(value)); }
  bytes() { const out = new Uint8Array(this.chunks.reduce((n, part) => n + part.length, 0)); let offset = 0; for (const part of this.chunks) { out.set(part, offset); offset += part.length; } return out; }
}
export function iceRequestBytes(request: IceRequest): Uint8Array<ArrayBuffer> {
  exact(request, ['challenge', 'request_nonce']); strictHex(request.request_nonce, 32);
  const ch = request.challenge;
  exact(ch, ['host_id', 'device_id', 'vm_id', 'scopes', 'grant_version', 'instance_nonce', 'nonce', 'issued_at', 'expires_at']);
  strictHex(ch.host_id, 32); strictHex(ch.device_id, 32); strictHex(ch.instance_nonce, 32); strictHex(ch.nonce, 32);
  ensure(typeof ch.vm_id === 'string' && ch.vm_id.length >= 1 && ch.vm_id.length <= 128 && !/[^A-Za-z0-9_.-]/.test(ch.vm_id), 'Invalid ICE VM');
  ensure(Array.isArray(ch.scopes) && ch.scopes.length > 0 && ch.scopes.length <= SCOPES.length && ch.scopes.every((scope, i) => SCOPES.includes(scope) && (i === 0 || ch.scopes[i - 1]! < scope)), 'Invalid ICE scopes');
  integer(ch.grant_version, 1); integer(ch.issued_at); integer(ch.expires_at);
  ensure(ch.expires_at > ch.issued_at && ch.expires_at - ch.issued_at <= 60, 'Invalid ICE challenge lifetime');
  const c = new Canon('ice-config-request');
  c.text(ch.host_id); c.text(ch.device_id); c.text(ch.vm_id); c.strings(ch.scopes); c.int(ch.grant_version); c.text(ch.instance_nonce); c.text(ch.nonce); c.int(ch.issued_at); c.int(ch.expires_at); c.text(request.request_nonce);
  return c.bytes();
}
export function iceConfigurationBytes(config: IceConfiguration): Uint8Array<ArrayBuffer> {
  const value = configuration(config), c = new Canon('ice-config');
  c.int(value.version); c.text(value.transport_policy); c.int(value.issued_at); c.int(value.expires_at);
  c.count(value.ice_servers.length);
  for (const server of value.ice_servers) { c.strings(server.urls); c.text(server.username ?? ''); c.text(server.credential ?? ''); }
  return c.bytes();
}
export async function iceResponseBytes(request: IceRequest, config: IceConfiguration): Promise<Uint8Array<ArrayBuffer>> {
  const c = new Canon('ice-config-response'); c.text(await sha256(iceRequestBytes(request))); c.text(await sha256(iceConfigurationBytes(config))); return c.bytes();
}
function currentRequest(request: IceRequest, now: number) { integer(now); ensure(request.challenge.issued_at <= now && request.challenge.expires_at > now, 'ICE challenge expired or not yet valid'); }
export async function signIceRequest(identity: DeviceIdentity, signedGrant: SignedGrant, pinnedHostPublic: string, challenge: Challenge, options: { vm_id: string; scopes: Scope[]; now: number }): Promise<SignedIceRequest> {
  const settings = structuredClone(options), request = { challenge: structuredClone(challenge), request_nonce: randomNonce() };
  const grant = await verifyGrant(signedGrant, pinnedHostPublic, { device_public_key: identity.publicKey, vm_id: settings.vm_id, scopes: settings.scopes });
  const bytes = iceRequestBytes(request), ch = request.challenge; currentRequest(request, settings.now);
  ensure(ch.host_id === grant.host_id && ch.device_id === identity.id && ch.vm_id === grant.vm_id && ch.grant_version === grant.version && JSON.stringify(ch.scopes) === JSON.stringify(settings.scopes), 'ICE challenge grant binding substituted');
  return { request, signature: await identity.sign(bytes) };
}
export async function verifyIceConfiguration(signed: SignedIceConfiguration, expected: IceRequest, pinnedHostPublic: string, now: number): Promise<IceConfiguration> {
  const value = structuredClone(signed), request = structuredClone(expected); exact(value, ['request', 'config', 'signature']);
  ensure(await sha256(iceRequestBytes(value.request)) === await sha256(iceRequestBytes(request)), 'ICE response request substituted');
  currentRequest(request, now); ensure(request.challenge.host_id === await identityId(pinnedHostPublic), 'ICE host substituted');
  const config = validateIceConfiguration(value.config, now);
  strictHex(value.signature, 64);
  await verifySignature(pinnedHostPublic, await iceResponseBytes(request, config), value.signature);
  return config;
}

/** Count only standardized SDP candidate types/protocols; never return a line,
 * foundation, address, port, ufrag, password, URL or arbitrary token. */
export function iceCandidateCounts(sdp: string) {
  const counts = { total: 0, host: 0, srflx: 0, prflx: 0, relay: 0, udp: 0, tcp: 0, invalid: 0, mediaSections: 0, endOfCandidates: 0 };
  if (typeof sdp !== 'string' || sdp.length > 128 * 1024) { counts.invalid = 1; return counts; }
  for (const line of sdp.split(/\r?\n/)) {
    if (line.startsWith('m=')) counts.mediaSections++;
    if (line === 'a=end-of-candidates') counts.endOfCandidates++;
    if (!line.startsWith('a=candidate:')) continue;
    counts.total++;
    const fields = line.slice(12).split(/ +/), type = fields[7], protocol = fields[2]?.toLowerCase();
    if (fields[6] !== 'typ' || !type || !candidateTypes.has(type) || (protocol !== 'udp' && protocol !== 'tcp')) { counts.invalid++; continue; }
    counts[type as 'host' | 'srflx' | 'prflx' | 'relay']++; counts[protocol]++;
  }
  return counts;
}
export function iceStatsCounts(report: RTCStatsReport) {
  const counts = { localCandidates: 0, remoteCandidates: 0, localRelay: 0, remoteRelay: 0, pairs: 0, frozen: 0, waiting: 0, inProgress: 0, failed: 0, succeeded: 0, nominated: 0, requestsSent: 0, responsesReceived: 0 };
  report.forEach(stat => {
    if (stat.type === 'local-candidate') { counts.localCandidates++; if (stat.candidateType === 'relay') counts.localRelay++; }
    if (stat.type === 'remote-candidate') { counts.remoteCandidates++; if (stat.candidateType === 'relay') counts.remoteRelay++; }
    if (stat.type !== 'candidate-pair') return;
    counts.pairs++; if (stat.nominated === true) counts.nominated++;
    if (stat.state === 'in-progress') counts.inProgress++;
    else if (stat.state === 'frozen' || stat.state === 'waiting' || stat.state === 'failed' || stat.state === 'succeeded') counts[stat.state as 'frozen' | 'waiting' | 'failed' | 'succeeded']++;
    for (const key of ['requestsSent', 'responsesReceived'] as const) if (Number.isSafeInteger(stat[key]) && stat[key] >= 0 && counts[key] <= Number.MAX_SAFE_INTEGER - stat[key]) counts[key] += stat[key];
  });
  return counts;
}

/** Zero decoded frames is different from absent RTP statistics. Null counters
 * remain unavailable rather than being presented as measured zero. */
export function videoReceptionCounts(report: RTCStatsReport) {
  const counts: { streams: number; packetsReceived: number | null; bytesReceived: number | null; framesReceived: number | null; framesDecoded: number | null; keyFramesDecoded: number | null; framesDropped: number | null; pliCount: number | null; nackCount: number | null } = { streams: 0, packetsReceived: null, bytesReceived: null, framesReceived: null, framesDecoded: null, keyFramesDecoded: null, framesDropped: null, pliCount: null, nackCount: null };
  report.forEach(stat => {
    if (stat.type !== 'inbound-rtp' || stat.kind !== 'video') return;
    counts.streams++;
    for (const key of ['packetsReceived', 'bytesReceived', 'framesReceived', 'framesDecoded', 'keyFramesDecoded', 'framesDropped', 'pliCount', 'nackCount'] as const) {
      if (Number.isSafeInteger(stat[key]) && stat[key] >= 0 && (counts[key] ?? 0) <= Number.MAX_SAFE_INTEGER - stat[key]) counts[key] = (counts[key] ?? 0) + stat[key];
    }
  });
  return counts;
}
