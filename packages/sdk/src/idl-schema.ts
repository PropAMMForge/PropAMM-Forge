/**
 * Types for the part of the Anchor IDL the SDK really reads.
 *
 * This is not the full schema of the format (spec 0.1.0) — it also has generic
 * types, aliases, `errors[].msg` with substitutions and several kinds of PDA
 * seeds that our program does not have. Describing them "for the future" would
 * mean branches that never execute and are never tested.
 *
 * The IDL in the SDK takes no part in encoding: instructions are assembled by the
 * explicit layout tables in `codecs.ts`. Its role is to be the **second text** the
 * tables are checked against in tests (`tests/idl-drift.test.ts`). So exactly
 * what the check needs is described here: discriminators, account order, type fields.
 */

/** A scalar field type in IDL terms. */
export type IdlScalarType = 'bool' | 'i16' | 'pubkey' | 'u128' | 'u16' | 'u32' | 'u64' | 'u8'

/** A reference to a named type from `types[]`. */
export interface IdlDefinedType {
  readonly defined: { readonly name: string }
}

export type IdlType = IdlDefinedType | IdlScalarType

export interface IdlField {
  readonly name: string
  readonly docs?: readonly string[]
  readonly type: IdlType
}

export interface IdlEnumVariant {
  readonly name: string
}

export interface IdlTypeDef {
  readonly name: string
  readonly docs?: readonly string[]
  readonly type:
    | { readonly kind: 'enum'; readonly variants: readonly IdlEnumVariant[] }
    | { readonly kind: 'struct'; readonly fields: readonly IdlField[] }
}

/**
 * A PDA seed in an account description.
 *
 * `kind: 'account'` means the seed comes from another account of the instruction
 * or from a field of an already read account (`path: 'vault.owner'`). The SDK does
 * not resolve such paths — `pda.ts` derives from explicit arguments; the check
 * verifies that the set and order of seeds are the same for us and in the IDL.
 */
export interface IdlSeed {
  readonly kind: 'account' | 'arg' | 'const'
  readonly path?: string
  readonly account?: string
  readonly value?: readonly number[]
}

/** A PDA description of an account. `program` is present where the PDA belongs to another program (ATA). */
export interface IdlPda {
  readonly seeds: readonly IdlSeed[]
  readonly program?: IdlSeed
}

export interface IdlInstructionAccount {
  readonly name: string
  readonly docs?: readonly string[]
  readonly signer?: boolean
  readonly writable?: boolean
  readonly optional?: boolean
  /** A fixed address given in the program (system program, ATA program). */
  readonly address?: string
  readonly pda?: IdlPda
  readonly relations?: readonly string[]
}

export interface IdlInstruction {
  readonly name: string
  readonly docs?: readonly string[]
  readonly discriminator: readonly number[]
  readonly accounts: readonly IdlInstructionAccount[]
  readonly args: readonly IdlField[]
}

export interface IdlDiscriminated {
  readonly name: string
  readonly discriminator: readonly number[]
}

export interface IdlErrorCode {
  readonly code: number
  readonly name: string
  readonly msg?: string
}

export interface PropammVaultIdl {
  readonly address: string
  readonly metadata: {
    readonly name: string
    readonly version: string
    readonly spec: string
    readonly description?: string
    readonly repository?: string
  }
  readonly instructions: readonly IdlInstruction[]
  readonly accounts: readonly IdlDiscriminated[]
  readonly events: readonly IdlDiscriminated[]
  readonly errors: readonly IdlErrorCode[]
  readonly types: readonly IdlTypeDef[]
}
