/**
 * Borsh layouts for everything the program puts on the wire: instruction
 * arguments, the `Vault` account, events.
 *
 * # Why the layouts are explicit rather than from the IDL
 *
 * The Anchor runtime in JS can encode by IDL — and that is exactly why it is not
 * in this file. Encoding by IDL turns any layout mistake into a **data**
 * mistake: it does not fail, it writes different bytes. An explicit table gives
 * a third text next to Rust and the IDL, and all three are checked by tests:
 *
 * - `tests/idl-drift.test.ts` — the table against the IDL: fields, order, types;
 * - `tests/golden.test.ts` — the table against the bytes Rust wrote;
 * - `tests/codecs.test.ts` — the table against itself on sentinels.
 *
 * The first catches divergence from the program, the second divergence from
 * borsh, the third a missing field.
 *
 * # Two names in one row
 *
 * Every table row carries **both** names of the field: what it is called in TS
 * (camelCase) and in the IDL (snake_case). That is not redundancy. On IssuerForge
 * the IDL type and the runtime value diverged by exactly the case, the compiler
 * did not see it, and it surfaced on live data. As long as both names sit side by
 * side, the check against the IDL can be automatic.
 *
 * # On the presence guard
 *
 * `getU16Encoder().encode(undefined)` in @solana/kit **silently writes zero**, and
 * `getBooleanEncoder()` writes `false`. That is, a missing field yields valid bytes
 * with the wrong number: exactly the class that already bit on the Anchor coder
 * from the Rust side. So [`structCodec`] checks the presence of every field before
 * encoding and fails with the field name rather than staying silent.
 */

import {
  type Address,
  type Decoder,
  type Encoder,
  type ReadonlyUint8Array,
  getAddressDecoder,
  getAddressEncoder,
  getBooleanDecoder,
  getBooleanEncoder,
  getI16Decoder,
  getI16Encoder,
  getStructDecoder,
  getStructEncoder,
  getU8Decoder,
  getU8Encoder,
  getU16Decoder,
  getU16Encoder,
  getU32Decoder,
  getU32Encoder,
  getU64Decoder,
  getU64Encoder,
  getU128Decoder,
  getU128Encoder,
} from '@solana/kit'

// ─── Field kinds ─────────────────────────────────────────────────────────────

/** The scalar types that occur in our program. */
export type ScalarKind = 'bool' | 'i16' | 'pubkey' | 'u128' | 'u16' | 'u32' | 'u64' | 'u8'

/**
 * A scalar Borsh enum: one byte with the variant index.
 *
 * `variants` is the order from Rust, and the order itself is the value here: Borsh
 * writes the index, not the name, so reordered variants give a valid event with
 * the opposite meaning. `idlName` keeps the link to the type description in the IDL for the check.
 */
export interface EnumKind {
  readonly idlName: string
  readonly variants: readonly string[]
}

export type FieldKind = EnumKind | ScalarKind

/** A table row: the name in TS, the name on the wire, the kind. */
export type Field<K extends string> = readonly [js: K, wire: string, kind: FieldKind]

/** Layout table of the structure `T`. */
export type Layout<T> = readonly Field<Extract<keyof T, string>>[]

function isEnumKind(kind: FieldKind): kind is EnumKind {
  return typeof kind !== 'string'
}

// ─── Scalar codecs ───────────────────────────────────────────────────────────

const SCALAR_ENCODERS: { readonly [K in ScalarKind]: Encoder<never> } = {
  bool: getBooleanEncoder() as Encoder<never>,
  i16: getI16Encoder() as Encoder<never>,
  pubkey: getAddressEncoder() as Encoder<never>,
  u128: getU128Encoder() as Encoder<never>,
  u16: getU16Encoder() as Encoder<never>,
  u32: getU32Encoder() as Encoder<never>,
  u64: getU64Encoder() as Encoder<never>,
  u8: getU8Encoder() as Encoder<never>,
}

const SCALAR_DECODERS: { readonly [K in ScalarKind]: Decoder<unknown> } = {
  bool: getBooleanDecoder(),
  i16: getI16Decoder(),
  pubkey: getAddressDecoder(),
  u128: getU128Decoder(),
  u16: getU16Decoder(),
  u32: getU32Decoder(),
  u64: getU64Decoder(),
  u8: getU8Decoder(),
}

/**
 * Codec of a scalar enum: variant name ↔ its index.
 *
 * The decoder fails on an unknown index instead of returning `undefined`. An
 * unknown index means the program on the network has a variant this copy of the
 * SDK does not — and an `undefined` that travelled on into the database would
 * hide that until the history can no longer be re-read.
 */
function enumEncoder(kind: EnumKind): Encoder<never> {
  const inner = getU8Encoder()
  return {
    fixedSize: 1,
    encode: (value: never) => inner.encode(variantIndex(kind, value)),
    write: (value: never, bytes: Uint8Array, offset: number) =>
      inner.write(variantIndex(kind, value), bytes, offset),
  } as Encoder<never>
}

function variantIndex(kind: EnumKind, value: unknown): number {
  const index = kind.variants.indexOf(value as string)
  if (index < 0) {
    throw new Error(
      `${kind.idlName}: unknown variant ${JSON.stringify(value)}; ` +
        `expected one of ${kind.variants.join(', ')}`,
    )
  }
  return index
}

function enumDecoder(kind: EnumKind): Decoder<unknown> {
  const inner = getU8Decoder()
  return {
    fixedSize: 1,
    decode: (bytes: ReadonlyUint8Array, offset = 0) =>
      variantName(kind, inner.decode(bytes, offset)),
    read: (bytes: ReadonlyUint8Array, offset: number) => {
      const [index, next] = inner.read(bytes, offset)
      return [variantName(kind, index), next]
    },
  } as Decoder<unknown>
}

function variantName(kind: EnumKind, index: number): string {
  const name = kind.variants[index]
  if (name === undefined) {
    throw new Error(
      `${kind.idlName}: variant index ${index} is out of range; ` +
        `this copy of the SDK knows ${kind.variants.length} — the vendored IDL may be stale`,
    )
  }
  return name
}

function fieldEncoder(kind: FieldKind): Encoder<never> {
  return isEnumKind(kind) ? enumEncoder(kind) : SCALAR_ENCODERS[kind]
}

function fieldDecoder(kind: FieldKind): Decoder<unknown> {
  return isEnumKind(kind) ? enumDecoder(kind) : SCALAR_DECODERS[kind]
}

// ─── Struct codec ────────────────────────────────────────────────────────────

/** A fixed-size struct codec assembled from a layout table. */
export interface StructCodec<T> {
  /** The type name in the IDL — also used in error messages. */
  readonly idlName: string
  readonly layout: Layout<T>
  /** Size in bytes without the discriminator. */
  readonly size: number
  encode(value: T): Uint8Array
  /** Reads from `offset` — to skip the 8 bytes of discriminator. */
  decode(bytes: ReadonlyUint8Array, offset?: number): T
}

export function structCodec<T extends object>(idlName: string, layout: Layout<T>): StructCodec<T> {
  const encoder = getStructEncoder(
    layout.map(([js, , kind]) => [js, fieldEncoder(kind)] as const) as never,
  )
  const decoder = getStructDecoder(
    layout.map(([js, , kind]) => [js, fieldDecoder(kind)] as const) as never,
  )
  const size = encoder.fixedSize

  return {
    idlName,
    layout,
    size,
    encode(value: T): Uint8Array {
      // A guard, not pedantry: kit would encode a missing number as zero, and the
      // bytes would come out valid. See the file header.
      for (const [js] of layout) {
        if (!Object.hasOwn(value, js) || (value as Record<string, unknown>)[js] === undefined) {
          throw new Error(`${idlName}: missing field "${js}" — encoding would write zero`)
        }
      }
      return new Uint8Array(encoder.encode(value as never))
    },
    decode(bytes: ReadonlyUint8Array, offset = 0): T {
      if (bytes.length - offset < size) {
        throw new Error(
          `${idlName}: expected ${size} B from offset ${offset}, got ${bytes.length - offset}`,
        )
      }
      return decoder.read(bytes, offset)[0] as T
    },
  }
}

// ─── Enums ───────────────────────────────────────────────────────────────────

export const SWAP_SIDE: EnumKind = {
  idlName: 'SwapSide',
  variants: ['baseToQuote', 'quoteToBase'],
}
/** Swap direction, named from the trader's side. */
export type SwapSide = 'baseToQuote' | 'quoteToBase'

export const TREASURY_SIDE: EnumKind = {
  idlName: 'TreasurySide',
  variants: ['base', 'quote'],
}
/** The side of the pair a treasury belongs to. */
export type TreasurySide = 'base' | 'quote'

export const CAPITAL_FLOW: EnumKind = {
  idlName: 'CapitalFlow',
  variants: ['deposit', 'withdraw'],
}
/** Direction of the owner's capital movement. */
export type CapitalFlow = 'deposit' | 'withdraw'

export const QUOTE_CLEAR_REASON: EnumKind = {
  idlName: 'QuoteClearReason',
  variants: ['explicit', 'capitalWithdrawn', 'pricingAuthorityChanged'],
}
/**
 * Why the quote disappeared.
 *
 * The three reasons are not equal for the console: `explicit` is normal engine
 * operation when the feed is silent (FR-014), the other two follow an owner
 * action after which the engine will not restore the price on its own.
 */
export type QuoteClearReason = 'capitalWithdrawn' | 'explicit' | 'pricingAuthorityChanged'

// ─── Instruction arguments ───────────────────────────────────────────────────

/** Deployment parameters (FR-001, FR-007, FR-010, FR-026). */
export interface InitializeVaultArgs {
  pricingAuthority: Address
  haltAuthority: Address
  maxQuoteAgeSlots: number
  maxSkewBps: number
}

export const initializeVaultArgsCodec = structCodec<InitializeVaultArgs>('InitializeVaultArgs', [
  ['pricingAuthority', 'pricing_authority', 'pubkey'],
  ['haltAuthority', 'halt_authority', 'pubkey'],
  ['maxQuoteAgeSlots', 'max_quote_age_slots', 'u32'],
  ['maxSkewBps', 'max_skew_bps', 'u16'],
])

/** Risk limits the owner changes after deployment (FR-007, FR-026). */
export interface RiskLimits {
  maxQuoteAgeSlots: number
  maxSkewBps: number
}

export const riskLimitsCodec = structCodec<RiskLimits>('RiskLimits', [
  ['maxQuoteAgeSlots', 'max_quote_age_slots', 'u32'],
  ['maxSkewBps', 'max_skew_bps', 'u16'],
])

/**
 * A quote (FR-006).
 *
 * `midE9` is in **raw** units of quote per raw unit of base, in fixed point 1e9.
 * Converting a human price into raw units is the engine's job: the program does
 * not read the mints' decimals (that would cost two extra accounts in the SC-002
 * budget).
 */
export interface QuoteUpdate {
  midE9: bigint
  spreadBps: number
  skewBps: number
  maxSizeBase: bigint
}

export const quoteUpdateCodec = structCodec<QuoteUpdate>('QuoteUpdate', [
  ['midE9', 'mid_e9', 'u128'],
  ['spreadBps', 'spread_bps', 'u16'],
  ['skewBps', 'skew_bps', 'i16'],
  ['maxSizeBase', 'max_size_base', 'u64'],
])

/** A swap order (FR-008, FR-009). */
export interface SwapArgs {
  side: SwapSide
  amountIn: bigint
  /** Zero means "any result" — a deliberate choice, not an omission. */
  minAmountOut: bigint
}

export const swapArgsCodec = structCodec<SwapArgs>('SwapArgs', [
  ['side', 'side', SWAP_SIDE],
  ['amountIn', 'amount_in', 'u64'],
  ['minAmountOut', 'min_amount_out', 'u64'],
])

// ─── Account ─────────────────────────────────────────────────────────────────

/** Vault state. `midE9 === 0n` means "no quote posted", not a price of zero. */
export interface Vault {
  owner: Address
  pricingAuthority: Address
  haltAuthority: Address
  baseMint: Address
  quoteMint: Address
  baseVault: Address
  quoteVault: Address
  midE9: bigint
  maxSizeBase: bigint
  quoteSlot: bigint
  maxQuoteAgeSlots: number
  spreadBps: number
  skewBps: number
  maxSkewBps: number
  halted: boolean
  bump: number
}

export const vaultCodec = structCodec<Vault>('Vault', [
  ['owner', 'owner', 'pubkey'],
  ['pricingAuthority', 'pricing_authority', 'pubkey'],
  ['haltAuthority', 'halt_authority', 'pubkey'],
  ['baseMint', 'base_mint', 'pubkey'],
  ['quoteMint', 'quote_mint', 'pubkey'],
  ['baseVault', 'base_vault', 'pubkey'],
  ['quoteVault', 'quote_vault', 'pubkey'],
  ['midE9', 'mid_e9', 'u128'],
  ['maxSizeBase', 'max_size_base', 'u64'],
  ['quoteSlot', 'quote_slot', 'u64'],
  ['maxQuoteAgeSlots', 'max_quote_age_slots', 'u32'],
  ['spreadBps', 'spread_bps', 'u16'],
  ['skewBps', 'skew_bps', 'i16'],
  ['maxSkewBps', 'max_skew_bps', 'u16'],
  ['halted', 'halted', 'bool'],
  ['bump', 'bump', 'u8'],
])

// ─── Events ──────────────────────────────────────────────────────────────────

/** A quote was posted (FR-006, FR-020). */
export interface QuoteUpdated {
  vault: Address
  slot: bigint
  midE9: bigint
  spreadBps: number
  skewBps: number
  maxSizeBase: bigint
}

export const quoteUpdatedCodec = structCodec<QuoteUpdated>('QuoteUpdated', [
  ['vault', 'vault', 'pubkey'],
  ['slot', 'slot', 'u64'],
  ['midE9', 'mid_e9', 'u128'],
  ['spreadBps', 'spread_bps', 'u16'],
  ['skewBps', 'skew_bps', 'i16'],
  ['maxSizeBase', 'max_size_base', 'u64'],
])

/** The quote was cleared (FR-014, FR-020). */
export interface QuoteCleared {
  vault: Address
  slot: bigint
  reason: QuoteClearReason
}

export const quoteClearedCodec = structCodec<QuoteCleared>('QuoteCleared', [
  ['vault', 'vault', 'pubkey'],
  ['slot', 'slot', 'u64'],
  ['reason', 'reason', QUOTE_CLEAR_REASON],
])

/**
 * A swap was executed (FR-020).
 *
 * `slot` is when the swap happened, `quoteSlot` is when the quote it was priced
 * by was posted; the difference is the quote's age at execution time.
 */
export interface Swapped {
  vault: Address
  slot: bigint
  side: SwapSide
  amountIn: bigint
  amountOut: bigint
  priceE9: bigint
  quoteSlot: bigint
  baseAmountAfter: bigint
  quoteAmountAfter: bigint
}

export const swappedCodec = structCodec<Swapped>('Swapped', [
  ['vault', 'vault', 'pubkey'],
  ['slot', 'slot', 'u64'],
  ['side', 'side', SWAP_SIDE],
  ['amountIn', 'amount_in', 'u64'],
  ['amountOut', 'amount_out', 'u64'],
  ['priceE9', 'price_e9', 'u128'],
  ['quoteSlot', 'quote_slot', 'u64'],
  ['baseAmountAfter', 'base_amount_after', 'u64'],
  ['quoteAmountAfter', 'quote_amount_after', 'u64'],
])

/** The owner's capital moved (FR-003, FR-020, FR-021). */
export interface CapitalMoved {
  vault: Address
  slot: bigint
  flow: CapitalFlow
  side: TreasurySide
  amount: bigint
  /** Balance of **this** treasury after the transfer. */
  treasuryAmountAfter: bigint
}

export const capitalMovedCodec = structCodec<CapitalMoved>('CapitalMoved', [
  ['vault', 'vault', 'pubkey'],
  ['slot', 'slot', 'u64'],
  ['flow', 'flow', CAPITAL_FLOW],
  ['side', 'side', TREASURY_SIDE],
  ['amount', 'amount', 'u64'],
  ['treasuryAmountAfter', 'treasury_amount_after', 'u64'],
])
