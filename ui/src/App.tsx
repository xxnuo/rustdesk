import {
  IconActivity,
  IconAddressBook,
  IconDownload,
  IconFolderOpen,
  IconKeyboard,
  IconDeviceDesktop,
  IconMouse,
  IconPlug,
  IconRefresh,
  IconSearch,
  IconSettings,
  IconShieldCheck,
  IconUpload,
} from "@tabler/icons-react"
import { useEffect, useMemo, useRef, useState } from "react"
import { create } from "zustand"

import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"

type Bootstrap = {
  version: string
  platform: string
  access_mode: "local" | "lan" | "public"
  password_set: boolean
  config: WebConfig
}

type WebConfig = {
  enabled: boolean
  bind: string
  port: number
  public_access: boolean
}

type Peer = {
  id: string
  username: string
  hostname: string
  platform: string
  alias: string
  note: string
  modified_at: number
  password_saved: boolean
}

type AppState = {
  token: string
  bootstrap?: Bootstrap
  peers: Peer[]
  activePeer?: Peer
  setToken: (token: string) => void
  setBootstrap: (bootstrap: Bootstrap) => void
  setPeers: (peers: Peer[]) => void
  setActivePeer: (peer: Peer) => void
}

const useAppStore = create<AppState>((set) => ({
  token: localStorage.getItem("web-client-token") ?? "",
  peers: [],
  setToken: (token) => {
    localStorage.setItem("web-client-token", token)
    set({ token })
  },
  setBootstrap: (bootstrap) => set({ bootstrap }),
  setPeers: (peers) => set({ peers }),
  setActivePeer: (activePeer) => set({ activePeer }),
}))

function App() {
  const token = useAppStore((state) => state.token)
  const bootstrap = useAppStore((state) => state.bootstrap)
  const setBootstrap = useAppStore((state) => state.setBootstrap)
  const setPeers = useAppStore((state) => state.setPeers)
  const [error, setError] = useState("")

  useEffect(() => {
    fetchJson<Bootstrap>("/api/bootstrap")
      .then(setBootstrap)
      .catch(() => setError("无法读取 Web 客户端状态"))
  }, [setBootstrap])

  useEffect(() => {
    if (!token) return
    fetchJson<Peer[]>("/api/peers")
      .then(setPeers)
      .catch(() => setError("无法读取设备列表"))
  }, [setPeers, token])

  if (!token) {
    return <LoginView bootstrap={bootstrap} error={error} />
  }

  return <ClientShell bootstrap={bootstrap} error={error} />
}

function LoginView({
  bootstrap,
  error,
}: {
  bootstrap?: Bootstrap
  error: string
}) {
  const setToken = useAppStore((state) => state.setToken)
  const [password, setPassword] = useState("")
  const [busy, setBusy] = useState(false)
  const [loginError, setLoginError] = useState("")

  async function login() {
    setBusy(true)
    setLoginError("")
    try {
      const response = await fetchJson<{ token: string }>("/api/auth/login", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ password }),
      })
      setToken(response.token)
    } catch {
      setLoginError("密码错误或当前访问不被允许")
    } finally {
      setBusy(false)
    }
  }

  return (
    <main className="grid min-h-svh place-items-center bg-background px-6">
      <section className="w-full max-w-sm border border-border bg-card p-6 shadow-sm">
        <div className="mb-6 flex items-center justify-between">
          <div>
            <h1 className="text-lg font-semibold">RustDesk Web</h1>
            <p className="mt-1 text-sm text-muted-foreground">
              {bootstrap
                ? `${bootstrap.platform} · ${bootstrap.version}`
                : "正在读取状态"}
            </p>
          </div>
          <Badge variant={bootstrap?.password_set ? "secondary" : "outline"}>
            {bootstrap?.access_mode ?? "local"}
          </Badge>
        </div>
        <div className="space-y-3">
          <Input
            type="password"
            placeholder="本机访问密码"
            value={password}
            onChange={(event) => setPassword(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter") login()
            }}
          />
          <Button className="w-full" disabled={busy} onClick={login}>
            <IconShieldCheck />
            登录
          </Button>
        </div>
        {(loginError || error) && (
          <p className="mt-4 text-sm text-destructive">{loginError || error}</p>
        )}
      </section>
    </main>
  )
}

function ClientShell({
  bootstrap,
  error,
}: {
  bootstrap?: Bootstrap
  error: string
}) {
  const peers = useAppStore((state) => state.peers)
  const activePeer = useAppStore((state) => state.activePeer)
  const setActivePeer = useAppStore((state) => state.setActivePeer)
  const [query, setQuery] = useState("")
  const filteredPeers = useMemo(() => {
    const keyword = query.trim().toLowerCase()
    if (!keyword) return peers
    return peers.filter((peer) =>
      [peer.id, peer.alias, peer.hostname, peer.username, peer.platform]
        .join(" ")
        .toLowerCase()
        .includes(keyword)
    )
  }, [peers, query])

  return (
    <main className="flex min-h-svh bg-background text-foreground">
      <aside className="flex w-16 flex-col items-center border-r border-border bg-muted/35 py-3">
        <IconDeviceDesktop className="mb-5 size-6" />
        <NavIcon active icon={<IconPlug />} label="连接" />
        <NavIcon icon={<IconFolderOpen />} label="文件" />
        <NavIcon icon={<IconAddressBook />} label="地址簿" />
        <div className="flex-1" />
        <NavIcon icon={<IconSettings />} label="设置" />
      </aside>
      <section className="w-80 border-r border-border">
        <div className="border-b border-border p-3">
          <div className="mb-3 flex items-center justify-between">
            <div>
              <h2 className="font-semibold">设备</h2>
              <p className="text-xs text-muted-foreground">
                {bootstrap
                  ? `${bootstrap.config.bind}:${bootstrap.config.port}`
                  : "未连接服务"}
              </p>
            </div>
            <Button
              variant="ghost"
              size="icon-sm"
              onClick={() => location.reload()}
            >
              <IconRefresh />
            </Button>
          </div>
          <div className="relative">
            <IconSearch className="absolute top-2 left-2 size-4 text-muted-foreground" />
            <Input
              className="pl-8"
              placeholder="搜索设备"
              value={query}
              onChange={(event) => setQuery(event.target.value)}
            />
          </div>
        </div>
        <div className="h-[calc(100svh-97px)] overflow-auto">
          {filteredPeers.map((peer) => (
            <button
              className={`flex w-full items-start gap-3 border-b border-border px-3 py-3 text-left hover:bg-muted/60 ${
                activePeer?.id === peer.id ? "bg-muted" : ""
              }`}
              key={peer.id}
              onClick={() => setActivePeer(peer)}
            >
              <span className="grid size-9 shrink-0 place-items-center border border-border bg-background">
                <IconDeviceDesktop className="size-4" />
              </span>
              <span className="min-w-0 flex-1">
                <span className="block truncate text-sm font-medium">
                  {peer.alias || peer.hostname || peer.id}
                </span>
                <span className="mt-0.5 block truncate text-xs text-muted-foreground">
                  {peer.id} · {peer.platform || "unknown"}
                </span>
              </span>
            </button>
          ))}
          {!filteredPeers.length && (
            <div className="p-6 text-sm text-muted-foreground">暂无设备</div>
          )}
        </div>
      </section>
      <section className="flex min-w-0 flex-1 flex-col">
        <TopBar bootstrap={bootstrap} error={error} />
        {activePeer ? (
          <RemoteWorkspace peer={activePeer} />
        ) : (
          <EmptyWorkspace />
        )}
      </section>
    </main>
  )
}

function TopBar({
  bootstrap,
  error,
}: {
  bootstrap?: Bootstrap
  error: string
}) {
  return (
    <header className="flex h-14 items-center justify-between border-b border-border px-4">
      <div className="flex items-center gap-2 text-sm">
        <IconActivity className="size-4 text-emerald-600" />
        <span>Web bridge</span>
        <Badge variant="outline">{bootstrap?.access_mode ?? "local"}</Badge>
        {error && <span className="text-destructive">{error}</span>}
      </div>
      <div className="flex items-center gap-2">
        <Button variant="outline" size="sm">
          <IconUpload />
          上传
        </Button>
        <Button variant="outline" size="sm">
          <IconDownload />
          下载
        </Button>
      </div>
    </header>
  )
}

function RemoteWorkspace({ peer }: { peer: Peer }) {
  const canvasRef = useRef<HTMLCanvasElement | null>(null)

  useEffect(() => {
    const canvas = canvasRef.current
    if (!canvas) return
    const ctx = canvas.getContext("2d")
    if (!ctx) return
    const image = ctx.createImageData(canvas.width, canvas.height)
    for (let i = 0; i < image.data.length; i += 4) {
      const x = (i / 4) % canvas.width
      const y = Math.floor(i / 4 / canvas.width)
      image.data[i] = 28 + (x % 80)
      image.data[i + 1] = 35 + (y % 70)
      image.data[i + 2] = 42
      image.data[i + 3] = 255
    }
    ctx.putImageData(image, 0, 0)
  }, [peer.id])

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="flex h-11 items-center justify-between border-b border-border px-4">
        <div className="min-w-0 text-sm">
          <span className="font-medium">
            {peer.alias || peer.hostname || peer.id}
          </span>
          <span className="ml-2 text-muted-foreground">{peer.username}</span>
        </div>
        <div className="flex items-center gap-1">
          <Button variant="ghost" size="icon-sm">
            <IconMouse />
          </Button>
          <Button variant="ghost" size="icon-sm">
            <IconKeyboard />
          </Button>
        </div>
      </div>
      <div className="grid min-h-0 flex-1 grid-cols-[minmax(0,1fr)_300px]">
        <div className="grid place-items-center bg-zinc-950 p-4">
          <canvas
            ref={canvasRef}
            width={1280}
            height={720}
            className="max-h-full max-w-full border border-zinc-800 bg-black"
          />
        </div>
        <aside className="border-l border-border p-4">
          <h3 className="mb-3 text-sm font-semibold">会话</h3>
          <InfoRow label="模式" value="远程控制" />
          <InfoRow label="帧格式" value="RGBA8888" />
          <InfoRow label="状态" value="等待后端会话桥接" />
          <div className="mt-5 grid grid-cols-2 gap-2">
            <Button variant="outline" size="sm">
              文件
            </Button>
            <Button variant="outline" size="sm">
              聊天
            </Button>
          </div>
        </aside>
      </div>
    </div>
  )
}

function EmptyWorkspace() {
  return (
    <div className="grid flex-1 place-items-center">
      <div className="text-center text-sm text-muted-foreground">
        <IconDeviceDesktop className="mx-auto mb-3 size-8" />
        选择设备开始连接
      </div>
    </div>
  )
}

function NavIcon({
  icon,
  label,
  active,
}: {
  icon: React.ReactNode
  label: string
  active?: boolean
}) {
  return (
    <button
      aria-label={label}
      title={label}
      className={`mb-1 grid size-10 place-items-center border border-transparent ${
        active
          ? "bg-background text-foreground shadow-sm"
          : "text-muted-foreground"
      }`}
    >
      {icon}
    </button>
  )
}

function InfoRow({ label, value }: { label: string; value: string }) {
  return (
    <div className="mb-2 flex items-center justify-between gap-4 text-sm">
      <span className="text-muted-foreground">{label}</span>
      <span className="truncate">{value}</span>
    </div>
  )
}

async function fetchJson<T>(url: string, init?: RequestInit): Promise<T> {
  const response = await fetch(url, init)
  if (!response.ok) throw new Error(response.statusText)
  return response.json() as Promise<T>
}

export default App
