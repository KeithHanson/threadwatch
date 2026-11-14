import { createHash, randomUUID } from 'node:crypto';

export const MAX_BATCH_BYTES = 1024 * 1024;
export const MAX_BATCH_EVENTS = 1000;
const KINDS = new Set(['message', 'tool_call', 'tool_result', 'status', 'todo_edit', 'session_exit', 'tool_start', 'import_error']);
const ROLES = new Set(['user', 'assistant', 'toolResult']);
export const digest = value => createHash('sha256').update(value).digest('hex');
export const byteLength = value => Buffer.byteLength(JSON.stringify(value));
export function normalizeEvent(event, sanitize) {
  if (!KINDS.has(event.kind)) return null;
  const timestamp = typeof event.timestamp === 'string' ? Date.parse(event.timestamp) : event.timestamp;
  const text = sanitize.text(event.text);
  if (!text) return null;
  return { id: sanitize.text(event.id), timestamp: Number.isSafeInteger(timestamp) && timestamp >= 0 ? timestamp : 0,
    kind: event.kind, ...(ROLES.has(event.role) ? { role: event.role } : {}),
    ...(typeof event.name === 'string' && event.name ? { name: sanitize.text(event.name) } : {}), text,
    ...(event.isError === true ? { isError: true } : {}) };
}
export function envelope(thread, todos, events = [], reset = false, id = randomUUID()) {
  return { thread, todos, events, delivery: { id, reset } };
}
// Continuations retain the exact sanitized text, split only at Unicode code-point boundaries.
export function splitEvent(event, availableBytes) {
  if (byteLength(event) <= availableBytes) return [event];
  const key = digest(event.id);
  const reserve = `[Continuation ${'9'.repeat(20)} of ${'9'.repeat(20)}; original event ${event.id}]\n`;
  const template = { ...event, id: `continuation:${key}:${'9'.repeat(20)}`, text: reserve };
  if (byteLength(template) + 8 > availableBytes) throw new Error('Event metadata exceeds delivery capacity');
  const chunks = [];
  let offset = 0;
  while (offset < event.text.length) {
    let low = 1;
    let high = event.text.length - offset;
    let count = 0;
    while (low <= high) {
      const middle = Math.floor((low + high) / 2);
      if (byteLength({ ...template, text: reserve + event.text.slice(offset, offset + middle) }) <= availableBytes) { count = middle; low = middle + 1; }
      else high = middle - 1;
    }
    const last = event.text.charCodeAt(offset + count - 1);
    const next = event.text.charCodeAt(offset + count);
    if (last >= 0xd800 && last <= 0xdbff && next >= 0xdc00 && next <= 0xdfff) count--;
    if (!count) throw new Error('No room for event text');
    chunks.push(event.text.slice(offset, offset + count));
    offset += count;
  }
  return chunks.map((text, index) => ({ ...event, id: `continuation:${key}:${index + 1}`, text: `[Continuation ${index + 1} of ${chunks.length}; original event ${event.id}]\n${text}` }));
}
export function eventCapacity(thread, todos, maxBytes = MAX_BATCH_BYTES) {
  const bytes = maxBytes - byteLength(envelope(thread, todos, [], false, '0'.repeat(36)));
  if (bytes < 256) throw new Error('Thread metadata or todos exceed delivery capacity');
  return bytes;
}
export function* batches(thread, todos, events, reset = false, { maxBytes = MAX_BATCH_BYTES, maxEvents = MAX_BATCH_EVENTS } = {}) {
  const base = byteLength(envelope(thread, todos, [], false, '0'.repeat(36)));
  eventCapacity(thread, todos, maxBytes);
  let batch = [];
  let size = base;
  let first = true;
  for (const event of events) {
    const bytes = byteLength(event);
    if (base + bytes > maxBytes) throw new Error('Unsplit event exceeds delivery capacity');
    if (batch.length && (batch.length === maxEvents || size + bytes + 1 > maxBytes)) {
      yield envelope(thread, todos, batch, first && reset);
      first = false; batch = []; size = base;
    }
    size += bytes + (batch.length ? 1 : 0);
    batch.push(event);
  }
  if (batch.length || first) yield envelope(thread, todos, batch, first && reset);
}
