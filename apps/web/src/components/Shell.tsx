import type { ReactNode } from 'react'
import { Link, useLocation } from 'react-router-dom'

const NAV: readonly { to: string; label: string }[] = [
  { to: '/', label: 'Console' },
  { to: '/vaults', label: 'Vaults' },
  { to: '/new-vault', label: 'New vault' },
]

interface ShellProps {
  children: ReactNode
}

const Shell = ({ children }: ShellProps) => {
  const { pathname } = useLocation()

  return (
    <div className="page">
      <div className="col">
        <div className="proj">
          <h1 className="proj-name">Halstead Quantitative — propamm-forge</h1>
          <span className="addr muted">7xKq…3Nde</span>
        </div>
        <nav className="nav" aria-label="Screens">
          <div className="linkrow">
            {NAV.map((item, i) => (
              <span key={item.to} className="linkrow">
                {i > 0 && <span className="sep">·</span>}
                <Link
                  className="navlink"
                  to={item.to}
                  data-current={pathname === item.to}
                  aria-current={pathname === item.to ? 'page' : undefined}
                >
                  {item.label}
                </Link>
              </span>
            ))}
          </div>
          <span className="stamp">devnet · mock data</span>
        </nav>
        {children}
      </div>
    </div>
  )
}

export default Shell
