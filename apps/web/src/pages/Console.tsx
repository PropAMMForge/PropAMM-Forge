import InstrumentBand from '@/components/InstrumentBand'
import PriceLadder from '@/components/PriceLadder'
import RegisterTable from '@/components/RegisterTable'
import type { RegisterColumn, RegisterRow } from '@/components/RegisterTable'
import Shell from '@/components/Shell'
import {
  activityRows,
  computeNote,
  computeRows,
  consoleBandFigures,
  inventoryRows,
  pnlNote,
  pnlRows,
  skewScaleEnds,
  skewScalePosition,
} from '@/mocks/console'
import type { KeyFigure } from '@/mocks/console'
import { solUsdcLadder } from '@/mocks/ladder'
import { Link } from 'react-router-dom'

const figureList = (rows: readonly KeyFigure[]) => (
  <table className="pairs">
    <tbody>
      {rows.map((row) => (
        <tr key={row.label} data-strong={row.strong === true}>
          <td className="k">{row.label}</td>
          <td>
            <span className="mono">{row.value}</span>
            {row.aside !== undefined && (
              <>
                {' · '}
                <span className="mono muted">{row.aside}</span>
              </>
            )}
          </td>
        </tr>
      ))}
    </tbody>
  </table>
)

const computeColumns: readonly RegisterColumn[] = [
  { key: 'instruction', label: 'Instruction' },
  { key: 'measured', label: 'Measured', align: 'right' },
  { key: 'budget', label: 'Budget', align: 'right' },
]

const computeTableRows: readonly RegisterRow[] = computeRows.map((row) => ({
  id: row.instruction,
  cells: {
    instruction: <span className="mono">{row.instruction}</span>,
    measured: (
      <div className="cell-right">
        <span className="mono">{row.measured}</span>
        {row.measuredShare !== null && (
          <div className="cu-bar-track">
            <div className="cu-bar" style={{ width: `${(row.measuredShare * 100).toFixed(2)}%` }} />
          </div>
        )}
      </div>
    ),
    budget: <span className="mono muted">{row.budget}</span>,
  },
}))

const activityColumns: readonly RegisterColumn[] = [
  { key: 'slot', label: 'Slot' },
  { key: 'side', label: 'Side' },
  { key: 'in', label: 'In', align: 'right' },
  { key: 'out', label: 'Out', align: 'right' },
  { key: 'price', label: 'Price', align: 'right' },
  { key: 'cu', label: 'CU', align: 'right' },
  { key: 'state', label: 'State', align: 'right' },
]

const activityTableRows: readonly RegisterRow[] = activityRows.map((row) => ({
  id: row.slot,
  cells: {
    slot: <span className="mono">{row.slot}</span>,
    side: row.side,
    in: <span className="mono">{row.amountIn}</span>,
    out: <span className="mono">{row.amountOut}</span>,
    price: <span className="mono">{row.price}</span>,
    cu: <span className="mono">{row.cu}</span>,
    state: row.refused ? (
      <span className="mono rust">{row.state}</span>
    ) : (
      <span className="mono">{row.state}</span>
    ),
  },
}))

const Console = () => (
  <Shell>
    <div className="selector" aria-label="Vault">
      <span className="navlink mono teal" aria-current="true">
        SOL/USDC
      </span>
      <span className="sep">·</span>
      <Link className="navlink mono" to="/vaults">
        ORVL/USDC
      </Link>
      <span className="sep">·</span>
      <Link className="navlink mono rust" to="/vaults">
        KESTR/USDC
      </Link>
    </div>

    <InstrumentBand>
      <PriceLadder data={solUsdcLadder} />
      <dl className="band-figs">
        {consoleBandFigures.map((fig) => (
          <div className="band-fig" key={fig.label}>
            <dt>{fig.label}</dt>
            <dd>{fig.value}</dd>
          </div>
        ))}
      </dl>
    </InstrumentBand>

    <section className="section">
      <h2 className="sec-h">Inventory</h2>
      {figureList(inventoryRows)}
      <div className="skew-scale">
        <div className="skew-mark" style={{ left: 0, background: 'var(--rust)' }} />
        <div
          className="skew-mark"
          style={{ left: `${(skewScalePosition * 100).toFixed(2)}%`, background: 'var(--teal)' }}
        />
        <div className="skew-mark" style={{ right: 0, background: 'var(--rust)' }} />
      </div>
      <div className="skew-ends">
        <span>{skewScaleEnds[0]}</span>
        <span>{skewScaleEnds[1]}</span>
      </div>
    </section>

    <section className="section">
      <h2 className="sec-h">Profit and loss — today</h2>
      {figureList(pnlRows)}
      <p className="note">{pnlNote}</p>
    </section>

    <section className="section">
      <h2 className="sec-h">Compute units</h2>
      <RegisterTable columns={computeColumns} rows={computeTableRows} />
      <p className="note mono">{computeNote}</p>
    </section>

    <section className="section">
      <h2 className="sec-h">Recent activity</h2>
      <RegisterTable columns={activityColumns} rows={activityTableRows} />
    </section>
  </Shell>
)

export default Console
