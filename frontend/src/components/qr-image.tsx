import { useEffect, useState } from "react"
import QRCode from "qrcode"

/** QR code rendered client-side (the value never leaves the browser). Always white-on-black-free for scanner contrast. */
export function QrImage({ value, size = 192, className }: { value: string; size?: number; className?: string }) {
  const [src, setSrc] = useState<string | null>(null)
  useEffect(() => {
    let alive = true
    QRCode.toDataURL(value, { width: size * 2, margin: 2, errorCorrectionLevel: "M", color: { dark: "#000000", light: "#ffffff" } })
      .then((u) => alive && setSrc(u))
      .catch(() => alive && setSrc(null))
    return () => {
      alive = false
    }
  }, [value, size])
  if (!src) return <div style={{ width: size, height: size }} className={`animate-pulse rounded-lg bg-muted ${className ?? ""}`} />
  return <img src={src} alt="QR コード" width={size} height={size} className={`rounded-lg border bg-white ${className ?? ""}`} />
}
