import type { ReactNode } from 'react'

export interface RegisterColumn {
  key: string
  label: string
  align?: 'left' | 'right'
}

export interface RegisterRow {
  id: string
  cells: Readonly<Record<string, ReactNode>>
}

interface RegisterTableProps {
  columns: readonly RegisterColumn[]
  rows: readonly RegisterRow[]
  caption?: string
}

/** A ruled register: hairlines only, no cards. Stacks to label/figure rows on narrow screens. */
const RegisterTable = ({ columns, rows, caption }: RegisterTableProps) => (
  <table className="rt">
    {caption !== undefined && (
      <caption
        style={{
          captionSide: 'top',
          textAlign: 'left',
          fontSize: 12,
          color: 'var(--doc-muted)',
          paddingBottom: 8,
        }}
      >
        {caption}
      </caption>
    )}
    <thead>
      <tr>
        {columns.map((col) => (
          <th key={col.key} scope="col" data-align={col.align ?? 'left'}>
            {col.label}
          </th>
        ))}
      </tr>
    </thead>
    <tbody>
      {rows.map((row) => (
        <tr key={row.id}>
          {columns.map((col) => {
            const cell = row.cells[col.key]
            return (
              <td key={col.key} data-label={col.label} data-align={col.align ?? 'left'}>
                {cell === undefined ? '—' : cell}
              </td>
            )
          })}
        </tr>
      ))}
    </tbody>
  </table>
)

export default RegisterTable
