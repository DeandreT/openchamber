import fs from 'node:fs';
import path from 'node:path';
import { Readable } from 'node:stream';
import { createHash, randomUUID } from 'node:crypto';
import { BLOB_BYTE_LIMIT } from './payloads.js';
import { queueRequestError } from './receipts.js';

const HEADER_LIMIT = 65536;
const hash = (bytes) => createHash('sha256').update(bytes).digest('hex');
const encodeMetadata = (metadata) => JSON.stringify(metadata, Object.keys(metadata).sort());
const privateEntry = (stat, directory = false) => {
  if (!(directory ? stat.isDirectory() : stat.isFile()) || (!directory && stat.nlink !== 1)
      || (process.platform !== 'win32' && ((stat.mode & 0o077) !== 0 || stat.uid !== process.getuid()))) {
    throw new Error('unsafe queue blob storage entry');
  }
};
const syncDirectory = async (directory) => {
  if (process.platform === 'win32') return;
  const handle = await fs.promises.open(directory, fs.constants.O_RDONLY | fs.constants.O_DIRECTORY | fs.constants.O_NOFOLLOW);
  try { await handle.sync(); } finally { await handle.close(); }
};

// Paths are derived only from the captured owner. Filenames never select a path.
export const blobIdentity = ({ sessionId, directory, requestId, attachmentId }) => hash(JSON.stringify([sessionId, directory, requestId, attachmentId]));

export function createQueueBlobStore(dataDir) {
  const root = async () => {
    await fs.promises.mkdir(dataDir, { recursive: true });
    const parent = await fs.promises.realpath(dataDir);
    const directory = path.join(parent, 'message-queue-blobs');
    try { await fs.promises.mkdir(directory, { mode: 0o700 }); } catch (error) { if (error.code !== 'EEXIST') throw error; }
    privateEntry(await fs.promises.lstat(directory), true);
    if (await fs.promises.realpath(directory) !== directory) throw new Error('unsafe queue blob directory');
    await syncDirectory(parent);
    return directory;
  };
  const filename = (directory, id) => {
    if (!/^[a-f0-9]{64}$/.test(id)) throw new TypeError('invalid blobId');
    return path.join(directory, `${id}.blob`);
  };
  const read = async (metadata, signal) => {
    signal?.throwIfAborted();
    const directory = await root();
    const target = filename(directory, metadata.blobId);
    const initial = await fs.promises.lstat(target);
    privateEntry(initial);
    const handle = await fs.promises.open(target, fs.constants.O_RDONLY | (fs.constants.O_NOFOLLOW ?? 0));
    try {
      const stat = await handle.stat();
      privateEntry(stat);
      if (initial.dev !== stat.dev || initial.ino !== stat.ino || stat.size > BLOB_BYTE_LIMIT + HEADER_LIMIT + 4) throw new Error('unsafe queue blob file');
      const prefix = Buffer.alloc(4);
      if ((await handle.read(prefix, 0, 4, 0)).bytesRead !== 4) throw new Error('invalid queue blob header');
      const length = prefix.readUInt32BE();
      if (length > HEADER_LIMIT || stat.size !== 4 + length + metadata.size) throw queueRequestError('blob owner or metadata differs', 409, 'queue_request_conflict');
      const header = Buffer.alloc(length);
      if ((await handle.read(header, 0, length, 4)).bytesRead !== length) throw new Error('incomplete queue blob header');
      if (header.toString('utf8') !== encodeMetadata(metadata)) throw queueRequestError('blob owner or metadata differs', 409, 'queue_request_conflict');
      const bytes = Buffer.alloc(metadata.size);
      let offset = 0;
      while (offset < bytes.length) {
        signal?.throwIfAborted();
        const result = await handle.read(bytes, offset, signal ? Math.min(65536, bytes.length - offset) : bytes.length - offset, 4 + length + offset);
        if (!result.bytesRead) throw new Error('incomplete queue blob bytes');
        offset += result.bytesRead;
      }
      signal?.throwIfAborted();
      if (hash(bytes) !== metadata.sha256) throw new Error('queue blob digest mismatch');
      privateEntry(await handle.stat());
      return bytes;
    } finally { await handle.close(); }
  };
  const write = async (metadata, body) => {
    const directory = await root();
    const target = filename(directory, metadata.blobId);
    const temporary = path.join(directory, `${randomUUID()}.tmp`);
    const header = Buffer.from(encodeMetadata(metadata));
    if (header.length > HEADER_LIMIT) throw new TypeError('blob metadata exceeds header limit');
    const prefix = Buffer.alloc(4);
    prefix.writeUInt32BE(header.length);
    const handle = await fs.promises.open(temporary, 'wx', 0o600);
    let size = 0;
    const digest = createHash('sha256');
    try {
      await handle.writeFile(Buffer.concat([prefix, header]));
      // A request stream stays open on an early exit so the route can drain it.
      const chunks = Buffer.isBuffer(body) ? [body] : (body instanceof Readable ? body.iterator({ destroyOnReturn: false }) : body);
      for await (const chunk of chunks) {
        if (!Buffer.isBuffer(chunk) && !(chunk instanceof Uint8Array)) throw new TypeError('upload body must be binary');
        size += chunk.length;
        if (size > BLOB_BYTE_LIMIT || size > metadata.size) throw queueRequestError('attachment exceeds declared or prepared size', 413, 'queue_request_too_large');
        digest.update(chunk);
        await handle.writeFile(chunk);
      }
      if (size !== metadata.size || digest.digest('hex') !== metadata.sha256) throw new TypeError('attachment size or SHA-256 differs');
      privateEntry(await handle.stat());
      await handle.sync();
    } catch (error) {
      await handle.close();
      await fs.promises.unlink(temporary).catch(() => {});
      throw error;
    }
    await handle.close();
    try {
      try {
        // Publish the synced envelope without replacing an existing identity.
        // Our private staging link is removed before acceptance.
        await fs.promises.link(temporary, target);
      } catch (error) {
        if (error.code !== 'EEXIST') throw error;
        // A prior commit may predate the root index. Its immutable header
        // still enforces exact replay after a lost checkpoint.
        await read(metadata);
      }
      await fs.promises.unlink(temporary);
      privateEntry(await fs.promises.lstat(target));
      await syncDirectory(directory);
    } catch (error) {
      await fs.promises.unlink(temporary).catch(() => {});
      throw error;
    }
  };
  const remove = async (metadata) => {
    const directory = await root();
    try {
      await read(metadata);
      await fs.promises.unlink(filename(directory, metadata.blobId));
    } catch (error) { if (error.code !== 'ENOENT') throw error; }
    await syncDirectory(directory);
  };
  const storageBoundary = (operation) => async (...args) => {
    try { return await operation(...args); } catch (error) {
      if (typeof error.code === 'string' && !error.code.startsWith('queue_request_')) {
        throw Object.assign(new Error('queue attachment storage unavailable'), { status: 503 });
      }
      throw error;
    }
  };
  return { read: storageBoundary(read), write: storageBoundary(write), remove: storageBoundary(remove) };
}
