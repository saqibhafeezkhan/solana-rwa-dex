import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
// Polyfill Buffer for @solana/web3.js in the browser.
import { Buffer } from "buffer"
(globalThis as any).Buffer = Buffer
import './index.css'
import App from './App.tsx'

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <App />
  </StrictMode>,
)
