# 分布式 eBPF 采集：安装、使用与验证

把采集下沉到每台业务主机的「瘦探针」`heron-probe`，在主机本地的 TLS 边界用
eBPF SSL-uprobe 抓取明文（带进程归因），脱敏后经 **mTLS** 上送到一个集中的
`heron` collector 做重的解析与聚合。本文覆盖**安装、使用、验证**三件事。

- 探针架构 / 线协议 / 脱敏的设计依据见
  [docs/design/02-capture.md](design/02-capture.md)。
- 探针打包（systemd / K8s 镜像）见 [deploy/probe/README.md](../deploy/probe/README.md)。
- 单机（NIC / pcap / cloud-probe）安装见 [docs/install.md](install.md)。本文只讲分布式拓扑。

---

## 1. 架构总览

```
每台业务主机                                         集中 collector
┌─────────────────────────────┐                   ┌────────────────────────────┐
│ heron-probe                 │   mTLS（探针拨出） │ heron                       │
│  eBPF SSL_read/SSL_write    │ ───────────────▶  │  type="thin-probe" 监听     │
│  → RawPacket（含 pid/comm） │   ProbeBatch 帧    │  → 解析 → turn → 存储       │
│  → 边缘脱敏（等长抹除密钥） │                   │  → REST API + 控制台         │
│  → 批处理上送               │                   │                            │
└─────────────────────────────┘                   └────────────────────────────┘
        ▲ N 台主机各一只探针，按 source_id 归因               │
                                                     DuckDB / 控制台 :3000
```

关键事实：

- **拆分点在 `RawPacket`。** 探针只做抓包 + 脱敏 + 上送；wire-API 解码、turn 组装、
  聚合、存储全部留在中央。所以探针很「瘦」，一支探针很少需要随业务升级。
- **mTLS 是准入边界。** 探针拨出连接中央，出示客户端证书；中央要求并校验客户端证书。
  明文（含 API key）绝不在未认证的连接上过网。
- **`source_id` 决定归因。** 优先用探针 batch 里声明的 `source_id`（如 K8s 节点名），
  否则回退到客户端证书 CN。中央按 `hash(source_id)` 把同一探针的包路由到同一 worker。
- **离线 / 在线对等。** 中央跑的是与本地 eBPF source **逐字节相同**的解析路径——这一点
  由差分回归测试（见 §7）强制保证。

---

## 2. 前置条件

| 角色 | 要求 |
|---|---|
| 中央 collector | 任意 Linux/macOS（thin-probe 监听在所有平台都可解析）；一个对探针可达的端口（默认 `5556`） |
| 探针 `heron-probe` | **Linux**；`CAP_BPF` + `CAP_PERFMON` + 内核 BTF（`/sys/kernel/btf/vmlinux`）。老内核（≲5.16，`perf_event_paranoid` 收紧）再加 `CAP_SYS_ADMIN` |
| 网络 | 探针**拨出**到中央（NAT/防火墙友好）；只需中央的监听端口对探针开放 |
| 证书 | 一套自签 mTLS PKI（CA + 中央服务端证书 + 探针客户端证书），见 §3 |

> eBPF 采集只能在 Linux 主机上跑通。无 BPF 工具链的开发机（如 macOS）可把探针的
> `[source]` 换成 `type = "pcap-file"` 回放——它走的是**完整的上送链路**，足以验证
> mTLS + 批处理 + 中央归因（见 §6.1 / §7）。

---

## 3. 签发 mTLS 证书（一次性）

mTLS 需要三类材料：一个 **CA**、一张**中央服务端证书**（带 `serverAuth` EKU +
SAN）、一张或多张**探针客户端证书**（带 `clientAuth` EKU）。

> **必须用 RSA，不要用 EC。** rustls 用的 ring 加载 ECDSA 私钥时要求 PKCS#8 里内嵌
> 公钥，而 openssl 的 EC keygen 不写入 → 中央/探针启动时报
> `failed to parse private key as RSA, ECDSA, or EdDSA`。**RSA-2048 PKCS#8**（openssl
> 默认）可被 rustls+ring 干净加载。这是实测踩过的坑（见 §8）。

下面这套命令与 CI 的 `distributed-soak.sh` 用的完全一致，已验证可用：

```bash
# 1) CA
openssl req -x509 -newkey rsa:2048 -nodes \
  -keyout ca.key -out ca.crt -days 3650 -subj "/CN=heron-ca"

# 2) 中央服务端证书：serverAuth EKU + SAN（rustls WebPki 两者都要）
#    把 DNS:central.internal 换成探针配置里 server_name 用的那个名字
openssl req -newkey rsa:2048 -nodes -keyout server.key -out server.csr \
  -subj "/CN=central"
openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key -CAcreateserial \
  -out server.crt -days 825 \
  -extfile <(printf 'subjectAltName=DNS:central.internal\nextendedKeyUsage=serverAuth\n')

# 3) 探针客户端证书：clientAuth EKU。CN 在不显式设置 source_id 时即探针身份
openssl req -newkey rsa:2048 -nodes -keyout client.key -out client.csr \
  -subj "/CN=gateway-1"
openssl x509 -req -in client.csr -CA ca.crt -CAkey ca.key -CAcreateserial \
  -out client.crt -days 825 \
  -extfile <(printf 'extendedKeyUsage=clientAuth\n')
```

- 所有探针可**共用一张客户端证书**——逐探针身份用 `source_id`（K8s 节点名等）承载，
  不必逐探针签证书。要按证书区分身份就每台签一张、CN 设成主机名。
- 私钥权限 `600`，绝不入库（`.gitignore` 已忽略；本仓任何外发面也不得泄露）。
- `server.crt` 里的 SAN 必须包含探针配置里 `server_name` 的那个名字，否则握手失败。

---

## 4. 部署中央 collector

中央是普通的 `heron`，在 pipeline 里加一个 `type = "thin-probe"` 的 source。
**无需 eBPF 特性**——监听 + 解析路径在所有平台都编译。

中央 `config.toml`：

```toml
[[pipeline.sources]]
type = "thin-probe"
# 监听所有网卡的 5556（区别于 cloud-probe 的 5555，二者可在同一中央并存）
listen = "0.0.0.0:5556"

[pipeline.sources.tls]
cert = "/etc/heron/server.crt"        # §3 的中央服务端证书
key = "/etc/heron/server.key"
client_ca = "/etc/heron/ca.crt"       # 签发探针客户端证书的 CA — 准入名单的根
```

启动（参照 [docs/install.md](install.md) 的 systemd 段，换上上面的 config）：

```bash
heron -c /etc/heron/config.toml
# 控制台 http://localhost:3000
```

中央侧无需抓包权限（不直接读网卡），按普通服务用户运行即可。`client_ca` 定义了
「允许接入的探针集合」——出示的证书不能链到它，握手即被拒。

---

## 5. 部署探针 `heron-probe`

打包细节（二进制构建、systemd 单元、K8s DaemonSet、镜像）见
[deploy/probe/README.md](../deploy/probe/README.md)，这里给最小路径。

### 5.0 构建（`--features ebpf`）

eBPF 引擎默认关闭，要用 `ebpf` 特性编进去（需 nightly + `rust-src` + `bpf-linker`）：

```bash
rustup toolchain install nightly --component rust-src
cargo install bpf-linker --locked
cd server && cargo build --release --bin heron-probe --features ebpf
# → server/target/release/heron-probe（仅 Linux）
```

### 5.1 探针配置

拷贝 [`server/config/heron-probe.example.toml`](../server/config/heron-probe.example.toml)
改之：

```toml
central_endpoint = "central.internal:5556"   # 中央 thin-probe 监听地址
server_name = "central.internal"             # 必须是中央 server.crt 的一个 SAN
# source_id = "gateway-1"                     # 不设则用客户端证书 CN

[tls]
cert = "/etc/heron-probe/client.crt"         # §3 的探针客户端证书
key = "/etc/heron-probe/client.key"
server_ca = "/etc/heron-probe/ca.crt"        # 校验中央服务端证书的 CA

[source]
type = "ebpf"                                 # 生产用 eBPF；ssl_libs 空 = 自动探测
ssl_libs = []
pid_allowlist = []
segment_size = 16384

[source.redaction]                            # 边缘脱敏：等长抹除密钥后再上送
enabled = true                                # 强烈建议开（mTLS 之上的纵深防御）

[batching]                                    # max_packets 或 flush_ms 先到者 flush
max_packets = 256
flush_ms = 100
```

### 5.2 systemd（推理 / 网关主机）

```bash
sudo install -m0755 server/target/release/heron-probe /usr/local/bin/heron-probe
sudo useradd --system --no-create-home --shell /usr/sbin/nologin heron-probe || true
sudo install -d -o heron-probe -g heron-probe /etc/heron-probe
sudo cp server/config/heron-probe.example.toml /etc/heron-probe/heron-probe.toml   # 然后编辑
sudo install -m0600 client.crt client.key ca.crt /etc/heron-probe/
sudo cp deploy/probe/heron-probe.service /etc/systemd/system/
sudo systemctl daemon-reload && sudo systemctl enable --now heron-probe
```

单元用 `AmbientCapabilities=CAP_BPF CAP_PERFMON CAP_SYS_ADMIN` 授予 eBPF 权限，进程
本身仍是非特权 `heron-probe` 用户。内核不要求时可去掉 `CAP_SYS_ADMIN`。

### 5.3 Kubernetes（每节点一只，DaemonSet）

```bash
kubectl create namespace heron
kubectl -n heron create secret generic heron-probe-tls \
  --from-file=client.crt --from-file=client.key --from-file=ca.crt
# 改 k8s-daemonset.yaml 里的镜像与 central_endpoint 占位符后 apply
kubectl apply -f deploy/probe/k8s-daemonset.yaml
```

Pod 的 `source_id` 通过 Downward API 注入为**节点名**
（`HERON_PROBE_SOURCE_ID ← spec.nodeName`），中央即按节点归因。

---

## 6. 验证采集链路

### 6.1 离线 smoke（任意主机，无需 eBPF / 集群）

校验 systemd 单元的权限、DaemonSet 结构、内嵌配置：

```bash
deploy/probe/smoke.sh
```

无 BPF 工具链的机器要验证**上送链路本身**，把探针 `[source]` 换成 pcap 回放
（example 配置里有现成片段），它会真实地建立 mTLS、批处理、上送到中央：

```toml
[source]
type = "pcap-file"
path = "/path/to/corpus.pcap"
source_id = "smoke"
```

### 6.2 在线真实采集验证（Linux + CAP_BPF + 可达中央）

1. **探针起来了。** systemd：`systemctl status heron-probe`；K8s：
   `kubectl -n heron rollout status ds/heron-probe`。日志应出现
   `heron-probe: uplink connected`。
2. **造点流量。** 在主机上跑一个 LLM 客户端，例如 `claude`，或
   `curl https://api.anthropic.com/...`。
3. **中央确认带进程归因地收到了：**

   ```bash
   # turns 的每个 call 应带 process_pid/comm/exe，source_id = 探针身份（节点名/CN）
   # 注意 start/end 是「秒」且必填，响应封装在 data.items 里（见 §8）
   curl 'http://localhost:3000/api/agent-turns?start=0&end=4102444800&page=1&page_size=100'

   # capture 组的 batches_received / pkts_received 应在涨，且无异常 drop
   curl http://localhost:3000/api/internal-metrics
   ```
4. **脱敏生效。** 抓到的请求头 / body 里凭据应被掩码——`Authorization: ****`、
   `sk-****`——绝不出现真实 key。

### 6.3 脱敏自检要点

边缘脱敏是**等长抹除**：密钥字节被等长替换，所以帧长不变，但密钥不过网。
`sk-` / `Bearer ` 这类前缀**保留**（便于识别类型），其后的密钥主体被掩。这一行为
由 §7 的 `redaction_over_wire` 回归测试钉死（含一个「不开脱敏确实泄漏」的对照，
证明测试有牙）。

---

## 7. 回归与发布门禁验证（开发 / CI 侧）

分布式链路有**两道**确定性验证，挡住回归进生产。

### 7.1 每 PR：进程内正确性（无需 eBPF / VM / root）

在 `server/` 下 `cargo test --workspace` 即跑全部。关键用例：

| 测试 | 断言 |
|---|---|
| `h-turn/tests/wire_equivalence.rs` | **差分基石**：同一 corpus，本地直采 vs 经 探针→mTLS→中央，产出的 turns/calls **逐字段一致**。中央漂移即红 |
| `h-turn/tests/redaction_over_wire.rs` | 密钥（header 与 body 两形态）都不过网、等长、前缀保留；关掉脱敏的对照**确实**泄漏 |
| `h-capture/src/transport_scale_test.rs` | 50 探针→中央零丢 + 按 `source_id` 分片隔离；探针重启续传；背压有界无损；坏帧/版本不一致不卡死连接与邻居 |

> corpus 用例依赖 git-LFS 的 pcap；LFS 对象缺失时这些用例**优雅跳过**而非失败。

判定器单测（stdlib，无需 pip）随 `ci.yml` 跑：

```bash
python3 scripts/staging/tests/test_distributed_invariants.py
```

### 7.2 合并到 main：大规模 soak 门禁（staging）

`distributed-soak.sh` 起一个**隔离的中央** + N 个合成探针（pcap 回放、`loop_secs` +
`rate_pps` 持续配速加压）→ 采样中央指标 → 喂给判定器 → stamp `distributed-soaked`
提交状态。本机可直接跑通（自签 PKI、单机）：

```bash
cd server && cargo build --release --bin heron --bin heron-probe
bash scripts/staging/distributed-soak.sh \
  --central server/target/release/heron \
  --probe   server/target/release/heron-probe \
  --corpus  server/h-protocol/tests/fixtures/keepalive_2sse_pipelined.pcap \
  --probes 3 --duration 30 --rate-pps 300 --json-out /tmp/verdict.json
```

判定器 `distributed_invariants.py` 检查：中央侧负载不变式（无 FATAL、队列有界
<80%、RSS 稳定、无背压丢弃、无 flush 错误、达到包量地板）**＋ 分布式专属**——
所有探针都上报、每探针都有 call、无越界 `source_id`、`CaptureZmqBatchesDropped == 0`。

### 7.3 发布门禁

`distributed-soak.yml`（与 `staging-soak` / `ebpf-soak` 并行）在 `deploy-staging`
成功后跑，stamp `distributed-soaked`。`deploy-prod.yml` 与 `release.yml` 现在都要求
三个状态**全绿**才放行：

```
ci(main) → deploy-staging → { staging-soaked, ebpf-soaked, distributed-soaked } → [人工审批] → deploy-prod
                                                  ↓
                                    v* tag → release.yml gate（三状态全绿才出包）
```

> **B2（真实 eBPF 多 VM 保真）** 复用 `ebpf-soak.sh` 机制，在 staging 的少量 VM 上跑
> 真实 uprobe 探针，校验进程归因端到端到达中央——只能在 Linux VM 上验证，是 B1
> 合成压测之外的「生产保真」半边。

---

## 8. 故障排查

| 症状 | 原因 / 处理 |
|---|---|
| 启动报 `failed to parse private key as RSA, ECDSA, or EdDSA` | 用了 EC 私钥；ring 加载 ECDSA 需 PKCS#8 内嵌公钥，openssl EC 不写入。**改用 RSA-2048**（§3） |
| 探针日志反复重连、握手失败 | ① 中央 `server.crt` 的 SAN 不含探针 `server_name`；② 探针证书没链到中央 `client_ca`；③ EKU 缺失（服务端要 `serverAuth`，客户端要 `clientAuth`） |
| `/api/agent-turns` 返回 `{}` 或空 | `start`/`end` **必填且单位是秒**（不是微秒；2100 年上限 = `4102444800`），响应封装在 `data.items` 里。按 §6.2 的查询串来 |
| 中央偶发 read error，恰在探针退出时 | 旧版探针不发 TLS `close_notify` 就断流。当前 `ProbeUplink` 优雅退出会 `framed.close()`，正常关闭不应再有此告警 |
| 探针起不来：`capture.ebpf` 检查失败 | 缺 `CAP_BPF`/`CAP_PERFMON` 或内核无 BTF（`/sys/kernel/btf/vmlinux`）。老内核再加 `CAP_SYS_ADMIN`。先跑 `heron-probe`/`heron doctor` 确认 |
| 控制台空白页（中央） | 中央二进制没带 `--features console`（见 [install.md](install.md) 的「Building from source」） |

---

## 相关文档

- [docs/install.md](install.md) — 单机安装、权限、pcap 回放
- [docs/configure.md](configure.md) — pipeline / source / 存储 / 保留 / API 配置参考
- [docs/design/02-capture.md](design/02-capture.md) — 采集层设计（含线协议、脱敏、分布式拓扑）
- [deploy/probe/README.md](../deploy/probe/README.md) — 探针打包（systemd / K8s 镜像）
