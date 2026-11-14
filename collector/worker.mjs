import { parentPort, workerData } from 'node:worker_threads';
import { BridgeReader } from '../server/bridge.mjs';
import { createSanitizer } from '../server/sanitize.mjs';
import { CollectorStore, DeliveryQueue, captureChanges } from './state.mjs';

const { dataDir, bridges, token } = workerData;
const sanitize = createSanitizer([token]);
const store = new CollectorStore(dataDir, sanitize);
const queue = new DeliveryQueue(dataDir, bridges[0]);
const reader = new BridgeReader(store, bridges, sanitize);
let closed = false;
function close() {
  if (closed) return;
  closed = true;
  reader.close();
  store.close();
  queue.close();
  parentPort.close();
}
parentPort.on('message', message => {
  if (message === 'close') { close(); return; }
  if (message !== 'collect' || closed) return;
  try {
    // Recover cache commits made before a prior interrupted capture before the
    // adapter can replace them with a newer source snapshot.
    captureChanges(store, queue, bridges[0], sanitize);
    const sources = reader.refresh();
    captureChanges(store, queue, bridges[0], sanitize);
    const source = sources[0];
    parentPort.postMessage({ type: 'collected', status: source?.status ?? 'unavailable',
      ...(source?.error ? { error: source.status === 'unavailable' ? 'Bridge database is unavailable or incompatible.' : 'One or more saved transcripts are unavailable or malformed.' } : {}) });
  } catch {
    parentPort.postMessage({ type: 'collected', status: 'degraded', error: 'Collector could not capture all saved updates; they will be retried.' });
  }
});
parentPort.postMessage({ type: 'ready' });
