export interface BandFigure {
  label: string
  value: string
}

export interface KeyFigure {
  label: string
  value: string
  aside?: string
  strong?: boolean
  tone?: 'ink' | 'rust'
}

export interface ComputeRow {
  instruction: string
  measured: string
  /** Measured cost as a share of the 60,000 CU router budget, 0–1. */
  measuredShare: number | null
  budget: string
}

export interface ActivityRow {
  slot: string
  side: string
  amountIn: string
  amountOut: string
  price: string
  cu: string
  state: string
  refused: boolean
}

export const consoleVaultPair = 'SOL/USDC'

export const consoleBandFigures: readonly BandFigure[] = [
  { label: 'Quote age', value: '3 slots of 25' },
  { label: 'Max order size', value: '60.000000000 SOL' },
  { label: 'Refresh', value: 'every 8 slots or 5 bps' },
  { label: 'Engine', value: 'running' },
]

export const inventoryRows: readonly KeyFigure[] = [
  { label: 'Base', value: '1,240.000000000 SOL', aside: '188,976.00 USDC at mid' },
  { label: 'Quote', value: '156,300.00 USDC' },
  { label: 'Skew', value: '+946 bps', aside: 'hard limit 3000 bps' },
]

/** Current skew as a share of the −3000 bps … +3000 bps scale, 0–1. */
export const skewScalePosition = (946 + 3000) / 6000

export const skewScaleEnds: readonly [string, string] = ['−3000 bps', '+3000 bps']

export const pnlRows: readonly KeyFigure[] = [
  { label: 'Realized', value: '+503.35 USDC' },
  { label: 'Quote update fees', value: '−3.15 USDC', aside: '4,130 updates, 0.020650000 SOL' },
  { label: 'Unrealized', value: '+118.42 USDC' },
  { label: 'Net', value: '+618.62 USDC', strong: true },
]

export const pnlNote = '318 swaps · 629,183.40 USDC volume · captured 8.0 bps'

export const computeRows: readonly ComputeRow[] = [
  {
    instruction: 'swap base→quote, SPL pair',
    measured: '18,647 CU',
    measuredShare: 18647 / 60000,
    budget: '60,000 CU',
  },
  {
    instruction: 'swap quote→base, SPL pair',
    measured: '18,603 CU',
    measuredShare: 18603 / 60000,
    budget: '60,000 CU',
  },
  {
    instruction: 'swap base→quote, Token-2022 pair',
    measured: '22,164 CU',
    measuredShare: 22164 / 60000,
    budget: '60,000 CU',
  },
  {
    instruction: 'swap base→quote, Token-2022 with extensions',
    measured: '22,950 CU',
    measuredShare: 22950 / 60000,
    budget: '60,000 CU',
  },
  {
    instruction: 'update_quote',
    measured: '6,028 CU',
    measuredShare: null,
    budget: 'no declared ceiling',
  },
]

export const computeNote = 'today: 4,130 updates × 6,028 CU + 318 swaps × 18,647 CU = 30,825,386 CU'

export const activityRows: readonly ActivityRow[] = [
  {
    slot: '342,118,904',
    side: 'sell base',
    amountIn: '25.000000000 SOL',
    amountOut: '3,802.383650 USDC',
    price: '152.095346',
    cu: '18,647',
    state: 'filled',
    refused: false,
  },
  {
    slot: '342,118,881',
    side: 'buy base',
    amountIn: '5,000.00 USDC',
    amountOut: '32.821559016 SOL',
    price: '152.338894',
    cu: '18,603',
    state: 'filled',
    refused: false,
  },
  {
    slot: '342,118,860',
    side: 'buy base',
    amountIn: '12,000.00 USDC',
    amountOut: '—',
    price: '152.338894',
    cu: '—',
    state: 'refused — size 78.771741640 SOL over max 60.000000000 SOL',
    refused: true,
  },
  {
    slot: '342,118,845',
    side: 'sell base',
    amountIn: '12.500000000 SOL',
    amountOut: '1,901.191825 USDC',
    price: '152.095346',
    cu: '18,647',
    state: 'filled',
    refused: false,
  },
  {
    slot: '342,118,812',
    side: 'buy base',
    amountIn: '1,800.50 USDC',
    amountOut: '11.819043402 SOL',
    price: '152.338894',
    cu: '18,603',
    state: 'filled',
    refused: false,
  },
  {
    slot: '342,118,799',
    side: 'sell base',
    amountIn: '40.000000000 SOL',
    amountOut: '6,083.813840 USDC',
    price: '152.095346',
    cu: '18,647',
    state: 'filled',
    refused: false,
  },
]
