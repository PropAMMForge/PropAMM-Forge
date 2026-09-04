/**
 * Check against the bytes Rust wrote.
 *
 * The fixture is created by `programs/propamm-vault/tests/golden_vectors.rs` with
 * the same borsh the program reads data with. Here it is parsed by the SDK's
 * **explicit layout tables** and assembled back: the first catches a layout that
 * diverged from the program, the second a layout that reads correctly but writes wrongly.
 *
 * The fixture is updated only deliberately:
 *   scripts/wsl-build.sh golden   (in WSL — see the header of the script itself)
 */

import { type Address, address, getAddressDecoder, getU64Decoder } from '@solana/kit'
import { describe, expect, it } from 'vitest'
import {
  type FieldKind,
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
import {
  type InstructionName,
  accountDiscriminator,
  eventDiscriminator,
  instructionDiscriminator,
} from '../src/program.js'
import golden from './fixtures/borsh-golden.json'

interface GoldenVector {
  name: string
  codec: string
  skip: number
  hex: string
  fields: Record<string, string>
}

interface Golden {
  generator: string
  addresses: Record<string, number | string>
  vectors: GoldenVector[]
}

// One cast for the whole file: JSON.parse has no type, and describing the union
// of all vector shapes in a type would mean rewriting the fixture once more, in TS.
const fixture = golden as unknown as Golden

/**
 * The shared codec shape without binding to a specific structure.
 *
 * `encode` takes `never` here on purpose: the values in this file are assembled
 * from fixture strings, i.e. their type is known only at runtime. Correctness of
 * the assembly is checked not by the compiler but by the bytes themselves.
 */
interface AnyCodec {
  idlName: string
  layout: readonly (readonly [js: string, wire: string, kind: FieldKind])[]
  size: number
  encode(value: never): Uint8Array
  decode(bytes: Uint8Array, offset?: number): unknown
}

const CODECS: Record<string, AnyCodec> = {
  CapitalMoved: capitalMovedCodec,
  InitializeVaultArgs: initializeVaultArgsCodec,
  QuoteCleared: quoteClearedCodec,
  QuoteUpdate: quoteUpdateCodec,
  QuoteUpdated: quoteUpdatedCodec,
  RiskLimits: riskLimitsCodec,
  SwapArgs: swapArgsCodec,
  Swapped: swappedCodec,
  Vault: vaultCodec,
}

function bytesOf(hex: string): Uint8Array {
  const out = new Uint8Array(hex.length / 2)
  for (let index = 0; index < out.length; index += 1) {
    out[index] = Number.parseInt(hex.slice(index * 2, index * 2 + 2), 16)
  }
  return out
}

function hexOf(bytes: Uint8Array): string {
  return [...bytes].map((byte) => byte.toString(16).padStart(2, '0')).join('')
}

/**
 * Rust writes variants as `BaseToQuote`, the SDK calls them `baseToQuote`.
 *
 * The conversion here is the one and only, and only for the case of the first
 * letter: anything else would mean the names really differ, not that they are spelled alike.
 */
function lowerFirst(name: string): string {
  return name.charAt(0).toLowerCase() + name.slice(1)
}

function fixtureValue(kind: FieldKind, raw: string): Address | bigint | boolean | number | string {
  if (typeof kind !== 'string') {
    const variant = lowerFirst(raw)
    expect(kind.variants, `${kind.idlName}: variant ${raw}`).toContain(variant)
    return variant
  }
  switch (kind) {
    case 'pubkey':
      return address(raw)
    case 'u64':
    case 'u128':
      return BigInt(raw)
    case 'bool':
      return raw === 'true'
    default:
      return Number(raw)
  }
}

/** The expected struct value, assembled from the fixture by the SDK layout table. */
function expectedFrom(codec: AnyCodec, fields: Record<string, string>) {
  const value: Record<string, unknown> = {}
  for (const [js, wire, kind] of codec.layout) {
    const raw = fields[wire]
    // Empty here means the field is named differently on the wire in the SDK than
    // in the IDL — exactly the divergence both names sit in one row for.
    expect(raw, `${codec.idlName}: the fixture has no field "${wire}"`).toBeDefined()
    value[js] = fixtureValue(kind, raw as string)
  }
  return value
}

/** The discriminator the vector's name demands. */
function expectedDiscriminator(name: string): Uint8Array | null {
  const [kind, second] = name.split('/')
  if (kind === 'instruction') {
    return instructionDiscriminator(second as InstructionName)
  }
  if (kind === 'account') {
    return accountDiscriminator(second as string)
  }
  if (kind === 'event') {
    return eventDiscriminator(second as string)
  }
  return null
}

describe('golden vectors', () => {
  it('the fixture is in place and not empty', () => {
    expect(fixture.vectors.length).toBeGreaterThan(0)
    expect(fixture.generator).toContain('golden_vectors.rs')
  })

  it.each(fixture.vectors.map((vector) => [vector.name, vector] as const))(
    '%s',
    (_name, vector) => {
      const bytes = bytesOf(vector.hex)
      const discriminator = expectedDiscriminator(vector.name)

      if (discriminator !== null) {
        expect(vector.skip).toBe(discriminator.length)
        expect(hexOf(bytes.slice(0, discriminator.length))).toBe(hexOf(discriminator))
      }

      const payload = bytes.slice(vector.skip)

      if (vector.codec === 'none') {
        expect(payload.length).toBe(0)
        return
      }

      if (vector.codec === 'pubkey') {
        const [only] = Object.values(vector.fields)
        expect(payload.length).toBe(32)
        expect(getAddressDecoder().decode(payload)).toBe(only)
        return
      }

      if (vector.codec === 'u64') {
        const [only] = Object.values(vector.fields)
        expect(payload.length).toBe(8)
        expect(getU64Decoder().decode(payload)).toBe(BigInt(only as string))
        return
      }

      const codec = CODECS[vector.codec]
      expect(codec, `unknown codec ${vector.codec}`).toBeDefined()
      const known = codec as AnyCodec

      // The layout reads Rust's bytes the way Rust meant them.
      const expected = expectedFrom(known, vector.fields)
      expect(known.decode(bytes, vector.skip)).toEqual(expected)

      // And writes them back byte for byte: a codec that reads correctly but writes
      // wrongly corrupts a transaction silently.
      expect(hexOf(known.encode(expected as never))).toBe(hexOf(payload))
      expect(payload.length).toBe(known.size)
    },
  )
})

describe('fixture sentinels', () => {
  it('u64s wider than double precision do not pass through Number', () => {
    // 2^53 + 1 is in the fixture for exactly this: if somewhere in the chain the
    // value passed through `Number`, it would come back as 2^53 and the test above
    // would go green on the wrong number.
    const vector = fixture.vectors.find((entry) => entry.name === 'instruction/deposit')
    expect(vector).toBeDefined()
    const amount = BigInt((vector as GoldenVector).fields.amount as string)
    expect(amount).toBe(9_007_199_254_740_993n)
    expect(BigInt(Number(amount))).not.toBe(amount)
  })
})
