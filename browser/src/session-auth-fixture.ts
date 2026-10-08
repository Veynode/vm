import { BrowserIdentityStore, DeviceIdentity, hex, identityId, sha256, pairingBytes, grantBytes, offerBytes, answerBytes, confirmationBytes, closeBytes, sdpBinding, verifyGrant, verifyOffer, verifyAnswer, verifyConfirmation, verifyClose, signClose, signOffer, confirmAnswer, randomNonce, type Pairing, type SignedGrant, type SignedOffer, type SignedAnswer, type Confirmation, type SignedClose, type Challenge } from './session-auth.js';
const button = document.querySelector<HTMLButtonElement>('#run')!;
const unpair = document.querySelector<HTMLButtonElement>('#unpair')!;
const download = document.querySelector<HTMLButtonElement>('#download')!;
const status = document.querySelector<HTMLElement>('#status')!;
const output = document.querySelector<HTMLElement>('#result')!;
const namespace = 'veynode-session-auth-fixture-v1';
let store: BrowserIdentityStore | undefined, identity: DeviceIdentity | undefined;
let report: Record<string, unknown> = {};
interface Golden { fixture_only: boolean; now: number; host_public_key: string; host_id: string; device_public_key: string; device_id: string; pairing: Pairing; pairing_digest: string; signed_grant: SignedGrant; signed_offer: SignedOffer; signed_answer: SignedAnswer; confirmation: Confirmation; signed_close: SignedClose; offer_sdp: string; answer_sdp: string; canonical_hex: Record<string, string> }
function assert(value: unknown, message: string): asserts value { if (!value) throw new Error(message); }
function show() { output.textContent = JSON.stringify(report, null, 2); download.disabled = false; }
async function rejects(action: () => Promise<unknown>) { let denied = false; try { await action(); } catch { denied = true; } assert(denied, 'Tampered input was accepted'); }
async function rustVerify(value: unknown) {
  const response = await fetch('/verify', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(value), credentials: 'omit', cache: 'no-store', signal: AbortSignal.timeout(6000) });
  const result = await response.json() as { verified?: boolean; authorization_checked?: boolean; error?: string };
  assert(response.ok && result.verified === true && result.authorization_checked === false, `Rust signature verification failed: ${result.error ?? response.status}`); return result;
}
button.addEventListener('click', () => { void (async () => {
  button.disabled = true; unpair.disabled = true; status.textContent = 'Running fixture checks…';
  report = { scope: 'synthetic SDP cryptography/IndexedDB fixture only', internet_e2ee_validated: false, vm_started: false, real_host_paired: false, database: namespace, user_agent: navigator.userAgent, started_at: new Date().toISOString(), checks: [] };
  const checks: string[] = []; const passed = (name: string) => { checks.push(name); report.checks = [...checks]; show(); };
  try {
    const golden = await (await fetch('/golden.json', { cache: 'no-store', credentials: 'omit' })).json() as Golden;
    assert(golden.fixture_only === true, 'Expected public synthetic Rust fixture');
    assert(await identityId(golden.host_public_key) === golden.host_id && await identityId(golden.device_public_key) === golden.device_id, 'Golden identity mismatch');
    for (const [name, bytes] of Object.entries({ pairing: await pairingBytes(golden.pairing), grant: await grantBytes(golden.signed_grant.grant), offer: offerBytes(golden.signed_offer.intent), answer: answerBytes(golden.signed_answer.transcript), confirm: confirmationBytes(golden.confirmation.transcript_sha256), close: closeBytes(golden.signed_close.session_id, golden.signed_close.transcript_sha256) })) assert(hex(bytes) === golden.canonical_hex[name], `${name} canonical bytes differ`);
    assert(await sha256(await pairingBytes(golden.pairing)) === golden.pairing_digest, 'Pairing digest mismatch'); passed('Rust identity and all six canonical byte fixtures');
    await verifyGrant(golden.signed_grant, golden.host_public_key, { device_public_key: golden.device_public_key, vm_id: golden.pairing.vm_id, scopes: golden.pairing.scopes, version: golden.signed_grant.grant.version });
    await verifyOffer(golden.signed_offer, golden.signed_grant.grant, golden.signed_offer.intent.challenge, golden.offer_sdp, golden.now);
    await verifyAnswer(golden.signed_answer, golden.host_public_key, golden.signed_offer.intent, golden.answer_sdp, golden.now);
    await verifyConfirmation(golden.confirmation, golden.signed_answer.transcript, golden.device_public_key, golden.now);
    await verifyClose(golden.signed_close, golden.signed_answer.transcript, golden.device_public_key); passed('Rust signatures verified in actual browser WebCrypto (fixture clock)');
    store?.close(); store = await BrowserIdentityStore.open(namespace, 'interop-device');
    const [loaded, concurrent] = await Promise.all([store.loadOrCreate(), store.loadOrCreate()]); identity = loaded.identity;
    assert(identity.id === concurrent.identity.id, 'Concurrent identity creation diverged');
    report.identity_created = loaded.created || concurrent.created; report.identity_id = identity.id; report.public_key = identity.publicKey;
    await identity.assertPrivateExportRejected(); assert(!identity.privateKeyExtractable, 'Private key extractable'); passed('Non-extractable P-256 and actual private export refusal');
    const original = identity.id; store.close(); store = await BrowserIdentityStore.open(namespace, 'interop-device'); const reopened = await store.loadOrCreate(); identity = reopened.identity;
    assert(!reopened.created && identity.id === original, 'IndexedDB reopen changed identity'); await identity.assertPrivateExportRejected(); passed('IndexedDB CryptoKey structured clone survives close/reopen');
    const host = await DeviceIdentity.generate(); const now = Math.floor(Date.now() / 1000);
    const grant = { host_id: host.id, device_id: identity.id, device_public_key: identity.publicKey, vm_id: 'synthetic-vm', scopes: ['observe'] as ['observe'], version: 1 };
    const signedGrant = { grant, signature: await host.sign(await grantBytes(grant)) };
    const challenge: Challenge = { host_id: host.id, device_id: identity.id, vm_id: grant.vm_id, scopes: ['observe'], grant_version: 1, instance_nonce: randomNonce(), nonce: randomNonce(), issued_at: now, expires_at: now + 60 };
    const offer = await signOffer(identity, signedGrant, host.publicKey, challenge, golden.offer_sdp, { vm_id: grant.vm_id, scopes: ['observe'], request_id: crypto.randomUUID(), codec: 'h264', now });
    const transcript = { intent: offer.intent, session_id: crypto.randomUUID(), answer: await sdpBinding(golden.answer_sdp) };
    const answer = { transcript, signature: await host.sign(answerBytes(transcript)) };
    const confirmed = await confirmAnswer(identity, answer, host.publicKey, offer.intent, golden.answer_sdp, now);
    await verifyConfirmation(confirmed.confirmation, transcript, identity.publicKey, now);
    for (const value of [{ kind: 'grant', host_public_key: host.publicKey, signed: signedGrant }, { kind: 'offer', device_public_key: identity.publicKey, signed: offer, exact_sdp: golden.offer_sdp }, { kind: 'answer', host_public_key: host.publicKey, expected_intent: offer.intent, exact_sdp: golden.answer_sdp, signed: answer }, { kind: 'confirm', device_public_key: identity.publicKey, transcript, signed: confirmed.confirmation }, { kind: 'close', device_public_key: identity.publicKey, transcript, signed: await signClose(identity, transcript) }]) await rustVerify(value);
    passed('Browser-generated grant/offer/answer/confirmation/close verified by Rust CLI');
    for (const [field, value] of Object.entries({ host_id: randomNonce(), device_id: randomNonce(), vm_id: 'another-vm', scopes: ['input'], grant_version: 2, instance_nonce: randomNonce(), nonce: randomNonce(), issued_at: now - 1, expires_at: now + 59 })) {
      const changed = structuredClone(answer); Object.assign(changed.transcript.intent.challenge, { [field]: value });
      await rejects(() => verifyAnswer(changed, host.publicKey, offer.intent, golden.answer_sdp, now));
    }
    await rejects(() => verifyAnswer(answer, host.publicKey, offer.intent, golden.answer_sdp + 'a=x:substituted\r\n', now));
    await rejects(() => verifyAnswer(answer, host.publicKey, offer.intent, golden.answer_sdp, now + 60));
    const wrongFingerprint = structuredClone(answer); wrongFingerprint.transcript.answer.fingerprint_sha256 = randomNonce();
    await rejects(() => verifyAnswer(wrongFingerprint, host.publicKey, offer.intent, golden.answer_sdp, now)); passed('Identity/nonce/VM/scope/fingerprint/SDP/expiry substitutions rejected');
    report.status = 'passed'; report.completed_at = new Date().toISOString(); status.textContent = 'Passed. Reload and run again for a separate-page persistence check; identity_created should be false with the same identity ID.'; unpair.disabled = false;
  } catch (error) { report.status = 'failed'; report.error = String(error); status.textContent = 'Failed; see exact result.'; }
  finally { button.disabled = false; unpair.disabled = !store || !identity; show(); }
})(); });
unpair.addEventListener('click', () => { void (async () => {
  if (!store || !identity) return; unpair.disabled = true; button.disabled = true;
  try { await store.unpair(identity); await rejects(() => identity!.sign(new Uint8Array([1]))); store.close(); store = undefined; identity = undefined; report.unpaired_fixture_identity = true; report.unpaired_at = new Date().toISOString(); status.textContent = 'Only this fixture identity was unpaired. Old handle cannot sign; a new Run creates a new identity.'; }
  catch (error) { report.unpair_error = String(error); status.textContent = 'Unpair failed; see result.'; }
  finally { button.disabled = false; unpair.disabled = !store || !identity; show(); }
})(); });
download.addEventListener('click', () => { const url = URL.createObjectURL(new Blob([JSON.stringify(report, null, 2)], { type: 'application/json' })); const link = document.createElement('a'); link.href = url; link.download = 'session-auth-browser-fixture.json'; link.click(); setTimeout(() => URL.revokeObjectURL(url), 1000); });
window.addEventListener('pagehide', () => store?.close());
