import { Link } from "react-router"

const PAGES = [
  {
    to: "/debug/pipeline-health",
    title: "Pipeline Health",
    desc: "Internal pipeline metrics, backpressure, funnel, errors.",
  },
]

export function DebugIndexPage() {
  return (
    <div className="mx-auto max-w-2xl p-6">
      <h1 className="mb-1 text-lg font-semibold">Debug</h1>
      <p className="mb-4 text-sm text-muted-foreground">
        Developer-only diagnostic pages. Not linked from the main nav.
      </p>
      <ul className="flex flex-col gap-2">
        {PAGES.map((p) => (
          <li key={p.to}>
            <Link
              to={p.to}
              className="block rounded-md border border-border bg-card p-3 hover:bg-accent"
            >
              <div className="text-sm font-medium">{p.title}</div>
              <div className="text-xs text-muted-foreground">{p.desc}</div>
              <div className="mt-1 font-mono text-xs text-muted-foreground">
                {p.to}
              </div>
            </Link>
          </li>
        ))}
      </ul>
    </div>
  )
}
