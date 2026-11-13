import { DatabaseSync } from 'node:sqlite';
import { readFileSync, realpathSync, statSync, openSync, closeSync, readSync } from 'node:fs';
import { homedir } from 'node:os';
import { resolve, relative, isAbsolute, join } from 'node:path';
import { deriveTodos, operationalRecord, sensitiveToolCall } from './sanitize.mjs';
import { threadId } from './store.mjs';
import { DiscordTitles } from './discord-titles.mjs';

const IMPORT_VERSION = 2;

export function expandPath(path) { return path === '~' ? homedir() : path.startsWith('~/') ? join(homedir(), path.slice(2)) : resolve(path); }
export function loadBridges(file) {
  const root = join(homedir(), '.local/state/omp-discord-bridge');
  const bridges = file ? JSON.parse(readFileSync(expandPath(file), 'utf8')) : [{ id: 'omp', name: 'OMP', databasePath: join(root, 'bridge.sqlite'), sessionDir: join(root, 'omp-sessions') }];
  if (!Array.isArray(bridges)) throw new Error('TW_BRIDGES_FILE must contain an array');
  const ids = new Set();
  return bridges.map(bridge => {
    if (!bridge || typeof bridge.id !== 'string' || !/^[\w-]{1,100}$/.test(bridge.id) || ids.has(bridge.id) || typeof bridge.name !== 'string' || !bridge.name || typeof bridge.databasePath !== 'string' || typeof bridge.sessionDir !== 'string') throw new Error('Invalid or duplicate bridge configuration');
    if (bridge.botTokenFile !== undefined && (typeof bridge.botTokenFile !== 'string' || !bridge.botTokenFile.trim())) throw new Error('Invalid bridge botTokenFile');
    ids.add(bridge.id);
    return { id: bridge.id, name: bridge.name, databasePath: expandPath(bridge.databasePath), sessionDir: expandPath(bridge.sessionDir), ...(bridge.botTokenFile ? { botTokenFile: expandPath(bridge.botTokenFile) } : {}) };
  });
}

function confinedPath(path, directory) {
  const root = realpathSync(directory);
  const candidate = realpathSync(path.startsWith('~') || isAbsolute(path) ? expandPath(path) : resolve(root, path));
  const rel = relative(root, candidate);
  if (!rel || rel === '..' || rel.startsWith(`..${process.platform === 'win32' ? '\\' : '/'}`) || isAbsolute(rel)) throw new Error('Session path outside source');
  return candidate;
}

function bridgeStatus(session, turn) {
  if (session.status === 'stopped') return 'stopped';
  return { queued: 'queued', running: 'working', done: 'completed', failed: 'failed', aborted: 'stopped', uncertain: 'unknown' }[turn?.status] ?? (session.status === 'active' ? 'waiting' : 'unknown');
}

export class BridgeReader {
  constructor(store, bridges, sanitize, options = {}) {
    this.store = store; this.bridges = bridges; this.sanitize = sanitize; this.sources = [];
    this.titles = new DiscordTitles(store, sanitize, options);
  }
  close() { this.titles.close(); }
  refresh() {
    this.sources = this.bridges.map(bridge => {
      const source = { id: `bridge:${bridge.id}`, name: this.sanitize.text(bridge.name), status: 'ready' };
      let database;
      try {
        database = new DatabaseSync(bridge.databasePath, { readOnly: true });
        database.exec('PRAGMA query_only=ON; PRAGMA busy_timeout=100;');
        const sessions = database.prepare('SELECT thread_id,guild_id,parent_channel_id,omp_session_path,status,updated_at_ms FROM sessions').all();
        const turnQuery = database.prepare('SELECT status,created_at_ms,started_at_ms,finished_at_ms FROM turns WHERE thread_id=? ORDER BY created_at_ms DESC,id DESC LIMIT 1');
        for (const session of sessions) {
          const id = threadId('bridge', bridge.id, String(session.thread_id));
          const turn = turnQuery.get(session.thread_id);
          const old = this.store.db.prepare('SELECT payload FROM threads WHERE id=?').get(id);
          const existing = old ? JSON.parse(old.payload) : null;
          const thread = { id, botId: bridge.id, botName: source.name, title: existing?.title?.trim() || `Thread ${session.thread_id}`, titleSource: existing?.title?.trim() ? existing.titleSource || 'session' : 'fallback', guildId: session.guild_id == null ? null : String(session.guild_id), channelId: String(session.thread_id), status: bridgeStatus(session, turn), updatedAt: Math.max(Number(session.updated_at_ms) || 0, existing?.updatedAt || 0), source: source.id };
          this.store.upsert(thread);
          this.titles.enqueue(bridge, thread);
          if (!session.omp_session_path) continue;
          try { this.importSession(thread, session.omp_session_path, bridge.sessionDir); }
          catch { source.status = 'degraded'; source.error = 'One or more saved transcripts are unavailable, malformed, or outside the configured session directory. Previously imported records may be stale.'; }
        }
      } catch { source.status = 'unavailable'; source.error = 'Bridge database is unavailable or incompatible. Previously imported records may be stale.'; }
      finally { database?.close(); }
      return source;
    });
    return this.sources;
  }
  importSession(thread, configuredPath, directory) {
    const path = confinedPath(configuredPath, directory);
    const stat = statSync(path);
    if (!stat.isFile()) throw new Error('Not a session file');
    const identity = `${stat.dev}:${stat.ino}`;
    const previous = this.store.checkpoint(thread.id);
    if (previous?.version === IMPORT_VERSION && previous.path === path && previous.identity === identity && previous.size === stat.size && previous.mtime === stat.mtimeMs && previous.ctime === stat.ctimeMs) {
      if (previous.malformed) throw new Error('Malformed transcript');
      return;
    }
    const append = previous?.version === IMPORT_VERSION && previous.path === path && previous.identity === identity && stat.size > previous.size;
    const state = append ? { ...previous } : { version: IMPORT_VERSION, offset: 0, line: 0, pending: {}, malformed: false };
    let todos = append ? this.store.todos(thread.id) : [];
    const fd = openSync(path, 'r');
    try {
      this.store.transaction(() => {
        if (!append) this.store.reset(thread.id);
        let position = state.offset;
        let pending = Buffer.alloc(0);
        const chunk = Buffer.allocUnsafe(64 * 1024);
        while (position < stat.size) {
          const length = readSync(fd, chunk, 0, Math.min(chunk.length, stat.size - position), position);
          if (!length) break;
          position += length;
          const bytes = pending.length ? Buffer.concat([pending, chunk.subarray(0, length)]) : chunk.subarray(0, length);
          let start = 0;
          for (let end = bytes.indexOf(10); end !== -1; end = bytes.indexOf(10, start)) {
            const line = bytes.subarray(start, end).toString('utf8');
            state.offset += end - start + 1;
            state.line++;
            start = end + 1;
            if (!line.trim()) continue;
            let record;
            try { record = JSON.parse(line); if (!record || typeof record !== 'object') throw new Error(); }
            catch {
              state.malformed = true;
              this.store.append(thread.id, [{ id: `${state.line}:malformed`, timestamp: null, kind: 'import_error', text: `Malformed saved record at line ${state.line}; its contents were not imported.`, isError: true }]);
              continue;
            }
            const savedTimestamp = record.timestamp ?? record.message?.timestamp;
            const recordedAt = typeof savedTimestamp === 'number' ? savedTimestamp : typeof savedTimestamp === 'string' ? Date.parse(savedTimestamp) : NaN;
            if (Number.isFinite(recordedAt) && recordedAt > thread.updatedAt) thread.updatedAt = recordedAt;
            if (record.type === 'title' && typeof record.title === 'string' && thread.titleSource !== 'discord') {
              const title = this.sanitize.text(record.title).trim();
              if (title) { thread.title = title; thread.titleSource = 'session'; }
            }
            if (thread.titleSource === 'fallback' && record.type === 'message' && record.message?.role === 'user') {
              const content = record.message.content;
              const text = typeof content === 'string' ? content : Array.isArray(content) ? content.filter(part => part?.type === 'text' && typeof part.text === 'string').map(part => part.text).join(' ') : '';
              const title = this.sanitize.text(text).replace(/\s+/g, ' ').trim();
              if (title) { thread.title = title.length > 120 ? `${title.slice(0, 117)}...` : title; thread.titleSource = 'user_message'; }
            }
            let withheld = false;
            if (record.type === 'message' && record.message?.role === 'assistant' && Array.isArray(record.message.content)) {
              for (const part of record.message.content) if (part?.type === 'toolCall' && typeof part.id === 'string' && (part.name === 'todo' || sensitiveToolCall(part))) {
                // Persist no tool arguments: only correlation flags and the todo operation.
                state.pending[part.id] = { op: part.name === 'todo' && typeof part.arguments?.op === 'string' ? this.sanitize.text(part.arguments.op) : null, withheld: sensitiveToolCall(part) };
              }
            }
            if (record.type === 'custom' && record.customType === 'tool_execution_start' && typeof record.data?.toolCallId === 'string' && sensitiveToolCall(record.data)) state.pending[record.data.toolCallId] = { withheld: true };
            if (record.type === 'message' && record.message?.role === 'toolResult' && typeof record.message.toolCallId === 'string') {
              const message = record.message;
              const args = state.pending[message.toolCallId];
              withheld = args?.withheld === true;
              if (args && !withheld && message.isError !== true && message.toolName === 'todo') {
                const text = Array.isArray(message.content) ? message.content.filter(part => part?.type === 'text' && typeof part.text === 'string').map(part => this.sanitize.text(part.text)).join('\n') : '';
                todos = deriveTodos(args, text, todos);
              }
              delete state.pending[message.toolCallId];
            }
            if (record.type === 'custom' && record.customType === 'user_todo_edit' && Array.isArray(record.data?.phases)) {
              todos = record.data.phases.flatMap(phase => Array.isArray(phase?.tasks) ? phase.tasks.filter(task => typeof task?.content === 'string' && ['pending', 'in_progress', 'completed'].includes(task.status)).map(task => ({ text: this.sanitize.text(task.content), status: task.status })) : []);
              this.store.append(thread.id, [{ id: `${state.line}:todos`, timestamp: record.timestamp ?? null, kind: 'todo_edit', text: JSON.stringify(todos) }]);
            }
            this.store.append(thread.id, operationalRecord(record, this.sanitize, state.line, withheld));
          }
          pending = Buffer.from(bytes.subarray(start));
        }
        this.store.upsert(thread, todos);
        this.store.saveCheckpoint(thread.id, { ...state, path, identity, size: stat.size, mtime: stat.mtimeMs, ctime: stat.ctimeMs });
      });
    } finally { closeSync(fd); }
    if (state.malformed) throw new Error('Malformed transcript');
  }
}
