import { expect, test } from 'bun:test';
import { attachmentPromptFile } from '@/lib/attachmentIdentity';
import { fileReferenceSchema, materializeFileReference, orderFileReferenceAttachments } from './fileReferences';

test('reference materialization preserves exact identity, description and mention without inventing bytes', () => {
    const reference = fileReferenceSchema.parse({ uri: 'file:///server/report.pdf', name: '/server/report.pdf',
        description: 'original PDF', mention: { start: 1, end: 5, text: '@pdf' } });
    const file = materializeFileReference(reference, 'reference-1', 'application/pdf');
    expect(file.file.size).toBe(0);
    expect(file.filename).toBe('report.pdf');
    expect(file.fileReference).toEqual(reference);
    expect(attachmentPromptFile(file)).toEqual({ type: 'file', mime: 'application/pdf', url: reference.uri,
        filename: reference.name, description: reference.description, mention: reference.mention });
});

test('unsafe URIs and malformed reference mention bounds are refused by the shared boundary', () => {
    for (const uri of ['https://example.test/file', 'data:text/plain,bytes', 'file://host/path', 'file://localhost/path',
        'file:relative', 'file:////host/path', 'file:///path?', 'file:///path#', 'file:///bad%ZZ', 'file:///bad%00']) {
        expect(fileReferenceSchema.safeParse({ uri, name: 'file' }).success).toBe(false);
    }
    for (const mention of [{ start: -1, end: 2, text: 'bad' }, { start: 3, end: 2, text: 'bad' },
        { start: 0, end: Number.MAX_SAFE_INTEGER + 1, text: 'bad' }]) {
        expect(fileReferenceSchema.safeParse({ uri: 'file:///safe/path', name: 'file', mention }).success).toBe(false);
    }
});

test('each source keeps prepared bytes before ordered reference occurrences', () => {
    const reference = materializeFileReference({ uri: 'file:///server/file.pdf', name: 'file.pdf' }, 'ref');
    const bytes = { ...reference, id: 'bytes', fileReference: undefined, dataUrl: 'data:text/plain;base64,YQ==' };
    expect(orderFileReferenceAttachments([reference, bytes, reference])).toEqual([bytes, reference, reference]);
    const original = [bytes];
    expect(orderFileReferenceAttachments(original)).toBe(original);
});

test('reference URI dot-segment normalization cannot erase malformed escapes or encoded controls at the UI boundary', () => {
    for (const uri of [
        'file:///bad%ZZ/../report.pdf', 'file:///bad%00/../report.pdf', 'file:///bad%0a/../report.pdf',
        'file:///bad%C2%80/../report.pdf', 'file:///bad%ZZ/%2e%2e/report.pdf', 'file:///bad%00/./../report.pdf',
        'file:///raw\u00a0space.pdf', 'file:///raw\u2003space/../report.pdf', 'file:///raw\u2028space.pdf',
    ]) {
        expect(fileReferenceSchema.safeParse({ uri, name: 'report.pdf' }).success).toBe(false);
    }
});

test('reference URI dot-segment positive controls preserve encoded spaces, Unicode and ordinary path segments', () => {
    for (const uri of [
        'file:///folder%20name/report.pdf', 'file:///caf%C3%A9/%E6%96%87%E6%9B%B8.pdf',
        'file:///ordinary/../report.pdf', 'file:///ordinary/./report.pdf', 'file:///ordinary/%2e%2e/report.pdf',
    ]) {
        expect(fileReferenceSchema.parse({ uri, name: 'report.pdf' }).uri).toBe(uri);
    }
});
