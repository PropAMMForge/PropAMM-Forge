/**
 * Address derivation.
 *
 * `vault` is a PDA of our program with seeds `[b"vault", owner, base_mint, quote_mint]`.
 * The mint order in the seeds is **significant**: the pair (A, B) and the pair
 * (B, A) are two different vaults with opposite meanings of `mid_e9`. The SDK
 * neither normalizes nor guesses the order — whoever deploys declares it (FR-004).
 *
 * The treasuries are ordinary ATAs owned by `vault`, which is exactly why they are
 * derived by `findAssociatedTokenAddress` rather than a separate seed scheme: an
 * ATA for someone else's owner can be created by anyone, so the program creates them via `init_if_needed`.
 */

import { type Address, getAddressEncoder, getProgramDerivedAddress } from '@solana/kit'
import {
  ASSOCIATED_TOKEN_PROGRAM_ADDRESS,
  PROPAMM_VAULT_PROGRAM_ADDRESS,
  VAULT_SEED,
} from './program.js'

const addressEncoder = getAddressEncoder()

/** A PDA address together with its bump — the bump is needed by calls that sign with seeds. */
export interface DerivedAddress {
  address: Address
  bump: number
}

export interface VaultSeeds {
  owner: Address
  baseMint: Address
  quoteMint: Address
  /** For a local network with a different program key. */
  programAddress?: Address
}

/** The vault state PDA (FR-004). */
export async function findVaultAddress(seeds: VaultSeeds): Promise<DerivedAddress> {
  const [derived, bump] = await getProgramDerivedAddress({
    programAddress: seeds.programAddress ?? PROPAMM_VAULT_PROGRAM_ADDRESS,
    seeds: [
      VAULT_SEED,
      addressEncoder.encode(seeds.owner),
      addressEncoder.encode(seeds.baseMint),
      addressEncoder.encode(seeds.quoteMint),
    ],
  })
  return { address: derived, bump }
}

export interface AssociatedTokenSeeds {
  owner: Address
  mint: Address
  tokenProgram: Address
}

/**
 * The owner's ATA under a specific token program.
 *
 * `tokenProgram` is required here, not defaulted: a pair may mix classic Token
 * and Token-2022, and a silent default would give an address that exists but
 * belongs to the wrong program.
 */
export async function findAssociatedTokenAddress(
  seeds: AssociatedTokenSeeds,
): Promise<DerivedAddress> {
  const [derived, bump] = await getProgramDerivedAddress({
    programAddress: ASSOCIATED_TOKEN_PROGRAM_ADDRESS,
    seeds: [
      addressEncoder.encode(seeds.owner),
      addressEncoder.encode(seeds.tokenProgram),
      addressEncoder.encode(seeds.mint),
    ],
  })
  return { address: derived, bump }
}

/** Both vault treasuries in one call — they are always taken together. */
export async function findTreasuryAddresses(params: {
  vault: Address
  baseMint: Address
  quoteMint: Address
  baseTokenProgram: Address
  quoteTokenProgram: Address
}): Promise<{ baseVault: DerivedAddress; quoteVault: DerivedAddress }> {
  const [baseVault, quoteVault] = await Promise.all([
    findAssociatedTokenAddress({
      owner: params.vault,
      mint: params.baseMint,
      tokenProgram: params.baseTokenProgram,
    }),
    findAssociatedTokenAddress({
      owner: params.vault,
      mint: params.quoteMint,
      tokenProgram: params.quoteTokenProgram,
    }),
  ])
  return { baseVault, quoteVault }
}
