import React, { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react';
import { createRoot } from 'react-dom/client';
import './styles.css';

const POLL_MS = 2000;
const ACTIVE = new Set(['working', 'queued', 'waiting']);
const STATUS_LABELS = { working: 'Working', queued: 'Queued', waiting: 'Waiting', completed: 'Completed', failed: 'Failed', unknown: 'Unknown', stopped: 'Stopped' };

function Icon({ name, size = 18, ...props }) {
  const paths = {
    activity: <><path d="M3 12h4l3-8 4 16 3-8h4" /></>,
    search: <><circle cx="10.5" cy="10.5" r="6.5" /><path d="m16 16 4 4" /></>,
    chevron: <path d="m9 5 7 7-7 7" />,
    external: <><path d="M14 3h7v7m0-7L10 14" /><path d="M10 3H5a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2v-5" /></>,
    pause: <><path d="M8 5v14M16 5v14" /></>,
    play: <path d="m8 4 12 8-12 8Z" />,
    check: <path d="m5 12 4 4L19 6" />,
    clock: <><circle cx="12" cy="12" r="9" /><path d="M12 7v5l3 2" /></>,
    code: <><path d="m8 7-5 5 5 5m8-10 5 5-5 5m-3-14-2 18" /></>,
    message: <path d="M21 11a8 8 0 0 1-8 8H7l-5 3 2-6a8 8 0 0 1-1-5 8 8 0 0 1 8-8h2a8 8 0 0 1 8 8Z" />,
    alert: <><path d="m12 3 10 18H2L12 3Z" /><path d="M12 9v5m0 3v.1" /></>,
    arrow: <><path d="M12 4v16m-6-6 6 6 6-6" /></>,
    close: <path d="m6 6 12 12M6 18 18 6" />,
    menu: <path d="M4 6h16M4 12h16M4 18h16" />,
    bot: <><rect x="3" y="6" width="18" height="15" rx="5" /><path d="M12 2v4M8 12v2m8-2v2m-7 3h6" /></>,
  };
  return <svg width={size} height={size} viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true" {...props}>{paths[name] || paths.activity}</svg>;
}

class ApiError extends Error {
  constructor(message, status) { super(message); this.status = status; }
}

async function api(path, { token, signal, body } = {}) {
  const response = await fetch(path, {
    signal,
    method: body ? 'POST' : 'GET',
    headers: { ...(body ? { 'Content-Type': 'application/json' } : {}), ...(token ? { Authorization: `Bearer ${token}` } : {}) },
    ...(body ? { body: JSON.stringify(body) } : {}),
    cache: 'no-store',
  });
  if (!response.ok) {
    // Do not display response bodies: an upstream error can contain credentials or provider metadata.
    const message = response.status === 401 || response.status === 403
      ? 'Your session is unavailable or has expired. Reconnect to continue.'
      : response.status === 404 ? 'This thread is no longer available.'
        : `The server could not complete the request (${response.status}).`;
    throw new ApiError(message, response.status);
  }
  return response.json();
}

function useSession(attempt) {
  const [session, setSession] = useState({ state: 'loading', step: 'Connecting to Threadwatch…' });
  const sdkRef = useRef(null);
  useEffect(() => () => sdkRef.current?.close(1000, 'Activity session closed'), []);
  useEffect(() => {
    const controller = new AbortController();
    let disposed = false;
    const update = value => { if (!disposed) setSession(value); };
    update({ state: 'loading', step: 'Connecting to Threadwatch…' });
    (async () => {
      try {
        const config = await api('/api/config', { signal: controller.signal });
        if (disposed) return;
        if (config.local) { update({ state: 'ready', token: null, local: true }); return; }
        if (!config.clientId) throw new Error('Discord authentication is not configured. Ask the operator to configure the application client ID.');
        if (!new URLSearchParams(window.location.search).get('frame_id')) {
          update({ state: 'error', error: 'Open Threadwatch as an Activity inside Discord to sign in. This server is not configured for standalone local access.' });
          return;
        }
        update({ state: 'loading', step: 'Connecting to Discord…' });
        const { DiscordSDK } = await import('@discord/embedded-app-sdk');
        if (disposed) return;
        const sdk = sdkRef.current || new DiscordSDK(config.clientId);
        sdkRef.current = sdk;
        await sdk.ready();
        if (disposed) return;
        update({ state: 'loading', step: 'Authorizing your Discord session…' });
        const { code } = await sdk.commands.authorize({ client_id: config.clientId, response_type: 'code', state: '', prompt: 'none', scope: ['identify'] });
        if (disposed) return;
        const auth = await api('/api/auth', { signal: controller.signal, body: { code } });
        if (!auth.access_token || !auth.sessionToken) throw new Error('The server returned an incomplete authentication response.');
        await sdk.commands.authenticate({ access_token: auth.access_token });
        update({ state: 'ready', token: auth.sessionToken, local: false });
      } catch (error) {
        if (disposed || error.name === 'AbortError') return;
        // SDK errors may include OAuth details, so only our known errors are presented verbatim.
        update({ state: 'error', error: error instanceof ApiError || error.message?.startsWith('Discord authentication is not configured') || error.message?.startsWith('The server returned an incomplete')
          ? error.message : 'Could not connect to Discord. Open this app as a Discord Activity, or use the server’s local mode on this computer.' });
      }
    })();
    return () => { disposed = true; controller.abort(); };
  }, [attempt]);
  return session;
}

function useThreadIndex(session) {
  const [state, setState] = useState({ threads: [], sources: [], checkedAt: null, error: null, loading: true });
  useEffect(() => {
    if (session.state !== 'ready') return;
    let active = true;
    let pending = false;
    const controller = new AbortController();
    const poll = async () => {
      if (pending) return;
      pending = true;
      try {
        const result = await api('/api/threads', { token: session.token, signal: controller.signal });
        if (active) setState({ ...result, checkedAt: Date.now(), error: null, loading: false });
      } catch (error) {
        if (active && error.name !== 'AbortError') setState(previous => ({ ...previous, error, loading: false }));
      } finally { pending = false; }
    };
    poll();
    const timer = setInterval(poll, POLL_MS);
    return () => { active = false; controller.abort(); clearInterval(timer); };
  }, [session.state, session.token]);
  return state;
}

function mergeEvents(previous, incoming) {
  const merged = new Map(previous.map(event => [event.id, event]));
  incoming.forEach(event => merged.set(event.id, event));
  return [...merged.values()].sort((a, b) => {
    const delta = new Date(a.timestamp).getTime() - new Date(b.timestamp).getTime();
    return Number.isFinite(delta) ? delta : 0;
  });
}

function useThread(id, session, paused, cache) {
  const [state, setState] = useState(() => cache.current.get(id) || { events: [], loading: true });
  const requestVersion = useRef(0);
  const olderController = useRef(null);
  useEffect(() => {
    const version = ++requestVersion.current;
    olderController.current?.abort();
    const cached = cache.current.get(id);
    setState(cached ? { ...cached, olderLoading: false } : { events: [], loading: true });
    if (!id || session.state !== 'ready' || paused) {
      return () => { ++requestVersion.current; olderController.current?.abort(); };
    }
    let pending = false;
    const controller = new AbortController();
    const poll = async () => {
      if (pending) return;
      pending = true;
      try {
        const knownIds = new Set((cache.current.get(id)?.events || []).map(event => event.id));
        const result = await api(`/api/threads/${encodeURIComponent(id)}?limit=100`, { token: session.token, signal: controller.signal });
        if (requestVersion.current !== version || controller.signal.aborted) return;
        const pages = [result.events];
        let page = result;
        // A busy thread may advance by several pages while paused or between polls.
        // Commit only after the new records connect to the history already displayed.
        while (knownIds.size && page.nextCursor && !page.events.some(event => knownIds.has(event.id))) {
          page = await api(`/api/threads/${encodeURIComponent(id)}?before=${encodeURIComponent(page.nextCursor)}&limit=100`, { token: session.token, signal: controller.signal });
          if (requestVersion.current !== version || controller.signal.aborted) return;
          pages.push(page.events);
        }
        const incoming = pages.reverse().flat();
        setState(previous => {
          if (requestVersion.current !== version || controller.signal.aborted) return previous;
          const next = { ...result, events: mergeEvents(previous.events || [], incoming),
            nextCursor: previous.events?.length ? previous.nextCursor : result.nextCursor,
            olderLoading: previous.olderLoading || false,
            olderError: previous.olderError, loading: false, error: null, checkedAt: Date.now(), id };
          cache.current.set(id, next);
          return next;
        });
      } catch (error) {
        if (requestVersion.current === version && !controller.signal.aborted) setState(previous => ({ ...previous, error, loading: false }));
      } finally { pending = false; }
    };
    poll();
    const timer = setInterval(poll, POLL_MS);
    return () => { ++requestVersion.current; controller.abort(); olderController.current?.abort(); clearInterval(timer); };
  }, [id, session.state, session.token, paused, cache]);

  const loadOlder = useCallback(async () => {
    if (!id || !state.nextCursor || state.olderLoading) return;
    const version = requestVersion.current;
    const controller = new AbortController();
    olderController.current = controller;
    setState(previous => ({ ...previous, olderLoading: true, olderError: null }));
    try {
      const result = await api(`/api/threads/${encodeURIComponent(id)}?before=${encodeURIComponent(state.nextCursor)}&limit=100`, { token: session.token, signal: controller.signal });
      if (requestVersion.current !== version || controller.signal.aborted) return;
      setState(previous => {
        if (requestVersion.current !== version || controller.signal.aborted) return previous;
        const next = { ...previous, events: mergeEvents(result.events, previous.events), nextCursor: result.nextCursor, olderLoading: false };
        cache.current.set(id, next);
        return next;
      });
    } catch (error) {
      if (requestVersion.current === version && !controller.signal.aborted) setState(previous => ({ ...previous, olderLoading: false, olderError: error }));
    }
  }, [id, state.nextCursor, state.olderLoading, session.token, cache]);
  return { ...state, loadOlder };
}

function relativeTime(value, now = Date.now()) {
  const timestamp = new Date(value).getTime();
  if (!Number.isFinite(timestamp)) return 'unknown';
  const seconds = Math.max(0, Math.floor((now - timestamp) / 1000));
  if (seconds < 5) return 'just now';
  if (seconds < 60) return `${seconds}s ago`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m ago`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)}h ago`;
  return `${Math.floor(seconds / 86400)}d ago`;
}

function timeLabel(value) {
  const date = new Date(value);
  return Number.isFinite(date.getTime()) ? date.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' }) : 'Unknown time';
}

function Status({ status = 'unknown' }) {
  return <span className={`status status-${status}`}><span className="status-dot" />{STATUS_LABELS[status] || 'Unknown'}</span>;
}

function Empty({ icon = 'message', title, children }) {
  return <div className="empty-state"><span className="empty-icon"><Icon name={icon} size={27} /></span><h2>{title}</h2><div>{children}</div></div>;
}

function ErrorNotice({ error, reconnect, compact = false }) {
  if (!error) return null;
  const authError = error.status === 401 || error.status === 403;
  return <div className={`error-notice ${compact ? 'compact' : ''}`} role="alert"><Icon name="alert" size={17} /><span>{error.message || 'Connection lost. Retrying every 2 seconds.'}</span>{authError && <button onClick={reconnect}>Reconnect</button>}</div>;
}

function TranscriptEvent({ event, expanded, toggle }) {
  const kind = String(event.kind || 'message');
  const tool = /tool|command|exec|result/i.test(kind) || event.role === 'tool';
  const isError = event.isError || /error|failed/i.test(kind);
  const label = tool && event.name ? `${event.name} · ${kind.replaceAll('_', ' ')}` : event.name || (tool ? kind.replaceAll('_', ' ') : event.role || kind.replaceAll('_', ' '));
  const text = typeof event.text === 'string' ? event.text : '';
  const summary = text.split('\n').find(line => line.trim()) || 'No text recorded';
  return <article className={`event ${tool ? 'event-tool' : ''} ${isError ? 'event-error' : ''}`}>
    <div className={`event-icon ${isError ? 'error' : ''}`}><Icon name={isError ? 'alert' : tool ? 'code' : event.role === 'assistant' ? 'bot' : 'message'} size={16} /></div>
    <div className="event-content">
      {tool ? <>
        <button className="tool-toggle" aria-expanded={expanded} onClick={toggle}>
          <span className="event-label">{label}{isError && <span className="error-tag">Error</span>}</span>
          <time title={event.timestamp}>{timeLabel(event.timestamp)}</time>
          <Icon name="chevron" size={14} className={expanded ? 'rotated' : ''} />
        </button>
        {expanded ? <pre className="tool-output">{text || 'No text recorded.'}</pre> : <button className="tool-preview" onClick={toggle} aria-label={`Expand ${label}`}>{summary}</button>}
      </> : <>
        <div className="event-heading"><span className="event-label">{label}{isError && <span className="error-tag">Error</span>}</span><time title={event.timestamp}>{timeLabel(event.timestamp)}</time></div>
        <div className="message-text">{text || <span className="muted">No text recorded.</span>}</div>
      </>}
    </div>
  </article>;
}

function ThreadDetail({ thread: indexThread, session, cache, views, now, sources, reconnect }) {
  const id = indexThread.id;
  const storedView = views.current.get(id);
  const [paused, setPaused] = useState(storedView?.paused || false);
  const [search, setSearch] = useState(storedView?.search || '');
  const [expanded, setExpanded] = useState(() => storedView?.expanded || new Map());
  const [following, setFollowing] = useState(storedView?.following ?? true);
  const data = useThread(id, session, paused, cache);
  const thread = data.thread || indexThread;
  const scrollRef = useRef(null);
  const followRef = useRef(following);
  const previousScrollHeight = useRef(0);
  const loadingOlder = useRef(false);
  const restored = useRef(false);
  const currentView = useRef(null);
  currentView.current = { paused, search, expanded, following, top: currentView.current?.top ?? storedView?.top ?? 0 };
  useEffect(() => () => { views.current.set(id, currentView.current); }, [id, views]);

  const sourceId = typeof thread.source === 'object' ? thread.source?.id : thread.source;
  const source = sources.find(item => item.id === sourceId) || (typeof thread.source === 'object' ? thread.source : null);
  const sourceStatus = source?.status || 'unknown';
  const sourceProblem = /disconnect|offline|error|unavailable|stale|fail|degraded/i.test(sourceStatus);
  const stale = data.checkedAt && now - data.checkedAt > 15000;
  const query = search.trim().toLowerCase();
  const events = (data.events || []).filter(event => !query || `${event.text || ''} ${event.name || ''} ${event.role || ''} ${event.kind || ''}`.toLowerCase().includes(query));
  const todos = data.todos || [];
  const completed = todos.filter(todo => todo.status === 'completed').length;
  const discordUrl = /^\d+$/.test(String(thread.guildId)) && /^\d+$/.test(String(thread.channelId))
    ? `https://discord.com/channels/${thread.guildId}/${thread.channelId}` : null;

  useLayoutEffect(() => {
    const element = scrollRef.current;
    if (!element || data.loading) return;
    if (!restored.current) {
      element.scrollTop = storedView && !storedView.following ? storedView.top : element.scrollHeight;
      restored.current = true;
    } else if (loadingOlder.current && !data.olderLoading) {
      element.scrollTop += element.scrollHeight - previousScrollHeight.current;
      loadingOlder.current = false;
    } else if (followRef.current && !paused && !query) {
      element.scrollTop = element.scrollHeight;
    }
  }, [data.events, data.loading, data.olderLoading, query, paused]);

  const onScroll = () => {
    const element = scrollRef.current;
    currentView.current.top = element.scrollTop;
    if (loadingOlder.current) return;
    const bottom = element.scrollHeight - element.scrollTop - element.clientHeight < 60;
    followRef.current = bottom;
    setFollowing(bottom);
  };
  const loadOlder = () => {
    previousScrollHeight.current = scrollRef.current?.scrollHeight || 0;
    loadingOlder.current = true;
    followRef.current = false;
    setFollowing(false);
    data.loadOlder();
  };
  const jumpToLatest = () => {
    followRef.current = true;
    setFollowing(true);
    if (scrollRef.current) scrollRef.current.scrollTop = scrollRef.current.scrollHeight;
  };
  const toggleEvent = event => setExpanded(previous => {
    const next = new Map(previous);
    next.set(event.id, !(previous.get(event.id) ?? Boolean(event.isError || /error|failed/i.test(event.kind))));
    return next;
  });

  return <>
    <header className="thread-header">
      <div className="breadcrumbs"><Icon name="bot" size={14} /><span>{thread.botName}</span><Icon name="chevron" size={12} /><span>Thread</span></div>
      <div className="thread-title-row"><h1>{thread.title || 'Untitled thread'}</h1>{discordUrl && <a className="icon-button discord-link" href={discordUrl} target="_blank" rel="noreferrer" aria-label="Open thread in Discord" title="Open in Discord"><Icon name="external" size={17} /></a>}</div>
      {thread.titleSource && thread.titleSource !== 'discord' && <span className="source-label">{thread.titleSource === 'session' ? 'Saved session title' : thread.titleSource === 'user_message' ? 'Label from first message · Discord title unavailable' : 'Thread identifier · Discord title unavailable'}</span>}
      <div className="thread-metadata"><Status status={thread.status} /><span className="metadata-divider" /><span>Updated {relativeTime(thread.updatedAt, now)}</span><span className="source-label" title={`Source: ${source?.name || sourceId || 'Unknown'}`}><span className={`connection-dot ${sourceProblem ? 'offline' : sourceStatus === 'unknown' ? 'unknown' : ''}`} />{source?.name || sourceId || 'Unknown source'} · {sourceStatus}</span></div>
    </header>
    <div className="detail-content">
      <section className="todo-panel" aria-labelledby="todo-heading">
        <div className="section-heading"><h2 id="todo-heading"><Icon name="check" size={16} />Task list</h2><span className="count-label">{completed} / {todos.length}</span></div>
        {todos.length > 0 ? <><div className="todo-progress" role="progressbar" aria-label="Completed tasks" aria-valuemin={0} aria-valuemax={todos.length} aria-valuenow={completed}><span style={{ width: `${completed / todos.length * 100}%` }} /></div><ul className="todo-list">{todos.map((todo, index) => <li key={`${index}:${todo.text}`} className={`todo todo-${todo.status}`}><span className="todo-mark" aria-label={todo.status === 'in_progress' ? 'In progress' : todo.status === 'completed' ? 'Completed' : 'Pending'}>{todo.status === 'completed' ? <Icon name="check" size={12} /> : todo.status === 'in_progress' ? <span /> : null}</span><span>{todo.text}</span>{todo.status === 'in_progress' && <span className="todo-working">In progress</span>}</li>)}</ul></> : <p className="todo-empty">{data.loading ? 'Loading recorded tasks…' : 'No task list recorded for this thread.'}</p>}
      </section>
      <section className="transcript-panel" aria-labelledby="transcript-heading">
        <div className="transcript-toolbar"><div className="section-heading"><h2 id="transcript-heading"><Icon name="message" size={16} />Transcript</h2><span className="count-label">{data.events?.length || 0} records</span></div><button className={`pause-button ${paused ? 'is-paused' : ''}`} onClick={() => setPaused(value => !value)} aria-pressed={paused} title={paused ? 'Resume polling for new records' : 'Freeze transcript updates'}><Icon name={paused ? 'play' : 'pause'} size={13} />{paused ? 'Resume' : 'Pause'}</button></div>
        <div className="transcript-subbar"><div className="search-field transcript-search"><Icon name="search" size={15} /><input aria-label="Search loaded transcript" placeholder="Search loaded transcript…" value={search} onChange={event => setSearch(event.target.value)} />{search && <button className="clear-search" aria-label="Clear transcript search" onClick={() => setSearch('')}><Icon name="close" size={13} /></button>}</div><span className={`freshness ${paused || stale || data.error || sourceProblem ? 'freshness-warning' : ''}`} title={data.checkedAt ? `Last successful read: ${new Date(data.checkedAt).toLocaleString()}` : 'Waiting for the first successful read'}><span className="connection-dot" />{paused ? 'Paused · updates frozen' : data.error ? 'Connection lost · retrying' : stale ? 'Stale · retrying' : sourceProblem ? `Source ${sourceStatus}` : 'Polling every 2s'}</span></div>
        <ErrorNotice error={data.error} reconnect={reconnect} />
        {sourceProblem && <div className="source-notice"><Icon name="alert" size={14} />Source is {sourceStatus}. Showing the last available records.</div>}
        <div className="transcript-scroll" ref={scrollRef} onScroll={onScroll} tabIndex={0} aria-label="Thread transcript" aria-busy={data.loading || data.olderLoading}>
          {data.loading ? <Empty icon="clock" title="Loading transcript"><p>Reading this thread’s recorded activity.</p></Empty> : <>
            {data.nextCursor && <div className="older-row"><button className="secondary-button" disabled={data.olderLoading} onClick={loadOlder}><Icon name="clock" size={13} />{data.olderLoading ? 'Loading…' : 'Load older records'}</button></div>}
            <ErrorNotice error={data.olderError} reconnect={reconnect} compact />
            {!data.nextCursor && data.events?.length > 0 && <div className="transcript-start"><span />Beginning of recorded activity<span /></div>}
            {query && <div className="search-count">{events.length} matching {events.length === 1 ? 'record' : 'records'} in loaded history</div>}
            {events.length ? events.map(event => <TranscriptEvent key={event.id} event={event} expanded={expanded.get(event.id) ?? Boolean(event.isError || /error|failed/i.test(event.kind))} toggle={() => toggleEvent(event)} />) : <Empty icon="message" title={query ? 'No matching records' : data.error ? 'Transcript unavailable' : 'Nothing recorded yet'}><p>{query ? 'Try another search, or load older records.' : data.error ? 'The server will be checked again automatically.' : 'Messages and tool activity will appear here when the source records them.'}</p></Empty>}
            {events.length > 0 && <div className="transcript-end"><span className="connection-dot" />{paused ? 'Updates paused' : 'End of loaded records'}</div>}
          </>}
        </div>
        {!following && events.length > 0 && !query && <button className="jump-latest" onClick={jumpToLatest}><Icon name="arrow" size={13} />Jump to latest</button>}
        <footer className="transcript-footer"><span>Read-only · recorded messages & tool activity</span><span>{data.checkedAt ? `Checked ${relativeTime(data.checkedAt, now)}` : 'Not yet checked'}</span></footer>
      </section>
    </div>
  </>;
}

function Dashboard({ session, reconnect }) {
  const index = useThreadIndex(session);
  const [selected, setSelected] = useState(null);
  const [query, setQuery] = useState('');
  const [filter, setFilter] = useState('all');
  const [sidebarOpen, setSidebarOpen] = useState(false);
  const [now, setNow] = useState(Date.now());
  const cache = useRef(new Map());
  const views = useRef(new Map());
  const selectedThread = useRef(null);
  useEffect(() => { const timer = setInterval(() => setNow(Date.now()), 1000); return () => clearInterval(timer); }, []);
  useEffect(() => { if (!selected && index.threads.length) setSelected(index.threads[0].id); }, [index.threads, selected]);
  useEffect(() => {
    if (!sidebarOpen) return;
    const close = event => { if (event.key === 'Escape') setSidebarOpen(false); };
    window.addEventListener('keydown', close);
    return () => window.removeEventListener('keydown', close);
  }, [sidebarOpen]);
  const currentThread = index.threads.find(thread => thread.id === selected);
  if (currentThread) selectedThread.current = currentThread;
  const thread = currentThread || (selectedThread.current?.id === selected ? selectedThread.current : null);
  const activeCount = index.threads.filter(thread => ACTIVE.has(thread.status)).length;
  const filtered = index.threads.filter(thread => (filter !== 'active' || ACTIVE.has(thread.status)) && `${thread.title} ${thread.botName}`.toLowerCase().includes(query.toLowerCase().trim()));
  const groups = new Map();
  for (const item of filtered) {
    if (!groups.has(item.botId)) groups.set(item.botId, { name: item.botName, threads: [] });
    groups.get(item.botId).threads.push(item);
  }
  const stale = index.checkedAt && now - index.checkedAt > 15000;
  const disconnectedSources = index.sources.filter(source => /disconnect|offline|error|unavailable|stale|fail|degraded/i.test(source.status));
  return <div className="app-shell">
    <a className="skip-link" href="#main-content">Skip to thread</a>
    <header className="mobile-header"><button className="icon-button" onClick={() => setSidebarOpen(value => !value)} aria-label="Toggle thread list" aria-expanded={sidebarOpen} aria-controls="thread-sidebar"><Icon name="menu" /></button><span>Threadwatch</span><Status status={thread?.status} /></header>
    {sidebarOpen && <button className="sidebar-backdrop" aria-label="Close thread list" onClick={() => setSidebarOpen(false)} />}
    <aside id="thread-sidebar" className={`sidebar ${sidebarOpen ? 'sidebar-open' : ''}`} aria-label="Threads">
      <div className="brand"><span className="brand-icon"><Icon name="activity" size={23} /></span><div><span className="brand-name">threadwatch<span className="brand-period">.</span></span><span className="brand-caption">YOUR AGENTS, IN FOCUS</span></div></div>
      <div className="sidebar-tools"><div className="search-field"><Icon name="search" size={16} /><input aria-label="Search threads and bots" value={query} onChange={event => setQuery(event.target.value)} placeholder="Find a thread…" />{query && <button className="clear-search" aria-label="Clear thread search" onClick={() => setQuery('')}><Icon name="close" size={13} /></button>}</div><div className="filter-tabs" aria-label="Thread filter"><button aria-pressed={filter === 'all'} onClick={() => setFilter('all')}>All threads<span>{index.threads.length}</span></button><button aria-pressed={filter === 'active'} onClick={() => setFilter('active')}>Active<span>{activeCount}</span></button></div></div>
      <ErrorNotice error={index.error} reconnect={reconnect} compact />
      <nav className="thread-list" aria-label="Bot threads">
        {index.loading ? <div className="sidebar-empty">Loading threads…</div> : groups.size ? [...groups].map(([botId, group]) => <section className="bot-group" key={botId}><h2><span className="bot-avatar"><Icon name="bot" size={14} /></span><span>{group.name}</span><span className="bot-count">{group.threads.length}</span></h2><div className="bot-threads">{group.threads.map(item => <button key={item.id} className={`thread-item ${item.id === selected ? 'selected' : ''}`} aria-current={item.id === selected ? 'true' : undefined} onClick={() => { setSelected(item.id); setSidebarOpen(false); }}><span className={`thread-status-marker status-${item.status}`} /><span className="thread-item-body"><span className="thread-item-title">{item.title || 'Untitled thread'}</span><span className="thread-item-meta"><span>{STATUS_LABELS[item.status] || 'Unknown'}</span><span>{relativeTime(item.updatedAt, now)}</span></span></span></button>)}</div></section>) : <div className="sidebar-empty">{query || filter === 'active' ? 'No threads match this view.' : index.error ? 'Unable to load threads.' : 'No threads recorded yet.'}</div>}
      </nav>
      <footer className="sidebar-footer"><div className="connection-summary"><span className={`connection-dot ${index.error || stale || disconnectedSources.length ? 'offline' : !index.checkedAt ? 'unknown' : ''}`} /><span>{index.error ? 'Server disconnected' : stale ? 'Server data is stale' : disconnectedSources.length ? `${disconnectedSources.length} source${disconnectedSources.length > 1 ? 's' : ''} unavailable` : index.checkedAt ? 'Connected to server' : 'Connecting…'}</span><span className="mode-badge">{session.local ? 'LOCAL' : 'ACTIVITY'}</span></div><div className="sidebar-footer-caption">Read-only observer<span>{index.sources.length} source{index.sources.length === 1 ? '' : 's'}</span></div></footer>
    </aside>
    <main id="main-content" className="main-panel" tabIndex={-1}>{thread ? <ThreadDetail key={thread.id} thread={thread} session={session} cache={cache} views={views} now={now} sources={index.sources} reconnect={reconnect} /> : <div className="welcome-panel"><span className="eyebrow">AGENT OBSERVABILITY</span><Empty icon="activity" title={index.loading ? 'Connecting the dots' : index.error ? 'Your workspace is offline' : 'A clear view of every thread'}><p>{index.loading ? 'Loading your bots and their recorded activity.' : index.error ? 'Threadwatch is retrying the server every 2 seconds. Your threads will appear when the connection returns.' : 'Connected bot threads will appear here. Select a thread to see its actual tasks, messages, and tool activity.'}</p></Empty><div className="welcome-features"><span><Icon name="check" size={15} />Actual task lists</span><span><Icon name="code" size={15} />Recorded tool activity</span><span><Icon name="clock" size={15} />2-second polling</span></div>{!index.loading && !index.error && <p className="welcome-hint">No simulated activity. No write access. Just the record.</p>}</div>}</main>
  </div>;
}

function App() {
  const [attempt, setAttempt] = useState(0);
  const session = useSession(attempt);
  if (session.state !== 'ready') return <div className="session-screen"><div className="session-card"><div className="session-brand"><Icon name="activity" size={32} /></div><span className="eyebrow">THREADWATCH</span><h1>{session.state === 'error' ? 'Let’s reconnect' : 'Your agents, in focus.'}</h1><p role={session.state === 'error' ? 'alert' : 'status'}>{session.error || session.step}</p>{session.state === 'error' ? <button className="primary-button" onClick={() => setAttempt(value => value + 1)}>Try again</button> : <div className="loading-bar" />}<span className="session-footnote">Read-only agent activity</span></div></div>;
  return <Dashboard key={attempt} session={session} reconnect={() => setAttempt(value => value + 1)} />;
}

createRoot(document.getElementById('root')).render(<App />);
