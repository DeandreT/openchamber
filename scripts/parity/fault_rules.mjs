/**
 * Fault-rule matching for the Electron bridge, the same rules
 * `steps.fault_matches` applies to the native proxy: exact method, a path
 * glob where `*` matches anything (including `/`), query strings ignored.
 */

/** A path pattern where `*` matches anything, as `fnmatch` does without classes. */
export const pathPattern = (glob) => new RegExp(`^${glob.split("*").map((part) => part.replace(/[.+?^${}()|[\]\\]/g, "\\$&")).join(".*")}$`)

/** The first rule with hits left that matches at this stage, counted as hit. */
export const claimFault = (rules, hits, { method, url, stage }) => {
  let path
  try { path = new URL(url).pathname } catch { return null }
  const rule = rules.find((candidate) => candidate.remaining > 0 && candidate.when === stage
    && candidate.method === method && pathPattern(candidate.path).test(path))
  if (!rule) return null
  rule.remaining -= 1
  hits.push({ id: rule.id, method, path, when: rule.when, action: rule.action })
  return rule
}
