import type { AttachedFile } from '@/stores/types/sessionTypes';

type AttachmentIdentity = {
  filename: string;
  /** The on-disk path, when the runtime could resolve one for the attachment. */
  agentPath?: string;
};

export const attachmentWireName = (attachment: AttachmentIdentity): string =>
  attachment.agentPath ?? attachment.filename;

type AttachmentPromptFile = Pick<AttachedFile, 'description' | 'mention'> & {
  type: 'file';
  mime: string;
  url: string;
  filename: string;
};

/**
 * Preserve official file metadata when editing or replaying a captured file.
 * Absent metadata stays absent from the serialized prompt part.
 */
export const attachmentPromptFile = (attachment: AttachedFile): AttachmentPromptFile => {
  const file: AttachmentPromptFile = {
    type: 'file',
    mime: attachment.mimeType,
    url: attachment.fileReference?.uri ?? attachment.dataUrl,
    filename: attachment.fileReference?.name ?? attachmentWireName(attachment),
  };
  if (attachment.description !== undefined) file.description = attachment.description;
  if (attachment.mention !== undefined) file.mention = attachment.mention;
  return file;
};

/** Preserve literal backslashes in POSIX filenames. */
export const attachmentBasename = (value: string): string => {
  const normalized = /^(?:[A-Za-z]:\\|\\\\)/.test(value) ? value.replace(/\\/g, '/') : value;
  const segments = normalized.split('/');
  return segments[segments.length - 1] || value;
};
