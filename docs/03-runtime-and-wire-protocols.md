# Runtime、Jupyter 与 SSH 协议移植规格

研究日期：2026-11-16。本文描述源码行为，不声称已经通过线上 Colab 验证。

## 1. 证据与兼容目标

- **事实**：可从固定提交中的可执行代码直接确定。
- **设计**：建议 Rust 采用的行为，可能有意修正原实现。
- **待验证**：需要真实账号、运行时或网络观测才能确定。
- CLI 基线：`googlecolab/google-colab-cli@02d2321c38dd215b7804eebf78b61d98b856bcd3`。
- Kernel 客户端基线：`googlecolab/jupyter-kernel-client@f18e982c3265df5e923aa9def101ab3fd737e139`。
- 后者就是 CLI 的 uv.lock 固定提交，不是另外选取的最新版本。[R01]
- 本轮未授权账号、未创建 VM、未执行远端代码。
- 本地临时 clone 只用于阅读；证据使用下面的固定 GitHub permalink。

## 2. 连接对象与生命周期

CLI 的 `ColabRuntime` 接收 runtime URL、proxy token、kernel ID、session ID。
这些对象不能混为一谈：[R02]

| 标识 | 角色 | Rust 数据模型建议 |
|---|---|---|
| runtime URL | VM 的 Jupyter HTTP/WebSocket 代理基址 | 校验后的 URL |
| proxy token | 访问该 runtime 的凭据 | 敏感字符串，禁止 Debug 明文 |
| kernel ID | 服务端 Python kernel 实例 | 可持久化的独立字段 |
| session ID | Jupyter 消息/WS 客户端身份 | 独立 UUID 字段 |
| msg_id | 单次执行或请求身份 | 每次请求生成新 UUID |
| parent_header.msg_id | 响应属于哪次请求 | 分发和完成判断的索引 |

**事实**：传入 kernel ID 时，依赖构造已有 kernel model 并 GET 刷新。[R03]
没有 kernel 时按依赖启动逻辑创建；CLI 把新 ID 写回本地状态。[R02]
普通命令退出只关闭 channel/socket，避免销毁持续存在的 kernel。
`runtime.stop(shutdown_kernel=True)` 才试图关闭 kernel。[R04]
因此不能把 Rust `Drop` 默认实现为 DELETE kernel。

**事实**：CLI 强制 `_own_kernel=False`，避免依赖析构时自动删除 kernel。[R02]
Rust 应将“拥有连接”与“拥有远端 VM/kernel”分开建模。
`run --keep` 退出保留 VM；无 keep 时在 finally 释放 VM。[R27]
清理失败不得覆盖脚本已经确定的退出码。

### 2.1 当前依赖的注释与实现差异

CLI 兼容三种类名：ColabKernelClient、KernelClient、JupyterKernelClient。[R02]
锁定依赖实际提供 KernelClient；不能从兼容注释推定另一分支已被运行。
CLI 传入 `extra_params` 和 `session`，但锁定依赖构造代码没有消费这两个值。[R05]
其 `WSSession` 是新建的，连接 query 使用该对象的 session 值。[R06]
所以“跨 CLI 进程恢复既有 Jupyter session 身份”只能认定为调用方意图。
**设计**：Rust 明确设置消息 session 和 WS session_id，并记录实际值。
**待验证**：服务端是否要求特定 session 身份；重连后状态能否一致恢复。

## 3. Jupyter REST 表面

以下是依赖直接使用的 HTTP 接口，不是推测出的私有 RPC：[R03][R26]

| 方法 | 相对路径 | 请求 / 作用 |
|---|---|---|
| GET | `/api/kernels` | 列出 kernel model |
| POST | `/api/kernels` | JSON `{"name":name,"path":path}` 创建 kernel |
| GET | `/api/kernels/{id}` | 刷新已有 kernel model |
| DELETE | `/api/kernels/{id}` | 关闭并删除 kernel |
| POST | `/api/kernels/{id}/restart` | 重启 kernel |
| POST | `/api/kernels/{id}/interrupt` | 中断 kernel |
| WS upgrade | `/api/kernels/{id}/channels` | 复用一条连接承载多个逻辑通道 |

HTTP 依赖 fetch 使用 Bearer token；CLI 额外加以下 header：[R02][R07]

```http
Authorization: Bearer <runtime-proxy-token>
X-Colab-Client-Agent: colab-cli
X-Colab-Runtime-Proxy-Token: <runtime-proxy-token>
```

WS URL 把 http/https 换成 ws/wss，追加 session_id 和 token。[R06]
headers 从 manager 传给 WebSocket 客户端。[R03]
不要把普通 Google OAuth token 和 runtime proxy token 互换。
不要把源 URL 简单拼接成重复 `?` 或丢失 path prefix 的字符串。
**设计**：URL 拼接统一走 URL 类型，query 以键值列表编码。

## 4. DEFAULT WebSocket 帧格式

当前 CLI 明确选择 DEFAULT，不协商 V1 subprotocol。[R02]
无 buffers 的消息以 WebSocket TEXT 发送 JSON；有 buffers 时用 BINARY。[R08]
JSON envelope 包含 header、parent_header、metadata、content、channel 等字段。
channel 决定消息进入 shell、iopub、stdin、control 等哪个队列。[R28]

DEFAULT 二进制格式为：[R09]

```text
u32be N
u32be offset[0]
...
u32be offset[N-1]
UTF-8 JSON bytes
buffer[1] bytes
...
buffer[N-1] bytes
```

- N 是 JSON 段加原始 binary buffer 段的总数量。
- offset 均相对整帧起点；`offset[0] = 4 * (N + 1)`。
- JSON 范围为 offset[0] 到 offset[1]。
- 最后 buffer 的结束位置就是帧长度。
- `buffers` 从 JSON envelope 移出，以原始 bytes 附加。
- 锁定依赖的 binary decoder 要求 `N >= 2`。
- 因此 N=1 的无 buffers 消息按当前 sender 行为走 TEXT。
- header.date 在编码时转换为字符串。
- TEXT 分支直接解析 JSON，无二进制 offset header。

**设计**：先验证帧长度、N 上限、header 长度、offset 单调性和所有边界。
不能直接相信服务端 N 并分配任意长度内存。
不能把二进制 buffers 当 UTF-8 解码或经 JSON 字符串往返。
必须为畸形 offsets、空 TEXT、无效 JSON、未知 channel 定义可控错误。
未知消息类型可保留原始 envelope，不能让后台 reader panic。

### 4.1 V1 的范围

依赖也实现 `v1.kernel.websocket.jupyter.org`，但当前 CLI 不选择它。[R06][R09]
V1 使用 little-endian u64 offset 数量及 offsets，先放 channel，再放分段消息。
这与 DEFAULT 的 big-endian u32 布局完全不同，不能共用一个未经区分的 parser。
**设计**：首个兼容版本实现 DEFAULT；V1 放在明确可选择的协议模块。
**待验证**：当前 Colab 对 V1 是否有功能差异，不能仅凭标准支持推定可用。

## 5. 消息路由与执行完成

| 通道 / 类型 | 客户端行为 | 证据 |
|---|---|---|
| shell / execute_request | 发送 code、执行选项，返回 msg_id | [R29] |
| iopub / stream | 追加 stdout/stderr 输出 | [R11] |
| iopub / execute_result | 保留 MIME data、metadata、execution_count | [R11] |
| iopub / display_data | 保留 MIME data、metadata、transient | [R11] |
| iopub / update_display_data | 按 transient.display_id 更新已有输出 | [R11] |
| iopub / clear_output | 清空累计输出；原实现忽略 wait | [R11] |
| iopub / error | 保留 ename/evalue/traceback | [R11] |
| iopub / status idle | 本次输出阶段完成，继续等 shell reply | [R10] |
| shell / execute_reply | 最终执行结果；不能用它替代输出收集 | [R10] |
| stdin / input_request | 显示提示并获取输入 | [R13] |
| stdin / input_reply | 返回用户输入 value | [R13] |
| iopub / colab_request | 进入 Colab 专用处理器 | [R14] |

执行状态机：[R10]

```text
Ready -> Send execute_request -> WaitingIopub
WaitingIopub -> 匹配 parent msg_id 的输出 -> WaitingIopub
WaitingIopub -> 匹配的 status=idle -> WaitingShellReply
WaitingShellReply -> 匹配的 execute_reply -> Completed
任何等待 -> 连接断开 / kernel dead或restarting -> Failed
```

只处理 parent_header.msg_id 等于本次执行 ID 的正常响应。
其他客户端的输出不能当成本次输出，也不能延长本次超时。
kernel dead/restarting 是无 parent 的广播，也必须使等待终止。
shell reply 可能先进入队列，iopub idle 后再读取队列仍能完成。
不能收到第一个 stream、status busy 或任意 shell 消息就结束。
reply 表示 error 但未收到 error output 时，CLI 合成一个 error output。[R10]
**设计**：请求上下文统一持有 output accumulator、reply 和终止原因。
连接 reader 可独立运行，执行顺序由 runtime 锁控制，不需后台 daemon。

### 5.1 超时的两套语义

非交互 exec/run/repl/install 使用“本次相关消息的空闲期限”。[R10]
持续有输出时可运行超过 timeout 总时长；None 或 <=0 无限制。
每次等待相关消息重新建立 deadline；无关请求消息不会重置 deadline。
原实现先等 iopub idle，再为 shell reply 建立新的空闲期限。
allow_stdin=true 路径交给依赖，其 timeout 是整次调用期限。[R13]
**设计**：用 `IdleTimeout` 与 `Deadline` 两种类型，避免同一参数暗换含义。
timeout 只表示客户端停止等待，不能据此声称远端执行已取消。
**待验证**：超时后如何安全重新附着同次执行及获取遗漏输出。

## 6. Colab 扩展与远端 Python

Drive 的 dfs_ephemeral 请求不是普通 input(prompt)。[R14]
收到请求后读 metadata.colab_msg_id，做 credentials-propagation 流程。
成功时发送 stdin/input_reply，content 为：

```json
{"value":{"type":"colab_reply","colab_msg_id":"<request-id>"}}
```

reply.parent_header 取原 colab_request.header。[R14]
这证明纯通用 Jupyter 客户端还不足以复现全部 Colab 自动化。
Rust 可以实现消息处理器，不需要本地 Python interpreter。
远端仍执行 Python，例如 google.colab.auth/drive 以及用户训练脚本。
“本地全 Rust”不等于“Colab VM 不使用 Python”。
DriveFS 扩展的另一条路径详见 [DriveFS 规格](04-drivefs.md)。

## 7. 输出、Notebook、REPL 和 run

输出对象不能只有 text：MIME bundle、图片和 traceback 都属于兼容范围。[R11]
CLI 显示 PNG/JPEG，并将 notebook code cell 的 outputs 写到新文件 `<name>_output.ipynb`。[R15][R30]
Notebook 的 markdown/raw cell 不执行，缺失 id 的 cell 会生成 UUID。[R15]
Rust 应用 serde_json 保留未知 notebook 字段，避免往返破坏 metadata。
REPL 的多行编辑、着色和终端图片是客户端 UI 能力，不是 Python 执行能力。[R16]
`run` 设置 sys.argv、__name__ 和环境变量，再原样附加去掉 shebang 的脚本。[R12]
参数和环境变量嵌入 Python 时必须编码为安全 literal，不能简单拼接引号。
SystemExit 需要映射脚本退出码；其他 kernel error 映射失败。[R12]
即使输送失败也必须尝试 teardown；长任务清理时需刷新过期 proxy token。[R27]

## 8. SSH-over-WebSocket 契约

SSH 是不同于 kernel channels 的独立传输。[R17]

```text
wss://<runtime-host>/colab/ssh
 ?colab-runtime-proxy-token=<proxy-token>
 &colab-client-agent=colab-cli
 &colab-client-agent=ssh-mode-<interactive|cmd|proxy>
```

```http
X-Colab-Ssh-Pubkey: <OpenSSH public key line>
X-Colab-Client-Agent: colab-cli
```

两个同名 colab-client-agent 参数必须都保留，不能转成会覆盖的 map。
当前代码丢弃 runtime URL 的原 path，固定使用 `/colab/ssh`。[R17]
公钥支持 .pub 文件或从 private key 经 ssh-keygen 导出；默认扫描 Ed25519/ECDSA。[R18]
服务端接受的 key 类型说明来自错误映射；本轮未向真实服务端验证。[R19]
桥接 stdin -> binary WS；WS text/binary payload -> stdout 原始 bytes。[R20]
PING/PONG 等控制帧不能写入 SSH 字节流。
stdout 必须没有进度条、自动更新提示或 session 创建文案。

### 8.1 OpenSSH、PTY 与参数

原实现依赖系统 ssh，且用自身 `ssh --proxy-mode` 作为 ProxyCommand。[R21]
Rust 保留系统 ssh 能达到相同依赖边界；移除 ssh 则要另实现 SSH 协议层。
ProxyCommand 子进程继承非默认 --auth 和绝对 --config，防止读取错误会话。[R21]
交互连接 root@colab-runtime，强制 -t，切到 /content 后启动 login shell。
命令模式仅在 stdin/stdout 都是 TTY 时 -t，否则 -T。
单个 CMD 参数解释为 shell command line；多个参数先 shell-quote 成精确 argv。
命令模式不创建 VM，--rm 被忽略；退出码透传 ssh subprocess。[R21][R22]
原实现关闭 host key 检查并使用空 known_hosts 文件，属于现有行为。[R21]

### 8.2 错误、信号与清理

握手失败映射 400/401/403/404/429/502，通常退出 1。[R19]
参数冲突和身份/会话选择错误通常退出 2。[R18][R22]
429 的 limit 仅在 JSON error=already-active-session、整数1..5时显示。[R19]
这只是客户端解析范围，不能据此承诺服务端固定最多五条连接。
proxy bridge 读写异常被原实现吞掉并返回 0；完整兼容需明确保留还是修正。[R20]
--rm proxy 收到 SIGHUP/SIGTERM/SIGINT 先清理再 os._exit(0)。[R23]
清理函数用 stopped 标记保证信号和 finally 不重复执行。
交互 --rm 只删除本次自动创建的 VM，不删除已复用 VM。[R23]
新发现：公钥解析和 WS connect 在 try/finally 之前，握手失败可能跳过 --rm 清理。[R23]
**设计**：创建 VM 后立即安装生命周期 guard；握手、子进程和桥接均置于 guard 内。
清理失败应留下可重试状态，不能只有“terminated”成功文案。

## 9. 验收矩阵与尚未证实的能力

- DEFAULT TEXT/BINARY 编解码：使用固定 bytes fixtures 做跨语言对照。
- 超时：持续输出、静默、无关请求、reply 迟到、kernel restart、断线。[R24]
- 输出：display_id 更新、clear_output、stderr、图片、error 合成。[R11]
- 生命周期：普通退出保留 kernel；stop/run teardown/keep 各自独立。[R04][R27]
- SSH：loopback server 检查重复 query、公钥 header、模式与字节透明性。[R25]
- 故障注入：WS 握手失败、EOF、SIGHUP、清理失败和脚本非零退出。[R23]
- 真实集成：认证、CPU/GPU/TPU、长任务、Drive、人机输入、SSH 各单独验证。
- 未完成真实集成前，只能声明协议实现与测试通过，不能声明线上完全等价。

## 10. 固定源码证据索引

[R01]: https://github.com/googlecolab/google-colab-cli/blob/02d2321c38dd215b7804eebf78b61d98b856bcd3/uv.lock#L557-L559
[R02]: https://github.com/googlecolab/google-colab-cli/blob/02d2321c38dd215b7804eebf78b61d98b856bcd3/src/colab_cli/runtime.py#L30-L160
[R03]: https://github.com/googlecolab/jupyter-kernel-client/blob/f18e982c3265df5e923aa9def101ab3fd737e139/jupyter_kernel_client/manager.py#L69-L155
[R04]: https://github.com/googlecolab/google-colab-cli/blob/02d2321c38dd215b7804eebf78b61d98b856bcd3/src/colab_cli/runtime.py#L337-L351
[R05]: https://github.com/googlecolab/jupyter-kernel-client/blob/f18e982c3265df5e923aa9def101ab3fd737e139/jupyter_kernel_client/wsclient.py#L490-L535
[R06]: https://github.com/googlecolab/jupyter-kernel-client/blob/f18e982c3265df5e923aa9def101ab3fd737e139/jupyter_kernel_client/wsclient.py#L565-L596
[R07]: https://github.com/googlecolab/jupyter-kernel-client/blob/f18e982c3265df5e923aa9def101ab3fd737e139/jupyter_kernel_client/manager.py#L22-L42
[R08]: https://github.com/googlecolab/jupyter-kernel-client/blob/f18e982c3265df5e923aa9def101ab3fd737e139/jupyter_kernel_client/wsclient.py#L247-L267
[R09]: https://github.com/googlecolab/jupyter-kernel-client/blob/f18e982c3265df5e923aa9def101ab3fd737e139/jupyter_kernel_client/utils.py#L22-L127
[R10]: https://github.com/googlecolab/google-colab-cli/blob/02d2321c38dd215b7804eebf78b61d98b856bcd3/src/colab_cli/runtime.py#L243-L335
[R11]: https://github.com/googlecolab/jupyter-kernel-client/blob/f18e982c3265df5e923aa9def101ab3fd737e139/jupyter_kernel_client/client.py#L24-L105
[R12]: https://github.com/googlecolab/google-colab-cli/blob/02d2321c38dd215b7804eebf78b61d98b856bcd3/src/colab_cli/commands/run.py#L60-L178
[R13]: https://github.com/googlecolab/google-colab-cli/blob/02d2321c38dd215b7804eebf78b61d98b856bcd3/src/colab_cli/runtime.py#L187-L242
[R14]: https://github.com/googlecolab/google-colab-cli/blob/02d2321c38dd215b7804eebf78b61d98b856bcd3/src/colab_cli/commands/automation.py#L69-L148
[R15]: https://github.com/googlecolab/google-colab-cli/blob/02d2321c38dd215b7804eebf78b61d98b856bcd3/src/colab_cli/commands/execution.py#L80-L269
[R16]: https://github.com/googlecolab/google-colab-cli/blob/02d2321c38dd215b7804eebf78b61d98b856bcd3/src/colab_cli/repl.py#L32-L174
[R17]: https://github.com/googlecolab/google-colab-cli/blob/02d2321c38dd215b7804eebf78b61d98b856bcd3/src/colab_cli/commands/ssh.py#L240-L251
[R18]: https://github.com/googlecolab/google-colab-cli/blob/02d2321c38dd215b7804eebf78b61d98b856bcd3/src/colab_cli/commands/ssh.py#L74-L178
[R19]: https://github.com/googlecolab/google-colab-cli/blob/02d2321c38dd215b7804eebf78b61d98b856bcd3/src/colab_cli/commands/ssh.py#L254-L397
[R20]: https://github.com/googlecolab/google-colab-cli/blob/02d2321c38dd215b7804eebf78b61d98b856bcd3/src/colab_cli/commands/ssh.py#L408-L451
[R21]: https://github.com/googlecolab/google-colab-cli/blob/02d2321c38dd215b7804eebf78b61d98b856bcd3/src/colab_cli/commands/ssh.py#L454-L554
[R22]: https://github.com/googlecolab/google-colab-cli/blob/02d2321c38dd215b7804eebf78b61d98b856bcd3/src/colab_cli/commands/ssh.py#L799-L835
[R23]: https://github.com/googlecolab/google-colab-cli/blob/02d2321c38dd215b7804eebf78b61d98b856bcd3/src/colab_cli/commands/ssh.py#L626-L714
[R24]: https://github.com/googlecolab/google-colab-cli/blob/02d2321c38dd215b7804eebf78b61d98b856bcd3/tests/test_streaming.py#L73-L155
[R25]: https://github.com/googlecolab/google-colab-cli/blob/02d2321c38dd215b7804eebf78b61d98b856bcd3/tests/test_ssh_wire_contract.py#L282-L508
[R26]: https://github.com/googlecolab/jupyter-kernel-client/blob/f18e982c3265df5e923aa9def101ab3fd737e139/jupyter_kernel_client/manager.py#L200-L319
[R27]: https://github.com/googlecolab/google-colab-cli/blob/02d2321c38dd215b7804eebf78b61d98b856bcd3/src/colab_cli/commands/run.py#L413-L508
[R28]: https://github.com/googlecolab/jupyter-kernel-client/blob/f18e982c3265df5e923aa9def101ab3fd737e139/jupyter_kernel_client/wsclient.py#L1253-L1271
[R29]: https://github.com/googlecolab/jupyter-kernel-client/blob/f18e982c3265df5e923aa9def101ab3fd737e139/jupyter_kernel_client/wsclient.py#L910-L982
[R30]: https://github.com/googlecolab/google-colab-cli/blob/02d2321c38dd215b7804eebf78b61d98b856bcd3/src/colab_cli/commands/execution.py#L266-L291
