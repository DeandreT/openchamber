import React from 'react';

/**
 * Collapses a toolbar step by step only while its row does not fit.
 *
 * The steps are attributes set on the toolbar in order, each hiding more (the
 * CSS is in `index.css`). A step is taken only while the row overflows or one
 * of its texts that may truncate (`clippedText`) is cut off, so labels go
 * before a branch name is clipped.
 *
 * CSS container breakpoints cannot do this: the row's content changes with the
 * scope (a base picker, a pull request picker, a count) and with the language,
 * so a fixed width either hides labels that fit or lets the row overflow and
 * the icons at its end overlap.
 *
 * Measuring forces a layout, so after a render it runs only when the row
 * mounted or its text changed; a resize or a web font loading refits through
 * the listeners.
 */
interface ToolbarFit {
  steps: readonly string[];
  /** Texts whose truncation counts as not fitting; none when omitted. */
  clippedText?: string;
}

/**
 * The diff toolbar: first the walkthrough label, then the review and expand
 * labels and the scope count. Its `.truncate` texts include the base and
 * commit pickers.
 */
const DIFF_TOOLBAR_FIT: ToolbarFit = {
  steps: ['data-diff-toolbar-compact-walkthrough', 'data-diff-toolbar-compact'],
  clippedText: '.truncate',
};

export const useToolbarFit = (ref: React.RefObject<HTMLElement | null>, fit: ToolbarFit): void => {
  // The row last fitted and its text and child count then; reading these forces no layout.
  const fittedRef = React.useRef<{ container: HTMLElement; signature: string } | null>(null);
  const observedRef = React.useRef<{ container: HTMLElement; release: () => void } | null>(null);
  const fitRef = React.useRef(fit);
  fitRef.current = fit;

  React.useLayoutEffect(() => {
    const container = ref.current;
    if (observedRef.current?.container !== container) {
      observedRef.current?.release();
      observedRef.current = container ? { container, release: observeResizes(container, () => fitRef.current) } : null;
    }
    if (!container) return;
    const signature = `${container.childElementCount}\0${container.textContent ?? ''}`;
    const fitted = fittedRef.current;
    if (fitted?.container === container && fitted.signature === signature) return;
    fittedRef.current = { container, signature };
    fitToolbar(container, fitRef.current);
  });

  React.useEffect(() => () => {
    observedRef.current?.release();
    observedRef.current = null;
  }, []);
};

export const useDiffToolbarFit = (ref: React.RefObject<HTMLElement | null>): void => useToolbarFit(ref, DIFF_TOOLBAR_FIT);

const fits = (container: HTMLElement, clippedText: string | undefined): boolean => {
  if (container.scrollWidth > container.clientWidth + 1) return false;
  if (!clippedText) return true;
  for (const text of container.querySelectorAll<HTMLElement>(clippedText)) {
    if (text.scrollWidth > text.clientWidth + 1) return false;
  }
  return true;
};

export const fitToolbar = (container: HTMLElement, { steps, clippedText }: ToolbarFit = DIFF_TOOLBAR_FIT): void => {
  for (const step of steps) container.removeAttribute(step);
  for (const step of steps) {
    if (fits(container, clippedText)) return;
    container.setAttribute(step, '');
  }
};

const observeResizes = (container: HTMLElement, currentFit: () => ToolbarFit): (() => void) => {
  let inCallback = false;
  const refit = () => {
    // Hiding a label never resizes the toolbar (its width comes from the
    // header around it), but guard the re-entry anyway.
    if (inCallback) return;
    inCallback = true;
    fitToolbar(container, currentFit());
    inCallback = false;
  };
  const fonts = globalThis.document?.fonts ?? null;
  fonts?.addEventListener('loadingdone', refit);
  const observer = new ResizeObserver(refit);
  observer.observe(container);
  return () => {
    fonts?.removeEventListener('loadingdone', refit);
    observer.disconnect();
  };
};
