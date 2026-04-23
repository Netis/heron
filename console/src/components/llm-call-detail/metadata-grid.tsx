import type { LlmCallDetail } from "@/types/api"

interface Props {
  detail: LlmCallDetail
}

export function MetadataGrid({ detail }: Props) {
  const rows: [string, string][] = [
    ["ID", detail.id],
    ["Source", detail.source_id || "—"],
    ["Response ID", detail.response_id ?? "—"],
    ["Path", detail.request_path],
    ["Client", `${detail.client_ip}:${detail.client_port}`],
    ["Server", `${detail.server_ip}:${detail.server_port}`],
    ["Stream", detail.is_stream ? "Yes" : "No"],
    ["API Type", detail.api_type],
  ]

  return (
    <div className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 px-4 py-3 text-sm">
      {rows.map(([label, value]) => (
        <div key={label} className="contents">
          <span className="text-muted-foreground">{label}</span>
          <span className="truncate font-mono text-xs" title={value}>
            {value}
          </span>
        </div>
      ))}
    </div>
  )
}
