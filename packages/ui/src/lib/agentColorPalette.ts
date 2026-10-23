import type { Theme } from '@/types/theme';
import { chromaticDistance, contrastRatio } from './theme/color';

const SYNTAX_COLORS = [
  { key: 'keyword', color: 'var(--syntax-keyword)' },
  { key: 'type', color: 'var(--syntax-type)' },
  { key: 'function', color: 'var(--syntax-function)' },
  { key: 'number', color: 'var(--syntax-number)' },
  { key: 'string', color: 'var(--syntax-string)' },
  { key: 'operator', color: 'var(--syntax-operator)' },
  { key: 'variable', color: 'var(--syntax-variable)' },
] as const;
const MIN_SEPARATION = 0.055;

/** Ordered, contrast-filtered syntax roles used by the agent color allocator. */
export function agentColorPalette(theme: Theme) {
  const { surface, syntax, status } = theme.colors;
  const backgrounds = [surface.background, surface.muted, surface.elevated];
  const distance = (a: string, b: string) => Math.min(...backgrounds.map((background) =>
    chromaticDistance(a, b, background, surface.background) ?? (a === b ? 0 : 1)));
  const candidates = SYNTAX_COLORS.map((color, index) => ({ ...color, index, value: syntax.base[color.key] }));
  const visible = candidates.filter((color) => backgrounds.every((background) =>
    (contrastRatio(color.value, background, surface.background) ?? 1.5) >= 1.5));
  const remaining = [...(visible.length ? visible : candidates)];
  const palette: typeof candidates = [];
  const chosen = [status.success];
  while (remaining.length) {
    let bestIndex = 0;
    let bestDistance = -1;
    for (let index = 0; index < remaining.length; index++) {
      const separation = Math.min(...chosen.map((value) => distance(remaining[index].value, value)));
      if (separation > bestDistance) { bestIndex = index; bestDistance = separation; }
    }
    if (bestDistance < MIN_SEPARATION) break;
    const [color] = remaining.splice(bestIndex, 1);
    palette.push(color);
    chosen.push(color.value);
  }
  // Sparse themes still use one of their own syntax roles.
  if (!palette.length) palette.push(visible[0] ?? candidates[0]);
  return palette.map((entry) => ({ ...entry, colorful: distance(entry.value, '#808080') >= 0.035 }));
}
