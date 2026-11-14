import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, mkdirSync, writeFileSync, appendFileSync, chmodSync, rmSync, utimesSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { once } from 'node:events';
import { createServer } from 'node:http';
import { DatabaseSync } from 'node:sqlite';
import { createApp } from '../server/index.mjs';
import { threadId } from '../server/store.mjs';
import { createSanitizer } from '../server/sanitize.mjs';
import { createCollector, createTransport, readCollectorConfig, CollectorStore, DeliveryQueue, acquireStateLock, captureChanges, batches, splitEvent, normalizeEvent, MAX_BATCH_BYTES } from './index.mjs';
import { eventCapacity } from './batches.mjs';

const token = 'collector-test-shared-ingestion-secret-123456789';
const sanitize = createSanitizer([token]);
function fixture(t, records = []) {
  const root = mkdtempSync(join(tmpdir(), 'threadwatch-collector-'));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  const sessionDir = join(root, 'sessions');
  mkdirSync(sessionDir);
  const transcript = join(sessionDir, 'one.jsonl');
  writeFileSync(transcript, records.map(record => JSON.stringify(record) + '\n').join(''));
  const bridge = { id: 'test-bot', name: 'Test Bot', databasePath: join(root, 'bridge.sqlite'), sessionDir };
  const db = new DatabaseSync(bridge.databasePath);
  db.exec(`CREATE TABLE sessions (thread_id TEXT PRIMARY KEY,guild_id TEXT,parent_channel_id TEXT,omp_session_path TEXT,status TEXT,updated_at_ms INTEGER);
    CREATE TABLE turns (id TEXT,thread_id TEXT,status TEXT,created_at_ms INTEGER,started_at_ms INTEGER,finished_at_ms INTEGER);`);
  db.prepare('INSERT INTO sessions VALUES(?,?,?,?,?,?)').run('123', '456', '789', transcript, 'active', 100);
  db.close();
  const bridgesFile = join(root, 'bridges.json');
  writeFileSync(bridgesFile, JSON.stringify([bridge]));
  return { root, bridge, transcript, bridgesFile, config: { dataDir: join(root, 'collector'), bridges: [bridge], serverUrl: 'http://127.0.0.1:1', token, collectInterval: 100, heartbeatInterval: 100, requestTimeout: 1000 } };
}
function message(text, timestamp) { return { type: 'message', ...(timestamp === undefined ? {} : { timestamp }), message: { role: 'assistant', content: text } }; }
function cachedThread() { return { id: threadId('bridge', 'test-bot', '123'), botId: 'test-bot', botName: 'Test Bot', title: 'Task', titleSource: 'session', guildId: null, channelId: '123', status: 'working', updatedAt: 100, source: 'bridge:test-bot' }; }
function event(id, text = `Message ${id}`) { return { id: String(id), timestamp: 0, role: 'assistant', kind: 'message', text }; }
function cached(t, f) {
  const store = new CollectorStore(f.config.dataDir, sanitize);
  const queue = new DeliveryQueue(f.config.dataDir, f.bridge);
  t.after(() => { store.close(); queue.close(); });
  const thread = cachedThread();
  store.upsert(thread, []);
  return { store, queue, thread, capture: () => captureChanges(store, queue, f.bridge, sanitize) };
}
function queued(queue) { return queue.db.prepare('SELECT payload FROM deliveries ORDER BY seq').all().map(row => JSON.parse(row.payload)); }
async function api(t, root) {
  const app = createApp({ local: true, lan: false, host: '127.0.0.1', port: 8787, clientId: null, clientSecret: '', allowedUsers: new Set(), ingestToken: token, dataDir: join(root, 'server'), bridges: [], distDir: join(root, 'dist') });
  app.server.listen(0, '127.0.0.1');
  await once(app.server, 'listening');
  t.after(async () => { app.server.closeAllConnections(); await new Promise(resolve => app.server.close(resolve)); app.close(); });
  return { app, url: `http://127.0.0.1:${app.server.address().port}` };
}

test('configuration enforces one bridge, protected shared token and bounded heartbeat interval', t => {
  const f = fixture(t);
  const env = { TW_SERVER_URL: 'http://127.0.0.1:8787', TW_BRIDGES_FILE: f.bridgesFile, TW_INGEST_TOKEN: token, TW_COLLECTOR_DATA_DIR: f.config.dataDir };
  assert.equal(readCollectorConfig(env).heartbeatInterval, 10000);
  assert.throws(() => readCollectorConfig({ ...env, TW_HEARTBEAT_INTERVAL_MS: '45000' }), /TW_HEARTBEAT/);
  assert.throws(() => readCollectorConfig({ ...env, TW_SERVER_URL: 'https://example.test' }), /plain HTTP/);
  assert.throws(() => readCollectorConfig({ ...env, TW_SERVER_URL: 'http://example.test/path' }), /plain HTTP/);
  assert.throws(() => readCollectorConfig({ ...env, TW_DATA_DIR: f.config.dataDir }), /differ/);
  const secretFile = join(f.root, 'token');
  writeFileSync(secretFile, `${token}\n`, { mode: 0o600 });
  assert.equal(readCollectorConfig({ ...env, TW_INGEST_TOKEN: undefined, TW_INGEST_TOKEN_FILE: secretFile }).token, token);
  chmodSync(secretFile, 0o644);
  assert.throws(() => readCollectorConfig({ ...env, TW_INGEST_TOKEN: undefined, TW_INGEST_TOKEN_FILE: secretFile }), /owner-only/);
  writeFileSync(f.bridgesFile, '[]');
  assert.throws(() => readCollectorConfig(env), /exactly one/);
});

test('SQLite ownership lock rejects concurrent collectors and releases without stale PID files', t => {
  const f = fixture(t);
  const release = acquireStateLock(f.config.dataDir);
  assert.throws(() => acquireStateLock(f.config.dataDir), /Another collector/);
  release();
  acquireStateLock(f.config.dataDir)();
});

test('events are whitelisted, sanitized and unknown timestamps never become collection time', () => {
  assert.equal(normalizeEvent({ kind: 'thinking', text: 'hidden' }, sanitize), null);
  const normalized = normalizeEvent({ ...event('one', `Visible ${token} <analysis>hidden</analysis>`), timestamp: null, providerPayload: 'hidden' }, sanitize);
  assert.equal(normalized.timestamp, 0);
  assert.equal(normalized.providerPayload, undefined);
  assert.ok(!normalized.text.includes(token));
  assert.ok(!normalized.text.includes('hidden'));
});

test('backfill pagination and oversized Unicode continuations preserve all sanitized text and wire bounds', t => {
  const f = fixture(t);
  const { store, queue, thread, capture } = cached(t, f);
  const text = '😀漢字"\\\n'.repeat(180_000);
  store.append(thread.id, [event('huge', text), ...Array.from({ length: 1005 }, (_, n) => event(`small-${n}`))]);
  capture();
  const deliveries = queued(queue);
  assert.ok(deliveries.length >= 3);
  for (const payload of deliveries) {
    assert.ok(Buffer.byteLength(JSON.stringify(payload)) <= MAX_BATCH_BYTES);
    assert.ok(payload.events.length <= 1000);
  }
  assert.equal(deliveries[0].delivery.reset, true);
  assert.ok(deliveries.slice(1).every(payload => !payload.delivery.reset));
  const all = deliveries.flatMap(payload => payload.events);
  const continuations = all.filter(item => item.id.startsWith('continuation:'));
  assert.ok(continuations.length > 1);
  assert.equal(continuations.map(item => item.text.slice(item.text.indexOf('\n') + 1)).join(''), text);
  assert.ok(continuations.every(item => !/[\uD800-\uDBFF]$|^[\uDC00-\uDFFF]/u.test(item.text.slice(item.text.indexOf('\n') + 1))));
  assert.deepEqual(all.filter(item => !item.id.startsWith('continuation:')).map(item => item.id), Array.from({ length: 1005 }, (_, n) => `small-${n}`));
  const outgoing = deliveries[0].thread;
  assert.deepEqual(splitEvent(event('huge', text), eventCapacity(outgoing, [])), continuations);
  assert.equal(capture(), 0);
});

test('capture and queue checkpoint roll back together on an interrupted multi-batch write', t => {
  const f = fixture(t);
  const { store, queue, thread, capture } = cached(t, f);
  store.append(thread.id, Array.from({ length: 1001 }, (_, n) => event(n)));
  queue.db.exec("CREATE TRIGGER fail_second BEFORE INSERT ON deliveries WHEN (SELECT COUNT(*) FROM deliveries)=1 BEGIN SELECT RAISE(ABORT,'simulated storage failure'); END");
  assert.throws(capture, /simulated storage failure/);
  assert.equal(queue.count(), 0);
  assert.equal(queue.db.prepare('SELECT COUNT(*) AS n FROM captured_threads').get().n, 0);
  queue.db.exec('DROP TRIGGER fail_second');
  assert.equal(capture(), 2);
  assert.equal(queued(queue).flatMap(payload => payload.events).length, 1001);
});

test('metadata-only changes, stable-ID corrections and split-count changes are not lost', t => {
  const f = fixture(t);
  const { store, queue, thread, capture } = cached(t, f);
  store.append(thread.id, [event('one')]);
  capture();
  queue.acknowledge(queue.peek().id);
  store.upsert({ ...thread, title: 'Renamed' }, [{ text: 'Ship', status: 'pending' }]);
  capture();
  let payload = JSON.parse(queue.peek().payload);
  assert.deepEqual(payload.events, []);
  assert.equal(payload.thread.title, 'Renamed');
  assert.equal(payload.todos[0].text, 'Ship');
  assert.equal(payload.delivery.reset, false);
  queue.acknowledge(queue.peek().id);
  store.append(thread.id, [event('one', 'Corrected earlier record')]);
  capture();
  payload = JSON.parse(queue.peek().payload);
  assert.equal(payload.delivery.reset, true);
  assert.equal(payload.events[0].text, 'Corrected earlier record');
  queue.acknowledge(queue.peek().id);
  store.append(thread.id, [event('one', '😀'.repeat(400_000))]);
  capture();
  assert.equal(JSON.parse(queue.peek().payload).delivery.reset, true);
  while (queue.peek()) queue.acknowledge(queue.peek().id);
  store.append(thread.id, [event('one', 'Small again')]);
  capture();
  payload = JSON.parse(queue.peek().payload);
  assert.equal(payload.delivery.reset, true);
  assert.deepEqual(payload.events.map(item => item.id), ['one']);
});

test('actual API handles outage, lost ACK, restart, incremental append and stable-ID source reset', async t => {
  const f = fixture(t, [message('Initial saved event')]);
  const { url } = await api(t, f.root);
  const config = { ...f.config, serverUrl: url };
  let mode = 'outage';
  const fetcher = async (address, options) => {
    if (new URL(address).pathname === '/api/ingest' && mode === 'outage') throw new Error('simulated unavailable network');
    const response = await fetch(address, options);
    if (new URL(address).pathname === '/api/ingest' && mode === 'lost') { await response.text(); throw new Error('simulated lost ACK'); }
    return response;
  };
  let collector = await createCollector(config, { fetch: fetcher });
  t.after(async () => { await collector?.close(); });
  await collector.collect();
  const deliveryId = collector.queue.peek().id;
  assert.equal(await collector.flush(), false);
  assert.equal(collector.queue.peek().id, deliveryId);
  mode = 'lost';
  assert.equal(await collector.flush(), false);
  assert.equal(collector.queue.peek().id, deliveryId);
  const listBefore = await (await fetch(`${url}/api/threads`)).json();
  const remoteId = listBefore.threads[0].id;
  const version = listBefore.threads[0].historyVersion;
  assert.equal(remoteId, threadId('ingest', f.bridge.id, '123'));
  await collector.close();
  collector = null;
  appendFileSync(f.transcript, `${JSON.stringify(message('Appended while disconnected'))}\n`);
  collector = await createCollector(config);
  assert.equal(collector.queue.peek().id, deliveryId);
  await collector.collect();
  assert.equal(collector.queue.count(), 2);
  assert.equal(await collector.flush(), true);
  let detail = await (await fetch(`${url}/api/threads/${remoteId}`)).json();
  assert.deepEqual(detail.events.map(item => item.text), ['Initial saved event', 'Appended while disconnected']);
  assert.ok(detail.events.every(item => item.timestamp === 0));
  assert.equal(detail.thread.historyVersion, version);
  writeFileSync(f.transcript, `${JSON.stringify(message('Replaced'))}\n`);
  utimesSync(f.transcript, new Date(1_000_000), new Date(1_000_000));
  await collector.collect();
  assert.equal(JSON.parse(collector.queue.peek().payload).delivery.reset, true);
  await collector.flush();
  detail = await (await fetch(`${url}/api/threads/${remoteId}`)).json();
  assert.deepEqual(detail.events.map(item => item.text), ['Replaced']);
  assert.notEqual(detail.thread.historyVersion, version);
  await collector.heartbeat();
  const listAfter = await (await fetch(`${url}/api/threads`)).json();
  const source = listAfter.sources.find(item => item.id === `ingest:${f.bridge.id}`);
  assert.equal(source.status, 'ready');
  assert.equal(source.pendingBatches, 0);
});

test('restart captures previously committed cache before a newer source replaces it', async t => {
  const f = fixture(t, [message('New source snapshot')]);
  const store = new CollectorStore(f.config.dataDir, sanitize);
  const thread = cachedThread();
  store.upsert(thread, []);
  store.append(thread.id, [event('uncaptured', 'Committed before interruption')]);
  store.close();
  const collector = await createCollector(f.config);
  try {
    await collector.collect();
    const deliveries = queued(collector.queue);
    assert.equal(deliveries.length, 2);
    assert.equal(deliveries[0].events[0].text, 'Committed before interruption');
    assert.equal(deliveries[1].events[0].text, 'New source snapshot');
    assert.ok(deliveries.every(payload => payload.delivery.reset));
  } finally { await collector.close(); }
});

test('heartbeats continue while a real multi-batch backfill awaits network acknowledgements', { timeout: 10_000 }, async t => {
  const f = fixture(t, Array.from({ length: 1001 }, (_, index) => message(`Saved ${index}`)));
  const { url } = await api(t, f.root);
  let draining = false;
  let pulsesDuringDrain = 0;
  let releaseAck;
  const gate = new Promise(resolve => { releaseAck = resolve; });
  const collector = await createCollector({ ...f.config, serverUrl: url }, {
    fetch: async (address, options) => {
      const path = new URL(address).pathname;
      if (path === '/api/ingest' && !draining) { draining = true; await gate; }
      if (path === '/api/heartbeat' && draining && ++pulsesDuringDrain >= 2) releaseAck();
      return fetch(address, options);
    },
  });
  try {
    assert.equal(await collector.runOnce(), true);
    assert.ok(pulsesDuringDrain >= 2);
    assert.equal(collector.queue.count(), 0);
  } finally { releaseAck(); await collector.close(); }
});

test('empty healthy source reports ready, while once mode exits unsuccessfully with durable undelivered data', async t => {
  const f = fixture(t);
  const db = new DatabaseSync(f.bridge.databasePath);
  db.exec('DELETE FROM sessions');
  db.close();
  const { url } = await api(t, f.root);
  let collector = await createCollector({ ...f.config, serverUrl: url });
  assert.equal(await collector.runOnce(), true);
  await collector.close();
  const list = await (await fetch(`${url}/api/threads`)).json();
  assert.equal(list.threads.length, 0);
  assert.ok(list.sources.some(source => source.id === `ingest:${f.bridge.id}` && source.status === 'ready'));
  const other = fixture(t, [message('Keep queued')]);
  collector = await createCollector(other.config, { fetch: async () => { throw new Error('offline'); } });
  try {
    assert.equal(await collector.runOnce(), false);
    assert.equal(collector.queue.count(), 1);
  } finally { await collector.close(); }
  const queue = new DeliveryQueue(other.config.dataDir, other.bridge);
  assert.equal(queue.count(), 1);
  queue.close();
});

test('transport refuses redirects and times out without forwarding the shared credential', async t => {
  let forwarded = 0;
  const destination = createServer((req, res) => { forwarded++; res.end('{}'); });
  destination.listen(0, '127.0.0.1');
  await once(destination, 'listening');
  const redirect = createServer((req, res) => { res.writeHead(302, { Location: `http://127.0.0.1:${destination.address().port}/api/ingest` }); res.end(); });
  redirect.listen(0, '127.0.0.1');
  await once(redirect, 'listening');
  const hanging = createServer(() => {});
  hanging.listen(0, '127.0.0.1');
  await once(hanging, 'listening');
  t.after(async () => { for (const server of [redirect, destination, hanging]) { server.closeAllConnections(); await new Promise(resolve => server.close(resolve)); } });
  const transport = createTransport({ serverUrl: `http://127.0.0.1:${redirect.address().port}`, token, requestTimeout: 1000 });
  await assert.rejects(transport('/api/ingest', {}));
  assert.equal(forwarded, 0);
  await assert.rejects(transport('http://other.test/api/ingest', {}), /Invalid collector API path/);
  await assert.rejects(createTransport({ serverUrl: `http://127.0.0.1:${hanging.address().port}`, token, requestTimeout: 25 })('/api/heartbeat', {}));
});
