import { useCallback, useEffect, useRef, useState } from 'react'

/**
 * Loads a snapshot with `load` whenever `deps` change; `refresh` re-runs it.
 * Errors are captured (accounts simply may not exist yet on a fresh cluster).
 */
export function useAccountState<T>(load: () => Promise<T>, deps: unknown[]) {
  const [state, setState] = useState<T | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [loading, setLoading] = useState(false)

  // Only the newest load may publish. Without this, switching from pool A to
  // pool B can let A's slower response land after B's and overwrite it with the
  // previous pool's state (the same applies to a mint's token program).
  const runId = useRef(0)
  const run = useCallback(async () => {
    const id = ++runId.current
    setLoading(true)
    try {
      const next = await load()
      if (id !== runId.current) return
      setState(next)
      setError(null)
    } catch (cause) {
      if (id !== runId.current) return
      setState(null)
      setError(cause instanceof Error ? cause.message : String(cause))
    } finally {
      if (id === runId.current) setLoading(false)
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, deps)

  useEffect(() => {
    void run()
  }, [run])

  return { state, error, loading, refresh: run }
}

/** Controlled form field helper. */
export function useFormField(initial: string) {
  const [value, setValue] = useState(initial)
  return { value, onChange: (event: React.ChangeEvent<HTMLInputElement | HTMLSelectElement>) => setValue(event.target.value), set: setValue }
}
