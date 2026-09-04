/**
 * Internal consistency of the codecs and the field presence guard.
 *
 * This is the weakest of the three layout tests and stands here not instead of
 * the golden vectors but next to them: it catches what does not depend on Rust —
 * a missing field, an unknown enum variant and truncated data.
 */

import { address } from '@solana/kit'
import { describe, expect, it } from 'vitest'
import { VAULT_ACCOUNT_SIZE, decodeVaultAccount, hasQuote, quoteAgeSlots } from '../src/accounts.js'
import {
  type QuoteUpdate,
  type Vault,
  quoteUpdateCodec,
  swapArgsCodec,
  swappedCodec,
  vaultCodec,
} from '../src/codecs.js'
import { accountDiscriminator, withDiscriminator } from '../src/program.js'

const KEY = address('4vJ9JU1bJJE96FWSJKvHsmmFADCg4gpZQff4P3bkLKi')

const QUOTE: QuoteUpdate = {
  midE9: 340_282_366_920_938_463_463_374_607_431_768_211_455n,
  spreadBps: 65_535,
  skewBps: -32_768,
  maxSizeBase: 18_446_744_073_709_551_615n,
}

const VAULT: Vault = {
  owner: KEY,
  pricingAuthority: KEY,
  haltAuthority: KEY,
  baseMint: KEY,
  quoteMint: KEY,
  baseVault: KEY,
  quoteVault: KEY,
  midE9: 12_345_678_901_234_567_890n,
  maxSizeBase: 9_007_199_254_740_993n,
  quoteSlot: 314_159_265n,
  maxQuoteAgeSlots: 4_294_967_295,
  spreadBps: 7,
  skewBps: -7,
  maxSkewBps: 9,
  halted: false,
  bump: 255,
}

describe('type bounds', () => {
  it('extreme values survive a round-trip', () => {
    // The upper bounds of every width together: a narrowed type would break right
    // here, while on "ordinary" numbers it would pass unnoticed.
    expect(quoteUpdateCodec.decode(quoteUpdateCodec.encode(QUOTE))).toEqual(QUOTE)
  })

  it('a negative skew stays negative', () => {
    const value = { ...QUOTE, skewBps: -1 }
    expect(quoteUpdateCodec.decode(quoteUpdateCodec.encode(value)).skewBps).toBe(-1)
  })

  it('Vault round-trip', () => {
    expect(vaultCodec.decode(vaultCodec.encode(VAULT))).toEqual(VAULT)
  })
})

describe('presence guard', () => {
  it('a missing number does not become zero', () => {
    // Without the guard kit would write 0 here, and the bytes would come out valid.
    const { spreadBps: _dropped, ...rest } = QUOTE
    expect(() => quoteUpdateCodec.encode(rest as QuoteUpdate)).toThrow(/spreadBps/)
  })

  it('an explicit undefined fails too', () => {
    expect(() =>
      quoteUpdateCodec.encode({ ...QUOTE, maxSizeBase: undefined as unknown as bigint }),
    ).toThrow(/maxSizeBase/)
  })

  it('a missing bool does not become false', () => {
    const { halted: _dropped, ...rest } = VAULT
    expect(() => vaultCodec.encode(rest as Vault)).toThrow(/halted/)
  })
})

describe('enums', () => {
  it('an unknown variant is refused on encoding', () => {
    expect(() =>
      swapArgsCodec.encode({
        side: 'sideways' as never,
        amountIn: 1n,
        minAmountOut: 0n,
      }),
    ).toThrow(/SwapSide/)
  })

  it('an unknown variant index is refused on reading', () => {
    // An index out of range means the program on the network knows a variant that
    // is not in this copy of the SDK. An `undefined` that travelled into the
    // database would hide that until the history can no longer be re-read.
    const bytes = swappedCodec.encode({
      vault: KEY,
      slot: 1n,
      side: 'baseToQuote',
      amountIn: 1n,
      amountOut: 2n,
      priceE9: 3n,
      quoteSlot: 4n,
      baseAmountAfter: 5n,
      quoteAmountAfter: 6n,
    })
    bytes[40] = 9
    expect(() => swappedCodec.decode(bytes)).toThrow(/SwapSide/)
  })
})

describe('account', () => {
  it('a foreign discriminator is refused', () => {
    const bytes = withDiscriminator(new Uint8Array(8), vaultCodec.encode(VAULT))
    expect(() => decodeVaultAccount(bytes)).toThrow(/Vault/)
  })

  it('truncated data is refused, not padded with zeros', () => {
    const full = withDiscriminator(accountDiscriminator('Vault'), vaultCodec.encode(VAULT))
    expect(full.length).toBe(VAULT_ACCOUNT_SIZE)
    expect(() => decodeVaultAccount(full.slice(0, full.length - 1))).toThrow()
  })

  it('quote age: without a price — null, not zero', () => {
    // A zero here would read as "just posted", i.e. the freshest possible.
    const empty = { ...VAULT, midE9: 0n }
    expect(hasQuote(empty)).toBe(false)
    expect(quoteAgeSlots(empty, 1_000n)).toBeNull()
    expect(quoteAgeSlots(VAULT, VAULT.quoteSlot + 42n)).toBe(42n)
  })
})
