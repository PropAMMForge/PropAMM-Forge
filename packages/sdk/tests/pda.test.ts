/**
 * Address derivation against the addresses Rust computed.
 *
 * A mistake in the seeds does not give wrong bytes — it gives the address of an
 * account that does not exist, and on the network it looks like "account not
 * found". From that message it is impossible to guess that the SDK has the seeds
 * reordered, so the addresses in the fixture are written by the same `find_program_address` as the program.
 */

import { address } from '@solana/kit'
import { describe, expect, it } from 'vitest'
import { findAssociatedTokenAddress, findTreasuryAddresses, findVaultAddress } from '../src/pda.js'
import {
  PROPAMM_VAULT_PROGRAM_ADDRESS,
  TOKEN_2022_PROGRAM_ADDRESS,
  TOKEN_PROGRAM_ADDRESS,
  VAULT_SEED,
} from '../src/program.js'
import golden from './fixtures/borsh-golden.json'

const expected = (golden as unknown as { addresses: Record<string, number | string> }).addresses

const OWNER = address(expected.owner as string)
const BASE_MINT = address(expected.base_mint as string)
const QUOTE_MINT = address(expected.quote_mint as string)

describe('constants', () => {
  it('the program key and the token programs are the same ones Rust sees', () => {
    expect(PROPAMM_VAULT_PROGRAM_ADDRESS).toBe(expected.program)
    expect(TOKEN_PROGRAM_ADDRESS).toBe(expected.token_program)
    expect(TOKEN_2022_PROGRAM_ADDRESS).toBe(expected.token_2022_program)
  })

  it('the vault seed is the bytes b"vault"', () => {
    expect([...VAULT_SEED]).toEqual([...'vault'].map((letter) => letter.charCodeAt(0)))
  })
})

describe('state PDA', () => {
  it('matches Rust together with the bump', async () => {
    const vault = await findVaultAddress({
      owner: OWNER,
      baseMint: BASE_MINT,
      quoteMint: QUOTE_MINT,
    })
    expect(vault.address).toBe(expected.vault)
    expect(vault.bump).toBe(expected.vault_bump)
  })

  it('the mint order matters', async () => {
    // The pair (base, quote) and the pair (quote, base) are two different vaults
    // with opposite meanings of `mid_e9`. The SDK has no right to normalize the
    // order "so it is the same": whoever deploys declares it (FR-004).
    const swapped = await findVaultAddress({
      owner: OWNER,
      baseMint: QUOTE_MINT,
      quoteMint: BASE_MINT,
    })
    expect(swapped.address).toBe(expected.vault_mints_swapped)
    expect(swapped.address).not.toBe(expected.vault)
  })

  it('a different program key gives a different vault', async () => {
    const elsewhere = await findVaultAddress({
      owner: OWNER,
      baseMint: BASE_MINT,
      quoteMint: QUOTE_MINT,
      programAddress: address('11111111111111111111111111111112'),
    })
    expect(elsewhere.address).not.toBe(expected.vault)
  })
})

describe('treasuries', () => {
  it('ATAs under two different token programs match Rust', async () => {
    const vault = address(expected.vault as string)
    const treasuries = await findTreasuryAddresses({
      vault,
      baseMint: BASE_MINT,
      quoteMint: QUOTE_MINT,
      baseTokenProgram: TOKEN_PROGRAM_ADDRESS,
      quoteTokenProgram: TOKEN_2022_PROGRAM_ADDRESS,
    })
    expect(treasuries.baseVault.address).toBe(expected.base_treasury)
    expect(treasuries.quoteVault.address).toBe(expected.quote_treasury)
  })

  it('the token program is part of the seeds, not an assumption', async () => {
    // A pair may mix classic Token and Token-2022. A default here would give an
    // address that exists but belongs to the wrong program.
    const vault = address(expected.vault as string)
    const classic = await findAssociatedTokenAddress({
      owner: vault,
      mint: BASE_MINT,
      tokenProgram: TOKEN_PROGRAM_ADDRESS,
    })
    const token2022 = await findAssociatedTokenAddress({
      owner: vault,
      mint: BASE_MINT,
      tokenProgram: TOKEN_2022_PROGRAM_ADDRESS,
    })
    expect(classic.address).toBe(expected.base_treasury)
    expect(token2022.address).not.toBe(classic.address)
  })
})
