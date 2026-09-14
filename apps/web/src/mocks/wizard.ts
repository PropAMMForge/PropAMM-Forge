export interface WizardStep {
  /** Step number, printed verbatim in the left margin. */
  n: string
  title: string
  /** One-line result shown once the step collapses into its receipt. */
  receipt: string
  note: string | null
  transcript: readonly string[]
}

export interface FundField {
  id: string
  label: string
  value: string
  unit: string
}

export interface QuoteField {
  label: string
  value: string
}

export const wizardSteps: readonly WizardStep[] = [
  {
    n: '1',
    title: 'Project',
    receipt: 'propamm.toml written · 1 vault · devnet',
    note: null,
    transcript: [
      'forge init . --pair So11111111111111111111111111111111111111112/9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM --cluster devnet --max-quote-age-slots 25 --max-skew-bps 3000',
    ],
  },
  {
    n: '2',
    title: 'Deploy',
    receipt: 'vault 4Btq…9Kdm created · 276 bytes · 0.00562368 SOL rent',
    note: 'one program serves every vault; a vault is an account, not a deployment of your own bytecode.',
    transcript: ['forge deploy'],
  },
  {
    n: '3',
    title: 'Fund',
    receipt: 'funded · 1,240.000000000 SOL · 156,300.00 USDC',
    note: null,
    transcript: ['forge fund --side base --amount 1240', 'forge fund --side quote --amount 156300'],
  },
  {
    n: '4',
    title: 'Quote',
    receipt: 'quote live · mid 152.400000 USDC · half-spread 8 bps',
    note: null,
    transcript: ['forge quote --mid 152.40 --spread-bps 8 --skew-bps -12 --size 60'],
  },
  {
    n: '5',
    title: 'Status',
    receipt: 'vault 4Btq…9Kdm quoting · 1 swap filled · 18,647 CU',
    note: null,
    transcript: ['forge status'],
  },
]

export const fundFields: readonly FundField[] = [
  { id: 'fund-base', label: 'Base', value: '1,240.000000000', unit: 'SOL' },
  { id: 'fund-quote', label: 'Quote', value: '156,300.00', unit: 'USDC' },
]

export const fundHelper =
  "capital is the owner's alone — this vault takes no outside deposits and issues no shares."

export const quoteFields: readonly QuoteField[] = [
  { label: 'Mid', value: '152.400000 USDC' },
  { label: 'Half-spread', value: '8 bps' },
  { label: 'Skew', value: '−12 bps' },
  { label: 'Max order size', value: '60.000000000 SOL' },
]

export const ladderCaption = 'what this quote will look like on chain'

export const rejectionMint = 'KESTR2'

export const rejectionNotice =
  'rejected at step 2 — the mint carries a Token-2022 transfer-fee extension, so the amount received would differ from the amount sent, and the quote would not match execution. Deploy a vault for a pair without such an extension, or use a different mint.'

export const wizardFooter =
  '5 commands, and the last one prints a vault that has already taken its first swap.'
