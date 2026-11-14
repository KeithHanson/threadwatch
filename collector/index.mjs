import { Worker } from 'node:worker_threads';
import { setTimeout as delay } from 'node:timers/promises';
import { pathToFileURL } from 'node:url';
import { createSanitizer } from '../server/sanitize.mjs';
import { readCollectorConfig } from './config.mjs';
import { acquireStateLock, DeliveryQueue } from './state.mjs';

export { readCollectorConfig } from './config.mjs';
export { CollectorStore, DeliveryQueue, acquireStateLock, captureChanges } from './state.mjs';
export { batches, splitEvent, normalizeEvent, MAX_BATCH_BYTES, MAX_BATCH_EVENTS } from './batches.mjs';

export function createTransport(config, { fetch: fetcher = globalThis.fetch, signal } = {}) {
  const origin = new URL(config.serverUrl);
  if (origin.protocol !== 'http:' || origin.username || origin.password || origin.pathname !== '/' || origin.search || origin.hash) throw new Error('Invalid collector HTTP origin');
  return async (path, payload) => {
    if (!['/api/ingest', '/api/heartbeat'].includes(path)) throw new Error('Invalid collector API path');
    const response = await fetcher(new URL(path, origin).href, {
      method: 'POST', headers: { Authorization: `Bearer ${config.token}`, 'Content-Type': 'application/json' },
      body: typeof payload === 'string' ? payload : JSON.stringify(payload), redirect: 'error',
      signal: AbortSignal.any([...(signal ? [signal] : []), AbortSignal.timeout(config.requestTimeout ?? 10_000)]),
    });
    if (!response.ok) throw new Error('Reporting endpoint rejected request');
    const result = await response.json();
    if (path === '/api/ingest' && (typeof result?.id !== 'string' || !result.id || typeof result.revision !== 'string')) throw new Error('Reporting endpoint returned an invalid acknowledgement');
    return result;
  };
}

// Only the adapter/cache worker performs synchronous backfills. The main event
// loop remains free to send heartbeats and drain previously captured batches.
export async function createCollector(config, options = {}) {
  const controller = new AbortController();
  const post = createTransport(config, { ...options, signal: controller.signal });
  const release = acquireStateLock(config.dataDir);
  let queue;
  let worker;
  try {
    queue = new DeliveryQueue(config.dataDir, config.bridges[0]);
    worker = new Worker(new URL('./worker.mjs', import.meta.url), { workerData: { dataDir: config.dataDir, bridges: config.bridges, token: config.token } });
  } catch (error) { queue?.close(); release(); throw error; }
  const sanitize = createSanitizer([config.token]);
  let source = { status: 'degraded', error: 'Initial source collection is in progress.' };
  let deliveryFailed = false;
  let heartbeatFailed = false;
  let workerFailed = false;
  let closed = false;
  let closing;
  let collection;
  let resolveCollection;
  let flushing;
  let beating;
  let resolveReady;
  let rejectReady;
  const ready = new Promise((resolve, reject) => { resolveReady = resolve; rejectReady = reject; });
  const exited = new Promise(resolve => worker.once('exit', resolve));
  function failWorker() {
    workerFailed = true;
    source = { status: 'unavailable', error: 'Collector adapter worker is unavailable.' };
    rejectReady(new Error('Collector worker failed to start'));
    resolveCollection?.(source);
    resolveCollection = null;
  }
  worker.on('error', failWorker);
  worker.on('exit', () => { if (!closed) failWorker(); });
  worker.on('message', message => {
    if (message.type === 'ready') resolveReady();
    if (message.type === 'collected') {
      source = { status: message.status, ...(message.error ? { error: message.error } : {}) };
      resolveCollection?.(source);
      resolveCollection = null;
    }
  });
  function stop() {
    controller.abort();
    resolveCollection?.({ status: 'unavailable', error: 'Collector is stopping.' });
    resolveCollection = null;
  }
  async function collect() {
    if (closed || controller.signal.aborted || workerFailed) return source;
    if (!collection) {
      collection = new Promise(resolve => { resolveCollection = resolve; worker.postMessage('collect'); }).finally(() => { collection = null; });
    }
    return collection;
  }
  function flush() {
    if (flushing) return flushing;
    flushing = (async () => {
      try {
        while (!controller.signal.aborted) {
          const batch = queue.peek();
          if (!batch) { deliveryFailed = false; return true; }
          await post('/api/ingest', batch.payload);
          queue.acknowledge(batch.id);
        }
        return false;
      } catch { deliveryFailed = true; return false; }
    })().finally(() => { flushing = null; });
    return flushing;
  }
  function heartbeat() {
    if (beating) return beating;
    beating = (async () => {
      const status = source.status === 'unavailable' ? 'unavailable' : source.status === 'degraded' || deliveryFailed || heartbeatFailed ? 'degraded' : 'ready';
      try {
        await post('/api/heartbeat', { botId: config.bridges[0].id, botName: sanitize.text(config.bridges[0].name), status,
          pendingBatches: queue.count(), ...(source.error ? { error: source.error } : deliveryFailed || heartbeatFailed ? { error: 'Reporting endpoint is unavailable; saved batches will be retried.' } : {}) });
        heartbeatFailed = false;
        return true;
      } catch { heartbeatFailed = true; return false; }
    })().finally(() => { beating = null; });
    return beating;
  }
  async function loop(action, interval) {
    while (!controller.signal.aborted) {
      await action();
      try { await delay(interval, undefined, { signal: controller.signal }); } catch { break; }
    }
  }
  async function runOnce() {
    const pulses = loop(heartbeat, config.heartbeatInterval);
    try {
      await collect();
      const delivered = await flush();
      // Do not reuse an initial in-flight heartbeat with stale collection status.
      if (beating) await beating;
      const reported = await heartbeat();
      return delivered && reported && queue.count() === 0 && source.status === 'ready';
    } finally { stop(); await pulses; }
  }
  async function run() {
    await Promise.all([
      loop(async () => { await collect(); if (workerFailed) { await heartbeat(); stop(); } }, config.collectInterval),
      loop(flush, config.collectInterval),
      loop(heartbeat, config.heartbeatInterval),
    ]);
    return !workerFailed;
  }
  function close() {
    if (closing) return closing;
    closed = true;
    stop();
    worker.postMessage('close');
    closing = (async () => {
      // A synchronous adapter import may be very large. Terminating after a short
      // grace period rolls back SQLite's active transaction rather than hanging.
      const timer = setTimeout(() => { void worker.terminate(); }, 1000);
      try { await exited; } finally { clearTimeout(timer); }
      await Promise.allSettled([flushing, beating, collection]);
      queue.close();
      release();
    })();
    return closing;
  }
  try { await ready; } catch (error) { await close(); throw error; }
  return { collect, flush, heartbeat, runOnce, run, stop, close, queue, get source() { return source; } };
}

export async function main(args = process.argv.slice(2), env = process.env) {
  if (args.includes('--help')) {
    console.log('Usage: node collector/index.mjs [--once]\nRequired: TW_SERVER_URL, TW_BRIDGES_FILE, TW_INGEST_TOKEN or TW_INGEST_TOKEN_FILE\nOptional: TW_COLLECTOR_DATA_DIR, TW_COLLECT_INTERVAL_MS (2000), TW_HEARTBEAT_INTERVAL_MS (10000)');
    return 0;
  }
  if (args.some(arg => arg !== '--once') || args.length > 1) throw new Error('Unsupported collector arguments');
  const collector = await createCollector(readCollectorConfig(env));
  const stop = () => collector.stop();
  process.on('SIGINT', stop);
  process.on('SIGTERM', stop);
  try { return (await (args.includes('--once') ? collector.runOnce() : collector.run())) ? 0 : 1; }
  finally {
    process.off('SIGINT', stop);
    process.off('SIGTERM', stop);
    await collector.close();
  }
}
if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main().then(code => { process.exitCode = code; }).catch(() => {
    console.error('Collector failed; check configuration, state-directory ownership, and source availability.');
    process.exitCode = 1;
  });
}
