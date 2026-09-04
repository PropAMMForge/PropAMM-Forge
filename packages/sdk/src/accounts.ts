/**
 * Reading the `Vault` account.
 *
 * The discriminator is always checked. Skipping the check would mean parsing any
 * 259 bytes as a `Vault` — a token account, say, or someone else's PDA — and
 * returning plausible numbers: addresses that are really pieces of a balance, and
 * a `halted` that is really a chunk of a mint.
 */

import type { ReadonlyUint8Array } from '@solana/kit'
import { type Vault, vaultCodec } from './codecs.js'
import { accountDiscriminator } from './program.js'

const VAULT_DISCRIMINATOR = accountDiscriminator('Vault')

/** Full account size: 8 bytes of discriminator plus the layout. */
export const VAULT_ACCOUNT_SIZE: number = VAULT_DISCRIMINATOR.length + vaultCodec.size

/** Whether this is our `Vault` data at all. */
export function isVaultAccountData(data: ReadonlyUint8Array): boolean {
  if (data.length < VAULT_DISCRIMINATOR.length) {
    return false
  }
  return VAULT_DISCRIMINATOR.every((byte, index) => data[index] === byte)
}

/** Parse the account data. Throws if the discriminator is foreign or the data is short. */
export function decodeVaultAccount(data: ReadonlyUint8Array): Vault {
  if (!isVaultAccountData(data)) {
    throw new Error('Not a Vault account: the discriminator does not match')
  }
  return vaultCodec.decode(data, VAULT_DISCRIMINATOR.length)
}

/**
 * Whether a quote is posted.
 *
 * There is one definition and it lives in the program: `mid_e9 == 0` means "not
 * posted", not "a price of zero". Spelling this condition out at call sites is
 * exactly the way to diverge that SC-006 forbids.
 */
export function hasQuote(vault: Vault): boolean {
  return vault.midE9 !== 0n
}

/**
 * Quote age in slots as of `currentSlot`.
 *
 * Returns `null` if there is no quote: a zero here would read as "just posted",
 * i.e. the freshest possible.
 */
export function quoteAgeSlots(vault: Vault, currentSlot: bigint): bigint | null {
  if (!hasQuote(vault)) {
    return null
  }
  return currentSlot > vault.quoteSlot ? currentSlot - vault.quoteSlot : 0n
}
