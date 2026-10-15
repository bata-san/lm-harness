import { createContext, useContext } from "react"

import type { VaultSession } from "@/lib/vault"

/** The one VaultSession of the page; only provided while the vault is unlocked. */
export const VaultContext = createContext<VaultSession | null>(null)

// eslint-disable-next-line react-refresh/only-export-components
export function useVault(): VaultSession {
  const s = useContext(VaultContext)
  if (!s) throw new Error("useVault outside an unlocked VaultGate")
  return s
}
