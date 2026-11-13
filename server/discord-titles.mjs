import { readFileSync } from 'node:fs';

// Only channel metadata is requested. Tokens stay transient, never enter SQLite,
// and failures leave the explicitly labelled saved/user-message title in place.
export class DiscordTitles {
  constructor(store, sanitize, { fetch = globalThis.fetch, now = Date.now } = {}) {
    this.store = store;
    this.sanitize = sanitize;
    this.fetch = fetch;
    this.now = now;
    this.queue = new Map();
    this.nextAttempt = new Map();
    this.pausedUntil = 0;
    this.pendingTask = null;
    this.closed = false;
    this.controller = new AbortController();
  }
  enqueue(bridge, thread) {
    if (this.closed || !bridge.botTokenFile || !/^\d+$/.test(thread.channelId) || this.now() < (this.nextAttempt.get(thread.id) || 0)) return;
    this.queue.set(thread.id, { bridge, thread });
    if (!this.pendingTask && this.now() >= this.pausedUntil) {
      this.pendingTask = this.drain().finally(() => { this.pendingTask = null; });
    }
  }
  async drain() {
    while (!this.closed && this.queue.size && this.now() >= this.pausedUntil) {
      const [id, { bridge, thread }] = this.queue.entries().next().value;
      this.queue.delete(id);
      this.nextAttempt.set(id, this.now() + 60_000);
      try {
        const token = readFileSync(bridge.botTokenFile, 'utf8').trim().replace(/^Bot\s+/i, '');
        if (!/^[A-Za-z0-9._-]{30,4096}$/.test(token)) continue;
        const response = await this.fetch(`https://discord.com/api/v10/channels/${thread.channelId}`, { headers: { Authorization: `Bot ${token}` }, signal: AbortSignal.any([this.controller.signal, AbortSignal.timeout(10_000)]) });
        if (this.closed) return;
        if (response.status === 429) {
          let retry;
          try { retry = (await response.json()).retry_after; } catch { retry = null; }
          const seconds = Number(retry ?? response.headers?.get('retry-after') ?? 1);
          this.pausedUntil = this.now() + (Number.isFinite(seconds) && seconds > 0 ? Math.ceil(seconds * 1000) : 1000);
          this.nextAttempt.set(id, this.pausedUntil);
          this.queue.set(id, { bridge, thread });
          return;
        }
        if (!response.ok) continue;
        const channel = await response.json();
        if (this.closed || channel.id !== thread.channelId || (thread.guildId && channel.guild_id !== thread.guildId) || typeof channel.name !== 'string') continue;
        const title = this.sanitize.text(channel.name.split(token).join('[REDACTED]')).trim();
        if (!title) continue;
        const current = this.store.db.prepare('SELECT payload FROM threads WHERE id=?').get(id);
        if (!current) continue;
        this.store.upsert({ ...JSON.parse(current.payload), title, titleSource: 'discord' });
        this.nextAttempt.set(id, this.now() + 5 * 60_000);
      } catch { /* Saved/fallback titles remain available; retry on a later poll. */ }
    }
  }
  close() { this.closed = true; this.queue.clear(); this.controller.abort(); }
}
