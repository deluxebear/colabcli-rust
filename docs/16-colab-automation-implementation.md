# Colab 专用请求、VM 授权与官方 Drive 挂载

更新日期：2026-11-16。本章记录已实现的 `dfs_ephemeral` 专用 WebSocket 流程、`auth` 和官方 `drivemount`。固定协议依据见 [运行时协议](03-runtime-and-wire-protocols.md) 和 [Drive 研究](04-drivefs.md)。这不代表扩展 DriveFS、跨账户挂载或真实 Google 授权已验收。

后续策略变更：根 drivemount 默认使用独立 DriveFS；本章的官方路径需显式 --upstream。drivemount 默认期限改为 30 秒，auth 仍为 600 秒。

## 入口与账户

```sh
colab auth -s cpu-demo
colab drivemount -s cpu-demo /content/drive --upstream
colab -a work auth -s cpu-demo --timeout 600 --json
```

这些命令只访问已有 VM，沿用显式账户、环境和会话归属解析，不创建 VM。`auth` 在远端设置 `USE_AUTH_EPHEM=0` 后调用 `google.colab.auth.authenticate_user()`；`drivemount` 调用 `google.colab.drive.mount()`。挂载点要求不含 NUL 的绝对路径，以 JSON 数据传入远端，不直接拼接进 Python 字符串。账户登录仍使用 `account login`；VM 授权是另一层流程。

auth 默认使用 600 秒、官方 drivemount 默认使用 30 秒总期限，包含人工输入与 HTTP 传播等待，非正数表示不限时。已授权且无需输入的流程允许无终端运行；需要人工输入时，Unix 终端使用 [12](12-jupyter-and-execution-implementation.md) 的可轮询输入，无终端则明确返回 Unknown，不自动读管道、打开浏览器或假定已经同意。Windows 交互终端尚未实现。

官方 `drivemount` 使用所选算力账户的 Colab 授权，不采用独立 Drive 账户绑定。后续已实现默认 DriveFS、跨账户与 shim/rt，见 [28](28-drivemount-strategy-implementation.md)；本章仅描述显式 --upstream 路径。

## 专用消息与凭据传播

运行时只接受本次 execute parent ID 对应的 IOPub `colab_request`，校验 `content.request.authType` 和 `metadata.colab_msg_id`（非空字符串或无符号整数，包含 0）。`InputHandler` 新增可轮询 `poll_colab`；普通 `exec --allow-stdin` 的输入处理器不会因此自动获得凭据传播能力。未知类型、畸形/重叠请求和传播失败均不给内核发送成功回复。

`AutomationInput` 在执行锁内绑定实际 VM endpoint，并使用同一 SessionService 的账户控制面客户端：

1. GET `/tun/m/credentials-propagation/<encoded-endpoint>`，参数为 `authuser=0`、`authtype=dfs_ephemeral`、`version=2`、`dryrun=true`、`propagate=true`、`record=false`，去除 XSSI 后读取非空 XSRF token。
2. 同参数发送 multipart POST，`file_id` 是文本字段 `empty.ipynb`，通过 `X-Goog-Colab-Token` 传 XSRF。
3. dry-run 未成功时，验证授权 URL 为无用户名/密码的 HTTPS 地址，通过 stderr 显示并等待用户确认；URL 不写历史。
4. 使用 `dryrun=false` 再 POST，只有响应严格包含 `success=true` 才确认完成。相较原版只判断最终 HTTP 200，这是有意的成功判定加严。
5. 发送 stdin/input_reply，value 为 `{"type":"colab_reply","colab_msg_id":原ID}`，parent_header 复制原专用请求 header，保留当前 session。

OAuth 只发控制面，runtime proxy token 只发 Jupyter；控制面请求不跟随重定向，继承 4 MiB 响应限制和请求超时。HTTP 在独立工作线程执行，主 WebSocket 循环继续响应 PING/PONG、检查执行期限和取消信号。没有为可能产生副作用的最终传播 POST 添加自动重试。

中断或超时会关闭本次等待并留下执行恢复线索，禁止自动重放代码；它不能取消已经发出的 Google HTTP 请求，工作线程可能在本地等待结束后完成。客户端未取得确认时不会发送成功回复，也不会声称已撤销传播或已停止远端代码。

## 历史与生命周期

保存 `automation:<op>` 的来源标签，以及 automation、automation_result、execution、colab_request、drive_auth_needed、drive_auth_success 事件。授权需求事件只记录原因，成功事件只在服务端确认后记录，不保存授权 URL、XSRF、OAuth 或输入回复。用户代码主动输出的内容仍按普通输出处理。历史失败会保留错误或停止本次推进，不隐式重跑授权。

auth/drivemount 以及 [install](17-install-implementation.md) 的每次新操作创建独立 kernel，不修改普通 exec 的 kernel_id/session_id；四个官方差分场景（两个命令各含无/有既有 kernel）已匹配。正常结束只断开 WebSocket，保留远端 automation kernel，与原版一致；stop 释放 VM 时会回收其全部 kernel。

独立 kernel 的创建意图、ID、session ID、操作类型与代码/设置的 SHA-256（install 还包含上传文件内容） 在 sessions 的 automation_pending 中持久化，不存 token 或授权 URL。它有 creating、ready、executing、unknown、finished、deleting 六个阶段：

- 创建请求前写 creating，结果未知时拒绝再次 POST。取得 ID 后写 ready；只有尚未提交执行、操作及指纹完全相同的 ready 才可重试连接。
- 每次 execute_request 前写 executing 和消息 ID。断线或超时写 unknown，保留部分结果并禁止重放；普通 exec 和 restart-kernel 也不会绕过此保护。
- 用户代码已确认完成后先写 finished，再退休记录并保存 automation_last。若此窗口崩溃，下一次调用不会把 finished 误当未执行而重跑。
- 清理请求前写 deleting 并变更 generation。并发旧执行即使收到迟到成功回复，也不能恢复已清理的记录。

status 文本显示 Automation 阶段，JSON 的 automation_pending 只展示操作、阶段、kernel ID 和消息 ID，隐藏指纹及其他日志字段。没有待恢复操作时不增加该 JSON 字段。

## 显式自动化清理

```sh
colab status -s cpu-demo --json
colab automation-cleanup -s cpu-demo
# 只有创建结果未知、尚未记录 ID 时，先人工核对该 VM 上的 kernels：
colab kernels -s cpu-demo --json
colab automation-cleanup -s cpu-demo --kernel-id VERIFIED_AUTOMATION_KERNEL
```

automation-cleanup 只删除当前待恢复记录对应的独立 kernel，不执行用户代码、不申请 VM、不重启普通 kernel。若 ID 已知，拒绝替换成其他目标；任何情况下都拒绝以普通 exec 已记录的 kernel 作为目标。创建结果未知时，必须由用户核对并显式提供 ID，在当前 VM 上 GET 验证后记录清理意图；不自动猜测所属 kernel。

DELETE 失败保留 deleting，重试继续同一 ID；404 视为已不存在。成功后清除本次 running 与 pending，保存清理历史，保留普通 kernel/session 和其他状态；无 pending 时幂等返回不操作。无法识别未知创建目标时，可以显式 stop 整个本次 VM，但不能手删日志绕过保护。该命令不批量清理已正常完成的 automation kernel。

清理在账户锁下写历史，避免重复申请同一生命周期锁；执行侧的 checkpoint 仍核对 endpoint/generation，保留并发刷新数据。损坏或非法的恢复记录不会触发创建、删除或覆盖。

## 验证与缺口

新增三项控制面测试和三项 Jupyter/CLI 测试，现有伪终端 CLI 测试扩展到 auth/drivemount。后续再增加六项生命周期测试，覆盖独立 kernel、保留普通状态、握手恢复、创建/删除结果未知、确认完成后崩溃、并发清理防旧写回和损坏日志。覆盖真实 multipart 字节、参数/XSRF、endpoint 编码、错误与重定向脱敏、人工确认门控、数字/字符串 ID、传播期间心跳、失败不回复/不重放、路径编码、完整 CLI JSON、显式双账户隔离及无终端已授权流程。

回归使用临时账户、合成 OAuth 和 loopback HTTP/WS；不使用真实 Google 凭据，不打开授权 URL，也不实际挂载用户 Drive。仍需真实 VM OAuth、Google 传播策略、Drive 挂载/只读/卸载及持续刷新验收；扩展 DriveFS、REPL、SSH 和其他完整设计门槛继续保留。supervisor 已移出本项目当前范围，见 [08](08-supervision-and-recovery.md)。
