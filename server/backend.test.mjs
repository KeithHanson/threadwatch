import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync, mkdirSync, writeFileSync, appendFileSync, readFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { DatabaseSync } from 'node:sqlite';
import { once } from 'node:events';
import { get } from 'node:http';
import { DiscordTitles } from './discord-titles.mjs';
import { createApp, readConfig } from './index.mjs';
import { Store, threadId } from './store.mjs';
import { BridgeReader } from './bridge.mjs';
import { createSanitizer, deriveTodos, operationalRecord } from './sanitize.mjs';

const ingestToken = 'test-ingest-secret-value-long-enough-12345';
const secret = 'test-discord-client-secret-value';
function temp(t) { const dir = mkdtempSync(join(tmpdir(), 'threadwatch-')); t.after(() => rmSync(dir, { recursive: true, force: true })); return dir; }
function config(dir, local = false) { return { local, host: '127.0.0.1', port: 8787, clientId: '123456789', clientSecret: secret, allowedUsers: new Set(['42']), ingestToken, dataDir: dir, bridges: [], distDir: join(dir, 'dist') }; }
async function running(t, cfg, options) {
  const app = createApp(cfg, options);
  app.server.listen(0, '127.0.0.1');
  await once(app.server, 'listening');
  t.after(async () => { app.server.closeAllConnections(); await new Promise(resolve => app.server.close(resolve)); app.close(); });
  return { ...app, url: `http://127.0.0.1:${app.server.address().port}` };
}
function ingestion(bot = 'bot', id = 'thread') {
  return { thread: { id, botId: bot, botName: 'Test Bot', title: 'A task', guildId: null, channelId: null, status: 'working', updatedAt: 1000 }, todos: [{ text: 'Ship it', status: 'in_progress' }], events: Array.from({ length: 5 }, (_, i) => ({ id: `e${i}`, timestamp: 1000 + i, kind: 'message', role: 'assistant', text: `Message ${i}` })) };
}
function jsonPost(value, token) { return { method: 'POST', headers: { 'Content-Type': 'application/json', ...(token ? { Authorization: `Bearer ${token}` } : {}) }, body: JSON.stringify(value) }; }

test('configuration fails closed and local bind cannot expose transcripts', () => {
  assert.throws(() => readConfig({}), /Nonlocal/);
  assert.throws(() => readConfig({ TW_LOCAL: '1', TW_HOST: '0.0.0.0' }), /loopback/);
  assert.throws(() => readConfig({ TW_LOCAL: '1', TW_INGEST_TOKEN: 'weak' }), /32/);
  assert.equal(readConfig({ TW_LOCAL: '1' }).port, 8787);
});

test('unauthorized reads and ingestion are rejected independently', async t => {
  const app = await running(t, config(temp(t)));
  assert.equal((await fetch(`${app.url}/api/config`)).status, 200);
  assert.equal((await fetch(`${app.url}/api/threads`)).status, 401);
  assert.equal((await fetch(`${app.url}/api/threads/anything`)).status, 401);
  assert.equal((await fetch(`${app.url}/api/threads`, { headers: { Authorization: `Bearer ${ingestToken}` } })).status, 401);
  assert.equal((await fetch(`${app.url}/api/ingest`, jsonPost(ingestion()))).status, 401);
  assert.equal((await fetch(`${app.url}/api/ingest`, jsonPost(ingestion(), ingestToken))).status, 200);
});

test('local reads reject foreign Origins and DNS rebinding hosts; ingest stays authenticated', async t => {
  const app = await running(t, config(temp(t), true));
  assert.equal((await fetch(`${app.url}/api/threads`)).status, 200);
  assert.equal((await fetch(`${app.url}/api/threads`, { headers: { Origin: 'https://attacker.example' } })).status, 403);
  const hostileStatus = await new Promise((resolve, reject) => {
    const request = get(`${app.url}/api/threads`, { headers: { Host: 'attacker.example' } }, response => { response.resume(); resolve(response.statusCode); });
    request.on('error', reject);
  });
  assert.equal(hostileStatus, 403);
  assert.equal((await fetch(`${app.url}/api/threads`, { headers: { 'Sec-Fetch-Site': 'cross-site' } })).status, 403);
  assert.equal((await fetch(`${app.url}/api/ingest`, jsonPost(ingestion()))).status, 401);
});

test('Discord identity allowlist grants session, not client channel claims or ingest privileges', async t => {
  let user = '99';
  const app = await running(t, config(temp(t)), { fetch: async url => ({ ok: true, json: async () => url.endsWith('/oauth2/token') ? { access_token: 'discord-access', scope: 'identify', expires_in: 3600 } : { id: user } }) });
  assert.equal((await fetch(`${app.url}/api/auth`, jsonPost({ code: 'code', userId: '42' }))).status, 403);
  user = '42';
  const result = await fetch(`${app.url}/api/auth`, jsonPost({ code: 'code' }));
  assert.equal(result.status, 200);
  const auth = await result.json();
  assert.equal(auth.access_token, 'discord-access');
  assert.equal((await fetch(`${app.url}/api/threads`, { headers: { Authorization: `Bearer ${auth.sessionToken}` } })).status, 200);
  assert.equal((await fetch(`${app.url}/api/ingest`, jsonPost(ingestion(), auth.sessionToken))).status, 401);
});

test('redaction omits thinking, injected system blocks, nested secret keys, and sensitive file calls', () => {
  const sanitize = createSanitizer([secret]);
  const text = sanitize.text(`hello <system-reminder>private instructions</system-reminder> token=abc123 Bearer xyz ${secret} https://u:p@example.com`);
  for (const hidden of ['private instructions', 'abc123', 'Bearer xyz', secret, 'u:p']) assert.ok(!text.includes(hidden));
  const value = sanitize.value({ apiKey: 'hidden', providerPayload: { data: 'hidden' }, nested: { thinking: 'hidden', text: 'visible' } });
  assert.ok(!JSON.stringify(value).includes('hidden'));
  const events = operationalRecord({ type: 'message', message: { role: 'assistant', content: [{ type: 'thinking', thinking: 'private thought' }, { type: 'text', text: 'visible' }, { type: 'toolCall', id: 'x', name: 'read', arguments: { path: '/tmp/AGENTS.md' } }] } }, sanitize, 1);
  assert.equal(events.length, 2);
  assert.match(events[1].text, /WITHHELD/);
  assert.deepEqual(operationalRecord({ type: 'message', message: { role: 'system', content: 'hidden' } }, sanitize, 2), []);
});

test('ingest validates body, sanitizes before persistence, and isolates same thread ids across bots', async t => {
  const dir = temp(t);
  const app = await running(t, config(dir, true));
  const first = ingestion('one');
  first.events[0].text = `secret=${secret}`;
  const a = await (await fetch(`${app.url}/api/ingest`, jsonPost(first, ingestToken))).json();
  const b = await (await fetch(`${app.url}/api/ingest`, jsonPost(ingestion('two'), ingestToken))).json();
  assert.notEqual(a.id, b.id);
  const newest = await (await fetch(`${app.url}/api/threads/${a.id}?limit=2`)).json();
  assert.deepEqual(newest.events.map(item => item.id), ['e3', 'e4']);
  const older = await (await fetch(`${app.url}/api/threads/${a.id}?limit=2&before=${newest.nextCursor}`)).json();
  assert.deepEqual(older.events.map(item => item.id), ['e1', 'e2']);
  assert.equal((await fetch(`${app.url}/api/threads/${b.id}?before=${newest.nextCursor}`)).status, 400);
  const oldest = await (await fetch(`${app.url}/api/threads/${a.id}?limit=2&before=${older.nextCursor}`)).json();
  assert.equal(oldest.nextCursor, null);
  assert.ok(!oldest.events[0].text.includes(secret));
  const payloads = app.store.db.prepare('SELECT payload FROM events').all().map(row => row.payload).join('');
  assert.ok(!payloads.includes(secret));
  const bad = ingestion(); bad.events[0].role = 'system';
  assert.equal((await fetch(`${app.url}/api/ingest`, jsonPost(bad, ingestToken))).status, 400);
  assert.equal((await fetch(`${app.url}/api/ingest`, jsonPost({ text: 'x'.repeat(1024 * 1024) }, ingestToken))).status, 413);
});

test('successful todo result derives actual completed and current tasks', () => {
  const result = '  Build [in_progress] (Work)\n  Verify [pending] (Work)\n  Work:\n    - [x] Plan\n    - [ ] Build (current task)\n    - [ ] Verify';
  assert.deepEqual(deriveTodos({ op: 'done' }, result), [{ text: 'Plan', status: 'completed' }, { text: 'Build', status: 'in_progress' }, { text: 'Verify', status: 'pending' }]);
  assert.deepEqual(deriveTodos({ op: 'drop' }, result), []);
});

function bridgeFixture(dir, id, lines) {
  const root = join(dir, id); mkdirSync(root);
  const file = join(root, 'session.jsonl'); writeFileSync(file, lines.map(line => JSON.stringify(line)).join('\n') + '\n');
  const dbPath = join(root, 'bridge.sqlite'); const db = new DatabaseSync(dbPath);
  db.exec('CREATE TABLE sessions(thread_id TEXT,guild_id TEXT,parent_channel_id TEXT,omp_session_path TEXT,status TEXT,updated_at_ms INTEGER); CREATE TABLE turns(id TEXT,thread_id TEXT,status TEXT,created_at_ms INTEGER,started_at_ms INTEGER,finished_at_ms INTEGER);');
  db.prepare('INSERT INTO sessions VALUES(?,?,?,?,?,?)').run('same-thread', 'guild', 'channel', file, 'active', 1000);
  db.prepare('INSERT INTO turns VALUES(?,?,?,?,?,?)').run('turn', 'same-thread', 'running', 1000, 1000, null);
  db.close();
  return { bridge: { id, name: id, databasePath: dbPath, sessionDir: root }, file };
}
const message = text => ({ type: 'message', timestamp: 1000, message: { role: 'assistant', content: [{ type: 'text', text }] } });

test('read-only bridge imports incrementally, handles partial/malformed lines, and isolates sources', t => {
  const dir = temp(t);
  const one = bridgeFixture(dir, 'one', [{ type: 'title', title: 'First' }, { ...message('one'), timestamp: new Date(2000).toISOString() }]);
  const two = bridgeFixture(dir, 'two', [message('two')]);
  const store = new Store(join(dir, 'data')); t.after(() => store.close());
  const reader = new BridgeReader(store, [one.bridge, two.bridge], createSanitizer());
  reader.refresh();
  const a = threadId('bridge', 'one', 'same-thread'); const b = threadId('bridge', 'two', 'same-thread');
  assert.equal(store.detail(a, null, 100).events[0].text, 'one');
  assert.equal(store.detail(a, null, 100).thread.channelId, 'same-thread');
  assert.equal(store.detail(a, null, 100).thread.updatedAt, 2000);
  assert.equal(store.detail(b, null, 100).events[0].text, 'two');
  const revision = store.revision(); reader.refresh(); assert.equal(store.revision(), revision);
  assert.equal(store.detail(a, null, 100).thread.updatedAt, 2000);
  const line = JSON.stringify({ ...message('appended'), timestamp: 3000 });
  appendFileSync(one.file, line.slice(0, 12)); reader.refresh();
  assert.equal(store.detail(a, null, 100).events.length, 1);
  appendFileSync(one.file, line.slice(12) + '\nnot-json\n'); reader.refresh();
  assert.equal(store.detail(a, null, 100).events.length, 3);
  assert.equal(reader.sources[0].status, 'degraded');
  assert.equal(store.detail(a, null, 100).events[1].text, 'appended');
  assert.equal(store.detail(a, null, 100).thread.updatedAt, 3000);
  reader.refresh();
  assert.equal(store.detail(a, null, 100).thread.updatedAt, 3000);
  rmSync(two.bridge.databasePath); reader.refresh(); assert.equal(reader.sources[1].status, 'unavailable');
  assert.equal(store.detail(b, null, 100).events[0].text, 'two');
});

test('bridge todos require matching successful results; credential file results are withheld', t => {
  const dir = temp(t);
  const call = (id, name, args) => ({ type: 'message', message: { role: 'assistant', content: [{ type: 'toolCall', id, name, arguments: args }] } });
  const result = (id, name, text, isError = false) => ({ type: 'message', message: { role: 'toolResult', toolCallId: id, toolName: name, isError, content: [{ type: 'text', text }] } });
  const fixture = bridgeFixture(dir, 'bot', [call('t', 'todo', { op: 'init' }), result('t', 'todo', '  Real [in_progress] (Work)\n    - [ ] Real (current task)'), call('bad', 'todo', { op: 'drop' }), result('bad', 'todo', 'failed', true), call('private', 'read', { path: '/tmp/.env' }), result('private', 'read', 'UNKNOWN_FORMAT_SECRET')]);
  const store = new Store(join(dir, 'data')); t.after(() => store.close());
  new BridgeReader(store, [fixture.bridge], createSanitizer()).refresh();
  const detail = store.detail(threadId('bridge', 'bot', 'same-thread'), null, 100);
  assert.deepEqual(detail.todos, [{ text: 'Real', status: 'in_progress' }]);
  assert.ok(!JSON.stringify(detail).includes('UNKNOWN_FORMAT_SECRET'));
  assert.match(detail.events.at(-1).text, /WITHHELD/);
  assert.ok(readFileSync(fixture.file, 'utf8').includes('UNKNOWN_FORMAT_SECRET'));
});

test('bridge refuses transcript paths outside its configured root', t => {
  const dir = temp(t);
  const fixture = bridgeFixture(dir, 'bot', []);
  const outside = join(dir, 'outside.jsonl'); writeFileSync(outside, JSON.stringify(message('private-outside')) + '\n');
  const db = new DatabaseSync(fixture.bridge.databasePath); db.prepare('UPDATE sessions SET omp_session_path=?').run(outside); db.close();
  const store = new Store(join(dir, 'data')); t.after(() => store.close());
  const reader = new BridgeReader(store, [fixture.bridge], createSanitizer()); reader.refresh();
  assert.equal(reader.sources[0].status, 'degraded');
  assert.equal(store.detail(threadId('bridge', 'bot', 'same-thread'), null, 100).events.length, 0);
});

test('redaction preserves ordinary bot prose while withholding Bot and Bearer credentials', () => {
  const sanitize = createSanitizer();
  const prose = 'Inspect bot integration and establish repository';
  assert.equal(sanitize.text(prose), prose);
  assert.equal(sanitize.text(`Bot ${'a'.repeat(60)}`), '[REDACTED AUTH]');
  assert.equal(sanitize.text('Bearer short-token'), '[REDACTED AUTH]');
});

test('empty saved titles fall back to first user text with provenance and never erase meaningful titles', t => {
  const dir = temp(t);
  const user = { type: 'message', message: { role: 'user', content: [{ type: 'text', text: '<system-reminder>hidden</system-reminder> Inspect bot integration and establish repository' }] } };
  const fixture = bridgeFixture(dir, 'bot', [{ type: 'title', title: '' }, user]);
  const store = new Store(join(dir, 'data')); t.after(() => store.close());
  const reader = new BridgeReader(store, [fixture.bridge], createSanitizer());
  const id = threadId('bridge', 'bot', 'same-thread');
  reader.refresh();
  assert.equal(store.detail(id, null, 100).thread.title, 'Inspect bot integration and establish repository');
  assert.equal(store.detail(id, null, 100).thread.titleSource, 'user_message');
  appendFileSync(fixture.file, JSON.stringify({ type: 'title', title: '   ' }) + '\n');
  reader.refresh();
  assert.equal(store.detail(id, null, 100).thread.titleSource, 'user_message');
  appendFileSync(fixture.file, JSON.stringify({ type: 'title', title: 'Saved session title' }) + '\n');
  reader.refresh();
  assert.equal(store.detail(id, null, 100).thread.title, 'Saved session title');
  assert.equal(store.detail(id, null, 100).thread.titleSource, 'session');
  const current = store.detail(id, null, 100).thread;
  store.upsert({ ...current, title: 'Actual Discord title', titleSource: 'discord' });
  appendFileSync(fixture.file, JSON.stringify({ type: 'title', title: 'Another session title' }) + '\n');
  reader.refresh();
  assert.equal(store.detail(id, null, 100).thread.title, 'Actual Discord title');
  assert.equal(store.detail(id, null, 100).thread.titleSource, 'discord');
});

test('Discord metadata title lookup respects rate limits, caches names, and rejects guild mixups', async t => {
  const dir = temp(t);
  const store = new Store(join(dir, 'data')); t.after(() => store.close());
  const tokenFile = join(dir, 'bot-token');
  const botToken = 'a'.repeat(60); writeFileSync(tokenFile, botToken);
  const bridge = { id: 'bot', botTokenFile: tokenFile };
  const thread = { id: threadId('bridge', 'bot', '123'), botId: 'bot', botName: 'Bot', title: 'Fallback', titleSource: 'fallback', guildId: '456', channelId: '123', status: 'waiting', updatedAt: 1000, source: 'bridge:bot' };
  store.upsert(thread);
  let clock = 1000; let calls = 0;
  const titles = new DiscordTitles(store, createSanitizer(), { now: () => clock, fetch: async (url, options) => {
    calls++;
    assert.equal(url, 'https://discord.com/api/v10/channels/123');
    assert.equal(options.headers.Authorization, `Bot ${botToken}`);
    if (calls === 1) return { status: 429, ok: false, json: async () => ({ retry_after: 2 }) };
    return { status: 200, ok: true, json: async () => ({ id: '123', guild_id: calls === 2 ? '456' : 'other-guild', name: calls === 2 ? 'Exact Discord thread title' : 'Wrong title' }) };
  } });
  t.after(() => titles.close());
  titles.enqueue(bridge, thread); await titles.pendingTask;
  assert.equal(calls, 1);
  clock += 1999; titles.enqueue(bridge, thread); assert.equal(calls, 1);
  clock++; titles.enqueue(bridge, thread); await titles.pendingTask;
  assert.equal(calls, 2);
  assert.equal(store.detail(thread.id, null, 100).thread.title, 'Exact Discord thread title');
  assert.equal(store.detail(thread.id, null, 100).thread.titleSource, 'discord');
  titles.enqueue(bridge, thread); assert.equal(calls, 2);
  clock += 300_000; titles.enqueue(bridge, thread); await titles.pendingTask;
  assert.equal(calls, 3);
  assert.equal(store.detail(thread.id, null, 100).thread.title, 'Exact Discord thread title');
  assert.ok(!store.db.prepare('SELECT payload FROM threads').get().payload.includes(botToken));
});
