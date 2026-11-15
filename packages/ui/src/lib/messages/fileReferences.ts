import { z } from 'zod';
import type { AttachedFile } from '@/stores/types/sessionTypes';
import { attachmentBasename } from '@/lib/attachmentIdentity';

const utf8Bytes = (value: string) => new TextEncoder().encode(value).byteLength;
const hasControl = (value: string) => {
    for (const character of value) {
        const code = character.charCodeAt(0);
        if (code < 32 || (code >= 127 && code <= 159)) return true;
    }
    return false;
};
const fileReferenceMentionSchema = z.object({
    start: z.number().int().nonnegative().safe(), end: z.number().int().nonnegative().safe(), text: z.string(),
}).strict().refine((mention) => mention.end >= mention.start);

/** Queue references identify files on the OpenCode server and own no bytes. */
export const fileReferenceSchema = z.object({
    uri: z.string().max(65536).refine((value) => {
        if (!value.startsWith('file:///') || value.startsWith('file:////')
            || /[\\?#\s]/.test(value) || hasControl(value) || utf8Bytes(value) > 65536) return false;
        try {
            // Check the captured path before URL parsing can remove dot segments.
            const path = decodeURIComponent(value.slice('file://'.length));
            const uri = new URL(value);
            return uri.protocol === 'file:' && !uri.host && !uri.search && !uri.hash && path.startsWith('/')
                && !hasControl(path);
        } catch { return false; }
    }),
    name: z.string().min(1).max(4096).refine((value) => utf8Bytes(value) <= 4096 && !hasControl(value)),
    description: z.string().optional(), mention: fileReferenceMentionSchema.optional(),
}).strict();

export type FileReference = z.infer<typeof fileReferenceSchema>;
export type FileReferenceMention = z.infer<typeof fileReferenceMentionSchema>;

/** Apply within one captured source, before combining queued and composer files. */
export const orderFileReferenceAttachments = (files: AttachedFile[]): AttachedFile[] => files.some((file) => file.fileReference)
    ? [...files.filter((file) => !file.fileReference), ...files.filter((file) => file.fileReference)]
    : files;

/** Uses the existing attachment editor while retaining the exact typed record. */
export const materializeFileReference = (reference: FileReference, id: string, mimeType = 'text/plain'): AttachedFile => {
    const attached: AttachedFile = {
        id, file: new File([], attachmentBasename(reference.name), { type: mimeType }),
        filename: attachmentBasename(reference.name), agentPath: reference.name,
        dataUrl: reference.uri, serverPath: reference.uri, mimeType, size: 0, source: 'local', fileReference: reference,
    };
    if (reference.description !== undefined) attached.description = reference.description;
    if (reference.mention !== undefined) attached.mention = reference.mention;
    return attached;
};
