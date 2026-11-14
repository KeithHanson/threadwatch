import { DatabaseSync } from 'node:sqlite';
import { mkdirSync, chmodSync } from 'node:fs';
import { join } from 'node:path';
import { Store } from '../server/store.mjs';
import { normalizeTodos } from '../server/sanitize.mjs';
import { batches, digest, eventCapacity, normalizeEvent, splitEvent } from './batches.mjs';

export function acquireStateLock(directory) {
  mkdirSync(directory, { recursive: true, mode: 0o700 });
  const path = join(directory, 'collector-lock.sqlite');
  const db = new DatabaseSync(path);
  try {
    chmodSync(path, 0o600);
    db.exec('PRAGMA busy_timeout=0; CREATE TABLE IF NOT EXISTS owner (id INTEGER PRIMARY KEY); BEGIN IMMEDIATE;');
    // SQLite releases this OS lock on close, process death, or machine restart.
    return () => db.close();
  } catch { db.close(); throw new Error('Another collector owns this state directory'); }
}

export class CollectorStore extends Store {
  constructor(directory, sanitize) {
    super(directory);
    this.sanitize = sanitize;
    this.db.exec('CREATE TABLE IF NOT EXISTS collector_resets (thread_id TEXT PRIMARY KEY, generation INTEGER NOT NULL)');
  }
  append(id, events) { super.append(id, events.map(event => normalizeEvent(event, this.sanitize)).filter(Boolean)); }
  reset(id) {
    super.reset(id);
    this.db.prepare('INSERT INTO collector_resets VALUES(?,1) ON CONFLICT(thread_id) DO UPDATE SET generation=generation+1').run(id);
  }
}

export class DeliveryQueue {
  constructor(directory, bridge) {
    mkdirSync(directory, { recursive: true, mode: 0o700 });
    const path = join(directory, 'collector-queue.sqlite');
    this.db = new DatabaseSync(path);
    try {
      chmodSync(path, 0o600);
      this.db.exec(`PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA busy_timeout=100;
        CREATE TABLE IF NOT EXISTS identity (id INTEGER PRIMARY KEY CHECK(id=1), value TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS deliveries (seq INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT UNIQUE NOT NULL, payload TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS captured_threads (id TEXT PRIMARY KEY, revision INTEGER NOT NULL, generation INTEGER NOT NULL, metadata TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS captured_events (thread_id TEXT NOT NULL, event_id TEXT NOT NULL, hash TEXT NOT NULL, parts INTEGER NOT NULL, PRIMARY KEY(thread_id,event_id));`);
      const identity = JSON.stringify([bridge.id, bridge.databasePath, bridge.sessionDir]);
      this.db.prepare('INSERT OR IGNORE INTO identity VALUES(1,?)').run(identity);
      if (this.db.prepare('SELECT value FROM identity WHERE id=1').get().value !== identity) throw new Error('State directory belongs to another bridge; use a distinct directory');
    } catch (error) { this.db.close(); throw error; }
  }
  transaction(fn) {
    this.db.exec('BEGIN IMMEDIATE');
    try { const result = fn(); this.db.exec('COMMIT'); return result; }
    catch (error) { this.db.exec('ROLLBACK'); throw error; }
  }
  count() { return this.db.prepare('SELECT COUNT(*) AS count FROM deliveries').get().count; }
  peek() { return this.db.prepare('SELECT id,payload FROM deliveries ORDER BY seq LIMIT 1').get() ?? null; }
  acknowledge(id) { this.db.prepare('DELETE FROM deliveries WHERE id=?').run(id); }
  close() { this.db.close(); }
}

function field(value, max, name, sanitize) {
  const text = sanitize.text(value);
  if (!text || text.length > max) throw new Error(`Invalid collector ${name}`);
  return text;
}
function threadPayload(thread, bridge, sanitize) {
  const identity = JSON.parse(Buffer.from(thread.id, 'base64url').toString('utf8'));
  if (identity[0] !== 'bridge' || identity[1] !== bridge.id) throw new Error('Unexpected cached bridge identity');
  return { id: field(identity[2], 200, 'thread ID', sanitize), botId: bridge.id,
    botName: field(bridge.name, 200, 'bot name', sanitize), title: field(thread.title, 1000, 'title', sanitize),
    ...(typeof thread.titleSource === 'string' ? { titleSource: sanitize.text(thread.titleSource) } : {}),
    guildId: thread.guildId == null ? null : field(thread.guildId, 100, 'guild ID', sanitize),
    channelId: thread.channelId == null ? null : field(thread.channelId, 100, 'channel ID', sanitize),
    status: thread.status, updatedAt: Number.isSafeInteger(thread.updatedAt) && thread.updatedAt >= 0 ? thread.updatedAt : 0 };
}
function* eventsInOrder(store, id) {
  const page = store.db.prepare('SELECT seq,payload FROM events WHERE thread_id=? AND seq>? ORDER BY seq LIMIT 250');
  let after = 0;
  for (;;) {
    const rows = page.all(id, after);
    if (!rows.length) return;
    for (const row of rows) { after = row.seq; yield JSON.parse(row.payload); }
  }
}

// Cache commits may precede capture: only queue snapshots are delivery checkpoints.
// Enqueuing every batch and advancing those snapshots is one durable transaction.
// A crash at any earlier point simply captures the still-persisted cache on restart.
export function captureChanges(store, queue, bridge, sanitize) {
  let added = 0;
  for (const row of store.db.prepare('SELECT id,payload,todos,revision FROM threads ORDER BY id').all()) {
    const previous = queue.db.prepare('SELECT * FROM captured_threads WHERE id=?').get(row.id);
    if (previous?.revision === row.revision) continue;
    const thread = threadPayload(JSON.parse(row.payload), bridge, sanitize);
    const todos = normalizeTodos(JSON.parse(row.todos), sanitize);
    if (todos.length > 2000) throw new Error('Too many todos for ingestion');
    const capacity = eventCapacity(thread, todos);
    const metadata = digest(JSON.stringify({ thread, todos }));
    const generation = store.db.prepare('SELECT generation FROM collector_resets WHERE thread_id=?').get(row.id)?.generation ?? 0;
    const old = new Map(queue.db.prepare('SELECT event_id,hash,parts FROM captured_events WHERE thread_id=?').all(row.id).map(event => [event.event_id, event]));
    const current = [];
    const changed = new Set();
    let reset = !previous || previous.generation !== generation;
    for (const event of eventsInOrder(store, row.id)) {
      const fragments = splitEvent(event, capacity);
      const hash = digest(JSON.stringify(fragments));
      const parts = fragments.length;
      const before = old.get(event.id);
      if (!before || before.hash !== hash || before.parts !== parts) changed.add(event.id);
      if (before && (before.hash !== hash || before.parts !== parts)) reset = true;
      old.delete(event.id);
      current.push({ id: event.id, hash, parts });
    }
    if (old.size) reset = true;
    queue.transaction(() => {
      if (reset || changed.size || previous?.metadata !== metadata) {
        function* selected() {
          for (const event of eventsInOrder(store, row.id)) if (reset || changed.has(event.id)) yield* splitEvent(event, capacity);
        }
        const insert = queue.db.prepare('INSERT INTO deliveries(id,payload) VALUES(?,?)');
        for (const batch of batches(thread, todos, selected(), reset)) { insert.run(batch.delivery.id, JSON.stringify(batch)); added++; }
      }
      const save = queue.db.prepare('INSERT INTO captured_events VALUES(?,?,?,?) ON CONFLICT(thread_id,event_id) DO UPDATE SET hash=excluded.hash,parts=excluded.parts');
      for (const event of current) save.run(row.id, event.id, event.hash, event.parts);
      const remove = queue.db.prepare('DELETE FROM captured_events WHERE thread_id=? AND event_id=?');
      for (const id of old.keys()) remove.run(row.id, id);
      queue.db.prepare('INSERT INTO captured_threads VALUES(?,?,?,?) ON CONFLICT(id) DO UPDATE SET revision=excluded.revision,generation=excluded.generation,metadata=excluded.metadata').run(row.id, row.revision, generation, metadata);
    });
  }
  return added;
}
