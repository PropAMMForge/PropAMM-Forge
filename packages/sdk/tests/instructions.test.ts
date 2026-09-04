/**
 * Account tables against the IDL and builder behaviour.
 *
 * The `data` bytes are checked by the golden vectors; here the second half of an
 * instruction is checked — the accounts. Order, roles and fixed addresses land in
 * no borsh vector, and a mistake in them gives a refusal already on the network:
 * reordered accounts — `ConstraintSeeds` or `AccountNotSigner`, an extra
 * `writable` — a silently wider lock conflict, visible only under load.
 */

import { AccountRole, address } from '@solana/kit'
import { describe, expect, it } from 'vitest'
import type { IdlInstructionAccount } from '../src/idl-schema.js'
import { PROPAMM_VAULT_IDL } from '../src/idl.js'
import {
  INSTRUCTION_ACCOUNTS,
  getClearQuoteInstruction,
  getDepositInstruction,
  getInitializeVaultInstruction,
  getSwapInstruction,
  getUpdateQuoteInstruction,
} from '../src/instructions.js'
import {
  ASSOCIATED_TOKEN_PROGRAM_ADDRESS,
  PROPAMM_VAULT_PROGRAM_ADDRESS,
  SYSTEM_PROGRAM_ADDRESS,
  TOKEN_PROGRAM_ADDRESS,
  instructionDiscriminator,
} from '../src/program.js'
import golden from './fixtures/borsh-golden.json'

const addresses = (golden as unknown as { addresses: Record<string, string> }).addresses
const OWNER = address(addresses.owner as string)
const VAULT = address(addresses.vault as string)
const BASE_MINT = address(addresses.base_mint as string)
const QUOTE_MINT = address(addresses.quote_mint as string)
const BASE_TREASURY = address(addresses.base_treasury as string)
const QUOTE_TREASURY = address(addresses.quote_treasury as string)

function roleOf(account: IdlInstructionAccount): AccountRole {
  if (account.signer === true) {
    return account.writable === true ? AccountRole.WRITABLE_SIGNER : AccountRole.READONLY_SIGNER
  }
  return account.writable === true ? AccountRole.WRITABLE : AccountRole.READONLY
}

describe('account tables', () => {
  it('cover exactly the instructions that are in the IDL', () => {
    expect(Object.keys(INSTRUCTION_ACCOUNTS).sort()).toEqual(
      PROPAMM_VAULT_IDL.instructions.map((ix) => ix.name).sort(),
    )
  })

  it.each(PROPAMM_VAULT_IDL.instructions.map((ix) => [ix.name, ix] as const))(
    '%s: order and roles',
    (name, ix) => {
      const specs = INSTRUCTION_ACCOUNTS[name as keyof typeof INSTRUCTION_ACCOUNTS]
      expect(specs.map(([, wire]) => wire)).toEqual(ix.accounts.map((account) => account.name))
      expect(specs.map(([, , role]) => role)).toEqual(ix.accounts.map(roleOf))
    },
  )

  it('vault in swap stays readonly', () => {
    // A swap changes no state field, and that is exactly what allows parallel
    // swaps on one vault. `writable` here is not a compile error but a loss of
    // throughput that nothing else makes visible.
    const vault = INSTRUCTION_ACCOUNTS.swap.find(([js]) => js === 'vault')
    expect(vault?.[2]).toBe(AccountRole.READONLY)
  })
})

describe('builders', () => {
  it('clear_quote: two accounts and the bare discriminator', () => {
    const ix = getClearQuoteInstruction({ pricingAuthority: OWNER, vault: VAULT })
    expect(ix.programAddress).toBe(PROPAMM_VAULT_PROGRAM_ADDRESS)
    expect(ix.accounts).toEqual([
      { address: OWNER, role: AccountRole.READONLY_SIGNER },
      { address: VAULT, role: AccountRole.WRITABLE },
    ])
    expect([...(ix.data ?? [])]).toEqual([...instructionDiscriminator('clear_quote')])
  })

  it('initialize_vault substitutes the fixed programs itself', () => {
    const ix = getInitializeVaultInstruction({
      owner: OWNER,
      baseMint: BASE_MINT,
      quoteMint: QUOTE_MINT,
      vault: VAULT,
      baseVault: BASE_TREASURY,
      quoteVault: QUOTE_TREASURY,
      baseTokenProgram: TOKEN_PROGRAM_ADDRESS,
      quoteTokenProgram: TOKEN_PROGRAM_ADDRESS,
      args: {
        pricingAuthority: OWNER,
        haltAuthority: OWNER,
        maxQuoteAgeSlots: 150,
        maxSkewBps: 2_500,
      },
    })

    const byName = new Map(
      INSTRUCTION_ACCOUNTS.initialize_vault.map(([, wire], index) => [wire, ix.accounts?.[index]]),
    )
    // The same addresses declared in the IDL: the program checks them, and passing
    // a foreign one here is a refusal on the spot, not flexibility.
    expect(byName.get('associated_token_program')?.address).toBe(ASSOCIATED_TOKEN_PROGRAM_ADDRESS)
    expect(byName.get('system_program')?.address).toBe(SYSTEM_PROGRAM_ADDRESS)

    const fromIdl = PROPAMM_VAULT_IDL.instructions.find(
      (entry) => entry.name === 'initialize_vault',
    )
    for (const account of fromIdl?.accounts ?? []) {
      if (account.address !== undefined) {
        expect(byName.get(account.name)?.address).toBe(account.address)
      }
    }
  })

  it('a missing account fails with its name rather than staying silent', () => {
    expect(() =>
      // @ts-expect-error deliberately incomplete input — that is exactly what is checked
      getUpdateQuoteInstruction({
        vault: VAULT,
        quote: { midE9: 1n, spreadBps: 1, skewBps: 0, maxSizeBase: 1n },
      }),
    ).toThrow(/pricingAuthority/)
  })

  it('a missing argument field fails rather than encoding as zero', () => {
    // `getU16Encoder().encode(undefined)` in kit writes 0. Without the guard
    // `spreadBps` would silently become a zero spread — a valid number with a different meaning.
    expect(() =>
      getUpdateQuoteInstruction({
        pricingAuthority: OWNER,
        vault: VAULT,
        // @ts-expect-error deliberately incomplete arguments
        quote: { midE9: 1n, skewBps: 0, maxSizeBase: 1n },
      }),
    ).toThrow(/spreadBps/)
  })

  it('a foreign program key reaches the instruction', () => {
    const other = address('11111111111111111111111111111112')
    const ix = getDepositInstruction({
      owner: OWNER,
      vault: VAULT,
      mint: BASE_MINT,
      treasury: BASE_TREASURY,
      ownerTokenAccount: QUOTE_TREASURY,
      tokenProgram: TOKEN_PROGRAM_ADDRESS,
      amount: 1n,
      programAddress: other,
    })
    expect(ix.programAddress).toBe(other)
  })

  it('swap returns ten accounts in IDL order', () => {
    const ix = getSwapInstruction({
      trader: OWNER,
      vault: VAULT,
      baseTreasury: BASE_TREASURY,
      quoteTreasury: QUOTE_TREASURY,
      traderBaseAccount: BASE_MINT,
      traderQuoteAccount: QUOTE_MINT,
      baseMint: BASE_MINT,
      quoteMint: QUOTE_MINT,
      baseTokenProgram: TOKEN_PROGRAM_ADDRESS,
      quoteTokenProgram: TOKEN_PROGRAM_ADDRESS,
      args: { side: 'baseToQuote', amountIn: 10n, minAmountOut: 0n },
    })
    expect(ix.accounts).toHaveLength(10)
    expect(ix.accounts?.[1]).toEqual({ address: VAULT, role: AccountRole.READONLY })
  })
})
