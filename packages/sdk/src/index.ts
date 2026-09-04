/**
 * PropAMM TS client.
 *
 * What is here: the vendored IDL, Borsh layouts, builders for nine instructions,
 * decoders for the `Vault` account and four events, the error code table and
 * address derivation.
 *
 * What is deliberately not here: sending transactions and any work with keys.
 * The builders return an `Instruction` from @solana/kit — whoever has the key
 * signs and sends (the engine — `pricing_authority`, the CLI — `owner`), and the
 * SDK must not be a place a key could be passed into by accident.
 */

export {
  type CapitalFlow,
  type CapitalMoved,
  type Field,
  type InitializeVaultArgs,
  type Layout,
  type QuoteClearReason,
  type QuoteCleared,
  type QuoteUpdate,
  type QuoteUpdated,
  type RiskLimits,
  type StructCodec,
  type SwapArgs,
  type SwapSide,
  type Swapped,
  type TreasurySide,
  type Vault,
  CAPITAL_FLOW,
  QUOTE_CLEAR_REASON,
  SWAP_SIDE,
  TREASURY_SIDE,
  capitalMovedCodec,
  initializeVaultArgsCodec,
  quoteClearedCodec,
  quoteUpdateCodec,
  quoteUpdatedCodec,
  riskLimitsCodec,
  structCodec,
  swapArgsCodec,
  swappedCodec,
  vaultCodec,
} from './codecs.js'

export {
  VAULT_ACCOUNT_SIZE,
  decodeVaultAccount,
  hasQuote,
  isVaultAccountData,
  quoteAgeSlots,
} from './accounts.js'

export {
  type VaultEvent,
  type VaultEventAtLog,
  decodeVaultEvent,
  decodeVaultEventFromBase64,
  parseVaultEvents,
} from './events.js'

export { type VaultErrorInfo, VAULT_ERRORS, vaultErrorByCode, vaultErrorByName } from './errors.js'

export {
  type AccountSpec,
  type AdminInput,
  type InitializeVaultInput,
  type MoveCapitalInput,
  type QuotingInput,
  type SwapInput,
  INSTRUCTION_ACCOUNTS,
  getClearQuoteInstruction,
  getDepositInstruction,
  getInitializeVaultInstruction,
  getSetHaltAuthorityInstruction,
  getSetPricingAuthorityInstruction,
  getSetRiskLimitsInstruction,
  getSwapInstruction,
  getUpdateQuoteInstruction,
  getWithdrawInstruction,
} from './instructions.js'

export {
  type AssociatedTokenSeeds,
  type DerivedAddress,
  type VaultSeeds,
  findAssociatedTokenAddress,
  findTreasuryAddresses,
  findVaultAddress,
} from './pda.js'

export {
  type InstructionName,
  ASSOCIATED_TOKEN_PROGRAM_ADDRESS,
  PROPAMM_VAULT_PROGRAM_ADDRESS,
  SYSTEM_PROGRAM_ADDRESS,
  TOKEN_2022_PROGRAM_ADDRESS,
  TOKEN_PROGRAM_ADDRESS,
  VAULT_SEED,
  accountDiscriminator,
  eventDiscriminator,
  instructionDiscriminator,
  withDiscriminator,
} from './program.js'

export { PROPAMM_VAULT_IDL } from './idl.js'
export type * from './idl-schema.js'
