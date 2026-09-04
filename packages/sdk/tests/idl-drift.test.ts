/**
 * Layout tables against the vendored IDL.
 *
 * The golden vectors prove that the SDK reads **the** bytes the program writes.
 * This file proves something else: that the tables describe **all** the program's
 * types and none is left undescribed. The difference matters — a new type in Rust
 * will not make the golden vectors red, because there simply will be no vector for it.
 *
 * Also checked here is what every table row carries two names for: `mid_e9` in
 * the IDL and `midE9` in TS — and no place where the crossing between them is
 * done other than by this table.
 */

import { describe, expect, it } from 'vitest'
import {
  CAPITAL_FLOW,
  type EnumKind,
  type FieldKind,
  type Layout,
  QUOTE_CLEAR_REASON,
  SWAP_SIDE,
  TREASURY_SIDE,
  capitalMovedCodec,
  initializeVaultArgsCodec,
  quoteClearedCodec,
  quoteUpdateCodec,
  quoteUpdatedCodec,
  riskLimitsCodec,
  swapArgsCodec,
  swappedCodec,
  vaultCodec,
} from '../src/codecs.js'
import type { IdlType } from '../src/idl-schema.js'
import { PROPAMM_VAULT_IDL } from '../src/idl.js'

interface Described {
  idlName: string
  layout: Layout<never>
  size: number
}

const STRUCTS: readonly Described[] = [
  capitalMovedCodec,
  initializeVaultArgsCodec,
  quoteClearedCodec,
  quoteUpdateCodec,
  quoteUpdatedCodec,
  riskLimitsCodec,
  swapArgsCodec,
  swappedCodec,
  vaultCodec,
] as unknown as readonly Described[]

const ENUMS: readonly EnumKind[] = [CAPITAL_FLOW, QUOTE_CLEAR_REASON, SWAP_SIDE, TREASURY_SIDE]

function lowerFirst(name: string): string {
  return name.charAt(0).toLowerCase() + name.slice(1)
}

/** The SDK field kind in IDL terms — for a comparison without guessing. */
function asIdlType(kind: FieldKind): IdlType {
  return typeof kind === 'string' ? kind : { defined: { name: kind.idlName } }
}

function typeDef(name: string) {
  const found = PROPAMM_VAULT_IDL.types.find((entry) => entry.name === name)
  expect(found, `the IDL has no type ${name}`).toBeDefined()
  return found as NonNullable<typeof found>
}

describe('layout tables describe the IDL', () => {
  it.each(STRUCTS.map((codec) => [codec.idlName, codec] as const))('%s', (name, codec) => {
    const definition = typeDef(name)
    expect(definition.type.kind).toBe('struct')
    const fields = definition.type.kind === 'struct' ? definition.type.fields : []

    // The order matters: Borsh writes the fields consecutively, without names.
    expect(codec.layout.map(([, wire]) => wire)).toEqual(fields.map((field) => field.name))
    expect(codec.layout.map(([, , kind]) => asIdlType(kind))).toEqual(
      fields.map((field) => field.type),
    )
  })

  it.each(ENUMS.map((kind) => [kind.idlName, kind] as const))('%s', (name, kind) => {
    const definition = typeDef(name)
    expect(definition.type.kind).toBe('enum')
    const variants = definition.type.kind === 'enum' ? definition.type.variants : []

    // The order here is the value itself: Borsh writes the variant index, not the name.
    expect(kind.variants).toEqual(variants.map((variant) => lowerFirst(variant.name)))
  })
})

describe('coverage', () => {
  it('every IDL type has a table in the SDK', () => {
    const described = new Set([
      ...STRUCTS.map((codec) => codec.idlName),
      ...ENUMS.map((kind) => kind.idlName),
    ])
    const missing = PROPAMM_VAULT_IDL.types
      .map((entry) => entry.name)
      .filter((name) => !described.has(name))
    // Empty here is not a formality: an undescribed type means the program can put
    // on the wire what the SDK will not read, and that will surface on data.
    expect(missing).toEqual([])
  })

  it('names in TS differ from names on the wire only by case', () => {
    for (const codec of STRUCTS) {
      for (const [js, wire] of codec.layout) {
        expect(js.replaceAll(/[A-Z]/g, (letter) => `_${letter.toLowerCase()}`)).toBe(wire)
      }
    }
  })

  it('account and event discriminators are eight bytes each and all distinct', () => {
    const all = [...PROPAMM_VAULT_IDL.accounts, ...PROPAMM_VAULT_IDL.events]
    for (const entry of all) {
      expect(entry.discriminator).toHaveLength(8)
    }
    const seen = new Set(all.map((entry) => entry.discriminator.join(',')))
    expect(seen.size).toBe(all.length)
  })
})
