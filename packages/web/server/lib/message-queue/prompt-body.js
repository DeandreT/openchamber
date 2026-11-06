export const MANIFEST_PROMPT_BYTE_LIMIT = 128 * 1024 * 1024;

const JSON_SCALAR_KINDS = new Set(['string', 'number', 'boolean']);

// The JSON vocabulary every queue walker accepts: 'null', 'string', 'number',
// 'boolean', 'array', a plain 'object', or undefined for anything else
// (undefined, functions, class instances, null-prototype objects).
export const jsonValueKind = (value) => {
  if (value === null) return 'null';
  if (Array.isArray(value)) return 'array';
  const kind = typeof value;
  if (JSON_SCALAR_KINDS.has(kind)) return kind;
  return kind === 'object' && Object.getPrototypeOf(value) === Object.prototype ? 'object' : undefined;
};

// Measure typed JSON before allocating its complete encoded body. Number
// spellings can expand substantially after parsing a compact source capture.
export const jsonByteLength = (value, compactNumbers = false) => {
  const kind = jsonValueKind(value);
  if (kind === 'null') return 4;
  if (kind === 'string' || kind === 'boolean') return Buffer.byteLength(JSON.stringify(value));
  if (kind === 'number') {
    const decimal = JSON.stringify(value);
    if (!compactNumbers) return decimal.length;
    const exponential = Number.isFinite(value) ? value.toExponential().replace('e+', 'e') : decimal;
    return Math.min(decimal.length, exponential.length);
  }
  if (kind === 'array') {
    let size = 2 + Math.max(0, value.length - 1);
    for (const entry of value) size += entry === undefined ? 4 : jsonByteLength(entry, compactNumbers);
    return size;
  }
  if (kind === 'object') {
    let size = 2;
    let count = 0;
    for (const key of Object.keys(value)) {
      if (value[key] === undefined) continue;
      size += (count++ ? 1 : 0) + Buffer.byteLength(JSON.stringify(key)) + 1 + jsonByteLength(value[key], compactNumbers);
    }
    return size;
  }
  throw new TypeError('prompt body must contain JSON values');
};

export const assertManifestPromptBudget = (contentLength) => {
  if (contentLength > MANIFEST_PROMPT_BYTE_LIMIT) throw Object.assign(new Error('manifest queue prompt exceeds 128 MiB of encoded JSON; take or edit the queued item'), {
    status: 413, code: 'queue_request_prompt_body_too_large',
  });
};

const JSON_CHUNK_BYTES = 64 * 1024;
// A multiple of 3 keeps every non-final base64 chunk free of padding.
const FILE_CHUNK_BYTES = 3 * (JSON_CHUNK_BYTES / 4);

const chunksOf = function* (text) {
  const bytes = Buffer.from(text);
  for (let offset = 0; offset < bytes.length; offset += JSON_CHUNK_BYTES) yield bytes.subarray(offset, offset + JSON_CHUNK_BYTES);
};

// The plan retains bounded manifest metadata only. No file read or base64
// allocation occurs until fetch pulls its body under the RPC's abort signal.
export function createManifestPromptBody(body, files, readFile, fileReferences = []) {
  const jsonLength = jsonByteLength(body);
  const filesField = ',"files":[';
  const entries = files.map((file) => ({
    file,
    prefix: `{"uri":"data:${file.mimeType};base64,`,
    // JSON.stringify drops an undefined description or mention.
    suffix: `",${JSON.stringify({ name: file.agentPath ?? file.filename, description: file.description, mention: file.mention }).slice(1)}`,
  }));
  const fileCount = files.length + fileReferences.length;
  const suffix = fileCount ? ']}' : '';
  const contentLength = jsonLength + (fileCount ? Buffer.byteLength(filesField) - 1 : 0) + Buffer.byteLength(suffix)
    + entries.reduce((size, entry, index) => size + (index ? 1 : 0) + Buffer.byteLength(entry.prefix)
      + 4 * Math.ceil(entry.file.size / 3) + Buffer.byteLength(entry.suffix), 0)
    + fileReferences.reduce((size, file, index) => size + (files.length || index ? 1 : 0) + jsonByteLength(file), 0);
  if (!Number.isSafeInteger(contentLength)) throw new TypeError('encoded prompt length is invalid');
  // Freeze an admissible plan as before. Refused plans retain their exact
  // length without allocating an amplified metadata JSON string.
  const json = contentLength <= MANIFEST_PROMPT_BYTE_LIMIT ? JSON.stringify(body) : null;
  const prefix = json === null ? null : fileCount ? `${json.slice(0, -1)}${filesField}` : json;
  const references = json === null ? [] : fileReferences.map((file) => JSON.stringify(file));

  return {
    contentLength,
    createStream(signal) {
      assertManifestPromptBudget(contentLength);
      const encode = async function* () {
        signal.throwIfAborted();
        yield* chunksOf(prefix);
        for (const [index, entry] of entries.entries()) {
          signal.throwIfAborted();
          if (index) yield Buffer.from(',');
          yield* chunksOf(entry.prefix);
          const bytes = await readFile(entry.file, signal);
          signal.throwIfAborted();
          if (bytes.length !== entry.file.size) throw new TypeError('verified prompt file size differs');
          for (let offset = 0; offset < bytes.length; offset += FILE_CHUNK_BYTES) {
            signal.throwIfAborted();
            yield Buffer.from(bytes.subarray(offset, offset + FILE_CHUNK_BYTES).toString('base64'));
          }
          yield* chunksOf(entry.suffix);
        }
        for (const [index, reference] of references.entries()) {
          signal.throwIfAborted();
          if (files.length || index) yield Buffer.from(',');
          yield* chunksOf(reference);
        }
        signal.throwIfAborted();
        yield* chunksOf(suffix);
      };
      const iterator = encode();
      return new ReadableStream({
        async pull(controller) {
          try {
            signal.throwIfAborted();
            const result = await iterator.next();
            if (result.done) controller.close();
            else controller.enqueue(result.value);
          } catch (error) {
            await iterator.return();
            controller.error(error);
          }
        },
        async cancel() { await iterator.return(); },
      }, { highWaterMark: 0 });
    },
  };
}
