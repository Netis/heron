# ts-protocol HTTP 解析重构设计

日期：2026-04-10

## 背景

基于 `docs/review/ts-protocol-http-review.md` 的评审意见，对 `server/ts-protocol/src/http.rs` 进行重构。目标：结构清晰、扩展性好、修正正确性问题。

本次只聚焦 `http.rs`，不改动 `tcp.rs` 的时间戳模型（评审第 4 点留后续处理）。

## 修复的问题

1. **Unknown framing response 完成判定不正确** — 当前靠"下一条 request 已出现"判断，对 1xx/204/304/HEAD/close-delimited 都不正确
2. **Chunked request body 错误产出空 body** — request 的 `Transfer-Encoding: chunked` 未识别，直接产出空 body 的 HttpRequest
3. **Chunked trailer 未正确消费** — 终止 chunk 后只处理了 `0\r\n\r\n`，trailer headers 会残留在 buffer
4. **SSE 与 chunked 耦合** — 非 chunked 的 `text/event-stream` response 不会产出 SseEvent

## 架构

```
HttpParser (状态机 + 事件 emit)
  ├── BodyReader (body framing 判定 + 解码)
  └── SseParser (SSE 事件切分)
```

### ParserState

简化为 4 个状态，去掉 `ReadingChunkedBody`（chunked 是 BodyReader 的内部细节）：

```rust
enum ParserState {
    WaitingForRequest,
    ReadingRequestBody,
    WaitingForResponse,
    ReadingResponseBody,
}
```

### BodyReader

```rust
enum BodyFraming {
    NoBody,              // 1xx, 204, 304, HEAD response
    ContentLength(usize),
    Chunked,
    CloseDelimited,      // 替代原来的 Unknown
}

enum ReadResult {
    NeedMore,
    Complete(Bytes),      // 完整 body
    ChunkDecoded(Bytes),  // chunked 下每解出一段就输出
}

struct BodyReader {
    framing: BodyFraming,
    decoded_body: BytesMut,
}
```

构造方法：

- `BodyReader::new_for_request(headers)` — 识别 chunked 或 Content-Length
- `BodyReader::new_for_response(status, req_method, headers)` — 完整 framing 判定：
  - 1xx / 204 / 304 → NoBody
  - req_method == HEAD → NoBody
  - Transfer-Encoding: chunked → Chunked
  - Content-Length → ContentLength(n)
  - 其他 → CloseDelimited

读取方法：

- `BodyReader::read(&mut self, buf: &mut BytesMut) → ReadResult`
  - NoBody → 立即 `Complete(empty)`
  - ContentLength(n) → buf 够长时 split_to 返回 `Complete`
  - Chunked → 解码一个 chunk 返回 `ChunkDecoded`，终止 chunk + trailer 消费完毕返回 `Complete`
  - CloseDelimited → 始终 `NeedMore`
- `BodyReader::finish(&mut self, buf: &mut BytesMut) → Bytes` — 供 close-delimited 场景，把 buf 剩余数据作为 body 返回

Chunked trailer 修复：终止 chunk 后，循环查找 `\r\n`，遇到空行才认为 trailer 结束。

### SseParser

```rust
struct SseParser {
    residual: String,
}
```

- `SseParser::push(&mut self, text: &str, ..., output)` — 追加文本，解析完整 SSE 事件
- `SseParser::flush(&mut self, ..., output)` — body 结束时 emit 残留事件
- `SseParser::reset(&mut self)` — 清空状态（新 response 开始时调用）

逻辑与当前 `parse_sse_chunk` / `flush_sse_residual` 一致，只是独立为 struct。

### HttpParser 主循环

**ReadingRequestBody**：
- 循环调用 `body_reader.read(client_buf)`
- `ChunkDecoded` → continue
- `Complete(body)` → emit HttpRequest，state → WaitingForResponse
- `NeedMore` → break

**WaitingForResponse → ReadingResponseBody**：
- headers 解析成功后，`BodyReader::new_for_response(status, method, headers)` 初始化 body_reader
- 如果 framing 是 NoBody，直接 emit HttpResponse，state → WaitingForRequest（不进入 ReadingResponseBody）

**ReadingResponseBody**：
- 循环调用 `body_reader.read(server_buf)`
- `ChunkDecoded(chunk)` → 如果 is_sse，喂给 `sse_parser.push()`
- `Complete(body)` → 如果 is_sse，调用 `sse_parser.flush()`；emit HttpResponse；state → WaitingForRequest
- `NeedMore` → break

**finish_response()**：
- 新增 `pub fn finish_response(server_buf, ..., output)` 方法
- 供 `TcpFlow` 在连接关闭时调用，把 server_buf 剩余数据作为 body flush 出去
- `tcp.rs` 需配合改动：`TcpFlow::push()` 检测到 Closed 时调用此方法（几行改动，不涉及时间戳）

### 辅助函数

保留 `find_crlf`、`is_chunked`、`is_sse`、`extract_content_length`。

删除 `looks_like_response_complete` — 被 BodyFraming 的正确语义替代。

## 测试计划

保留现有 4 个测试。新增：

| 测试 | 验证点 |
|------|--------|
| `204 No Content` response | NoBody 立即完成 |
| `304 Not Modified` response | NoBody 立即完成 |
| `HEAD` response | 即使有 Content-Length 也视为 NoBody |
| chunked request body | 正确解码，不再产出空 body |
| chunked trailer | trailer 被完整消费，不污染后续 |
| keep-alive 两轮 request/response | 状态正确重置 |
| 非 chunked SSE | Content-Length SSE 也产出 SseEvent |
| close-delimited + finish | FIN 后 flush 出 HttpResponse |

## 不在本次范围

- `tcp.rs` 时间戳模型修正（keep-alive 时间戳串消息）
- SSE 字段解析完整性（`data` 无冒号等边缘 case）
- HTTP/2 支持
