/**
 * Builders for the nine instructions.
 *
 * Every instruction is declared as an account table: the name in TS, the name in
 * the IDL and the role. The role is not decoration but part of the protocol: an
 * extra `writable` widens the lock conflict, a forgotten one gives `AccountNotMutable`
 * already on the network. So the tables are checked against the IDL in
 * `tests/instructions.test.ts` rather than rewritten from memory.
 *
 * The special row is `vault` in `swap`: it is **readonly**. A swap changes no state
 * field (inventory lives in the token accounts, the quote stays in force until
 * the next update), and that is exactly what allows parallel swaps on one vault.
 * Making it writable "just in case" would serialize the whole order flow.
 */

import {
  AccountRole,
  type Address,
  type Instruction,
  type ReadonlyUint8Array,
  getAddressEncoder,
} from '@solana/kit'
import {
  type InitializeVaultArgs,
  type QuoteUpdate,
  type RiskLimits,
  type SwapArgs,
  initializeVaultArgsCodec,
  quoteUpdateCodec,
  riskLimitsCodec,
  swapArgsCodec,
} from './codecs.js'
import {
  ASSOCIATED_TOKEN_PROGRAM_ADDRESS,
  type InstructionName,
  PROPAMM_VAULT_PROGRAM_ADDRESS,
  SYSTEM_PROGRAM_ADDRESS,
  instructionDiscriminator,
  withDiscriminator,
} from './program.js'

/** An account table row: the name in TS, the name in the IDL, the role. */
export type AccountSpec = readonly [js: string, wire: string, role: AccountRole]

/** Account tables of all nine instructions — also the subject of the IDL check. */
export const INSTRUCTION_ACCOUNTS: { readonly [K in InstructionName]: readonly AccountSpec[] } = {
  clear_quote: [
    ['pricingAuthority', 'pricing_authority', AccountRole.READONLY_SIGNER],
    ['vault', 'vault', AccountRole.WRITABLE],
  ],
  deposit: [
    ['owner', 'owner', AccountRole.READONLY_SIGNER],
    ['vault', 'vault', AccountRole.WRITABLE],
    ['mint', 'mint', AccountRole.READONLY],
    ['treasury', 'treasury', AccountRole.WRITABLE],
    ['ownerTokenAccount', 'owner_token_account', AccountRole.WRITABLE],
    ['tokenProgram', 'token_program', AccountRole.READONLY],
  ],
  initialize_vault: [
    ['owner', 'owner', AccountRole.WRITABLE_SIGNER],
    ['baseMint', 'base_mint', AccountRole.READONLY],
    ['quoteMint', 'quote_mint', AccountRole.READONLY],
    ['vault', 'vault', AccountRole.WRITABLE],
    ['baseVault', 'base_vault', AccountRole.WRITABLE],
    ['quoteVault', 'quote_vault', AccountRole.WRITABLE],
    ['baseTokenProgram', 'base_token_program', AccountRole.READONLY],
    ['quoteTokenProgram', 'quote_token_program', AccountRole.READONLY],
    ['associatedTokenProgram', 'associated_token_program', AccountRole.READONLY],
    ['systemProgram', 'system_program', AccountRole.READONLY],
  ],
  set_halt_authority: [
    ['owner', 'owner', AccountRole.READONLY_SIGNER],
    ['vault', 'vault', AccountRole.WRITABLE],
  ],
  set_pricing_authority: [
    ['owner', 'owner', AccountRole.READONLY_SIGNER],
    ['vault', 'vault', AccountRole.WRITABLE],
  ],
  set_risk_limits: [
    ['owner', 'owner', AccountRole.READONLY_SIGNER],
    ['vault', 'vault', AccountRole.WRITABLE],
  ],
  swap: [
    ['trader', 'trader', AccountRole.READONLY_SIGNER],
    ['vault', 'vault', AccountRole.READONLY],
    ['baseTreasury', 'base_treasury', AccountRole.WRITABLE],
    ['quoteTreasury', 'quote_treasury', AccountRole.WRITABLE],
    ['traderBaseAccount', 'trader_base_account', AccountRole.WRITABLE],
    ['traderQuoteAccount', 'trader_quote_account', AccountRole.WRITABLE],
    ['baseMint', 'base_mint', AccountRole.READONLY],
    ['quoteMint', 'quote_mint', AccountRole.READONLY],
    ['baseTokenProgram', 'base_token_program', AccountRole.READONLY],
    ['quoteTokenProgram', 'quote_token_program', AccountRole.READONLY],
  ],
  update_quote: [
    ['pricingAuthority', 'pricing_authority', AccountRole.READONLY_SIGNER],
    ['vault', 'vault', AccountRole.WRITABLE],
  ],
  withdraw: [
    ['owner', 'owner', AccountRole.READONLY_SIGNER],
    ['vault', 'vault', AccountRole.WRITABLE],
    ['mint', 'mint', AccountRole.READONLY],
    ['treasury', 'treasury', AccountRole.WRITABLE],
    ['ownerTokenAccount', 'owner_token_account', AccountRole.WRITABLE],
    ['tokenProgram', 'token_program', AccountRole.READONLY],
  ],
}

const addressEncoder = getAddressEncoder()

/** Shared by all builders: the program address, if the key differs from `declare_id!`. */
interface WithProgram {
  programAddress?: Address
}

function build(
  name: InstructionName,
  accounts: Record<string, Address>,
  data: Uint8Array,
  programAddress: Address | undefined,
): Instruction {
  const specs = INSTRUCTION_ACCOUNTS[name]
  return {
    programAddress: programAddress ?? PROPAMM_VAULT_PROGRAM_ADDRESS,
    accounts: specs.map(([js, , role]) => {
      const value = accounts[js]
      if (value === undefined) {
        throw new Error(`${name}: account "${js}" was not passed`)
      }
      return { address: value, role }
    }),
    data: data as ReadonlyUint8Array,
  }
}

function encodeArgs<T extends object>(
  name: InstructionName,
  codec: { encode(value: T): Uint8Array },
  args: T,
): Uint8Array {
  return withDiscriminator(instructionDiscriminator(name), codec.encode(args))
}

// ─── Deployment ──────────────────────────────────────────────────────────────

export interface InitializeVaultInput extends WithProgram {
  owner: Address
  baseMint: Address
  quoteMint: Address
  vault: Address
  baseVault: Address
  quoteVault: Address
  baseTokenProgram: Address
  quoteTokenProgram: Address
  args: InitializeVaultArgs
}

/**
 * Deployment (FR-001, FR-004, FR-005). The pair is fixed forever, the treasuries
 * are created empty, there is no quote yet.
 *
 * `associatedTokenProgram` and `systemProgram` are not part of the input: their
 * addresses are fixed in the program itself, and letting them be passed from
 * outside would let the wrong ones be passed.
 */
export function getInitializeVaultInstruction(input: InitializeVaultInput): Instruction {
  return build(
    'initialize_vault',
    {
      owner: input.owner,
      baseMint: input.baseMint,
      quoteMint: input.quoteMint,
      vault: input.vault,
      baseVault: input.baseVault,
      quoteVault: input.quoteVault,
      baseTokenProgram: input.baseTokenProgram,
      quoteTokenProgram: input.quoteTokenProgram,
      associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ADDRESS,
      systemProgram: SYSTEM_PROGRAM_ADDRESS,
    },
    encodeArgs('initialize_vault', initializeVaultArgsCodec, input.args),
    input.programAddress,
  )
}

// ─── Authorities and limits ──────────────────────────────────────────────────

export interface AdminInput extends WithProgram {
  owner: Address
  vault: Address
}

/** Replace the quote signer (FR-010). Clears the current quote. */
export function getSetPricingAuthorityInstruction(
  input: AdminInput & { newAuthority: Address },
): Instruction {
  return build(
    'set_pricing_authority',
    { owner: input.owner, vault: input.vault },
    withDiscriminator(
      instructionDiscriminator('set_pricing_authority'),
      new Uint8Array(addressEncoder.encode(input.newAuthority)),
    ),
    input.programAddress,
  )
}

/** Replace the emergency-halt signer (FR-024). */
export function getSetHaltAuthorityInstruction(
  input: AdminInput & { newAuthority: Address },
): Instruction {
  return build(
    'set_halt_authority',
    { owner: input.owner, vault: input.vault },
    withDiscriminator(
      instructionDiscriminator('set_halt_authority'),
      new Uint8Array(addressEncoder.encode(input.newAuthority)),
    ),
    input.programAddress,
  )
}

/** Change the risk limits (FR-007, FR-026). */
export function getSetRiskLimitsInstruction(
  input: AdminInput & { limits: RiskLimits },
): Instruction {
  return build(
    'set_risk_limits',
    { owner: input.owner, vault: input.vault },
    encodeArgs('set_risk_limits', riskLimitsCodec, input.limits),
    input.programAddress,
  )
}

// ─── Quoting ─────────────────────────────────────────────────────────────────

export interface QuotingInput extends WithProgram {
  pricingAuthority: Address
  vault: Address
}

/** Post a quote (FR-006). `quote_slot` is taken by the chain, not from the arguments. */
export function getUpdateQuoteInstruction(
  input: QuotingInput & { quote: QuoteUpdate },
): Instruction {
  return build(
    'update_quote',
    { pricingAuthority: input.pricingAuthority, vault: input.vault },
    encodeArgs('update_quote', quoteUpdateCodec, input.quote),
    input.programAddress,
  )
}

/** Clear the quote (FR-014) — the feed went silent. */
export function getClearQuoteInstruction(input: QuotingInput): Instruction {
  return build(
    'clear_quote',
    { pricingAuthority: input.pricingAuthority, vault: input.vault },
    instructionDiscriminator('clear_quote'),
    input.programAddress,
  )
}

// ─── Capital movement ────────────────────────────────────────────────────────

export interface MoveCapitalInput extends WithProgram {
  owner: Address
  vault: Address
  mint: Address
  treasury: Address
  ownerTokenAccount: Address
  tokenProgram: Address
  amount: bigint
}

function moveCapital(name: 'deposit' | 'withdraw', input: MoveCapitalInput): Instruction {
  return build(
    name,
    {
      owner: input.owner,
      vault: input.vault,
      mint: input.mint,
      treasury: input.treasury,
      ownerTokenAccount: input.ownerTokenAccount,
      tokenProgram: input.tokenProgram,
    },
    encodeArgs(name, amountCodec, { amount: input.amount }),
    input.programAddress,
  )
}

// A `u64` as a single field — there is no separate type in the IDL, the argument is called `amount`.
const amountCodec = {
  encode({ amount }: { amount: bigint }): Uint8Array {
    if (typeof amount !== 'bigint') {
      throw new Error('deposit/withdraw: amount must be a bigint')
    }
    const bytes = new Uint8Array(8)
    new DataView(bytes.buffer).setBigUint64(0, amount, true)
    return bytes
  },
}

/** Treasury deposit by the owner (FR-003). */
export function getDepositInstruction(input: MoveCapitalInput): Instruction {
  return moveCapital('deposit', input)
}

/** Capital withdrawal by the owner (FR-003). Clears the current quote. */
export function getWithdrawInstruction(input: MoveCapitalInput): Instruction {
  return moveCapital('withdraw', input)
}

// ─── Swap ────────────────────────────────────────────────────────────────────

export interface SwapInput extends WithProgram {
  trader: Address
  vault: Address
  baseTreasury: Address
  quoteTreasury: Address
  traderBaseAccount: Address
  traderQuoteAccount: Address
  baseMint: Address
  quoteMint: Address
  baseTokenProgram: Address
  quoteTokenProgram: Address
  args: SwapArgs
}

/** Swap at the posted quote (FR-007, FR-008, FR-009). */
export function getSwapInstruction(input: SwapInput): Instruction {
  return build(
    'swap',
    {
      trader: input.trader,
      vault: input.vault,
      baseTreasury: input.baseTreasury,
      quoteTreasury: input.quoteTreasury,
      traderBaseAccount: input.traderBaseAccount,
      traderQuoteAccount: input.traderQuoteAccount,
      baseMint: input.baseMint,
      quoteMint: input.quoteMint,
      baseTokenProgram: input.baseTokenProgram,
      quoteTokenProgram: input.quoteTokenProgram,
    },
    encodeArgs('swap', swapArgsCodec, input.args),
    input.programAddress,
  )
}
