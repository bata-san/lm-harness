/** Masked secrets returned by the server start with "****". */
export function isMasked(v: unknown): boolean {
  return typeof v === "string" && v.startsWith("****")
}

/** Mirrors backend `is_secret_name`: names containing TOKEN/KEY/SECRET/PASSWORD are secrets. */
export function isSecretName(name: string): boolean {
  return /TOKEN|KEY|SECRET|PASSWORD/i.test(name)
}

/** Save a JSON value as a file through a temporary object URL. */
export function downloadJson(filename: string, data: unknown) {
  const blob = new Blob([JSON.stringify(data, null, 2)], { type: "application/json" })
  const url = URL.createObjectURL(blob)
  const a = document.createElement("a")
  a.href = url
  a.download = filename
  document.body.appendChild(a)
  a.click()
  a.remove()
  setTimeout(() => URL.revokeObjectURL(url), 1000)
}
