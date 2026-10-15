import { ChatWorkspace } from "@/components/chat/chat-workspace"
import { VaultGate } from "@/components/chat/vault-gate"

/** Encrypted chat. The vault gate asks for the passphrase before any history is decrypted. */
export function ChatPage() {
  return (
    <VaultGate>
      <ChatWorkspace />
    </VaultGate>
  )
}
