import { useCallback, useEffect, useState } from 'react'

/**
 * Loads a snapshot with `load` whenever `deps` change; `refresh` re-runs it.
 * Errors are captured (accounts simply may not exist yet on a fresh cluster).
 */
export function useAccountState<T>(load: () => Promise<T>, deps: unknown[]) {
  const [state, setState] = useState<T | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [loading, setLoading] = useState(false)

  const run = useCallback(async () => {
    setLoading(true)
    try {
      const next = await load()
      setState(next)
      setError(null)
    } catch (cause) {
      setState(null)
      setError(cause instanceof Error ? cause.message : String(cause))
    } finally {
      setLoading(false)
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
