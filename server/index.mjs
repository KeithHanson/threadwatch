import http from 'node:http';
import { randomBytes, timingSafeEqual, createHash } from 'node:crypto';
import { readFileSync, statSync, realpathSync } from 'node:fs';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { dirname, join, resolve, relative, extname, isAbsolute } from 'node:path';
import { homedir } from 'node:os';
import { Store, STATUSES, threadId } from './store.mjs';
import { BridgeReader, loadBridges, expandPath } from './bridge.mjs';
import { createSanitizer, normalizeTodos } from './sanitize.mjs';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const MAX_BODY = 1024 * 1024;
function fail(status, message) { const error = new Error(message); error.status = status; throw error; }
function string(value, name, max = 1000) { if (typeof value !== 'string' || !value.length || value.length > max) fail(400, `Invalid ${name}`); return value; }
function equal(a, b) { if (!a || !b) return false; const x = Buffer.from(a); const y = Buffer.from(b); return x.length === y.length && timingSafeEqual(x, y); }
function loopback(address) { return address === '::1' || /^127\./.test(address ?? '') || /^::ffff:127\./.test(address ?? ''); }

export function readConfig(env = process.env) {
  const local = env.TW_LOCAL === '1';
  const host = env.TW_HOST || '127.0.0.1';
  const port = Number(env.TW_PORT || 8787);
  if (!Number.isInteger(port) || port < 1 || port > 65535) throw new Error('Invalid TW_PORT');
  if (local && !['127.0.0.1', 'localhost', '::1'].includes(host)) throw new Error('TW_LOCAL requires a loopback TW_HOST');
  const clientId = env.TW_DISCORD_CLIENT_ID || null;
  const clientSecret = env.TW_DISCORD_CLIENT_SECRET || '';
  const allowedUsers = new Set((env.TW_ALLOWED_USER_IDS || '').split(',').map(value => value.trim()).filter(Boolean));
  if (!local && (!clientId || !/^\d+$/.test(clientId) || !clientSecret || !allowedUsers.size || [...allowedUsers].some(value => !/^\d+$/.test(value)))) throw new Error('Nonlocal mode requires TW_DISCORD_CLIENT_ID, TW_DISCORD_CLIENT_SECRET and TW_ALLOWED_USER_IDS');
  if (env.TW_INGEST_TOKEN && env.TW_INGEST_TOKEN.length < 32) throw new Error('TW_INGEST_TOKEN must be at least 32 characters');
  return { local, host, port, clientId, clientSecret, allowedUsers, ingestToken: env.TW_INGEST_TOKEN || '', dataDir: expandPath(env.TW_DATA_DIR || join(homedir(), '.local/state/threadwatch')), bridges: loadBridges(env.TW_BRIDGES_FILE), distDir: join(ROOT, 'dist') };
}

async function body(req) {
  if (!/^application\/json(?:\s*;|$)/i.test(req.headers['content-type'] || '')) fail(415, 'Content-Type must be application/json');
  const length = req.headers['content-length'];
  if (length && (!/^\d+$/.test(length) || Number(length) > MAX_BODY)) fail(413, 'Request body too large');
  const chunks = [];
  let total = 0;
  for await (const chunk of req) { total += chunk.length; if (total > MAX_BODY) fail(413, 'Request body too large'); chunks.push(chunk); }
  let parsed;
  try { parsed = JSON.parse(Buffer.concat(chunks).toString('utf8')); } catch { fail(400, 'Invalid JSON'); }
  if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) fail(400, 'Expected a JSON object');
  return parsed;
}

export function validateIngest(input, sanitize) {
  const data = input.thread;
  if (!data || typeof data !== 'object' || Array.isArray(data)) fail(400, 'Expected thread');
  const botId = string(data.botId, 'botId', 100);
  const externalId = string(data.id, 'thread.id', 200);
  if (sanitize.text(botId) !== botId || sanitize.text(externalId) !== externalId) fail(400, 'Identifiers must not contain sensitive content');
  if (!STATUSES.has(data.status)) fail(400, 'Invalid thread status');
  if (!Number.isSafeInteger(data.updatedAt) || data.updatedAt < 0) fail(400, 'updatedAt must be epoch milliseconds');
  if (!Array.isArray(input.events) || input.events.length > 1000) fail(400, 'events must be an array of at most 1000 records');
  if (input.todos !== undefined && (!Array.isArray(input.todos) || input.todos.length > 2000)) fail(400, 'Invalid todos');
  const thread = { id: threadId('ingest', botId, externalId), botId, botName: sanitize.text(string(data.botName, 'botName', 200)), title: sanitize.text(string(data.title, 'title', 1000)), guildId: data.guildId == null ? null : sanitize.text(string(data.guildId, 'guildId', 100)), channelId: data.channelId == null ? null : sanitize.text(string(data.channelId, 'channelId', 100)), status: data.status, updatedAt: data.updatedAt, source: `ingest:${botId}` };
  const ids = new Set();
  const events = input.events.map(event => {
    if (!event || !['message', 'tool_call', 'tool_result', 'status', 'todo_edit', 'session_exit'].includes(event.kind)) fail(400, 'Invalid event kind');
    const id = string(event.id, 'event.id', 200);
    if (sanitize.text(id) !== id) fail(400, 'Identifiers must not contain sensitive content');
    if (ids.has(id)) fail(400, 'Duplicate event id');
    ids.add(id);
    if (event.role !== undefined && !['user', 'assistant', 'toolResult'].includes(event.role)) fail(400, 'Invalid event role');
    if (!(Number.isSafeInteger(event.timestamp) && event.timestamp >= 0) && !(typeof event.timestamp === 'string' && Number.isFinite(Date.parse(event.timestamp)))) fail(400, 'Invalid event timestamp');
    if (event.isError !== undefined && typeof event.isError !== 'boolean') fail(400, 'Invalid isError');
    return { id, timestamp: typeof event.timestamp === 'string' ? Date.parse(event.timestamp) : event.timestamp, kind: event.kind, ...(event.role ? { role: event.role } : {}), ...(event.name !== undefined ? { name: sanitize.text(string(event.name, 'event.name', 200)) } : {}), text: sanitize.text(string(event.text, 'event.text', MAX_BODY)), ...(event.isError === true ? { isError: true } : {}) };
  });
  let todos;
  try { todos = input.todos === undefined ? undefined : normalizeTodos(input.todos, sanitize); } catch { fail(400, 'Invalid todos'); }
  return { thread, events, todos };
}

export function createApp(config, options = {}) {
  const sanitize = createSanitizer([config.clientSecret, config.ingestToken]);
  const store = options.store || new Store(config.dataDir);
  const reader = new BridgeReader(store, config.bridges, sanitize, { fetch: options.fetch || globalThis.fetch });
  const sessions = new Map();
  const fetcher = options.fetch || globalThis.fetch;
  let refreshedAt = 0;
  let authWindow = 0;
  let authAttempts = 0;
  function refresh() { if (Date.now() - refreshedAt >= 1500) { reader.refresh(); refreshedAt = Date.now(); } }
  function guardRequest(req) {
    const host = req.headers.host;
    if (!host || /[\s/@\\]/.test(host)) fail(403, 'Invalid request host');
    let authority;
    try { authority = new URL(`http://${host}`); } catch { fail(403, 'Invalid request host'); }
    if (config.local) {
      if (!loopback(req.socket.remoteAddress) || !['localhost', '127.0.0.1', '[::1]'].includes(authority.hostname)) fail(403, 'Local mode accepts loopback requests only');
      if (req.headers['sec-fetch-site'] === 'cross-site') fail(403, 'Cross-origin request rejected');
    }
    const origin = req.headers.origin;
    if (origin) {
      const allowed = new Set([`http://${host}`, `https://${host}`]);
      if (!config.local && config.clientId) { allowed.add(`https://${config.clientId}.discordsays.com`); allowed.add(`https://${config.clientId}.discordsez.com`); }
      if (!allowed.has(origin)) fail(403, 'Cross-origin request rejected');
    }
  }
  function authorized(req) {
    if (config.local) return true;
    const token = /^Bearer (\S+)$/.exec(req.headers.authorization || '')?.[1];
    const session = token ? sessions.get(token) : null;
    if (!session || session.expiresAt <= Date.now() || !config.allowedUsers.has(session.userId)) { if (token) sessions.delete(token); return false; }
    return true;
  }
  function json(res, status, value) { res.writeHead(status, { 'Content-Type': 'application/json; charset=utf-8', 'Cache-Control': 'no-store' }); res.end(JSON.stringify(value)); }
  const server = http.createServer(async (req, res) => {
    res.setHeader('X-Content-Type-Options', 'nosniff');
    res.setHeader('Referrer-Policy', 'no-referrer');
    res.setHeader('Cross-Origin-Resource-Policy', 'same-origin');
    try {
      guardRequest(req);
      const url = new URL(req.url, 'http://localhost');
      if (url.pathname === '/api/config' && req.method === 'GET') return json(res, 200, { clientId: config.clientId, local: config.local });
      if (url.pathname === '/api/auth' && req.method === 'POST') {
        if (config.local) fail(400, 'OAuth is not used in local mode');
        if (Date.now() - authWindow > 60_000) { authWindow = Date.now(); authAttempts = 0; }
        if (++authAttempts > 30) fail(429, 'Too many authorization attempts');
        const input = await body(req);
        const code = string(input.code, 'code', 4096);
        const response = await fetcher('https://discord.com/api/v10/oauth2/token', { method: 'POST', headers: { 'Content-Type': 'application/x-www-form-urlencoded' }, body: new URLSearchParams({ client_id: config.clientId, client_secret: config.clientSecret, grant_type: 'authorization_code', code }), signal: AbortSignal.timeout(10_000) });
        if (!response.ok) fail(401, 'Discord authorization failed');
        const token = await response.json();
        if (typeof token.access_token !== 'string' || !String(token.scope || '').split(' ').includes('identify') || !Number.isFinite(token.expires_in) || token.expires_in <= 0) fail(401, 'Discord identify authorization required');
        const identityResponse = await fetcher('https://discord.com/api/v10/users/@me', { headers: { Authorization: `Bearer ${token.access_token}` }, signal: AbortSignal.timeout(10_000) });
        if (!identityResponse.ok) fail(401, 'Discord identity verification failed');
        const identity = await identityResponse.json();
        if (!config.allowedUsers.has(identity.id)) fail(403, 'This Discord user is not authorized');
        for (const [key, session] of sessions) if (session.expiresAt <= Date.now()) sessions.delete(key);
        if (sessions.size >= 1000) fail(503, 'Session capacity reached');
        const sessionToken = randomBytes(32).toString('base64url');
        sessions.set(sessionToken, { userId: identity.id, expiresAt: Date.now() + Math.min(token.expires_in * 1000, 8 * 60 * 60 * 1000) });
        return json(res, 200, { access_token: token.access_token, sessionToken });
      }
      if (url.pathname === '/api/ingest' && req.method === 'POST') {
        const token = /^Bearer (\S+)$/.exec(req.headers.authorization || '')?.[1];
        if (!equal(token, config.ingestToken)) fail(401, 'Ingest authorization required');
        const { thread, events, todos } = validateIngest(await body(req), sanitize);
        store.transaction(() => { store.upsert(thread, todos); store.append(thread.id, events); });
        return json(res, 200, { id: thread.id, revision: store.revision() });
      }
      if (url.pathname.startsWith('/api/')) {
        if (!authorized(req)) fail(401, 'Authorization required');
        if (req.method !== 'GET') fail(405, 'Method not allowed');
        refresh();
        if (url.pathname === '/api/threads') {
          const configured = new Set(reader.sources.map(source => source.id));
          const threads = store.list().filter(thread => thread.source.startsWith('ingest:') || configured.has(thread.source));
          const sources = [...reader.sources];
          for (const thread of threads) if (thread.source.startsWith('ingest:') && !sources.some(source => source.id === thread.source)) sources.push({ id: thread.source, name: thread.botName, status: 'saved' });
          return json(res, 200, { threads, sources, revision: `${store.revision()}:${createHash('sha256').update(JSON.stringify(sources)).digest('hex').slice(0, 16)}` });
        }
        const match = /^\/api\/threads\/([A-Za-z0-9_-]+)$/.exec(url.pathname);
        if (match) {
          const rawLimit = url.searchParams.get('limit') ?? '100';
          if (!/^\d+$/.test(rawLimit) || Number(rawLimit) < 1 || Number(rawLimit) > 500) fail(400, 'limit must be between 1 and 500');
          let detail;
          try { detail = store.detail(match[1], url.searchParams.get('before'), Number(rawLimit)); } catch { fail(400, 'Invalid pagination cursor'); }
          if (!detail || (!detail.thread.source.startsWith('ingest:') && !reader.sources.some(source => source.id === detail.thread.source))) fail(404, 'Thread not found');
          return json(res, 200, detail);
        }
        fail(404, 'API route not found');
      }
      if (!['GET', 'HEAD'].includes(req.method)) fail(405, 'Method not allowed');
      let pathname;
      try { pathname = decodeURIComponent(url.pathname); } catch { fail(400, 'Invalid path'); }
      if (pathname.includes('\0')) fail(400, 'Invalid path');
      let file = resolve(config.distDir, `.${pathname}`);
      const rel = relative(config.distDir, file);
      if (rel.startsWith('..') || isAbsolute(rel)) fail(404, 'Not found');
      try { if (!statSync(file).isFile()) file = join(config.distDir, 'index.html'); }
      catch { if (extname(pathname)) fail(404, 'Not found'); file = join(config.distDir, 'index.html'); }
      let data;
      try {
        const root = realpathSync(config.distDir);
        const actual = realpathSync(file);
        const path = relative(root, actual);
        if (path.startsWith('..') || isAbsolute(path)) fail(404, 'Not found');
        data = readFileSync(actual);
      } catch { fail(404, 'Frontend is not built or resource is unavailable'); }
      const mime = { '.html': 'text/html; charset=utf-8', '.js': 'text/javascript; charset=utf-8', '.css': 'text/css; charset=utf-8', '.svg': 'image/svg+xml', '.png': 'image/png', '.ico': 'image/x-icon', '.woff2': 'font/woff2' }[extname(file)] || 'application/octet-stream';
      res.writeHead(200, { 'Content-Type': mime, 'Cache-Control': 'no-cache', 'Content-Security-Policy': "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; connect-src 'self' https://discord.com https://*.discordsays.com https://*.discordsez.com; img-src 'self' data:; frame-ancestors https://discord.com https://*.discord.com https://*.discordsays.com https://*.discordsez.com" });
      res.end(req.method === 'HEAD' ? undefined : data);
    } catch (error) { if (!res.headersSent) json(res, error.status || 500, { error: error.status ? error.message : 'Request failed' }); else res.end(); }
  });
  server.requestTimeout = 15_000;
  server.headersTimeout = 10_000;
  return { server, store, reader, close: () => { sessions.clear(); reader.close(); store.close(); } };
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  try {
    const config = readConfig();
    const app = createApp(config);
    app.server.on('error', () => { console.error('Threadwatch could not start its HTTP listener.'); process.exitCode = 1; app.close(); });
    app.server.listen(config.port, config.host, () => console.log(`Threadwatch listening on ${config.host}:${config.port} (${config.local ? 'local loopback' : 'Discord allowlist'} mode)`));
    const stop = () => app.server.close(() => { app.close(); process.exit(0); });
    process.once('SIGINT', stop);
    process.once('SIGTERM', stop);
  } catch (error) { console.error(error.message.startsWith('TW_') || error.message.startsWith('Nonlocal') || error.message.startsWith('Invalid TW_') ? error.message : 'Invalid Threadwatch configuration.'); process.exitCode = 1; }
}
