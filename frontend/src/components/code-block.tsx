import { CopyIcon } from "lucide-react"

import { Button } from "@/components/ui/button"
import { copyText } from "@/lib/clipboard"
import { cn } from "@/lib/utils"

/** Monospace snippet with a copy button; scrolls horizontally instead of overflowing the page. */
export function CodeBlock({ code, className }: { code: string; className?: string }) {
  return (
    <div className={cn("relative rounded-lg border bg-muted/50", className)}>
      <pre className="overflow-x-auto p-3 pr-10 font-mono text-xs leading-relaxed whitespace-pre">{code}</pre>
      <Button type="button" size="icon-xs" variant="ghost" className="absolute top-1.5 right-1.5" title="コピー" aria-label="コピー" onClick={() => void copyText(code)}>
        <CopyIcon />
      </Button>
    </div>
  )
}
