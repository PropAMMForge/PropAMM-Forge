/**
 * Program error codes.
 *
 * The table is built from the vendored IDL rather than rewritten as numbers:
 * Anchor assigns codes by the **order** of variants in `VaultError`, so a variant
 * inserted in the middle shifts all the following ones. Written out by hand, they
 * would diverge from the program silently — and the console would show the wrong cause of a refusal.
 */

import { PROPAMM_VAULT_IDL } from './idl.js'

export interface VaultErrorInfo {
  code: number
  name: string
  message: string
}

const BY_CODE: ReadonlyMap<number, VaultErrorInfo> = new Map(
  PROPAMM_VAULT_IDL.errors.map((entry) => [
    entry.code,
    { code: entry.code, name: entry.name, message: entry.msg ?? entry.name },
  ]),
)

const BY_NAME: ReadonlyMap<string, VaultErrorInfo> = new Map(
  [...BY_CODE.values()].map((info) => [info.name, info]),
)

/** All program codes, in declaration order. */
export const VAULT_ERRORS: readonly VaultErrorInfo[] = [...BY_CODE.values()]

/** Description by code; `null` — not our code (Anchor's own codes < 6000, another program). */
export function vaultErrorByCode(code: number): VaultErrorInfo | null {
  return BY_CODE.get(code) ?? null
}

/** Description by variant name, as it is called in Rust. */
export function vaultErrorByName(name: string): VaultErrorInfo | null {
  return BY_NAME.get(name) ?? null
}
