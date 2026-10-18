import { useContext } from 'react'
import { AppContext, type AppContextValue } from './appContextValue'

/** The app context. Lives apart from `AppContext.tsx` for Fast Refresh. */
export function useApp(): AppContextValue {
  const context = useContext(AppContext)
  if (!context) throw new Error('useApp must be used inside <AppProvider>')
  return context
}
