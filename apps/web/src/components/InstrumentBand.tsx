import type { ReactNode } from 'react'

interface InstrumentBandProps {
  children: ReactNode
}

/** The one filled surface in the app: full-bleed to the content column, no radius, no border. */
const InstrumentBand = ({ children }: InstrumentBandProps) => <div className="band">{children}</div>

export default InstrumentBand
