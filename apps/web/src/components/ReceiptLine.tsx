interface ReceiptLineProps {
  n: string
  title: string
  result: string
  note?: string | null
}

/** What a completed step collapses into: number, title, and its one-line result. */
const ReceiptLine = ({ n, title, result, note }: ReceiptLineProps) => (
  <div className="receipt">
    <span className="mono muted">{n}</span>
    <div>
      <h3 className="step-title">{title}</h3>
      <p className="receipt-line">{result}</p>
      {note !== undefined && note !== null && <p className="receipt-line">{note}</p>}
    </div>
  </div>
)

export default ReceiptLine
