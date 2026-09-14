import type { LadderData, LadderQuote } from '@/mocks/ladder'
import { useEffect, useRef, useState } from 'react'

interface PriceLadderProps {
  data: LadderData
}

const TICKS: readonly number[] = [30, 20, 10, 0, -10, -20, -30]

const MONO = "ui-monospace, 'SF Mono', 'Cascadia Mono', Menlo, Consolas, monospace"

const tickLabel = (bps: number): string => {
  if (bps === 0) return '0 bps'
  return bps > 0 ? `+${bps} bps` : `−${Math.abs(bps)} bps`
}

/** Axis geometry: computed once in [`PriceLadder`] and handed to the markings. */
interface Geometry {
  narrow: boolean
  left: number
  axisRight: number
  ruleEnd: number
  y: (bps: number) => number
}

/**
 * Markings that exist only with a live quote: the spread band, the two sides
 * and the skew bracket. Pulled out of [`PriceLadder`] as a separate component —
 * together with the geometry and the axis it is one function of excessive complexity.
 */
const QuoteMarks = ({ data, geom }: { data: LadderQuote; geom: Geometry }) => {
  const { narrow, left, axisRight, ruleEnd, y } = geom
  const sideFont = narrow ? 8.5 : 11

  return (
    <>
      {/* the spread, drawn as an area */}
      <rect
        x={left}
        y={y(data.ask.bps)}
        width={ruleEnd - left}
        height={y(data.bid.bps) - y(data.ask.bps)}
        fill="#1b2a28"
      />
      <text
        x={(left + ruleEnd) / 2}
        y={(y(data.ask.bps) + y(data.bid.bps)) / 2 + 3.5}
        textAnchor="middle"
        fill="#7c838b"
        fontFamily="var(--mono)"
        fontSize={narrow ? 9 : 10.5}
      >
        {data.spreadLabel}
      </text>

      {/* ask and bid rules */}
      {[data.ask, data.bid].map((side) => (
        <g key={side.name}>
          <line
            x1={left}
            x2={ruleEnd}
            y1={y(side.bps)}
            y2={y(side.bps)}
            stroke="#2e6f63"
            strokeWidth={2}
          />
          {narrow ? (
            <>
              <text
                x={ruleEnd + 6}
                y={y(side.bps) - 2}
                fill="#e9eae6"
                fontFamily="var(--mono)"
                fontSize={sideFont}
              >
                {side.name}
              </text>
              <text
                x={ruleEnd + 6}
                y={y(side.bps) + 9}
                fill="#e9eae6"
                fontFamily="var(--mono)"
                fontSize={sideFont}
              >
                {side.price}
              </text>
            </>
          ) : (
            <text
              x={ruleEnd + 10}
              y={y(side.bps) + 4}
              fill="#e9eae6"
              fontFamily="var(--mono)"
              fontSize={sideFont}
            >
              {`${side.name}  ${side.price}`}
            </text>
          )}
        </g>
      ))}

      {/* the lean bracket */}
      <g>
        <line
          x1={axisRight + 12}
          x2={axisRight + 12}
          y1={y(0)}
          y2={y(data.leanBps)}
          stroke="#e9eae6"
          strokeWidth={1}
        />
        <line
          x1={axisRight + 8}
          x2={axisRight + 16}
          y1={y(0)}
          y2={y(0)}
          stroke="#e9eae6"
          strokeWidth={1}
        />
        <line
          x1={axisRight + 8}
          x2={axisRight + 16}
          y1={y(data.leanBps)}
          y2={y(data.leanBps)}
          stroke="#e9eae6"
          strokeWidth={1}
        />
        <text
          x={axisRight + 20}
          y={y(data.leanBps / 2) + 3}
          fill="#e9eae6"
          fontFamily="var(--mono)"
          fontSize={narrow ? 9 : 11}
        >
          {data.leanLabel}
        </text>
        <text
          x={axisRight + 20}
          y={y(data.leanBps / 2) + (narrow ? 15 : 18)}
          fill="#7c838b"
          fontFamily="var(--mono)"
          fontSize={narrow ? 8 : 9.5}
        >
          {data.leanNote[0]}
        </text>
        <text
          x={axisRight + 20}
          y={y(data.leanBps / 2) + (narrow ? 25 : 30)}
          fill="#7c838b"
          fontFamily="var(--mono)"
          fontSize={narrow ? 8 : 9.5}
        >
          {data.leanNote[1]}
        </text>
      </g>
    </>
  )
}

/**
 * Vertical price axis measured in basis points away from the mid.
 * Hand-drawn SVG; nothing here is interactive and nothing animates.
 */
const PriceLadder = ({ data }: PriceLadderProps) => {
  const hostRef = useRef<HTMLDivElement>(null)
  const [width, setWidth] = useState<number>(900)

  useEffect(() => {
    const el = hostRef.current
    if (!el) return undefined
    setWidth(el.clientWidth)
    const observer = new ResizeObserver((entries) => {
      const entry = entries[0]
      if (!entry) return
      setWidth(entry.contentRect.width)
    })
    observer.observe(el)
    return () => observer.disconnect()
  }, [])

  const narrow = width < 640
  const height = narrow ? 220 : 260
  const top = 30
  const bottom = height - 30
  const left = narrow ? 56 : 64
  const right = narrow ? 120 : 140
  const axisRight = Math.max(left + 40, width - right)

  const y = (bps: number): number => top + ((30 - bps) / 60) * (bottom - top)

  const geom: Geometry = {
    narrow,
    left,
    axisRight,
    ruleEnd: left + Math.max(64, (axisRight - left) * (narrow ? 0.42 : 0.55)),
    y,
  }
  const tickFont = narrow ? 8.5 : 9.5

  return (
    <div className="ladder-host" ref={hostRef}>
      <svg
        width={width}
        height={height}
        role="img"
        aria-label="Price ladder in basis points away from the mid"
        style={{ display: 'block', fontFamily: MONO }}
      >
        {/* bps ticks */}
        {TICKS.map((bps) => (
          <g key={bps}>
            {bps !== 0 && (
              <line
                x1={left}
                x2={axisRight}
                y1={y(bps)}
                y2={y(bps)}
                stroke="#262b31"
                strokeWidth={1}
              />
            )}
            <text
              x={left - 8}
              y={y(bps) + 3}
              textAnchor="end"
              fill="#7c838b"
              fontFamily="var(--mono)"
              fontSize={tickFont}
            >
              {tickLabel(bps)}
            </text>
          </g>
        ))}

        {data.kind === 'quote' && <QuoteMarks data={data} geom={geom} />}

        {/* the mid */}
        <line x1={left} x2={axisRight} y1={y(0)} y2={y(0)} stroke="#e9eae6" strokeWidth={1} />
        <text
          x={left + 6}
          y={y(0) - 7}
          fill="#e9eae6"
          fontFamily="var(--mono)"
          fontSize={narrow ? 9.5 : 11}
        >
          {data.kind === 'quote' ? 'MID' : data.lastMidLabel}
        </text>
        {data.kind === 'quote' && (
          <text
            x={axisRight}
            y={y(0) - 7}
            textAnchor="end"
            fill="#e9eae6"
            fontFamily="var(--mono)"
            fontSize={narrow ? 9.5 : 11}
          >
            {data.midPrice}
          </text>
        )}

        {data.kind === 'noQuote' && (
          <text
            x={(left + axisRight) / 2}
            y={y(-15)}
            textAnchor="middle"
            fill="#a6402f"
            fontFamily="var(--mono)"
            fontSize={narrow ? 10.5 : 13}
          >
            {data.message}
          </text>
        )}
      </svg>
    </div>
  )
}

export default PriceLadder
