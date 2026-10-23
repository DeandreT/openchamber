import type { Agent } from '@/lib/opencode/model';
import type { Theme } from '@/types/theme';
import { agentColorPalette } from './agentColorPalette';
import { isHexColor } from './colorPresets';

/** `color` is a CSS color: a theme variable, or the hex the agent's config sets. */
export type AgentColor = { color: string };

const BUILD_COLOR: AgentColor = { color: 'var(--status-success)' };

// OpenCode's v1 migration writes #aaaaaa for theme names it cannot keep
// ("primary", "accent"), so it stands for "no colour chosen", not grey.
const MIGRATED_COLOR_PLACEHOLDER = '#aaaaaa';

/** The colour set in the agent's config, when it is one OpenCode v2 accepts (six-digit hex only). */
export const configuredAgentColor = (color: string | null | undefined): string | null => {
  const value = color?.trim();
  if (!value || !isHexColor(value) || value.toLowerCase() === MIGRATED_COLOR_PLACEHOLDER) return null;
  return value;
};

function hashName(name: string): number {
  let hash = 0;
  for (let index = 0; index < name.length; index++) hash = Math.imul(hash, 31) + name.charCodeAt(index) | 0;
  return hash >>> 0;
}

/** Allocate against the complete visible roster, never a filtered picker list.
 * Build owns success; other agents exhaust distinct syntax colors before reuse.
 * A colour set in an agent's config wins over the allocation, Build included;
 * the others keep the colours they would have had. */
export function createAgentColorResolver(theme: Theme, agents: readonly (Pick<Agent, 'name'> & Partial<Pick<Agent, 'mode' | 'color'>>)[]) {
  const palette = agentColorPalette(theme);
  const ordered = [...agents].sort((a, b) => Number(a.mode === 'subagent') - Number(b.mode === 'subagent')
    || (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));
  const assigned = new Map<string, AgentColor>();
  const used = new Set<number>();
  for (const agent of ordered) {
    if (agent.name === 'build' || assigned.has(agent.name)) continue;
    if (used.size === palette.length) used.clear();
    const available = palette.map((_, index) => index).filter((index) => !used.has(index));
    // Prefer colored syntax accents to the neutral code foreground while possible.
    const colorful = available.filter((index) => palette[index].colorful);
    const choices = colorful.length ? colorful : available;
    const index = choices[hashName(agent.name) % choices.length];
    assigned.set(agent.name, { color: palette[index].color });
    used.add(index);
  }
  const configured = new Map<string, AgentColor>();
  for (const agent of agents) {
    const color = configuredAgentColor(agent.color);
    if (color) configured.set(agent.name, { color });
  }
  return (name: string | undefined): AgentColor => {
    const own = name ? configured.get(name) : undefined;
    if (own) return own;
    if (!name || name === 'build') return BUILD_COLOR;
    return assigned.get(name) ?? { color: palette[hashName(name) % palette.length].color };
  };
}
