export interface VaultRow {
  pair: string
  /** Shown once beside an invented devnet test asset. */
  assetNote: string | null
  network: string
  inventoryBase: string
  inventoryQuote: string
  skew: string
  quote: string
  quoteAge: string | null
  pnlToday: string
  state: string
  tone: 'ink' | 'rust'
}

export const vaultRows: readonly VaultRow[] = [
  {
    pair: 'SOL/USDC',
    assetNote: null,
    network: 'devnet',
    inventoryBase: '1,240.000000000 SOL',
    inventoryQuote: '156,300.00 USDC',
    skew: '+946 bps',
    quote: '152.400000 USDC',
    quoteAge: 'age 3 slots',
    pnlToday: '+618.62 USDC',
    state: 'quoting',
    tone: 'ink',
  },
  {
    pair: 'ORVL/USDC',
    assetNote: 'devnet test asset',
    network: 'devnet',
    inventoryBase: '812,400.000000 ORVL',
    inventoryQuote: '344,190.00 USDC',
    skew: '−62 bps',
    quote: 'cleared — feed silent 41 slots',
    quoteAge: null,
    pnlToday: '+41.08 USDC',
    state: 'no quote',
    tone: 'rust',
  },
  {
    pair: 'KESTR/USDC',
    assetNote: 'devnet test asset',
    network: 'devnet',
    inventoryBase: '96,500.000000 KESTR',
    inventoryQuote: '291,640.00 USDC',
    skew: '−196 bps',
    quote: 'cleared by halt',
    quoteAge: null,
    pnlToday: '−212.90 USDC',
    state: 'halted',
    tone: 'rust',
  },
]

export const vaultNotes: readonly string[] = [
  'ORVL/USDC has no quote because the Pyth price feed stopped arriving, and the engine withdrew the quote rather than republishing the last price it knew. A stale price that keeps being republished is how a market maker gets picked off.',
  "KESTR/USDC is halted because the day's loss crossed the limit set on that vault, so quoting stopped automatically. Resuming is a separate deliberate action by the operator and is not offered on this screen.",
]
