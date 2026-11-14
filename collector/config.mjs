import { openSync, closeSync, readFileSync, fstatSync, constants, realpathSync, existsSync } from 'node:fs';
import { homedir } from 'node:os';
import { join, resolve } from 'node:path';
import { expandPath, loadBridges } from '../server/bridge.mjs';

function interval(value, fallback, name, maximum = 2_147_483_647) {
  const result = Number(value ?? fallback);
  if (!Number.isSafeInteger(result) || result < 100 || result > maximum) throw new Error(`Invalid ${name}`);
  return result;
}
function protectedToken(path) {
  const fd = openSync(expandPath(path), constants.O_RDONLY | constants.O_NOFOLLOW);
  try {
    const stat = fstatSync(fd);
    if (!stat.isFile() || (stat.mode & 0o077) || (process.getuid && stat.uid !== process.getuid())) throw new Error('Token file must be owner-only and owned by this user');
    return readFileSync(fd, 'utf8').trim();
  } finally { closeSync(fd); }
}
function canonical(path) { return existsSync(path) ? realpathSync(path) : resolve(path); }
export function readCollectorConfig(env = process.env) {
  const url = new URL(env.TW_SERVER_URL || '');
  if (url.protocol !== 'http:' || url.username || url.password || url.pathname !== '/' || url.search || url.hash) throw new Error('TW_SERVER_URL must be a plain HTTP origin without credentials or paths');
  if (env.TW_INGEST_TOKEN && env.TW_INGEST_TOKEN_FILE) throw new Error('Choose TW_INGEST_TOKEN or TW_INGEST_TOKEN_FILE');
  const token = env.TW_INGEST_TOKEN_FILE ? protectedToken(env.TW_INGEST_TOKEN_FILE) : env.TW_INGEST_TOKEN;
  if (typeof token !== 'string' || token.length < 32 || /\s/.test(token)) throw new Error('Ingest token must contain at least 32 non-whitespace characters');
  if (!env.TW_BRIDGES_FILE) throw new Error('TW_BRIDGES_FILE is required');
  const bridges = loadBridges(env.TW_BRIDGES_FILE);
  if (bridges.length !== 1) throw new Error('A collector requires exactly one bridge');
  const dataDir = expandPath(env.TW_COLLECTOR_DATA_DIR || join(homedir(), '.local/state/threadwatch-collector'));
  const serverDir = expandPath(env.TW_DATA_DIR || join(homedir(), '.local/state/threadwatch'));
  if (canonical(dataDir) === canonical(serverDir)) throw new Error('Collector data directory must differ from server data directory');
  return { serverUrl: url.origin, token, bridges, dataDir,
    collectInterval: interval(env.TW_COLLECT_INTERVAL_MS, 2000, 'TW_COLLECT_INTERVAL_MS'),
    heartbeatInterval: interval(env.TW_HEARTBEAT_INTERVAL_MS, 10000, 'TW_HEARTBEAT_INTERVAL_MS', 30_000), requestTimeout: 10_000 };
}
