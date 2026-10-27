/** Accept only the single, vertical 1%-to-transparent sheen used by the composer. */
export function isComposerSheen(value) {
  const alpha = (color) => {
    if (color === 'transparent') return 0
    const functional = /^(rgba?|color)\((.*)\)$/.exec(color)
    if (!functional) return null
    let token
    if (functional[2].includes('/')) token = functional[2].split('/').at(-1).trim()
    else if (functional[1] === 'rgba') token = functional[2].split(',').at(-1).trim()
    else return null
    if (!/^(?:\d*\.)?\d+%?$/.test(token)) return null
    return token.endsWith('%') ? Number(token.slice(0, -1)) / 100 : Number(token)
  }
  const match = /^linear-gradient\((.*)\)$/.exec(value.trim())
  if (!match) return false
  const args = []
  let depth = 0
  let start = 0
  for (let index = 0; index < match[1].length; index++) {
    const character = match[1][index]
    if (character === '(') depth++
    if (character === ')') depth--
    if (depth < 0) return false
    if (character === ',' && depth === 0) {
      args.push(match[1].slice(start, index).trim())
      start = index + 1
    }
  }
  if (depth !== 0) return false
  args.push(match[1].slice(start).trim())
  if (args.length === 3 && /^(180deg|to bottom)$/.test(args[0])) args.shift()
  if (args.length !== 2) return false
  const first = /^(.*\))(?:\s+0%)?$/.exec(args[0])
  const last = /^(.*)\s+50%$/.exec(args[1])
  // CSS color-mix is serialized through float32 (1% becomes 0.00999999).
  return first !== null && last !== null && Math.abs(alpha(first[1]) - 0.01) <= 0.0000001 && alpha(last[1]) === 0
}
