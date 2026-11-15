import assert from 'node:assert/strict'
import test from 'node:test'
import { isComposerSheen } from './composer_sheen.mjs'

test('accepts only the computed single vertical composer sheen', () => {
  for (const value of [
    'linear-gradient(rgba(219, 215, 204, 0.01), rgba(0, 0, 0, 0) 50%)',
    'linear-gradient(180deg, color(srgb 0.8 0.7 0.6 / 0.01) 0%, transparent 50%)',
    'linear-gradient(color(srgb 0.858824 0.843137 0.8 / 0.00999999), rgba(0, 0, 0, 0) 50%)',
    'linear-gradient(to bottom, rgb(219 215 204 / 1%), transparent 50%)',
  ]) assert.equal(isComposerSheen(value), true, value)
  for (const value of [
    'none',
    'linear-gradient(rgba(219,215,204,0.01), transparent 150%)',
    'linear-gradient(90deg, rgba(219,215,204,0.01), transparent 50%)',
    'linear-gradient(rgba(219,215,204,0.02), transparent 50%)',
    'linear-gradient(rgba(219,215,204,0.0101), transparent 50%)',
    'linear-gradient(rgba(219,215,204,0.01), red 25%, transparent 50%)',
    'linear-gradient(rgba(219,215,204,0.01), transparent 50%), linear-gradient(90deg, white, transparent)',
    'linear-gradient(rgba(219,215,204,0.01), rgba(0,0,0,0.1) 50%)',
  ]) assert.equal(isComposerSheen(value), false, value)
})
