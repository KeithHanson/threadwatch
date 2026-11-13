const HIDDEN = /^(?:thinking|reasoning|analysis|system|systemPrompt|instructions|contextSnapshot|providerPayload|providerMetadata|credential.*)$/i;
const SECRET_KEY = /(?:password|passwd|secret|token|authorization|api[_-]?key|private[_-]?key|cookie|credential)/i;
const TODO_STATUSES = new Set(['pending', 'in_progress', 'completed']);

export function createSanitizer(secretValues = []) {
  const secrets = secretValues.filter(value => typeof value === 'string' && value.length >= 8).sort((a, b) => b.length - a.length);
  function text(value) {
    let result = String(value ?? '')
      .replace(/<(system-reminder|system_instruction|system-instruction|system|thinking|analysis|contextSnapshot|providerPayload)\b[^>]*>[\s\S]*?(?:<\/\1\s*>|$)/gi, '')
      .replace(/-----BEGIN (?:[A-Z ]*PRIVATE KEY)-----[\s\S]*?-----END (?:[A-Z ]*PRIVATE KEY)-----/g, '[REDACTED]')
      .replace(/\b(?:Bearer\s+[A-Za-z0-9._~+\/-]+=*|Bot\s+[A-Za-z0-9._~+\/-]{30,}=*)/gi, '[REDACTED AUTH]')
      .replace(/\b(?:sk-[A-Za-z0-9_-]{16,}|gh[pousr]_[A-Za-z0-9_]{16,}|github_pat_[A-Za-z0-9_]{16,}|AKIA[A-Z0-9]{16}|mfa\.[A-Za-z0-9_-]{20,}|[A-Za-z0-9_-]{23,28}\.[A-Za-z0-9_-]{6}\.[A-Za-z0-9_-]{25,})\b/g, '[REDACTED]')
      .replace(/((?:[A-Za-z][A-Za-z0-9_-]*[_-])?(?:password|passwd|secret|token|authorization|api[_-]?key|private[_-]?key|cookie)\s*["']?\s*[:=]\s*)(?:"[^"\n]*"|'[^'\n]*'|[^\s,;}]+)/gi, '$1[REDACTED]')
      .replace(/(https?:\/\/)[^\s/@:]+:[^\s/@]+@/gi, '$1[REDACTED]@')
      .replace(/(https?:\/\/(?:canary\.|ptb\.)?discord(?:app)?\.com\/api\/webhooks\/\d+\/)[^\s/?#]+/gi, '$1[REDACTED]');
    for (const secret of secrets) result = result.split(secret).join('[REDACTED]');
    return result;
  }
  function value(input, depth = 0) {
    if (depth > 30) return '[OMITTED: excessive nesting]';
    if (typeof input === 'string') return text(input);
    if (input === null || typeof input === 'boolean' || typeof input === 'number') return input;
    if (Array.isArray(input)) return input.map(item => value(item, depth + 1));
    if (input && typeof input === 'object') {
      const result = Object.create(null);
      for (const [key, item] of Object.entries(input)) {
        if (HIDDEN.test(key)) continue;
        result[key] = SECRET_KEY.test(key) ? '[REDACTED]' : value(item, depth + 1);
      }
      return result;
    }
    return null;
  }
  return { text, value };
}

export function normalizeTodos(input, sanitize) {
  if (!Array.isArray(input)) throw new Error('todos must be an array');
  return input.map(todo => {
    if (!todo || typeof todo.text !== 'string' || !TODO_STATUSES.has(todo.status)) throw new Error('Invalid todo');
    return { text: sanitize.text(todo.text), status: todo.status };
  });
}

// OMP's successful todo result contains the complete checkbox list, including
// completed tasks; its summary gives the current task's explicit status.
export function deriveTodos(argumentsValue, resultText, previous = []) {
  if (argumentsValue?.op === 'drop') return [];
  const summaries = [...resultText.matchAll(/^\s*(?:[-*•]\s*)?(.+?)\s+\[(pending|in_progress|completed)\](?:\s+.*)?$/gm)]
    .map(match => ({ text: match[1].trim(), status: match[2] }));
  const checks = [...resultText.matchAll(/^\s*[-*•]\s+\[([ xX✓])\]\s+(.+)$/gm)];
  if (checks.length) return checks.map(match => {
    const raw = match[2].trim();
    const summary = summaries.find(item => raw === item.text || raw.startsWith(`${item.text} (`));
    return { text: summary?.text ?? raw, status: /[xX✓]/.test(match[1]) ? 'completed' : summary?.status ?? 'pending' };
  });
  try {
    const parsed = JSON.parse(resultText);
    const todos = Array.isArray(parsed) ? parsed : parsed.todos;
    if (Array.isArray(todos) && todos.every(item => TODO_STATUSES.has(item.status) && typeof (item.text ?? item.content) === 'string')) {
      return todos.map(item => ({ text: item.text ?? item.content, status: item.status }));
    }
  } catch { /* Plain-text operational results are normal. */ }
  if (summaries.length) return [...previous.filter(item => item.status === 'completed'), ...summaries];
  return previous;
}

export function sensitiveToolCall(part) {
  return /(?:AGENTS\.md|SKILL\.md|(?:^|[\/\\\s"'=])\.env(?:[.\s"'\/\\]|$)|auth\.json|credentials?(?:[.\s"'\/\\]|$)|id_(?:rsa|ed25519|ecdsa)|(?:private|secret)[_-]?key|\/(?:etc\/shadow|\.ssh\/)|system[_-]?prompt)/i.test(JSON.stringify(part?.arguments ?? part?.args ?? {}));
}

export function operationalRecord(record, sanitize, line, withheld = false) {
  const timestamp = typeof record.timestamp === 'string' || typeof record.timestamp === 'number' ? record.timestamp : null;
  const events = [];
  if (record.type === 'message' && record.message && ['user', 'assistant', 'toolResult'].includes(record.message.role)) {
    const message = record.message;
    if (withheld && message.role === 'toolResult') return [{ id: `${line}:withheld`, timestamp, role: 'toolResult', kind: 'tool_result', name: typeof message.toolName === 'string' ? sanitize.text(message.toolName) : 'tool', text: '[WITHHELD: tool accessed a credential or instruction file]', ...(message.isError === true ? { isError: true } : {}) }];
    const content = typeof message.content === 'string' ? [{ type: 'text', text: message.content }] : message.content;
    if (!Array.isArray(content)) return events;
    for (const [index, part] of content.entries()) {
      if (!part || typeof part !== 'object') continue;
      const base = { id: `${line}:${index}`, timestamp, role: message.role };
      if (part.type === 'text' && typeof part.text === 'string') {
        const text = sanitize.text(part.text);
        if (text.trim()) events.push({ ...base, kind: message.role === 'toolResult' ? 'tool_result' : 'message', ...(typeof message.toolName === 'string' ? { name: sanitize.text(message.toolName) } : {}), text, ...(message.isError === true ? { isError: true } : {}) });
      } else if (part.type === 'toolCall' && message.role === 'assistant' && typeof part.name === 'string') {
        events.push({ ...base, kind: 'tool_call', name: sanitize.text(part.name), text: sensitiveToolCall(part) ? '[WITHHELD: tool arguments target a credential or instruction file]' : JSON.stringify({ ...(typeof part.intent === 'string' ? { intent: sanitize.text(part.intent) } : {}), arguments: sanitize.value(part.arguments) }, null, 2) });
      }
    }
  } else if (record.type === 'custom' && record.customType === 'session_exit') {
    const data = record.data;
    if (data && typeof data === 'object') events.push({ id: `${line}:exit`, timestamp, kind: 'session_exit', text: JSON.stringify(sanitize.value({ kind: data.kind, reason: data.reason, recordedAt: data.recordedAt })) });
  } else if (record.type === 'custom' && record.customType === 'tool_execution_start' && record.data && typeof record.data.toolName === 'string') {
    const data = record.data;
    events.push({ id: `${line}:start`, timestamp, kind: 'tool_start', name: sanitize.text(data.toolName), text: sensitiveToolCall(data) ? '[WITHHELD: tool arguments target a credential or instruction file]' : JSON.stringify(sanitize.value({ intent: data.intent, arguments: data.args, startedAt: data.startedAt })) });
  }
  return events;
}
