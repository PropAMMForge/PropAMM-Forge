/**
 * Addresses, seeds and discriminators.
 *
 * The discriminators are not written out as numbers: they come from the vendored
 * IDL, i.e. from the very text the program generated. Writing them by hand would
 * mean a fourth copy of eight bytes that change when an instruction is renamed —
 * and silently diverging from the program exactly when it is renamed.
 */

import { type Address, address } from '@solana/kit'
import { PROPAMM_VAULT_IDL } from './idl.js'

/** The program address — from the IDL, i.e. from `declare_id!`. */
export const PROPAMM_VAULT_PROGRAM_ADDRESS: Address = address(PROPAMM_VAULT_IDL.address)

/**
 * The state PDA seed — the bytes `b"vault"`, taken from the PDA description in the IDL.
 *
 * Writing them as a string would be shorter, but the seed is part of the address:
 * changed in Rust, it would stay old in a written-out copy, and the SDK would
 * compute the address of a vault that does not exist. The error would look like "account not found".
 */
export const VAULT_SEED: Uint8Array = (() => {
  const seeds = PROPAMM_VAULT_IDL.instructions
    .find((ix) => ix.name === 'clear_quote')
    ?.accounts.find((account) => account.name === 'vault')?.pda?.seeds
  const constant = seeds?.[0]
  if (constant?.kind !== 'const' || constant.value === undefined) {
    throw new Error('The vendored IDL has no constant seed for the vault PDA')
  }
  return Uint8Array.from(constant.value)
})()

export const SYSTEM_PROGRAM_ADDRESS: Address = address('11111111111111111111111111111111')
export const ASSOCIATED_TOKEN_PROGRAM_ADDRESS: Address = address(
  'ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL',
)
export const TOKEN_PROGRAM_ADDRESS: Address = address('TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA')
export const TOKEN_2022_PROGRAM_ADDRESS: Address = address(
  'TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb',
)

/** Instruction names as they are called in the IDL. */
export type InstructionName =
  | 'clear_quote'
  | 'deposit'
  | 'initialize_vault'
  | 'set_halt_authority'
  | 'set_pricing_authority'
  | 'set_risk_limits'
  | 'swap'
  | 'update_quote'
  | 'withdraw'

function discriminatorOf(source: readonly { name: string; discriminator: readonly number[] }[]) {
  return (name: string): Uint8Array => {
    const found = source.find((entry) => entry.name === name)
    if (found === undefined) {
      throw new Error(`The vendored IDL has no "${name}" — the copy may be stale`)
    }
    return Uint8Array.from(found.discriminator)
  }
}

/** The eight bytes at the start of instruction `data`. */
export const instructionDiscriminator = discriminatorOf(PROPAMM_VAULT_IDL.instructions)

/** The eight bytes at the start of account data. */
export const accountDiscriminator = discriminatorOf(PROPAMM_VAULT_IDL.accounts)

/** The eight bytes at the start of an event's `Program data:`. */
export const eventDiscriminator = discriminatorOf(PROPAMM_VAULT_IDL.events)

/** `disc ++ payload` — the form in which data travels in an instruction and into the log. */
export function withDiscriminator(discriminator: Uint8Array, payload: Uint8Array): Uint8Array {
  const data = new Uint8Array(discriminator.length + payload.length)
  data.set(discriminator, 0)
  data.set(payload, discriminator.length)
  return data
}
