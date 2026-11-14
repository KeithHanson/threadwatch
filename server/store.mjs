import { DatabaseSync } from 'node:sqlite';
import { mkdirSync, chmodSync } from 'node:fs';
import { join } from 'node:path';

export const STATUSES = new Set(['working', 'queued', 'waiting', 'completed', 'failed', 'unknown', 'stopped']);
export function threadId(kind, bot, thread) { return Buffer.from(JSON.stringify([kind, bot, thread])).toString('base64url'); }

export class Store {
  constructor(directory) {
    mkdirSync(directory, { recursive: true, mode: 0o700 });
    const path = join(directory, 'threadwatch.sqlite');
    this.db = new DatabaseSync(path);
    chmodSync(path, 0o600);
    this.db.exec(`PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;
      CREATE TABLE IF NOT EXISTS threads (id TEXT PRIMARY KEY, source TEXT NOT NULL, payload TEXT NOT NULL, todos TEXT NOT NULL DEFAULT '[]', revision INTEGER NOT NULL DEFAULT 0);
      CREATE TABLE IF NOT EXISTS events (thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE, seq INTEGER NOT NULL, event_id TEXT NOT NULL, payload TEXT NOT NULL, PRIMARY KEY(thread_id,event_id), UNIQUE(thread_id,seq));
      CREATE TABLE IF NOT EXISTS imports (thread_id TEXT PRIMARY KEY, payload TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS deliveries (bot_id TEXT NOT NULL, delivery_id TEXT NOT NULL, thread_id TEXT NOT NULL, PRIMARY KEY(bot_id,delivery_id));
      CREATE TABLE IF NOT EXISTS heartbeats (bot_id TEXT PRIMARY KEY, payload TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value INTEGER NOT NULL);
      INSERT OR IGNORE INTO meta VALUES ('revision',0);`);
  }
  transaction(fn) {
    this.db.exec('BEGIN IMMEDIATE');
    try { const result = fn(); this.db.exec('COMMIT'); return result; }
    catch (error) { this.db.exec('ROLLBACK'); throw error; }
  }
  revision() { return String(this.db.prepare("SELECT value FROM meta WHERE key='revision'").get().value); }
  bump(id) {
    this.db.prepare("UPDATE meta SET value=value+1 WHERE key='revision'").run();
    this.db.prepare("UPDATE threads SET revision=(SELECT value FROM meta WHERE key='revision') WHERE id=?").run(id);
  }
  upsert(thread, todos) {
    const current = this.db.prepare('SELECT payload,todos FROM threads WHERE id=?').get(thread.id);
    if (current && thread.historyVersion === undefined) {
      const historyVersion = JSON.parse(current.payload).historyVersion;
      if (historyVersion !== undefined) thread = { ...thread, historyVersion };
    }
    const payload = JSON.stringify(thread);
    const todoText = todos === undefined ? current?.todos ?? '[]' : JSON.stringify(todos);
    if (current?.payload === payload && current?.todos === todoText) return;
    this.db.prepare('INSERT INTO threads(id,source,payload,todos) VALUES(?,?,?,?) ON CONFLICT(id) DO UPDATE SET source=excluded.source,payload=excluded.payload,todos=excluded.todos').run(thread.id, thread.source, payload, todoText);
    this.bump(thread.id);
  }
  append(id, events) {
    let seq = this.db.prepare('SELECT COALESCE(MAX(seq),0) AS n FROM events WHERE thread_id=?').get(id).n;
    const insert = this.db.prepare('INSERT INTO events(thread_id,seq,event_id,payload) VALUES(?,?,?,?)');
    const lookup = this.db.prepare('SELECT payload FROM events WHERE thread_id=? AND event_id=?');
    const update = this.db.prepare('UPDATE events SET payload=? WHERE thread_id=? AND event_id=?');
    let changed = false;
    for (const event of events) {
      const previous = lookup.get(id, event.id);
      const payload = JSON.stringify(event);
      if (!previous) { insert.run(id, ++seq, event.id, payload); changed = true; }
      else if (previous.payload !== payload) { update.run(payload, id, event.id); changed = true; }
    }
    if (changed) this.bump(id);
  }
  reset(id) {
    this.db.prepare('DELETE FROM events WHERE thread_id=?').run(id);
    this.db.prepare("UPDATE threads SET todos='[]' WHERE id=?").run(id);
    this.bump(id);
  }
  ingest(thread, events, todos, delivery) {
    return this.transaction(() => {
      if (delivery) {
        const previous = this.db.prepare('SELECT thread_id FROM deliveries WHERE bot_id=? AND delivery_id=?').get(thread.botId, delivery.id);
        if (previous) return previous.thread_id;
        if (delivery.reset) {
          this.reset(thread.id);
          thread = { ...thread, historyVersion: delivery.id };
        }
      }
      this.upsert(thread, todos);
      this.append(thread.id, events);
      if (delivery) this.db.prepare('INSERT INTO deliveries VALUES(?,?,?)').run(thread.botId, delivery.id, thread.id);
      return thread.id;
    });
  }
  recordHeartbeat(heartbeat, receivedAt) {
    this.transaction(() => {
      const payload = JSON.stringify({ id: `ingest:${heartbeat.botId}`, name: heartbeat.botName, status: heartbeat.status, lastSeenAt: receivedAt, pendingBatches: heartbeat.pendingBatches, ...(heartbeat.error ? { error: heartbeat.error } : {}) });
      this.db.prepare('INSERT INTO heartbeats VALUES(?,?) ON CONFLICT(bot_id) DO UPDATE SET payload=excluded.payload').run(heartbeat.botId, payload);
      this.db.prepare("UPDATE meta SET value=value+1 WHERE key='revision'").run();
    });
  }
  collectorSources(now = Date.now()) {
    return this.db.prepare('SELECT payload FROM heartbeats ORDER BY bot_id').all().map(row => {
      const source = JSON.parse(row.payload);
      return now - source.lastSeenAt >= 45_000 ? { ...source, status: 'offline' } : source;
    });
  }
  checkpoint(id) {
    const row = this.db.prepare('SELECT payload FROM imports WHERE thread_id=?').get(id);
    return row ? JSON.parse(row.payload) : null;
  }
  saveCheckpoint(id, state) { this.db.prepare('INSERT INTO imports VALUES(?,?) ON CONFLICT(thread_id) DO UPDATE SET payload=excluded.payload').run(id, JSON.stringify(state)); }
  todos(id) { const row = this.db.prepare('SELECT todos FROM threads WHERE id=?').get(id); return row ? JSON.parse(row.todos) : []; }
  list() { return this.db.prepare('SELECT payload FROM threads').all().map(row => JSON.parse(row.payload)).sort((a, b) => b.updatedAt - a.updatedAt); }
  detail(id, before, limit) {
    const row = this.db.prepare('SELECT payload,todos,revision FROM threads WHERE id=?').get(id);
    if (!row) return null;
    let boundary = Number.MAX_SAFE_INTEGER;
    if (before) {
      let cursor;
      try { cursor = JSON.parse(Buffer.from(before, 'base64url').toString()); } catch { throw new Error('Invalid cursor'); }
      if (!Array.isArray(cursor) || cursor[0] !== id || !Number.isSafeInteger(cursor[1]) || cursor[1] < 1) throw new Error('Invalid cursor');
      boundary = cursor[1];
    }
    const rows = this.db.prepare('SELECT seq,payload FROM events WHERE thread_id=? AND seq<? ORDER BY seq DESC LIMIT ?').all(id, boundary, limit + 1);
    const more = rows.length > limit;
    if (more) rows.pop();
    rows.reverse();
    return { thread: JSON.parse(row.payload), todos: JSON.parse(row.todos), events: rows.map(item => JSON.parse(item.payload)), nextCursor: more ? Buffer.from(JSON.stringify([id, rows[0].seq])).toString('base64url') : null, revision: String(row.revision) };
  }
  close() { this.db.close(); }
}
