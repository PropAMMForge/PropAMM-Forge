/**
 * Reading events from logs.
 *
 * The main thing here is not byte parsing (the golden vectors already proved
 * that) but **attribution**: `Program data:` does not carry the address of the
 * program that wrote it, and in a router transaction our events sit mixed with foreign ones.
 */

import { address, getBase64Decoder } from '@solana/kit'
import { describe, expect, it } from 'vitest'
import { decodeVaultEvent, decodeVaultEventFromBase64, parseVaultEvents } from '../src/events.js'
import { PROPAMM_VAULT_PROGRAM_ADDRESS } from '../src/program.js'
import golden from './fixtures/borsh-golden.json'

interface GoldenVector {
  name: string
  hex: string
}

const vectors = (golden as unknown as { vectors: GoldenVector[] }).vectors
const base64 = getBase64Decoder()

function bytesOf(hex: string): Uint8Array {
  const out = new Uint8Array(hex.length / 2)
  for (let index = 0; index < out.length; index += 1) {
    out[index] = Number.parseInt(hex.slice(index * 2, index * 2 + 2), 16)
  }
  return out
}

function eventBytes(name: string): Uint8Array {
  const vector = vectors.find((entry) => entry.name === name)
  if (vector === undefined) {
    throw new Error(`no vector ${name}`)
  }
  return bytesOf(vector.hex)
}

const SWAPPED = eventBytes('event/Swapped')
const CLEARED = eventBytes('event/QuoteCleared/CapitalWithdrawn')

function programData(bytes: Uint8Array): string {
  return `Program data: ${base64.decode(bytes)}`
}

const OURS = PROPAMM_VAULT_PROGRAM_ADDRESS
const RIVAL = address('11111111111111111111111111111112')

describe('parsing one event', () => {
  it('recognizes its own discriminators', () => {
    const event = decodeVaultEvent(SWAPPED)
    expect(event?.name).toBe('Swapped')
    expect(decodeVaultEvent(CLEARED)?.name).toBe('QuoteCleared')
  })

  it('foreign bytes give null, not an exception', () => {
    // The logs also contain foreign `Program data:`; throwing on each would mean
    // stopping the collector on a transaction we have nothing to do with.
    expect(decodeVaultEvent(new Uint8Array(64))).toBeNull()
  })

  it('reads from base64, as it sits in the log', () => {
    expect(decodeVaultEventFromBase64(base64.decode(SWAPPED))?.name).toBe('Swapped')
  })
})

describe('attribution in logs', () => {
  it('takes only what our program wrote', () => {
    // Another venue in the same transaction wrote a `Program data:` with our
    // discriminator. Eight bytes are not a signature, and the only thing that
    // tells the events apart is who was on top of the call stack at that moment.
    const logs = [
      `Program ${RIVAL} invoke [1]`,
      programData(SWAPPED),
      `Program ${RIVAL} success`,
      `Program ${OURS} invoke [1]`,
      'Program log: Instruction: Swap',
      programData(SWAPPED),
      `Program ${OURS} success`,
    ]
    const found = parseVaultEvents(logs)
    expect(found).toHaveLength(1)
    expect(found[0]?.logIndex).toBe(5)
  })

  it('sees our program on a nested call', () => {
    // This is exactly what a swap inside a route transaction looks like (FR-019).
    const logs = [
      `Program ${RIVAL} invoke [1]`,
      `Program ${OURS} invoke [2]`,
      programData(SWAPPED),
      `Program ${OURS} success`,
      programData(CLEARED),
      `Program ${RIVAL} success`,
    ]
    const found = parseVaultEvents(logs)
    expect(found.map((entry) => entry.event.name)).toEqual(['Swapped'])
  })

  it('keeps the order and the line numbers', () => {
    const logs = [
      `Program ${OURS} invoke [1]`,
      programData(CLEARED),
      programData(SWAPPED),
      `Program ${OURS} success`,
    ]
    const found = parseVaultEvents(logs)
    expect(found.map((entry) => [entry.event.name, entry.logIndex])).toEqual([
      ['QuoteCleared', 1],
      ['Swapped', 2],
    ])
  })

  it('a failed call is popped from the stack the same way', () => {
    const logs = [
      `Program ${OURS} invoke [1]`,
      `Program ${OURS} failed: custom program error: 0x1771`,
      programData(SWAPPED),
    ]
    expect(parseVaultEvents(logs)).toEqual([])
  })

  it('a foreign program key is taken from the argument', () => {
    // On a local network the program lives under a different key, and attribution
    // has to follow it, not `declare_id!`.
    const logs = [`Program ${RIVAL} invoke [1]`, programData(SWAPPED), `Program ${RIVAL} success`]
    expect(parseVaultEvents(logs, RIVAL)).toHaveLength(1)
    expect(parseVaultEvents(logs)).toEqual([])
  })

  it('the data is parsed to the end, not to the first match', () => {
    const logs = [`Program ${OURS} invoke [1]`, programData(SWAPPED), `Program ${OURS} success`]
    const event = parseVaultEvents(logs)[0]?.event
    expect(event?.name).toBe('Swapped')
    if (event?.name === 'Swapped') {
      expect(event.data.side).toBe('quoteToBase')
      expect(event.data.baseAmountAfter).toBe(9_007_199_254_740_993n)
    }
  })
})
