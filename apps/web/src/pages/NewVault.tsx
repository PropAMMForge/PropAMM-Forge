import InstrumentBand from '@/components/InstrumentBand'
import PriceLadder from '@/components/PriceLadder'
import ReceiptLine from '@/components/ReceiptLine'
import Shell from '@/components/Shell'
import { solUsdcLadder } from '@/mocks/ladder'
import {
  fundFields,
  fundHelper,
  ladderCaption,
  quoteFields,
  rejectionMint,
  rejectionNotice,
  wizardFooter,
  wizardSteps,
} from '@/mocks/wizard'
import { useEffect, useRef, useState } from 'react'
import type { ReactNode } from 'react'

const NewVault = () => {
  const [completed, setCompleted] = useState<number>(2)
  const [collapsing, setCollapsing] = useState<number | null>(null)
  const [amounts, setAmounts] = useState<Record<string, string>>(() =>
    Object.fromEntries(fundFields.map((field) => [field.id, field.value])),
  )
  const timer = useRef<number | null>(null)

  useEffect(
    () => () => {
      if (timer.current !== null) window.clearTimeout(timer.current)
    },
    [],
  )

  const complete = (index: number): void => {
    setCollapsing(index)
    timer.current = window.setTimeout(() => {
      setCompleted(index + 1)
      setCollapsing(null)
    }, 180)
  }

  const bodyFor = (index: number, active: boolean): ReactNode => {
    if (index === 2) {
      return (
        <div>
          {fundFields.map((field) => (
            <div className="field" key={field.id}>
              <label htmlFor={field.id}>{field.label}</label>
              <input
                id={field.id}
                value={amounts[field.id] ?? field.value}
                inputMode="decimal"
                onChange={(event) => {
                  const next = event.target.value
                  setAmounts((prev) => ({ ...prev, [field.id]: next }))
                }}
              />
              <span className="unit">{field.unit}</span>
            </div>
          ))}
          <p className="note">{fundHelper}</p>
          {active && (
            <div className="btn-row">
              <button type="button" className="btn" onClick={() => complete(2)}>
                Fund vault →
              </button>
            </div>
          )}
        </div>
      )
    }
    if (index === 3) {
      return (
        <div>
          {quoteFields.map((field) => (
            <div className="field" data-muted={!active} key={field.label}>
              <span className="fld-name">{field.label}</span>
              <span className="val">{field.value}</span>
            </div>
          ))}
          {active && (
            <div className="btn-row">
              <button type="button" className="btn" onClick={() => complete(3)}>
                Publish quote →
              </button>
            </div>
          )}
        </div>
      )
    }
    if (index === 4) {
      return (
        <div>
          <p className="note">
            prints the vault, its quote, its inventory skew and the swaps it has taken.
          </p>
          {active && (
            <div className="btn-row">
              <button type="button" className="btn" onClick={() => complete(4)}>
                Run status →
              </button>
            </div>
          )}
        </div>
      )
    }
    return null
  }

  return (
    <Shell>
      <div className="wizard">
        <div className="wizard-forms">
          {wizardSteps.map((step, index) => {
            const isDone = index < completed
            const isOpen = index === completed
            const state = isDone ? 'done' : isOpen ? 'open' : 'ahead'

            if (isDone) {
              return (
                <div className="step" data-state="done" key={step.n}>
                  <ReceiptLine
                    n={step.n}
                    title={step.title}
                    result={step.receipt}
                    note={step.note}
                  />
                  {index === 3 && (
                    <div>
                      <p className="band-label">{ladderCaption}</p>
                      <InstrumentBand>
                        <PriceLadder data={solUsdcLadder} />
                      </InstrumentBand>
                    </div>
                  )}
                </div>
              )
            }

            return (
              <div className="step" data-state={state} key={step.n}>
                <span className="step-num">{step.n}</span>
                <div>
                  <h3 className="step-title">{step.title}</h3>
                  <div className="step-body" data-collapsed={collapsing === index}>
                    {bodyFor(index, isOpen)}
                  </div>
                  {index === 3 && (
                    <>
                      <p className="band-label">{ladderCaption}</p>
                      <InstrumentBand>
                        <PriceLadder data={solUsdcLadder} />
                      </InstrumentBand>
                    </>
                  )}
                </div>
              </div>
            )
          })}
        </div>

        <div className="transcript">
          <h2>Transcript</h2>
          <dl style={{ margin: 0 }}>
            {wizardSteps.map((step, index) => (
              <div className="tr-block" data-current={index === completed} key={step.n}>
                <dt>
                  {step.n} · {step.title.toLowerCase()}
                </dt>
                {step.transcript.map((line) => (
                  <dd className={index === completed ? '' : 'muted'} key={line}>
                    {line}
                  </dd>
                ))}
              </div>
            ))}
          </dl>
        </div>
      </div>

      <p className="rejection">
        <span className="mono">{rejectionMint}</span> {rejectionNotice}
      </p>
      <p className="note" style={{ paddingTop: 18 }}>
        {wizardFooter}
      </p>
    </Shell>
  )
}

export default NewVault
