export interface LadderSide {
  /** Distance from the mid, in basis points. Negative is below the mid. */
  bps: number
  name: string
  price: string
}

export interface LadderQuote {
  kind: 'quote'
  midPrice: string
  ask: LadderSide
  bid: LadderSide
  /** Height of the filled spread band on the bps axis, printed verbatim. */
  spreadLabel: string
  leanBps: number
  leanLabel: string
  leanNote: readonly [string, string]
}

export interface LadderNoQuote {
  kind: 'noQuote'
  message: string
  lastMidLabel: string
}

export type LadderData = LadderQuote | LadderNoQuote

export const solUsdcLadder: LadderQuote = {
  kind: 'quote',
  midPrice: '152.400000 USDC',
  ask: { bps: -4.01, name: 'ASK', price: '152.338894 USDC' },
  bid: { bps: -19.99, name: 'BID', price: '152.095346 USDC' },
  spreadLabel: '15.98 bps',
  leanBps: -12,
  leanLabel: 'lean −12 bps',
  leanNote: ['inventory +946 bps', 'of 3000 bps limit'],
}

export const orvlUsdcLadder: LadderNoQuote = {
  kind: 'noQuote',
  message: 'NO QUOTE — feed silent 41 slots',
  lastMidLabel: 'last mid 0.418500 USDC',
}
