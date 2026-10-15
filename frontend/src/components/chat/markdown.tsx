import { Children, isValidElement, memo, type ReactNode } from "react"
import ReactMarkdown, { type Components } from "react-markdown"
import remarkGfm from "remark-gfm"

import { CodeBlock } from "@/components/code-block"
import { cn } from "@/lib/utils"

// No rehype-raw: raw HTML in model output is rendered as text. Images are not loaded
// (a remote image URL would send a request from the browser), so they show their alt text.

function textOf(node: ReactNode): string {
  if (node === null || node === undefined || typeof node === "boolean") return ""
  if (typeof node === "string" || typeof node === "number") return String(node)
  if (Array.isArray(node)) return node.map(textOf).join("")
  if (isValidElement<{ children?: ReactNode }>(node)) return textOf(node.props.children)
  return ""
}

const components: Components = {
  a({ href, children }) {
    // react-markdown drops unsafe URLs (javascript:, etc.) to an empty href: show plain text instead of a dead link.
    if (!href) return <span>{children}</span>
    return (
      <a href={href} target="_blank" rel="noopener noreferrer" className="text-primary underline underline-offset-4 break-all">
        {children}
      </a>
    )
  },
  img({ alt }) {
    return <span className="text-muted-foreground">[画像: {alt || "省略"}]</span>
  },
  pre({ children }) {
    const child = Children.toArray(children)[0]
    const className = isValidElement<{ className?: string }>(child) ? (child.props.className ?? "") : ""
    const lang = /language-([\w+-]+)/.exec(className)?.[1]
    const code = textOf(child).replace(/\n$/, "")
    return (
      <div className="my-2">
        {lang && <div className="rounded-t-lg border border-b-0 bg-muted px-3 py-1 font-mono text-[10px] text-muted-foreground">{lang}</div>}
        <CodeBlock code={code} className={cn(lang && "rounded-t-none")} />
      </div>
    )
  },
  table({ children }) {
    return (
      <div className="my-2 overflow-x-auto">
        <table className="w-full border-collapse text-sm">{children}</table>
      </div>
    )
  },
  th({ children }) {
    return <th className="border px-2 py-1 text-left font-medium">{children}</th>
  },
  td({ children }) {
    return <td className="border px-2 py-1 align-top">{children}</td>
  },
}

/** Markdown for model output and chat messages. Safe by construction: no raw HTML, no remote images. */
export const Markdown = memo(function Markdown({ text, className }: { text: string; className?: string }) {
  return (
    <div className={cn("markdown min-w-0 break-words text-[15px] leading-7 [&_blockquote]:border-l-2 [&_blockquote]:pl-3 [&_blockquote]:text-muted-foreground [&_h1]:mt-4 [&_h1]:mb-2 [&_h1]:text-xl [&_h1]:font-semibold [&_h2]:mt-4 [&_h2]:mb-2 [&_h2]:text-lg [&_h2]:font-semibold [&_h3]:mt-3 [&_h3]:mb-1 [&_h3]:font-semibold [&_li]:my-0.5 [&_ol]:list-decimal [&_ol]:pl-6 [&_p]:my-2 [&_p:first-child]:mt-0 [&_p:last-child]:mb-0 [&_ul]:list-disc [&_ul]:pl-6 [&_code:not(pre_code)]:rounded [&_code:not(pre_code)]:bg-muted [&_code:not(pre_code)]:px-1 [&_code:not(pre_code)]:font-mono [&_code:not(pre_code)]:text-[0.85em]", className)}>
      <ReactMarkdown remarkPlugins={[remarkGfm]} components={components}>
        {text}
      </ReactMarkdown>
    </div>
  )
})
