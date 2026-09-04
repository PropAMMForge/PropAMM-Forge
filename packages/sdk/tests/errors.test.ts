/**
 * Error codes.
 *
 * Anchor assigns codes by the order of variants in `VaultError`, so a variant
 * inserted in the middle shifts all the following ones. That is why the table is
 * built from the IDL, and here it is checked that it is really contiguous and
 * that nobody else's error is passed off as ours.
 */

import { describe, expect, it } from 'vitest'
import { VAULT_ERRORS, vaultErrorByCode, vaultErrorByName } from '../src/errors.js'
import { PROPAMM_VAULT_IDL } from '../src/idl.js'

describe('code table', () => {
  it('covers every IDL code and invents nothing', () => {
    expect(VAULT_ERRORS.map((error) => error.code)).toEqual(
      PROPAMM_VAULT_IDL.errors.map((error) => error.code),
    )
    expect(VAULT_ERRORS.map((error) => error.name)).toEqual(
      PROPAMM_VAULT_IDL.errors.map((error) => error.name),
    )
  })

  it('codes start at 6000 and do not repeat', () => {
    expect(VAULT_ERRORS[0]?.code).toBe(6000)
    expect(new Set(VAULT_ERRORS.map((error) => error.code)).size).toBe(VAULT_ERRORS.length)
  })

  it('finds by code and by name', () => {
    const quoteStale = vaultErrorByName('QuoteStale')
    expect(quoteStale).not.toBeNull()
    expect(vaultErrorByCode(quoteStale?.code as number)?.name).toBe('QuoteStale')
  })

  it('a foreign code stays foreign', () => {
    // Anchor's own codes lie below 6000, and passing them off as ours would mean
    // showing on the console a cause of refusal that did not happen.
    expect(vaultErrorByCode(2006)).toBeNull()
    expect(vaultErrorByCode(9_999)).toBeNull()
    expect(vaultErrorByName('NoSuchError')).toBeNull()
  })
})
