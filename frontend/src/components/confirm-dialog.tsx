import { useState, type ReactNode } from "react"
import { Loader2Icon } from "lucide-react"
import { toast } from "sonner"

import { Button } from "@/components/ui/button"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"

/**
 * Confirmation dialog for destructive actions. The confirm button shows a spinner and is
 * disabled while `onConfirm` runs (no double submits) and the dialog cannot be dismissed meanwhile.
 * onConfirm should throw on failure; the error is shown as a toast and the dialog stays open.
 */
export function ConfirmDialog({
  open,
  onOpenChange,
  title,
  description,
  children,
  confirmLabel,
  destructive = true,
  disabled = false,
  onConfirm,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
  title: string
  description?: ReactNode
  children?: ReactNode
  confirmLabel: string
  destructive?: boolean
  disabled?: boolean
  onConfirm: () => Promise<unknown> | unknown
}) {
  const [pending, setPending] = useState(false)

  async function run() {
    if (pending) return
    setPending(true)
    try {
      await onConfirm()
      onOpenChange(false)
    } catch (e) {
      toast.error((e as Error).message)
    } finally {
      setPending(false)
    }
  }

  return (
    <Dialog open={open} onOpenChange={(o) => !pending && onOpenChange(o)}>
      <DialogContent className="max-h-[90dvh] overflow-y-auto" showCloseButton={!pending}>
        <DialogHeader>
          <DialogTitle>{title}</DialogTitle>
          {description && <DialogDescription>{description}</DialogDescription>}
        </DialogHeader>
        {children}
        <DialogFooter>
          <Button variant="outline" disabled={pending} onClick={() => onOpenChange(false)}>
            キャンセル
          </Button>
          <Button variant={destructive ? "destructive" : "default"} disabled={pending || disabled} onClick={run}>
            {pending && <Loader2Icon className="animate-spin" />}
            {confirmLabel}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
