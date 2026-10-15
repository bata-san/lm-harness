import { useState } from "react"
import { CheckIcon, InfoIcon, LockIcon, Settings2Icon } from "lucide-react"

import { useVault } from "@/components/chat/vault-context"
import { Button } from "@/components/ui/button"
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover"
import { IDLE_CHOICES, readIdlePref, writeIdlePref } from "@/lib/vault-prefs"
import { cn } from "@/lib/utils"

const IDLE_LABEL: Record<(typeof IDLE_CHOICES)[number], string> = { 5: "5 分", 15: "15 分", 60: "60 分", 0: "なし" }

/** Auto-lock period. Stored per viewer; applied to the live session immediately. */
export function AutoLockPopover() {
  const session = useVault()
  const [minutes, setMinutes] = useState(readIdlePref)
  return (
    <Popover>
      <PopoverTrigger render={<Button variant="ghost" size="icon-sm" aria-label="自動ロックの設定" title="自動ロックの設定" />}>
        <Settings2Icon />
      </PopoverTrigger>
      <PopoverContent align="end" className="w-60">
        <p className="mb-2 font-medium">自動ロック</p>
        <p className="mb-3 text-xs text-muted-foreground">操作がない時間が続くと、鍵を捨てて合言葉の入力に戻ります。</p>
        <div role="radiogroup" aria-label="自動ロックまでの時間" className="grid gap-1">
          {IDLE_CHOICES.map((m) => (
            <button
              key={m}
              type="button"
              role="radio"
              aria-checked={minutes === m}
              onClick={() => {
                setMinutes(m)
                writeIdlePref(m)
                session.setIdleMinutes(m)
              }}
              className={cn("flex items-center justify-between rounded-md px-2.5 py-1.5 text-left hover:bg-muted", minutes === m && "font-medium")}
            >
              {IDLE_LABEL[m]}
              {minutes === m && <CheckIcon className="size-4" />}
            </button>
          ))}
        </div>
      </PopoverContent>
    </Popover>
  )
}

/** The privacy statement. Kept deliberately plain: what is encrypted, and what is not. */
export function InfoPopover() {
  return (
    <Popover>
      <PopoverTrigger render={<Button variant="ghost" size="icon-sm" aria-label="プライバシーについて" title="プライバシーについて" />}>
        <InfoIcon />
      </PopoverTrigger>
      <PopoverContent align="end" className="w-80 text-sm leading-6">
        <p className="font-medium">プライバシーについて</p>
        <ul className="mt-2 grid list-disc gap-1.5 pl-4 text-muted-foreground">
          <li>履歴はブラウザで暗号化され、サーバには暗号文だけが保存されます。</li>
          <li>ただしモデルが応答を生成する間、サーバ(推論機)は平文を処理します。</li>
          <li>トンネル事業者は通信内容を見られる場合があります。</li>
          <li>合言葉と復号鍵はこのタブのメモリにのみ保持され、ロックやページ終了で消えます。</li>
          <li>タイトル検索と書き出しはブラウザ内で行われます。</li>
        </ul>
      </PopoverContent>
    </Popover>
  )
}

export function LockButton() {
  const session = useVault()
  return (
    <Button variant="outline" size="sm" onClick={() => session.lock()} aria-label="ロック" title="鍵を捨てて合言葉の入力に戻ります">
      <LockIcon /> <span className="hidden sm:inline">ロック</span>
    </Button>
  )
}
