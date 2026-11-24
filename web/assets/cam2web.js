'use strict';

const element = id => document.getElementById(id);
const makeElement = (tag, text, className) => {
  const item = document.createElement(tag);
  if (text !== undefined) item.textContent = text;
  if (className) item.className = className;
  return item;
};
const parseJson = source => JSON.parse(source, (_key, value, context) => {
  if (typeof value !== 'number' || Number.isSafeInteger(value)) return value;
  return /^-?\d+$/.test(context?.source ?? '') ? BigInt(context.source) : value;
});
function encodeJson(value) {
  if (typeof value === 'bigint') return value.toString();
  if (typeof value === 'number' && (!Number.isFinite(value) ||
      (Number.isInteger(value) && !Number.isSafeInteger(value)))) throw new Error('数值无法精确下发');
  if (Array.isArray(value)) return `[${value.map(encodeJson).join(',')}]`;
  if (value && typeof value === 'object') {
    return `{${Object.entries(value).map(([key, item]) => `${JSON.stringify(key)}:${encodeJson(item)}`).join(',')}}`;
  }
  return JSON.stringify(value);
}
const showJson = (value, indent = 2) => JSON.stringify(value, (_key, item) => {
  if (typeof item === 'bigint') return JSON.rawJSON?.(item.toString()) ?? item.toString();
  if (typeof item === 'number' && Number.isInteger(item) && !Number.isSafeInteger(item)) return '浏览器无法精确读取此数值';
  return item;
}, indent) ?? '—';
const isIdentity = value => typeof value === 'bigint' || Number.isSafeInteger(value) ||
  (typeof value === 'string' && value.length > 0);
const isSameSelection = (first, second) => first?.node_id === second?.node_id &&
  first?.selection_id === second?.selection_id;

class ApiError extends Error {
  constructor(status, message) { super(message); this.status = status; }
}
async function requestJson(path, signal, payload) {
  const reply = await fetch(`/api/${path}`, {
    cache: 'no-store', signal,
    ...(payload === undefined ? {} : {
      method: 'POST', headers: { 'Content-Type': 'application/json' }, body: encodeJson(payload),
    }),
  });
  const source = await reply.text();
  if (reply.ok) return source ? parseJson(source) : undefined;
  let message;
  try { message = parseJson(source)?.error; } catch { /* HTTP status is the fallback. */ }
  throw new ApiError(reply.status, typeof message === 'string' ? message : `HTTP ${reply.status}`);
}

const streamBoundary = 'cam2web';
class MultipartJpegReader {
  static maxFrameBytes = 16 * 1024 * 1024;
  static maxHeaderBytes = 8 * 1024;
  static decoder = new TextDecoder();
  #line = new Uint8Array(MultipartJpegReader.maxHeaderBytes);
  #lineBytes = 0;
  #headerBytes = 0;
  #headers = new Map();
  #state = 'boundary';
  #frame = null;
  #frameBytes = 0;

  push(chunk, onFrame) {
    let offsetBytes = 0;
    while (offsetBytes < chunk.length && this.#state !== 'ended') {
      if (this.#state === 'jpeg') {
        offsetBytes = this.#copyJpeg(chunk, offsetBytes);
        continue;
      }
      const newlineBytes = chunk.indexOf(10, offsetBytes);
      const endBytes = newlineBytes === -1 ? chunk.length : newlineBytes;
      const countBytes = endBytes - offsetBytes;
      this.#headerBytes += countBytes + (newlineBytes === -1 ? 0 : 1);
      if (this.#headerBytes > MultipartJpegReader.maxHeaderBytes) throw new Error('MJPEG 分隔行或头部过长');
      this.#line.set(chunk.subarray(offsetBytes, endBytes), this.#lineBytes);
      this.#lineBytes += countBytes;
      if (newlineBytes === -1) return;
      const hasCr = this.#line[this.#lineBytes - 1] === 13;
      if (!hasCr) throw new Error('MJPEG 缺少 CRLF');
      const line = MultipartJpegReader.decoder.decode(this.#line.subarray(0, this.#lineBytes - 1));
      this.#lineBytes = 0;
      offsetBytes = newlineBytes + 1;
      this.#readLine(line, onFrame);
    }
  }
  get isEnded() { return this.#state === 'ended'; }
  #copyJpeg(chunk, offsetBytes) {
    const countBytes = Math.min(chunk.length - offsetBytes, this.#frame.length - this.#frameBytes);
    this.#frame.set(chunk.subarray(offsetBytes, offsetBytes + countBytes), this.#frameBytes);
    this.#frameBytes += countBytes;
    if (this.#frameBytes === this.#frame.length) this.#state = 'trailer';
    return offsetBytes + countBytes;
  }
  #readLine(line, onFrame) {
    if (this.#state === 'trailer') {
      if (line !== '') throw new Error('MJPEG 帧长度与分隔符不匹配');
      const frame = this.#frame;
      this.#frame = null;
      this.#state = 'boundary';
      this.#headerBytes = 0;
      const end = frame.length;
      const hasJpegMarkers = end >= 4 && frame[0] === 0xff && frame[1] === 0xd8 &&
        frame[end - 2] === 0xff && frame[end - 1] === 0xd9;
      if (hasJpegMarkers) onFrame({ jpeg: frame, metadata: this.#readTofMetadata() });
      return;
    }
    if (this.#state === 'boundary') {
      if (line === `--${streamBoundary}--`) { this.#state = 'ended'; return; }
      if (line !== `--${streamBoundary}`) throw new Error('MJPEG 分隔符无效');
      this.#headers.clear();
      this.#state = 'headers';
      return;
    }
    if (line === '') { this.#beginJpeg(); return; }
    const colon = line.indexOf(':');
    if (colon < 1) throw new Error('MJPEG part 头部无效');
    const name = line.slice(0, colon).trim().toLowerCase();
    if (this.#headers.has(name)) throw new Error('MJPEG part 头部重复');
    this.#headers.set(name, line.slice(colon + 1).trim());
  }
  #readTofMetadata() {
    const source = this.#headers.get('x-cam2web-tof');
    if (!source) return null;
    try { return JSON.parse(source); }
    catch { return null; }
  }
  #beginJpeg() {
    const length = this.#headers.get('content-length');
    if (length === undefined) throw new Error('MJPEG part 缺少 Content-Length');
    const lengthBytes = /^\d+$/.test(length) ? Number(length) : 0;
    if (this.#headers.get('content-type')?.split(';')[0].trim().toLowerCase() !== 'image/jpeg' ||
        lengthBytes < 1 || lengthBytes > MultipartJpegReader.maxFrameBytes) throw new Error('MJPEG 帧类型或长度无效');
    this.#frame = new Uint8Array(lengthBytes);
    this.#frameBytes = 0;
    this.#headerBytes = 0;
    this.#state = 'jpeg';
  }
}

class PreviewStream {
  static frameTimeoutMs = 6000;
  #subscription = null;
  #imageUrl = null;
  #frame = element('frame');
  #emptyMessage = element('empty-frame');
  #onStale;
  #onMetadata;

  constructor(onStale, onMetadata) {
    this.#onStale = onStale;
    this.#onMetadata = onMetadata;
  }
  setSession(sessionId) {
    if (!isIdentity(sessionId)) { this.stop(); return; }
    if (this.#subscription?.sessionId === sessionId) return;
    this.stop();
    const subscription = { sessionId, attempt: null, retryTimer: null };
    this.#subscription = subscription;
    this.#connect(subscription, 0);
  }
  stop(message = '预览未启动') {
    const subscription = this.#subscription;
    this.#subscription = null;
    if (subscription) {
      clearTimeout(subscription.retryTimer);
      this.#closeAttempt(subscription);
    }
    this.#clearFrame('未订阅', message);
  }
  #clearFrame(label, message) {
    this.#frame.hidden = true;
    this.#frame.removeAttribute('src');
    if (this.#imageUrl) URL.revokeObjectURL(this.#imageUrl);
    this.#imageUrl = null;
    this.#emptyMessage.hidden = false;
    if (this.#emptyMessage.textContent !== message) this.#emptyMessage.textContent = message;
    this.#setViewer(label, false);
    this.#onMetadata(null);
  }
  #setViewer(label, isLive) {
    const viewer = element('viewer');
    const text = `本页：${label}`;
    if (viewer.textContent !== text) viewer.textContent = text;
    viewer.dataset.live = isLive;
  }
  #closeAttempt(subscription) {
    const attempt = subscription.attempt;
    if (!attempt) return;
    subscription.attempt = null;
    clearTimeout(attempt.frameTimer);
    attempt.abort.abort();
    if (attempt.decodeUrl) URL.revokeObjectURL(attempt.decodeUrl);
    attempt.pending = null;
  }
  #setDeadline(attempt) {
    clearTimeout(attempt.frameTimer);
    attempt.frameTimer = setTimeout(() => {
      attempt.abort.abort(new Error('收帧超时'));
    }, PreviewStream.frameTimeoutMs);
  }
  async #connect(subscription, failures) {
    const attempt = { abort: new AbortController(), frameTimer: null, isDecoding: false, pending: null, decodeUrl: null, hasFrame: false };
    subscription.attempt = attempt;
    this.#clearFrame('等待首帧', '等待画面…');
    this.#setDeadline(attempt);
    try {
      await this.#readStream(subscription, attempt);
      throw new Error('预览流已结束');
    } catch (error) {
      if (this.#subscription !== subscription) return;
      const reason = attempt.abort.signal.reason ?? error;
      this.#closeAttempt(subscription);
      if (reason.status === 410) {
        this.#clearFrame('会话已结束', '预览已结束');
        this.#onStale();
        return;
      }
      this.#clearFrame('等待重连', `${reason.message}，正在重连…`);
      const nextFailures = attempt.hasFrame ? 0 : failures + 1;
      const delayMs = Math.min(4000, 500 * 2 ** Math.min(nextFailures, 3));
      subscription.retryTimer = setTimeout(() => this.#connect(subscription, nextFailures), delayMs);
    }
  }
  async #readStream(subscription, attempt) {
    const reply = await fetch(`/stream/${encodeURIComponent(subscription.sessionId)}`, {
      cache: 'no-store', signal: attempt.abort.signal,
    });
    if (!reply.ok) throw new ApiError(reply.status, `流连接失败（HTTP ${reply.status}）`);
    const [mediaType, ...params] = (reply.headers.get('content-type') ?? '').split(';');
    if (mediaType.trim().toLowerCase() !== 'multipart/x-mixed-replace' ||
        !params.some(param => /^\s*boundary\s*=\s*(?:cam2web|"cam2web")\s*$/.test(param)) || !reply.body) {
      throw new Error('响应不是 cam2web MJPEG 流');
    }
    const reader = reply.body.getReader();
    const multipart = new MultipartJpegReader();
    try {
      while (!multipart.isEnded) {
        const { done, value } = await reader.read();
        if (done) return;
        multipart.push(value, frame => { attempt.pending = frame; });
        void this.#decodeLatest(subscription, attempt);
      }
    } finally {
      reader.releaseLock();
    }
  }
  async #decodeLatest(subscription, attempt) {
    if (!attempt.pending || attempt.isDecoding || subscription.attempt !== attempt) return;
    attempt.isDecoding = true;
    const { jpeg, metadata } = attempt.pending;
    const url = URL.createObjectURL(new Blob([jpeg], { type: 'image/jpeg' }));
    attempt.pending = null;
    attempt.decodeUrl = url;
    try {
      const candidate = new Image();
      candidate.src = url;
      await candidate.decode();
      if (subscription.attempt !== attempt || attempt.abort.signal.aborted) return;
      if (this.#imageUrl) URL.revokeObjectURL(this.#imageUrl);
      this.#imageUrl = url;
      attempt.decodeUrl = null;
      this.#frame.src = url;
      this.#frame.hidden = false;
      this.#emptyMessage.hidden = true;
      this.#setViewer('收帧中', true);
      this.#onMetadata(metadata);
      attempt.hasFrame = true;
      this.#setDeadline(attempt);
    } catch {
      if (subscription.attempt === attempt && !attempt.abort.signal.aborted) this.#clearFrame('等待有效帧', '收到无法解码的 JPEG，等待下一帧…');
    } finally {
      if (attempt.decodeUrl === url) { URL.revokeObjectURL(url); attempt.decodeUrl = null; }
      attempt.isDecoding = false;
      void this.#decodeLatest(subscription, attempt);
    }
  }
}

function tryInteger(value) {
  if (typeof value === 'bigint') return value;
  if (Number.isSafeInteger(value)) return BigInt(value);
  if (typeof value === 'string' && /^-?\d{1,24}$/.test(value)) return BigInt(value);
  return null;
}
function makeWheelAdjuster(input, min, max, step) {
  if (min === null || max === null || min >= max) return null;
  const gridStep = step > 0n ? step : 1n;
  const lastIndex = (max - min) / gridStep;
  if (lastIndex === 0n) return null;
  const maxMagnitude = max < 0n ? -max : max;
  const roundedTicks = (maxMagnitude + 25n * gridStep) / (50n * gridStep);
  const ticks = roundedTicks > 0n ? roundedTicks : 1n;
  return direction => {
    const current = tryInteger(input.value);
    if (current === null || current < min || current > max || (current - min) % gridStep !== 0n) return false;
    const index = (current - min) / gridStep + direction * ticks;
    const clamped = index < 0n ? 0n : index > lastIndex ? lastIndex : index;
    const next = min + clamped * gridStep;
    if (current === next) return false;
    input.value = next.toString();
    return true;
  };
}
function makeIntegerClamper(input, min, max, kind) {
  if (min === null && max === null) return null;
  const clampTo = value => { input.value = value.toString(); return true; };
  return () => {
    const exact = tryInteger(input.value);
    if (exact === null) return false;
    if (kind === 'bitmask') {
      const allowed = max ?? (1n << 32n) - 1n;
      return (exact & allowed) === exact ? false : clampTo(exact & allowed);
    }
    if (min !== null && exact < min) return clampTo(min);
    if (max !== null && exact > max) return clampTo(max);
    return false;
  };
}
function getIntegerEditor(field, kind) {
  const bounds = [field.min, field.max, field.step];
  if ([...bounds, field.value].some(value => value != null && tryInteger(value) === null)) return null;
  const [min, max, step] = bounds.map(tryInteger);
  const isStringWire = typeof (field.value ?? field.default ?? field.min ?? field.max) === 'string';
  const input = makeElement('input');
  input.type = 'text';
  input.inputMode = 'numeric';
  input.required = true;
  input.maxLength = 25;
  return {
    input,
    adjustByWheel: kind === 'bitmask' ? null : makeWheelAdjuster(input, min, max, step),
    clampInPlace: makeIntegerClamper(input, min, max, kind),
    set: value => { input.value = tryInteger(value)?.toString() ?? ''; },
    get: () => {
      const exact = tryInteger(input.value);
      if (exact === null) throw new Error('请输入整数。');
      const isWide = ['int64', 'integer64', 'integer_64', 'u64', 'i64'].includes(kind);
      const nativeMin = kind === 'u64' || kind === 'bitmask' ? 0n : isWide ? -(1n << 63n) : -(1n << 31n);
      const nativeMax = kind === 'u64' ? (1n << 64n) - 1n : kind === 'bitmask' ? (1n << 32n) - 1n :
        isWide ? (1n << 63n) - 1n : (1n << 31n) - 1n;
      if (exact < (min ?? nativeMin) || exact > (max ?? nativeMax)) throw new Error('超出允许范围。');
      if (kind === 'bitmask' && (exact & ~(max ?? nativeMax)) !== 0n) throw new Error('包含设备未公布的位。');
      if (kind !== 'bitmask' && step > 0n && (exact - (min ?? 0n)) % step !== 0n) throw new Error('不符合步长要求。');
      return isStringWire ? exact.toString() : exact;
    },
  };
}
function getChoiceEditor(options, label) {
  const input = makeElement('select');
  const placeholder = makeElement('option', label);
  placeholder.value = '';
  placeholder.disabled = true;
  input.append(placeholder);
  options.forEach(({ text, value }, index) => {
    const option = makeElement('option', text ?? showJson(value, 0));
    option.value = String(index);
    input.append(option);
  });
  return {
    input,
    set: value => {
      const index = options.findIndex(option => option.value === value ||
        (typeof option.value === 'bigint' && Number.isSafeInteger(value) && option.value === BigInt(value)));
      input.value = index < 0 ? '' : String(index);
    },
    get: () => {
      if (input.value === '') throw new Error('请选择一个有效选项。');
      return options[Number(input.value)].value;
    },
  };
}
function getBooleanEditor() {
  const input = makeElement('input');
  input.type = 'checkbox';
  return {
    input,
    set: value => {
      input.indeterminate = value == null;
      input.checked = isEnabledValue(value);
    },
    get: () => Number(input.checked),
  };
}
function isEnabledValue(value) {
  return value === true || tryInteger(value) === 1n;
}
function isBooleanField(field, kind) {
  return kind === 'boolean' || kind === 'bool' ||
    (['integer', 'int', 'i32', 'integer32', 'integer64', 'integer_64', 'int64', 'i64'].includes(kind) &&
      tryInteger(field.min) === 0n && tryInteger(field.max) === 1n && tryInteger(field.step) === 1n);
}
function getMenuChoices(field) {
  const source = field.menu ?? field.choices ?? field.options;
  if (!source || typeof source !== 'object') return [];
  const entries = Array.isArray(source) ? source : Object.entries(source).map(([index, name]) => ({ index, name }));
  return entries.flatMap(entry => {
    const isObject = entry !== null && typeof entry === 'object';
    const index = tryInteger(isObject ? entry.index ?? entry.id ?? entry.value : entry);
    if (index === null || index < 0n || index > 4294967295n) return [];
    const value = typeof (field.value ?? field.default) === 'string' ? index.toString() : Number(index);
    const label = isObject ? entry.name ?? entry.label ?? entry.value : null;
    return [{ value, text: label == null ? null : `${label} (${value})` }];
  });
}
function getControlTooltip(field, kind, isBoolean) {
  if (kind === 'button' || field.has_payload === true) return '';
  if (isBoolean) return '取值：0（关闭）、1（开启）';
  const choices = getMenuChoices(field);
  if (choices.length) return `可选值：${choices.map(({ text, value }) => text ?? value).join('、')}`;
  const [min, max, step] = [field.min, field.max, field.step].map(tryInteger);
  if (kind === 'bitmask') return max === null ? '' : `允许的位掩码：0x${max.toString(16)}`;
  if (min === null || max === null) return '';
  const unit = field.unit ? ` ${field.unit}` : '';
  return `取值范围：${min} ～ ${max}${unit}${step > 0n ? `；步长：${step}` : ''}`;
}
function canEditField(field) {
  const flags = field.flags;
  const isBlocked = Array.isArray(flags) && flags.some(flag =>
    /^(?:v4l2_ctrl_flag_)?(?:disabled|read_?only|inactive|grabbed|has_payload)$/i.test(flag));
  const hasBlockedBit = Number.isSafeInteger(flags) && (flags & (1 | 2 | 4 | 16 | 256)) !== 0;
  return !isBlocked && !hasBlockedBit && field.has_payload !== true && field.can_write === true;
}
function makeEditor(field, kind, isBoolean) {
  if (!canEditField(field)) return null;
  if (kind === 'button') return { get: () => 0 };
  if (isBoolean) return getBooleanEditor();
  if (['integer', 'int', 'i32', 'integer32', 'integer64', 'integer_64', 'int64', 'i64', 'u64', 'bitmask'].includes(kind))
    return getIntegerEditor(field, kind);
  if (['menu', 'integer_menu', 'int_menu', 'enum', 'choice'].includes(kind)) {
    const choices = getMenuChoices(field);
    return choices.length ? getChoiceEditor(choices, '请选择一项') : null;
  }
  return null;
}

class FieldList {
  #rows = new Map();
  #root;
  #onWrite;
  constructor(rootId, onWrite) {
    this.#root = element(rootId);
    this.#onWrite = onWrite;
  }
  clear() { this.#rows.clear(); this.#root.replaceChildren(); }
  lock() {
    for (const row of this.#rows.values()) {
      this.#setLocked(row, true);
    }
  }
  render(fields, isLocked) {
    const nextRows = new Map();
    fields.forEach((field, index) => {
      if (!field || typeof field !== 'object') return;
      const target = isIdentity(field.id) ? { id: field.id } : null;
      const key = target ? encodeJson(target) : `unknown-${index}`;
      if (nextRows.has(key)) return;
      const { value, ...descriptor } = field;
      const wireType = value == null ? typeof field.default : typeof value === 'bigint' ? 'number' : typeof value;
      const signature = showJson({ descriptor, wireType, isUnsafe: typeof value === 'number' && Number.isInteger(value) && !Number.isSafeInteger(value) }, 0);
      const previous = this.#rows.get(key);
      const row = previous?.signature === signature ? previous : this.#makeRow(field, target, signature);
      if (previous && previous !== row) previous.section.replaceWith(row.section);
      else if (!previous) this.#root.append(row.section);
      row.lastValue = value;
      this.#syncValue(row);
      this.#setLocked(row, isLocked);
      nextRows.set(key, row);
    });
    for (const [key, row] of this.#rows) if (!nextRows.has(key)) row.section.remove();
    this.#rows = nextRows;
  }
  #syncValue(row) {
    if (row.output) {
      const value = row.lastValue;
      const text = value == null ? '—' : row.isBoolean ? (isEnabledValue(value) ? '开启' : '关闭') : showJson(value, 0);
      if (row.output.textContent !== text) row.output.textContent = text;
    }
    if (!row.isDirty && row.pendingCount === 0 &&
        (row.needsReset || row.renderedValue !== row.lastValue)) {
      row.editor?.set?.(row.lastValue);
      row.renderedValue = row.lastValue;
      row.needsReset = false;
    }
  }
  #setLocked(row, isLocked) {
    if (row.input) row.input.disabled = isLocked;
  }
  #makeRow(field, target, signature) {
    const title = field.name ?? `控件 ${field.id ?? '?'}`;
    const section = makeElement('section');
    const kind = String(field.kind ?? field.value_type ?? field.type ?? '').toLowerCase().replaceAll('-', '_');
    const isBoolean = isBooleanField(field, kind);
    const isWriteOnly = (Number.isSafeInteger(field.flags) && (field.flags & 64) !== 0) ||
      (Array.isArray(field.flags) && field.flags.some(flag => /^(?:v4l2_ctrl_flag_)?write_?only$/i.test(flag)));
    const line = makeElement('div', undefined, 'field-line');
    const tooltip = getControlTooltip(field, kind, isBoolean);
    if (tooltip) line.title = tooltip;
    line.append(makeElement('h3', title));
    section.append(line);
    const editor = target && makeEditor(field, kind, isBoolean);
    const row = { section, signature, editor, isBoolean, isDirty: false, pendingCount: 0, needsReset: true };
    if (editor) this.#addEditor(row, line, target, title, field.unit);
    else if (!isWriteOnly) {
      row.output = makeElement('code', undefined, 'small');
      line.append(row.output);
    }
    return row;
  }
  #addEditor(row, line, target, title, unit) {
    const editorRow = makeElement('div', undefined, 'field-editor');
    const error = makeElement('p', '', 'small error');
    error.setAttribute('role', 'status');
    const editorInput = row.editor.input;
    const input = editorInput ?? makeElement('button', '触发');
    if (editorInput) input.setAttribute('aria-label', `设置 ${title}${unit == null ? '' : `（${unit}）`}`);
    else {
      input.type = 'button';
      input.setAttribute('aria-label', `触发 ${title}`);
    }
    row.input = input;
    editorRow.append(input);
    if (unit != null) editorRow.append(makeElement('span', unit, 'small'));
    line.append(editorRow);
    row.section.append(error);

    let wheelTimer;
    const markDirty = () => {
      clearTimeout(wheelTimer);
      row.isDirty = true;
      error.textContent = '';
    };
    const commit = async () => {
      clearTimeout(wheelTimer);
      if (!input.isConnected || input.disabled || !row.isDirty) return;
      let value;
      try { value = row.editor.get(); }
      catch (failure) { error.textContent = failure.message; return; }
      if (editorInput && row.pendingCount === 0 && row.lastValue != null &&
          tryInteger(value) === tryInteger(row.lastValue)) {
        row.isDirty = false;
        row.needsReset = true;
        this.#syncValue(row);
        return;
      }
      const write = this.#onWrite({ ...target, value });
      if (!write) return;
      row.isDirty = false;
      row.pendingCount += 1;
      await write.catch(() => {});
      row.pendingCount -= 1;
      row.needsReset = true;
      this.#syncValue(row);
    };
    const changeAndCommit = () => { markDirty(); void commit(); };
    if (!editorInput) input.addEventListener('click', changeAndCommit);
    else if (input.type === 'text') {
      input.addEventListener('input', () => {
        row.editor.clampInPlace?.();
        markDirty();
      });
      if (row.editor.adjustByWheel) {
        input.addEventListener('wheel', event => {
          if (input.disabled || document.activeElement !== input || event.ctrlKey || !event.deltaY) return;
          event.preventDefault();
          if (!row.editor.adjustByWheel(event.deltaY < 0 ? 1n : -1n)) return;
          markDirty();
          wheelTimer = setTimeout(commit, 180);
        }, { passive: false });
      }
      input.addEventListener('blur', () => { void commit(); });
      input.addEventListener('keydown', event => {
        if (event.key !== 'Enter' || event.isComposing) return;
        event.preventDefault();
        void commit();
      });
    } else input.addEventListener('change', changeAndCommit);
  }
}

function previewBlockReason(selection) {
  if (!selection) return '请选择采集节点';
  if (selection.capabilities?.can_stream !== true) return '此节点不支持预览';
  if (selection.has_profile !== true) return '此节点未配置预览';
  return '';
}

function formatSensorClock(clock) {
  if (!clock?.year || !clock.month || !clock.day) return '—';
  const pad = (part, digits = 2) => String(part).padStart(digits, '0');
  return `${pad(clock.year, 4)}-${pad(clock.month)}-${pad(clock.day)} ` +
    `${pad(clock.hour)}:${pad(clock.minute)}:${pad(clock.second)}.${pad(clock.microsecond, 6)}`;
}

class TofMetadataView {
  #pair = null;
  #details = element('metadata-details');
  static channels = [['depth', 'Depth'], ['nir', 'NIR']];

  constructor() {
    this.#details.addEventListener('toggle', () => this.#renderRaw());
  }
  render(pair) {
    if (pair === this.#pair) return;
    this.#pair = pair;
    const hasPair = Boolean(pair?.depth && pair?.nir);
    element('metadata-empty').hidden = hasPair;
    this.#details.hidden = !hasPair;
    for (const [channel, title] of TofMetadataView.channels) {
      const label = element(`metadata-${channel}`);
      const sample = pair?.[channel];
      label.hidden = !sample;
      label.textContent = sample ? `${title} 帧 ${sample.sensor_frame_count} · 采集序号 ${sample.capture_sequence} · ${formatSensorClock(sample.sensor_clock)}` : '';
    }
    this.#renderRaw();
  }
  #renderRaw() {
    if (!this.#details.open) return;
    const hex = value => Number.isInteger(value) ? value.toString(16).padStart(2, '0') : '—';
    const bytes = values => Array.isArray(values) ? values.map(hex).join(' ') : '—';
    for (const [channel, title] of TofMetadataView.channels) {
      const sample = this.#pair?.[channel];
      element(`metadata-${channel}-raw`).textContent = sample ?
        `${title} · 标记 0x${hex(sample.frame_flag)} · dummy 0x${hex(sample.dummy_byte)} · 填充 0x${hex(sample.padding_byte)}` +
        ` · 保留 [16–31] ${bytes(sample.reserved_bytes)} · 未定义 [32–63] ${bytes(sample.undocumented_bytes)}` : '';
    }
  }
}

class Workbench {
  #metadata = new TofMetadataView();
  #stream = new PreviewStream(() => { void this.#refresh(); }, pair => this.#metadata.render(pair));
  #fields = {
    controls: new FieldList('control-rows', payload => this.#queueControlWrite('control', payload)),
    'camera-controls': new FieldList('camera-control-rows', payload => this.#queueControlWrite('camera-control', payload)),
  };
  #controlWrites = Promise.resolve();
  #pendingControlWriteCount = 0;
  #nodeList = [];
  #nodeListKey = '';
  #lastNodesMs = -Infinity;
  #status = null;
  #fieldResults = {};
  #fieldRead = null;
  #lastControlValuesMs = -Infinity;
  #read = null;
  #timer = null;
  #isActive = false;
  #isWriting = false;
  #isFresh = false;
  #shouldRefreshNodes = true;
  #nodesError = '';
  #statusError = '';

  start() {
    element('camera').addEventListener('change', () => {
      const index = element('camera').value;
      if (index === '') return;
      const node = this.#nodeList[Number(index)];
      if (node && isIdentity(node.id)) this.#startMutation('select', { node_id: node.id });
    });
    element('preview-toggle').addEventListener('click', () => {
      this.#startMutation(this.#status?.preview ? 'stop' : 'start', {});
    });
    window.addEventListener('pagehide', () => this.stop());
    window.addEventListener('pageshow', event => { if (event.persisted) this.#resume(); });
    this.#resume();
  }
  #resume() {
    this.#isActive = true;
    this.#shouldRefreshNodes = true;
    this.#render();
    void this.#refresh();
  }
  stop() {
    this.#isActive = false;
    this.#isFresh = false;
    this.#cancelRead();
    this.#cancelFieldRead();
    this.#stream.stop();
  }
  #cancelRead() {
    clearTimeout(this.#timer);
    this.#read?.abort();
    this.#read = null;
  }
  #acceptStatus(status) {
    if (!status || !Object.hasOwn(status, 'selection') || !Object.hasOwn(status, 'preview')) throw new Error('状态响应缺少 selection / preview');
    if (!isSameSelection(this.#status?.selection, status.selection)) {
      this.#stream.stop('等待当前节点的会话状态…');
      this.#cancelFieldRead();
      this.#fieldResults = {};
      this.#lastControlValuesMs = performance.now();
      for (const [route, list] of Object.entries(this.#fields)) {
        list.clear();
        element(route).scrollTop = 0;
      }
    }
    const sessionId = status.preview?.session_id;
    this.#status = status;
    this.#isFresh = true;
    this.#statusError = '';
    if (isIdentity(sessionId)) this.#stream.setSession(sessionId);
    else this.#stream.stop(status.last_error ? '预览已停止' :
      previewBlockReason(status.selection) || '预览未启动');
    this.#render();
    void this.#readNextFieldGroup();
  }
  #cancelFieldRead() {
    this.#fieldRead?.abort.abort();
    this.#fieldRead = null;
  }
  async #readNextFieldGroup() {
    const selection = this.#status?.selection;
    if (!this.#isActive || !this.#isFresh || !selection || this.#isWriting ||
        this.#pendingControlWriteCount || this.#fieldRead) return;
    const route = Object.keys(this.#fields).find(route => !Object.hasOwn(this.#fieldResults, route));
    if (!route) {
      if (performance.now() - this.#lastControlValuesMs >= 2200 &&
          Object.values(this.#fieldResults).some(result => result.items?.some(field => field.can_read))) {
        void this.#refreshControlValues(selection);
      }
      return;
    }
    const read = { route, abort: new AbortController() };
    this.#fieldRead = read;
    this.#render();
    const signal = AbortSignal.any([read.abort.signal, AbortSignal.timeout(5000)]);
    try {
      const items = await requestJson(route, signal);
      if (!Array.isArray(items)) throw new Error('响应不是列表');
      const confirmed = await requestJson('status', signal);
      signal.throwIfAborted();
      if (!isSameSelection(selection, confirmed?.selection)) throw new Error('采集节点已变化');
      this.#fieldResults[route] = { items };
    } catch (error) {
      if (!read.abort.signal.aborted) {
        this.#fieldResults[route] = { error: error.message };
      }
    } finally {
      if (this.#fieldRead === read) {
        this.#fieldRead = null;
        this.#lastControlValuesMs = performance.now();
        this.#render();
        void this.#readNextFieldGroup();
      }
    }
  }
  async #refreshControlValues(selection) {
    const read = { abort: new AbortController() };
    this.#fieldRead = read;
    const signal = AbortSignal.any([read.abort.signal, AbortSignal.timeout(5000)]);
    try {
      const readings = await requestJson('control-values', signal, {
        node_id: selection.node_id, selection_id: selection.selection_id,
      });
      signal.throwIfAborted();
      if (!isSameSelection(selection, this.#status?.selection)) return;
      const values = new Map(readings.map(({ id, value }) => [id, value]));
      for (const result of Object.values(this.#fieldResults)) {
        for (const field of result.items ?? []) {
          if (values.has(field.id)) field.value = values.get(field.id);
        }
      }
    } catch {
      // Keep the last good values on a timeout or a busy camera; try again on a later status tick.
    } finally {
      if (this.#fieldRead === read) {
        this.#fieldRead = null;
        this.#lastControlValuesMs = performance.now();
        this.#render();
      }
    }
  }
  async #readNodes(signal) {
    try {
      const nodes = await requestJson('nodes', signal);
      if (!Array.isArray(nodes)) throw new Error('响应不是列表');
      signal.throwIfAborted();
      this.#nodeList = nodes.filter(node => node && typeof node === 'object');
      this.#nodesError = '';
    } catch (error) {
      if (error.name !== 'AbortError') this.#nodesError = `节点枚举失败：${error.message}`;
    }
  }
  async #refresh() {
    if (!this.#isActive || this.#read || this.#isWriting || this.#pendingControlWriteCount) return;
    this.#cancelRead();
    const read = new AbortController();
    this.#read = read;
    const signal = AbortSignal.any([read.signal, AbortSignal.timeout(5000)]);
    const hasNodeRefresh = this.#shouldRefreshNodes || performance.now() - this.#lastNodesMs > 10000;
    this.#shouldRefreshNodes = false;
    const nodes = hasNodeRefresh ? this.#readNodes(signal) : null;
    try {
      const status = await requestJson('status', signal);
      signal.throwIfAborted();
      this.#acceptStatus(status);
    } catch (error) {
      if (read.signal.aborted) return;
      this.#isFresh = false;
      this.#stream.stop('预览连接中断');
      this.#statusError = `状态查询失败：${error.message}`;
    } finally {
      await nodes;
      if (this.#read === read) {
        this.#read = null;
        if (hasNodeRefresh) this.#lastNodesMs = performance.now();
        this.#render();
        this.#timer = setTimeout(() => this.#refresh(), this.#shouldRefreshNodes ? 0 : 2200);
      }
    }
  }
  #queueControlWrite(route, payload) {
    const selection = this.#status?.selection;
    if (!this.#isActive || !this.#isFresh || this.#isWriting ||
        !isIdentity(selection?.node_id) || !isIdentity(selection?.selection_id)) return null;
    this.#cancelRead();
    this.#cancelFieldRead();
    this.#pendingControlWriteCount += 1;
    const target = { node_id: selection.node_id, selection_id: selection.selection_id };
    const write = this.#controlWrites.then(async () => {
      if (!this.#isActive || !isSameSelection(selection, this.#status?.selection)) throw new Error('采集节点已变化');
      const result = await requestJson(route, AbortSignal.timeout(8000), { ...target, ...payload });
      this.#updateControlValue(`${route}s`, result);
    }).finally(() => {
      this.#pendingControlWriteCount -= 1;
      this.#render();
      void this.#refresh();
    });
    this.#controlWrites = write.catch(() => {});
    return write;
  }
  #startMutation(route, payload) {
    if (!this.#isActive || this.#isWriting || !this.#isFresh) return false;
    const { selection, preview } = this.#status;
    if (route !== 'select' && (!isIdentity(selection?.node_id) || !isIdentity(selection?.selection_id))) return false;
    const needsSession = route === 'stop';
    if (needsSession && !isIdentity(preview?.session_id)) return false;
    const target = route === 'select' ? {} : { node_id: selection.node_id, selection_id: selection.selection_id };
    if (needsSession) target.session_id = preview.session_id;
    this.#cancelRead();
    this.#cancelFieldRead();
    this.#isWriting = true;
    if (route === 'stop' || route === 'select') this.#stream.stop('等待板端确认操作…');
    element('operation').textContent = '';
    this.#render();
    void this.#controlWrites.then(() => this.#mutate(route, { ...target, ...payload }));
    return true;
  }
  async #mutate(route, payload) {
    try {
      await requestJson(route, AbortSignal.timeout(8000), payload);
    } catch (error) {
      if (error.status === 409) this.#stream.stop('选择或会话已过期，正在刷新…');
      element('operation').textContent = error.status === 409 ?
        '状态已变化，请重试' : `操作失败：${error.message}`;
    } finally {
      this.#isFresh = false;
      this.#isWriting = false;
      this.#shouldRefreshNodes = true;
      this.#render();
      void this.#refresh();
    }
  }
  #updateControlValue(route, result) {
    const field = this.#fieldResults[route]?.items?.find(item => item?.id === result?.id);
    if (field && typeof result.value === 'string') field.value = parseJson(result.value);
  }
  #renderNodes(selection) {
    const camera = element('camera');
    const key = showJson(this.#nodeList.map(node => [node.id, node.path, node.name ?? node.capabilities?.card]), 0);
    if (key !== this.#nodeListKey) {
      this.#nodeListKey = key;
      const placeholder = makeElement('option', this.#nodeList.length ? '选择采集节点' : '未发现采集节点');
      placeholder.value = '';
      placeholder.disabled = true;
      camera.replaceChildren(placeholder);
      this.#nodeList.forEach((node, index) => {
        const title = [node.path ?? node.id, node.name ?? node.capabilities?.card].filter(Boolean).join(' · ');
        const option = makeElement('option', title);
        option.value = String(index);
        option.disabled = !isIdentity(node.id);
        camera.append(option);
      });
    }
    const index = this.#nodeList.findIndex(node => node.id === selection?.node_id);
    const value = index < 0 ? '' : String(index);
    if (camera.value !== value) camera.value = value;
    camera.disabled = this.#isWriting || !this.#isFresh || !this.#nodeList.some(node => isIdentity(node.id));
  }
  #renderFields(route, list, isLocked) {
    const result = this.#fieldResults[route];
    const note = element(`${route}-note`);
    note.textContent = this.#fieldRead?.route === route ? '读取中…' :
      result?.error ? `读取失败：${result.error}` : result?.items?.length === 0 ? '无可用项目' : '';
    if (result?.items) list.render(result.items, isLocked);
    else list.lock();
  }
  #render() {
    const selection = this.#status?.selection;
    const preview = this.#status?.preview;
    const isLocked = this.#isWriting || !this.#isFresh || !isIdentity(selection?.node_id) || !isIdentity(selection?.selection_id);
    this.#renderNodes(selection);
    for (const [route, list] of Object.entries(this.#fields)) this.#renderFields(route, list, isLocked);
    const isSessionLocked = isLocked || !isIdentity(preview?.session_id);

    const toggle = element('preview-toggle');
    toggle.textContent = preview ? '停止预览' : '启动预览';
    toggle.dataset.action = preview ? 'stop' : 'start';
    toggle.disabled = preview ? isSessionLocked : isLocked || Boolean(previewBlockReason(selection));

    const board = element('board');
    const boardError = this.#statusError || this.#nodesError || (!preview && this.#status?.last_error) || '';
    const boardText = boardError ? '板端：异常' : this.#isWriting ? '板端：处理中' :
      !this.#isFresh ? '板端：连接中' : preview ? '板端：预览中' : '板端：待机';
    if (board.textContent !== boardText) board.textContent = boardText;
    board.title = boardError;
    board.dataset.live = this.#isFresh && Boolean(preview) && !boardError;

    const spec = preview?.spec;
    const outputFps = this.#status?.diagnostics?.output_fps;
    const metrics = [spec && `采集 ${spec.width} × ${spec.height}`,
      Number.isFinite(outputFps) && `预览 ${outputFps.toFixed(1)} fps`].filter(Boolean).join(' · ');
    const metricsLabel = element('preview-metrics');
    const metricsText = this.#isFresh && preview ? metrics : '';
    if (metricsLabel.textContent !== metricsText) metricsLabel.textContent = metricsText;
  }
}

new Workbench().start();
