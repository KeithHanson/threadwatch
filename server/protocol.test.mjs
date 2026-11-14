import test from 'node:test';
import assert from 'node:assert/strict';
import { once } from 'node:events';
import { randomUUID } from 'node:crypto';
import { mkdtempSync, rmSync, writeFileSync, chmodSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createApp, readConfig } from './index.mjs';
import { Store, threadId } from './store.mjs';

const token = 'protocol-shared-ingestion-credential-123456789';
function directory(t) {
  const dir = mkdtempSync(join(tmpdir(), 'threadwatch-protocol-'));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  return dir;
}
function config(dir) {
  return { local: true, host: '127.0.0.1', port: 8787, clientId: null, clientSecret: '', allowedUsers: new Set(), ingestToken: token, dataDir: dir, bridges: [], distDir: join(dir, 'dist') };
}
async function running(t, cfg, options = {}) {
  const app = createApp(cfg, options);
  app.server.listen(0, '127.0.0.1');
  await once(app.server, 'listening');
  let closed = false;
  const stop = async () => {
    if (closed) return;
    closed = true;
    app.server.closeAllConnections();
    await new Promise(resolve => app.server.close(resolve));
    app.close();
  };
  t.after(stop);
  return { ...app, stop, url: `http://127.0.0.1:${app.server.address().port}` };
}
function batch(botId = 'bot', id = 'thread', eventIds = ['old']) {
  return {
    thread: { id, botId, botName: 'Collector bot', title: 'Task', status: 'working', updatedAt: 1000 },
    events: eventIds.map(id => ({ id, timestamp: 0, kind: 'message', role: 'assistant', text: id })),
    todos: [{ text: 'Current task', status: 'in_progress' }],
  };
}
function post(value, credential = token) {
  return { method: 'POST', headers: { 'Content-Type': 'application/json', ...(credential ? { Authorization: `Bearer ${credential}` } : {}) }, body: JSON.stringify(value) };
}
async function ingest(app, value) {
  const response = await fetch(`${app.url}/api/ingest`, post(value));
  assert.equal(response.status, 200);
  return response.json();
}
async function heartbeat(app, value = {}) {
  const response = await fetch(`${app.url}/api/heartbeat`, post({ botId: 'bot', botName: 'Collector bot', status: 'ready', pendingBatches: 0, ...value }));
  assert.equal(response.status, 200);
  return response.json();
}
async function list(app) { return (await fetch(`${app.url}/api/threads`)).json(); }
async function detail(app, id, query = '') { return (await fetch(`${app.url}/api/threads/${id}${query}`)).json(); }

test('reset delivery retries after a dropped acknowledgement and restart cannot erase later batches', async t => {
  const cfg = config(directory(t));
  let app = await running(t, cfg);
  const original = await ingest(app, batch());
  const neighbor = await ingest(app, batch('bot', 'neighbor'));
  const replacement = { ...batch('bot', 'thread', ['replacement']), delivery: { id: randomUUID(), reset: true } };
  // Commit the request but discard its acknowledgement, as a disconnected collector would.
  const lostAck = await fetch(`${app.url}/api/ingest`, post(replacement));
  assert.equal(lostAck.status, 200);
  await lostAck.body.cancel();
  const later = { ...batch('bot', 'thread', ['later']), delivery: { id: randomUUID(), reset: false } };
  later.thread.title = 'Later metadata';
  later.thread.status = 'completed';
  later.todos = [{ text: 'Current task', status: 'completed' }];
  await ingest(app, later);
  const revision = app.store.revision();
  assert.equal((await ingest(app, replacement)).revision, revision);
  await app.stop();
  app = await running(t, cfg);
  assert.equal((await ingest(app, replacement)).revision, revision);
  const result = await detail(app, original.id);
  assert.deepEqual(result.events.map(event => event.id), ['replacement', 'later']);
  assert.equal(result.thread.title, 'Later metadata');
  assert.equal(result.thread.historyVersion, replacement.delivery.id);
  assert.equal((await list(app)).threads.find(thread => thread.id === original.id).historyVersion, replacement.delivery.id);
  assert.equal(result.thread.status, 'completed');
  assert.deepEqual(result.todos, later.todos);
  assert.deepEqual((await detail(app, neighbor.id)).events.map(event => event.id), ['old']);
  const page = await detail(app, original.id, '?limit=1');
  assert.deepEqual(page.events.map(event => event.id), ['later']);
  assert.deepEqual((await detail(app, original.id, `?limit=1&before=${page.nextCursor}`)).events.map(event => event.id), ['replacement']);
  await ingest(app, batch('bot', 'thread', ['legacy-followup']));
  assert.equal((await detail(app, original.id)).thread.historyVersion, replacement.delivery.id);
  const nextReset = { ...batch('bot', 'thread', ['next-history']), delivery: { id: randomUUID(), reset: true } };
  await ingest(app, nextReset);
  await ingest(app, replacement);
  const nextHistory = await detail(app, original.id);
  assert.equal(nextHistory.thread.historyVersion, nextReset.delivery.id);
  assert.deepEqual(nextHistory.events.map(event => event.id), ['next-history']);
});

test('delivery IDs are isolated across bots and deduplicated across all threads within one bot', async t => {
  const app = await running(t, config(directory(t)));
  const delivery = { id: randomUUID(), reset: true };
  const first = await ingest(app, { ...batch('one'), delivery });
  const second = await ingest(app, { ...batch('two'), delivery });
  assert.notEqual(first.id, second.id);
  assert.deepEqual((await list(app)).threads.map(thread => thread.botId).sort(), ['one', 'two']);
  const revision = app.store.revision();
  const duplicate = await ingest(app, { ...batch('one', 'different'), delivery });
  assert.equal(duplicate.id, first.id);
  assert.equal(duplicate.revision, revision);
  assert.equal(app.store.list().length, 2);
  await ingest(app, batch('one', 'thread', ['legacy']));
  assert.deepEqual((await detail(app, first.id)).events.map(event => event.id), ['old', 'legacy']);
});

test('delivery receipt, reset, metadata, and events commit or roll back together', t => {
  const store = new Store(directory(t));
  t.after(() => store.close());
  const thread = { ...batch().thread, id: threadId('ingest', 'bot', 'thread'), source: 'ingest:bot' };
  store.ingest(thread, batch().events, batch().todos);
  const revision = store.revision();
  const delivery = { id: randomUUID(), reset: true };
  store.db.exec("CREATE TRIGGER reject_event BEFORE INSERT ON events WHEN NEW.event_id='replacement' BEGIN SELECT RAISE(ABORT,'injected persistence failure'); END;");
  const updated = { ...thread, title: 'Replacement' };
  const events = batch('bot', 'thread', ['replacement']).events;
  assert.throws(() => store.ingest(updated, events, [], delivery), /injected persistence failure/);
  assert.equal(store.revision(), revision);
  assert.deepEqual(store.detail(thread.id, null, 100).events.map(event => event.id), ['old']);
  assert.equal(store.detail(thread.id, null, 100).thread.title, 'Task');
  assert.deepEqual(store.todos(thread.id), batch().todos);
  assert.equal(store.db.prepare('SELECT COUNT(*) AS count FROM deliveries').get().count, 0);
  store.db.exec('DROP TRIGGER reject_event');
  store.ingest(updated, events, [], delivery);
  assert.deepEqual(store.detail(thread.id, null, 100).events.map(event => event.id), ['replacement']);
  assert.deepEqual(store.todos(thread.id), []);
});

test('heartbeat freshness changes list and detail revisions without mutating saved statuses', async t => {
  let now = 0;
  const app = await running(t, config(directory(t)), { now: () => now });
  const ids = {};
  for (const status of ['working', 'queued', 'waiting', 'completed', 'failed', 'unknown', 'stopped']) {
    const payload = batch('bot', status);
    payload.thread.status = status;
    ids[status] = (await ingest(app, payload)).id;
  }
  await ingest(app, batch('legacy'));
  await heartbeat(app);
  await heartbeat(app, { botId: 'empty', botName: 'Zero threads', status: 'unavailable', pendingBatches: 2 });
  const initial = await list(app);
  const initialDetail = await detail(app, ids.working);
  assert.deepEqual(initial.sources.find(source => source.id === 'ingest:bot'), { id: 'ingest:bot', name: 'Collector bot', status: 'ready', lastSeenAt: 0, pendingBatches: 0 });
  assert.equal(initial.sources.find(source => source.id === 'ingest:empty').status, 'unavailable');
  const persistedRevision = app.store.revision();
  now = 44_999;
  assert.equal((await list(app)).revision, initial.revision);
  now = 45_000;
  const offline = await list(app);
  const offlineDetail = await detail(app, ids.working);
  assert.notEqual(offline.revision, initial.revision);
  assert.notEqual(offlineDetail.revision, initialDetail.revision);
  assert.equal(app.store.revision(), persistedRevision);
  assert.equal(offline.sources.find(source => source.id === 'ingest:bot').status, 'offline');
  assert.equal(offline.sources.find(source => source.id === 'ingest:empty').status, 'offline');
  assert.equal(offline.sources.find(source => source.id === 'ingest:legacy').status, 'saved');
  assert.equal(offline.threads.find(thread => thread.botId === 'legacy').status, 'working');
  for (const [status, id] of Object.entries(ids)) {
    const expected = ['working', 'queued', 'waiting'].includes(status) ? 'unknown' : status;
    const thread = offline.threads.find(thread => thread.id === id);
    assert.equal(thread.status, expected);
    assert.equal(thread.reportedStatus, ['working', 'queued', 'waiting'].includes(status) ? status : undefined);
    assert.equal((await detail(app, id)).thread.status, expected);
    assert.equal(app.store.detail(id, null, 100).thread.status, status);
  }
  await heartbeat(app, { status: 'degraded', pendingBatches: 3 });
  const recovered = await detail(app, ids.working);
  assert.equal(recovered.thread.status, 'working');
  assert.equal(recovered.thread.reportedStatus, undefined);
  assert.notEqual(recovered.revision, offlineDetail.revision);
  assert.equal((await list(app)).sources.find(source => source.id === 'ingest:bot').pendingBatches, 3);
  now += 1;
  await heartbeat(app, { status: 'degraded', pendingBatches: 3 });
  assert.notEqual((await detail(app, ids.working)).revision, recovered.revision);
});

test('heartbeats persist across restart, include zero-thread collectors, and discard private error details', async t => {
  let now = 1000;
  const cfg = config(directory(t));
  let app = await running(t, cfg, { now: () => now });
  await heartbeat(app, { botName: `Bot ${token}`, status: 'degraded', pendingBatches: 7, error: `Cannot open /private/machine/bridge.sqlite using ${token}` });
  const saved = await list(app);
  const persisted = app.store.db.prepare('SELECT payload FROM heartbeats').get().payload;
  assert.ok(!persisted.includes(token));
  assert.ok(!persisted.includes('/private/machine'));
  assert.equal(saved.threads.length, 0);
  await app.stop();
  app = await running(t, cfg, { now: () => now });
  assert.deepEqual(await list(app), saved);
  now = 46_000;
  const expired = await list(app);
  assert.equal(expired.sources[0].status, 'offline');
  assert.equal(expired.sources[0].lastSeenAt, 1000);
  assert.equal(expired.sources[0].pendingBatches, 7);
  assert.notEqual(expired.revision, saved.revision);
});

test('heartbeat authentication and validation use the shared ingest credential', async t => {
  const app = await running(t, config(directory(t)));
  const valid = { botId: 'bot', botName: 'Bot', status: 'ready', pendingBatches: 0 };
  for (const credential of ['', 'wrong-token']) assert.equal((await fetch(`${app.url}/api/heartbeat`, post(valid, credential))).status, 401);
  for (const invalid of [
    { botId: token }, { botId: '' }, { botName: null }, { status: 'working' },
    { pendingBatches: -1 }, { pendingBatches: 0.5 }, { pendingBatches: '0' },
    { pendingBatches: Number.MAX_SAFE_INTEGER + 1 }, { error: { path: '/private/path' } },
  ]) assert.equal((await fetch(`${app.url}/api/heartbeat`, post({ ...valid, ...invalid }))).status, 400);
  assert.deepEqual((await list(app)).sources, []);
  await heartbeat(app);
});

test('ingest accepts whitelisted adapter event kinds and title provenance and rejects malformed deliveries', async t => {
  const app = await running(t, config(directory(t)));
  const payload = batch();
  payload.thread.titleSource = 'user_message';
  payload.thread.historyVersion = 'untrusted-client-version';
  payload.events = [
    { id: 'start', timestamp: 0, kind: 'tool_start', name: 'bash', text: `Starting ${token}`, arguments: { private: true } },
    { id: 'error', timestamp: 0, kind: 'import_error', text: 'Malformed transcript record', raw: token },
  ];
  const result = await ingest(app, payload);
  const stored = await detail(app, result.id);
  assert.equal(stored.thread.titleSource, 'user_message');
  assert.equal(stored.thread.historyVersion, undefined);
  assert.deepEqual(stored.events.map(event => event.kind), ['tool_start', 'import_error']);
  assert.ok(stored.events.every(event => event.timestamp === 0));
  assert.equal(stored.events[0].arguments, undefined);
  assert.equal(stored.events[1].raw, undefined);
  assert.ok(!JSON.stringify(stored).includes(token));
  for (const delivery of [null, [], {}, { id: '', reset: false }, { id: token, reset: false }, { id: 'valid', reset: 'true' }]) {
    assert.equal((await fetch(`${app.url}/api/ingest`, post({ ...payload, delivery }))).status, 400);
  }
  payload.thread.titleSource = '/private/machine/path';
  assert.equal((await fetch(`${app.url}/api/ingest`, post(payload))).status, 400);
});

test('server token files are trimmed, private, mutually exclusive, and sanitized during ingestion', async t => {
  const dir = directory(t);
  const file = join(dir, 'private-token');
  const env = { TW_LOCAL: '1', TW_DATA_DIR: join(dir, 'data'), TW_INGEST_TOKEN_FILE: file };
  writeFileSync(file, `  ${token}\n`, { mode: 0o600 });
  const cfg = readConfig(env);
  assert.equal(cfg.ingestToken, token);
  assert.equal(cfg.ingestTokenFile, undefined);
  assert.throws(() => readConfig({ ...env, TW_INGEST_TOKEN: token }), /mutually exclusive/);
  assert.throws(() => readConfig({ ...env, TW_INGEST_TOKEN: '' }), /mutually exclusive/);
  const app = await running(t, cfg);
  const payload = batch();
  payload.thread.title = token;
  const result = await ingest(app, payload);
  assert.ok(!JSON.stringify(await detail(app, result.id)).includes(token));
  for (const value of ['', '  \n ', 'too-short']) {
    writeFileSync(file, value);
    assert.throws(() => readConfig(env), /32 characters/);
  }
  assert.throws(() => readConfig({ TW_LOCAL: '1', TW_INGEST_TOKEN: '' }), /32 characters/);
  writeFileSync(file, token);
  if (process.platform !== 'win32') {
    for (const mode of [0o640, 0o604, 0o644, 0o601]) {
      chmodSync(file, mode);
      assert.throws(() => readConfig(env), /private regular file/);
    }
    chmodSync(file, 0o600);
  }
  assert.throws(() => readConfig({ ...env, TW_INGEST_TOKEN_FILE: dir }), /private regular file/);
  const missing = join(dir, 'private-machine-name', 'missing-token');
  assert.throws(() => readConfig({ ...env, TW_INGEST_TOKEN_FILE: missing }), error => !error.message.includes(missing) && /TW_INGEST_TOKEN_FILE/.test(error.message));
});
