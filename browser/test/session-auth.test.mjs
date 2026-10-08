import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { spawnSync } from 'node:child_process';
import { DeviceIdentity, identityId, hex, sha256, pairingBytes, grantBytes, offerBytes, answerBytes, confirmationBytes, sdpBinding, verifySignature, verifyGrant, verifyOffer, verifyAnswer, verifyConfirmation, signOffer, confirmAnswer, closeBytes, signClose, verifyClose } from '../dist/session-auth.js';
const pairBinary = process.env.VEYNODE_PAIR_BIN ?? new URL('../../target/debug/vm-pair', import.meta.url).pathname;
const fp = value => Array(32).fill(value).join(':');
const offerSdp = `v=0\r\ns=synthetic-only\r\na=fingerprint:sha-256 ${fp('AA')}\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\n`;
const answerSdp = `v=0\r\ns=synthetic-answer\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\na=fingerprint:sha-256 ${fp('BB')}\r\n`;
async function fixture() {
  const host = await DeviceIdentity.generate(), device = await DeviceIdentity.generate();
  const grant = { host_id: host.id, device_id: device.id, device_public_key: device.publicKey, vm_id: 'fixture-vm', scopes: ['observe'], version: 1 };
  const signedGrant = { grant, signature: await host.sign(await grantBytes(grant)) };
  const challenge = { host_id: host.id, device_id: device.id, vm_id: 'fixture-vm', scopes: ['observe'], grant_version: 1, instance_nonce: '01'.repeat(32), nonce: '02'.repeat(32), issued_at: 1000, expires_at: 1060 };
  const options = { vm_id: 'fixture-vm', scopes: ['observe'], request_id: 'request-one', codec: 'h264', now: 1001 };
  const signedOffer = await signOffer(device, signedGrant, host.publicKey, challenge, offerSdp, options);
  const transcript = { intent: signedOffer.intent, session_id: 'session-one', answer: await sdpBinding(answerSdp) };
  const signedAnswer = { transcript, signature: await host.sign(answerBytes(transcript)) };
  return { host, device, grant, signedGrant, challenge, options, signedOffer, transcript, signedAnswer };
}
test('non-extractable WebCrypto identity and typed offer/answer/confirmation roundtrip (Node, not IndexedDB)', async () => {
  const f = await fixture();
  assert.equal(f.device.privateKeyExtractable, false); await f.device.assertPrivateExportRejected();
  assert.equal(f.device.publicKey.length, 130); assert.equal(await identityId(f.device.publicKey), f.device.id);
  assert.equal(f.signedOffer.signature.length, 128);
  const grant = await verifyGrant(f.signedGrant, f.host.publicKey, { device_public_key: f.device.publicKey, vm_id: 'fixture-vm', scopes: ['observe'], version: 1 });
  await verifyOffer(f.signedOffer, grant, f.challenge, offerSdp, 1001);
  const { confirmation } = await confirmAnswer(f.device, f.signedAnswer, f.host.publicKey, f.signedOffer.intent, answerSdp, 1001);
  await verifyConfirmation(confirmation, f.transcript, f.device.publicKey, 1001);
  f.device.revokeHandle(); await assert.rejects(f.device.sign(confirmationBytes(confirmation.transcript_sha256)), /unpaired/);
});
test('host/device/nonce/instance/VM/scopes/version/expiry/request/codec/SDP/fingerprint substitutions fail', async () => {
  const f = await fixture();
  for (const [field, value] of Object.entries({ host_id: '03'.repeat(32), device_id: '04'.repeat(32), vm_id: 'other-vm', scopes: ['input', 'observe'], grant_version: 2, instance_nonce: '05'.repeat(32), nonce: '06'.repeat(32), issued_at: 999, expires_at: 1059 })) {
    const changed = structuredClone(f.signedAnswer); changed.transcript.intent.challenge[field] = value;
    await assert.rejects(verifyAnswer(changed, f.host.publicKey, f.signedOffer.intent, answerSdp, 1001), undefined, field);
  }
  for (const [field,value] of [['request_id','other-request'],['codec','h265']]) {
    const changed = structuredClone(f.signedAnswer); changed.transcript.intent[field]=value;
    await assert.rejects(verifyAnswer(changed,f.host.publicKey,f.signedOffer.intent,answerSdp,1001));
  }
  for (const field of ['sha256','fingerprint_sha256']) {
    const changed = structuredClone(f.signedAnswer); changed.transcript.answer[field]='07'.repeat(32);
    await assert.rejects(verifyAnswer(changed,f.host.publicKey,f.signedOffer.intent,answerSdp,1001));
  }
  await assert.rejects(verifyAnswer(f.signedAnswer,f.device.publicKey,f.signedOffer.intent,answerSdp,1001));
  await assert.rejects(verifyAnswer(f.signedAnswer,f.host.publicKey,f.signedOffer.intent,answerSdp+'a=x:changed\r\n',1001));
  await assert.rejects(verifyAnswer(f.signedAnswer,f.host.publicKey,f.signedOffer.intent,answerSdp,1060),/expired/);
  await assert.rejects(verifyAnswer(f.signedAnswer,f.host.publicKey,f.signedOffer.intent,answerSdp,999),/valid/);
  await assert.rejects(signOffer(f.device,f.signedGrant,f.host.publicKey,f.challenge,offerSdp,{...f.options,vm_id:'other-vm'}));
  await assert.rejects(signOffer(f.device,f.signedGrant,f.host.publicKey,f.challenge,offerSdp,{...f.options,scopes:['input']}));
  const wrongGrant=structuredClone(f.signedGrant);wrongGrant.grant.scopes=['input','observe'];
  await assert.rejects(verifyGrant(wrongGrant,f.host.publicKey,{device_public_key:f.device.publicKey,vm_id:'fixture-vm',scopes:['input','observe']}));
});
test('strict schema, safe integers, sorted scopes and fixed signature/public key encodings', async () => {
  const f=await fixture();
  for(const mutate of [i=>i.extra=true,i=>i.challenge.extra=true,i=>i.offer.extra=true,i=>i.challenge.issued_at=Number.MAX_SAFE_INTEGER+1,i=>i.challenge.scopes=['observe','input'],i=>i.challenge.scopes=['observe','observe'],i=>i.challenge.nonce=i.challenge.nonce.toUpperCase().replace('02','AB'),i=>i.request_id='bad/id']) {
    const intent=structuredClone(f.signedOffer.intent);mutate(intent);assert.throws(()=>offerBytes(intent));
  }
  const bytes=offerBytes(f.signedOffer.intent);
  await assert.rejects(verifySignature(f.device.publicKey,bytes,f.signedOffer.signature+'00'));
  await assert.rejects(verifySignature(f.device.publicKey,bytes,f.signedOffer.signature.toUpperCase()));
  await assert.rejects(identityId('04'+'00'.repeat(64)));
  await assert.rejects(identityId('02'+'01'.repeat(32)));
  const confirmation={transcript_sha256:await sha256(answerBytes(f.transcript)),signature:await f.device.sign(bytes)};
  await assert.rejects(verifyConfirmation(confirmation,f.transcript,f.device.publicKey,1001));
});
test('SDP digest covers exact bytes and every media section has one consistent SHA256 identity', async () => {
  assert.notEqual((await sdpBinding(offerSdp)).sha256,(await sdpBinding(offerSdp.replaceAll('\r\n','\n'))).sha256);
  assert.equal((await sdpBinding(offerSdp)).fingerprint_sha256,'aa'.repeat(32));
  const media = `m=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=fingerprint:sha-256 ${fp('AA')}\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\na=fingerprint:sha-256 ${fp('AA')}\r\n`;
  await sdpBinding(media);
  for(const bad of ['', 'v=0\n', offerSdp.replace('sha-256','sha-1'),offerSdp.replace(fp('AA'),fp('A')),media.replaceAll(fp('AA'),fp('BB')).replace(fp('BB'),fp('AA')),media.replace(`a=fingerprint:sha-256 ${fp('AA')}\r\n`,''),offerSdp+'\0',offerSdp+'\ud800',offerSdp.replace('\r\n','\r\r\n'),offerSdp+'x'.repeat(128*1024)]) await assert.rejects(sdpBinding(bad));
});
test('WebCrypto-generated grant/offer/answer/confirmation/close verifies in real Rust crypto CLI', async () => {
  const f=await fixture(); const {confirmation}=await confirmAnswer(f.device,f.signedAnswer,f.host.publicKey,f.signedOffer.intent,answerSdp,1001);
  const cases=[{kind:'grant',host_public_key:f.host.publicKey,signed:f.signedGrant},{kind:'offer',device_public_key:f.device.publicKey,signed:f.signedOffer,exact_sdp:offerSdp},{kind:'answer',host_public_key:f.host.publicKey,expected_intent:f.signedOffer.intent,exact_sdp:answerSdp,signed:f.signedAnswer},{kind:'confirm',device_public_key:f.device.publicKey,transcript:f.transcript,signed:confirmation},{kind:'close',device_public_key:f.device.publicKey,transcript:f.transcript,signed:await signClose(f.device,f.transcript)}];
  for(const value of cases){const result=spawnSync(pairBinary,['verify'],{input:JSON.stringify(value),encoding:'utf8',timeout:5000});assert.equal(result.status,0,result.error?.message??result.stderr);assert.equal(JSON.parse(result.stdout).verified,true);}
  const altered=structuredClone(cases[1]);altered.signed.intent.challenge.vm_id='altered';
  assert.notEqual(spawnSync(pairBinary,['verify'],{input:JSON.stringify(altered),encoding:'utf8',timeout:5000}).status,0);
});

test('Rust golden canonical bytes and P256 signatures verify in WebCrypto', async () => {
  const g = JSON.parse(await readFile(new URL('../../crates/session-auth/tests/golden.json',import.meta.url),'utf8'));
  assert.equal(await identityId(g.host_public_key),g.host_id); assert.equal(await identityId(g.device_public_key),g.device_id);
  for(const [name,bytes] of Object.entries({pairing:await pairingBytes(g.pairing),grant:await grantBytes(g.signed_grant.grant),offer:offerBytes(g.signed_offer.intent),answer:answerBytes(g.signed_answer.transcript),confirm:confirmationBytes(g.confirmation.transcript_sha256),close:closeBytes(g.signed_close.session_id,g.signed_close.transcript_sha256)})) assert.equal(hex(bytes),g.canonical_hex[name],name);
  assert.equal(await sha256(await pairingBytes(g.pairing)),g.pairing_digest);
  await verifyGrant(g.signed_grant,g.host_public_key,{device_public_key:g.device_public_key,vm_id:g.pairing.vm_id,scopes:g.pairing.scopes,version:g.signed_grant.grant.version});
  await verifyOffer(g.signed_offer,g.signed_grant.grant,g.signed_offer.intent.challenge,g.offer_sdp,g.now);
  await verifyAnswer(g.signed_answer,g.host_public_key,g.signed_offer.intent,g.answer_sdp,g.now);
  await verifyConfirmation(g.confirmation,g.signed_answer.transcript,g.device_public_key,g.now);
  await verifyClose(g.signed_close,g.signed_answer.transcript,g.device_public_key);
});
test('signed close binds this device, session and transcript independently of expired setup challenge', async () => {
  const f=await fixture(),close=await signClose(f.device,f.transcript);
  await verifyClose(close,f.transcript,f.device.publicKey);
  await assert.rejects(verifyClose({...close,session_id:'other'},f.transcript,f.device.publicKey));
  await assert.rejects(verifyClose({...close,transcript_sha256:'08'.repeat(32)},f.transcript,f.device.publicKey));
  await assert.rejects(verifyClose(close,f.transcript,f.host.publicKey));
});
