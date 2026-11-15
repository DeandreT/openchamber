import { prompt, type PromptCapabilities, type PromptContentBlock, type PromptInput } from '@cognition-ai/sdk';
import { NativeFailure } from './errors.ts';

export type UserPart = { type: string; text?: string; data?: Uint8Array | string | URL; mediaType?: string; filename?: string };

/** The AI SDK supplies file bytes/base64. External URLs are never fetched by this adapter. */
export function toDevinPrompt(content: string | UserPart[]): PromptInput {
  if (typeof content === 'string') return content;
  return content.map((part, index): PromptContentBlock => {
    if (part.type === 'text' && typeof part.text === 'string') return prompt.text(part.text);
    if (part.type !== 'file' || !part.mediaType) throw new NativeFailure('unsupported');
    if (part.data instanceof URL || part.data === undefined) throw new NativeFailure('unsupported');
    const data = typeof part.data === 'string' ? part.data : Buffer.from(part.data).toString('base64');
    if (typeof part.data === 'string' && !validBase64(data)) throw new NativeFailure('invalid_request');
    if (part.mediaType.startsWith('image/')) return prompt.image(data, part.mediaType);
    if (part.mediaType.startsWith('audio/')) return prompt.audio(data, part.mediaType);
    return prompt.blobResource(`urn:openchamber:attachment:${index}:${encodeURIComponent(part.filename ?? 'file')}`, data, { mimeType: part.mediaType });
  });
}

function validBase64(value: string): boolean {
  if (value.length % 4 !== 0) return false;
  let end = value.length;
  if (value[end - 1] === '=') end--;
  if (value[end - 1] === '=') end--;
  for (let index = 0; index < end; index++) {
    const code = value.charCodeAt(index);
    if (!((code >= 65 && code <= 90) || (code >= 97 && code <= 122) || (code >= 48 && code <= 57) || code === 43 || code === 47)) return false;
  }
  return true;
}

export function assertPromptSupported(input: PromptInput, capabilities?: PromptCapabilities) {
  const blocks = prompt.toPromptBlocks(input);
  const maxBytes = capabilities?.meta['cognition.ai/maxEmbeddedResourceBytes'];
  for (const block of blocks) {
    if (block.type === 'text') continue;
    if (block.type === 'resource_link') throw new NativeFailure('unsupported');
    if (block.type === 'image' && !capabilities?.image) throw new NativeFailure('unsupported');
    if (block.type === 'audio' && !capabilities?.audio) throw new NativeFailure('unsupported');
    if (block.type === 'resource' && !capabilities?.embeddedContext) throw new NativeFailure('unsupported');
    const bytes = block.type === 'resource'
      ? 'blob' in block.resource ? Buffer.byteLength(block.resource.blob, 'base64') : Buffer.byteLength(block.resource.text)
      : Buffer.byteLength(block.data, 'base64');
    if (typeof maxBytes === 'number' && bytes > maxBytes) throw new NativeFailure('unsupported');
  }
}
