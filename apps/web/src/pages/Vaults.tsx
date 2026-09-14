import RegisterTable from '@/components/RegisterTable'
import type { RegisterColumn, RegisterRow } from '@/components/RegisterTable'
import Shell from '@/components/Shell'
import { vaultNotes, vaultRows } from '@/mocks/vaults'
import { Link } from 'react-router-dom'

const columns: readonly RegisterColumn[] = [
  { key: 'pair', label: 'Pair' },
  { key: 'network', label: 'Network' },
  { key: 'inventory', label: 'Inventory', align: 'right' },
  { key: 'skew', label: 'Skew', align: 'right' },
  { key: 'quote', label: 'Quote', align: 'right' },
  { key: 'pnl', label: 'P&L today', align: 'right' },
  { key: 'state', label: 'State', align: 'right' },
]

const rows: readonly RegisterRow[] = vaultRows.map((vault) => ({
  id: vault.pair,
  cells: {
    pair: (
      <span>
        <span className="mono">{vault.pair}</span>
        {vault.assetNote !== null && (
          <>
            {' '}
            <span className="muted" style={{ fontSize: 11 }}>
              {vault.assetNote}
            </span>
          </>
        )}
      </span>
    ),
    network: <span className="mono">{vault.network}</span>,
    inventory: (
      <div className="cell-right">
        <div className="mono">{vault.inventoryBase}</div>
        <div className="mono muted">{vault.inventoryQuote}</div>
      </div>
    ),
    skew: <span className="mono">{vault.skew}</span>,
    quote: (
      <div className="cell-right">
        <div className={vault.tone === 'rust' ? 'mono rust' : 'mono'}>{vault.quote}</div>
        {vault.quoteAge !== null && <div className="mono muted">{vault.quoteAge}</div>}
      </div>
    ),
    pnl: <span className="mono">{vault.pnlToday}</span>,
    state:
      vault.tone === 'rust' ? (
        <span className="mono rust">{vault.state}</span>
      ) : (
        <span className="mono">
          <span className="dot" />
          {vault.state}
        </span>
      ),
  },
}))

const Vaults = () => (
  <Shell>
    <section className="section">
      <h2 className="sec-h">Vault register</h2>
      <RegisterTable
        columns={columns}
        rows={rows}
        caption="One vault trades one pair, with its own capital and its own risk limits."
      />
      <div style={{ display: 'grid', gap: 12, paddingTop: 20 }}>
        {vaultNotes.map((note) => (
          <p className="prose" key={note.slice(0, 12)}>
            {note}
          </p>
        ))}
      </div>
      <div className="btn-row">
        <Link className="btn" to="/new-vault">
          New vault →
        </Link>
      </div>
    </section>
  </Shell>
)

export default Vaults
