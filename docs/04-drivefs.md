# DriveFS 机制、OAuth 与 Rust 移植规格

研究日期：2026-11-16。本文区分官方 CLI 的 Drive 流程与 colab-ext 的替代流程。

## 1. 范围和证据等级

- **事实**：固定提交源码直接实现的行为。
- **作者声明**：注释/文档描述的历史实验，没有本轮独立运行证据。
- **设计**：Rust 版建议采用的实现或修正。
- **待验证**：需要真实授权、VM、DriveFS 或服务端返回才能确认。
- 官方 CLI 基线：`googlecolab/google-colab-cli@02d2321c38dd215b7804eebf78b61d98b856bcd3`。
- 扩展基线：`deluxebear/colab-ext@980a39748badc3168a5af8734b2467351bdafcbc`。
- 本轮未进行真实授权、挂载、卸载、文件删除或 VM 创建。
- 本文不记录任何内嵌 client secret、用户 token 或实际账号身份。

## 2. 两条 Drive 挂载路径

| 路径 | 认证来源 | 远端实际执行者 | 交互边界 |
|---|---|---|---|
| 官方 drivemount | Colab credentials-propagation | google.colab.drive + DriveFS | 可能打开授权 URL 并等待用户确认 |
| 扩展 shim | 自行取得的 refresh token + 匹配 OAuth client | VM 内 metadata shim + DriveFS | 已取得 token 后可无浏览器挂载 |
| 扩展 rt | 属于 DriveFS client 的 refresh token | DriveFS 自行刷新 token | 已取得匹配 token 后可无浏览器挂载 |

官方流程执行 `google.colab.drive.mount(path)`，拦截 dfs_ephemeral 请求。[D01][D21]
扩展绕过此交互协调，直接执行 VM 自带 `/opt/google/drive/drive`。[D02]
因此扩展并没有实现 DriveFS 文件系统、同步引擎或 Google Drive 后端。
**设计**：Rust 保留官方路径与扩展路径为明确策略，不自动混用凭据。

## 3. 官方 credentials-propagation 流程

收到 iopub 的 colab_request，要求 content.request.authType=dfs_ephemeral。[D01]
提取 metadata.colab_msg_id，关联随后发送的回复。
请求路径为 `/tun/m/credentials-propagation/{endpoint}`。
GET query 包含 authuser=0、authtype=dfs_ephemeral、version=2。
初始 dryrun=true、propagate=true、record=false。
GET 响应去掉首行前缀后读 token，作为 `x-goog-colab-token`。
接着 multipart POST，同参数、同 token，file_id 为 empty.ipynb。
如果 success 不成立，展示 unauthorized_redirect_uri，并等待用户 Enter。
随后 dryrun=false，再次 POST 完成凭据传播。
成功后发 stdin/input_reply，其 value 是 colab_reply 对象并带 colab_msg_id。
parent_header 取原请求 header；不可用普通文本输入替代此回复。
**待验证**：当前账号策略、错误响应及浏览器登录态如何影响传播。
**设计**：人机交互等待需有明确阶段与可取消状态，不掩饰成自动完成。

## 4. 扩展的凭据前提

Drive 挂载凭据与 CLI 控制平面的 Colab OAuth 凭据独立。[D03]
扩展允许计算账号与 Drive 存储账号不同，并分别记录关联。
主要配置字段如下；持久化兼容时必须保留未知字段：[D03][D04]

| 字段 | 用途 | 约束 |
|---|---|---|
| refresh_token | 后续取得 access token | 绑定签发它的 OAuth client |
| client_id | token refresh 的客户端身份 | 必须与 refresh token 配套 |
| client_secret | 某些 client 的 token 参数 | 不得在日志、history、错误中明文输出 |
| token_uri | 刷新端点 | 默认 Google OAuth token endpoint |
| scopes | 请求/记录的权限集合 | 配置值不等于服务端实际授予值 |
| user_id | DriveFS auth/user-id | Google userinfo.sub，即 Gaia ID |
| email | DriveFS 用户邮箱 | 可由 userinfo 获取或显式提供 |
| flavor | 凭据来源标签 | sdk、dfs、custom 或 imported |

**事实**：缺少 refresh_token 或 Gaia ID 时挂载构造失败。[D03]
环境变量模式优先于 store，支持 CI 已有凭据注入。
`--cli-token` 是实验路径，强制 shim，并不自动补足缺失的 drive scope。
其 CLI 帮助声明 drive.file token 可能被拒绝，是作者的兼容性判断。[D03]

### 4.1 Scopes

扩展完整列表包含 email、drive、drive.activity.readonly、drive.photos.readonly。
还包括 client_channel、docs.test、experimentsandconfigs、peopleapi.readonly。[D05]
minimal 列表为 openid、userinfo.email、drive。
作者注释声称 drive scope 是主要文件访问所需权限，额外 scopes 用于配置类调用。
作者也声明 include_granted_scopes 可能重新带入 flexible-api 导致 invalid_scope。
代码默认关闭该选项，并设置 OAuth scope 放宽行为。[D05]
**待验证**：这些 scopes 对当前任意自建 client 是否可授权，以及是否需验证或白名单。
不能因 OAuth URL 构造成功就宣布授权可用。
**设计**：记录实际 granted scope；挂载前精确检查完整 scope URI。
原实现检查 `"drive" in granted` 是子串判断，不是严格 scope membership。[D06]

## 5. 首次授权与 headless

| flavor | 源码中的流程 | 适用边界 |
|---|---|---|
| sdk | CLI OAuth client + SDK remote redirect + token_usage=remote | 浏览器可在另一台机器，人工复制 code |
| dfs | 内置 DriveFS client + localhost callback | 浏览器需要能访问运行 CLI 机器的 loopback |
| custom | 自带 client、可配置 token URI/redirect | 取决于该 client 实际注册和允许的 scopes |
| imported | 导入既有 refresh token/client | 上游必须已完成合法授权 |

sdk 和 custom 的构造逻辑公开可见，不需要本地 Python 才能实现。[D07]
dfs 的内嵌 client 参数属于兼容数据，不应在技术报告中复述敏感值。
默认 flavor 在 CLI 环境是 dfs、独立脚本是 sdk。
`--url-only` 且没有显式 redirect 时自动选择 sdk。[D08]
`--url-only` 保存 pending 的 state、code_verifier、redirect、client 和账号信息。
后续 `--code` 从 pending 恢复 flow 完成交换。[D09]
普通 sdk 流程输出 URL、等待用户粘贴 code，再 fetch_token。
loopback 流程等待 localhost callback，默认有超时；--no-browser 只是不主动打开页面。
**结论**：首次授权仍需要用户同意；headless 不等于首次完全零交互。

### 5.1 导入与持续使用

`import` 接收 refresh token、client、scopes 和可选 token_uri。[D10]
代码实际总会刷新一次进行快速验证，docstring 的 optionally 不能当作行为。
userinfo 成功时存 sub/email；失败则要求用户提供 Gaia ID。
之后 `token` 命令用相同 client 换取 access token 并输出 JSON。[D11]
**设计**：Rust 的 consent、pending-resume、import、refresh 是独立可测试模块。
拒绝授权、expired grant、组织策略和 token 撤销应显示真实服务端原因。
不要将“可以导入 token”解释成可以绕过 Google 授权。

## 6. Shim metadata 协议

远端 shim 是 Python ThreadingHTTPServer，默认监听 127.0.0.1:8012。[D12]
若端口冲突，从配置端口起搜索最多 30 个端口。
它模仿 GCE metadata 的有限子集；不是完整 metadata 服务。

| GET 路径后缀 | 状态 / Content-Type | 响应 body |
|---|---|---|
| `/instance/service-accounts/default/token` | 200 / application/json | access_token、expires_in、token_type、scope |
| `/guest-attributes/auth/user-id` | 200 / text/plain | Gaia user ID |
| `/email` | 200 / text/plain | 用户 email 或空字符串 |
| `/guest-attributes/auth/scopes` | 200 / text/plain | 空格分隔 scopes |
| 其他 | 404 / 未指定 | 空 body，Content-Length: 0 |
| handler 异常 | 500 / 未指定 | exception 文本 |

成功响应设置 Metadata-Flavor: Google 和准确 Content-Length。[D12]
路径用字符串 endswith 匹配，query string 不被解析或去除。
没有实现通用 GCE metadata 的目录枚举、递归或 service-account 列表。
没有强制检查请求的 Metadata-Flavor header，也没有本地额外鉴权。
监听 loopback 限制网络暴露，但 VM 内其他进程仍能请求 token。
**设计**：如果保持兼容，最小实现这些确切路径，并明确同 VM 的信任边界。
如果扩展 parser 接受带 query 的请求，这是设计改进，不是原实现事实。

### 6.1 Token JSON 示例与刷新

```json
{
  "access_token": "<redacted>",
  "expires_in": 3599,
  "token_type": "Bearer",
  "scope": "openid https://www.googleapis.com/auth/drive"
}
```

mint() 以互斥锁保护缓存，并在到期前 120 秒重新刷新。[D13]
POST body 是 application/x-www-form-urlencoded：
grant_type=refresh_token、refresh_token、client_id、可选 client_secret。
请求 token_uri 的 timeout 为 20 秒，expires_in 缺失时按 3599 秒缓存。
metadata 返回 expires_in 最小 60 秒；可用期不应仅相信这个下限。
刷新失败时返回 500；没有指数退避、持久错误状态或刷新事件通知。
启动探针用 curl 请求 token，再访问 Google tokeninfo 检查有效性和 scopes。[D06]
**设计**：token 和 OAuth 错误只进入脱敏日志，不在异常 body 回显请求敏感信息。

## 7. DriveFS 启动与 rt 模式

shim 模式以 `--metadata_server_auth_uri=127.0.0.1:PORT/computeMetadata/v1` 启动。[D14]
rt 模式直接传 --refresh_token、--token_uri、--user_id、--user_email。
rt 不传可任意选择的 client_id/client_secret；它依赖 DriveFS 自带 client。
所以 sdk/custom client 签发的 refresh token 不能据此推断可用于 rt。
**设计**：凭据来源不匹配时提前报错或显式要求 shim，不静默尝试 rt。

features 包含 FUSE 并行度、读写 QPS、opendir 超时和 readonly 等参数。[D14]
preferences 包含 roots.pem、feature_flag_restart_seconds 和 mountpoint。
环境变量设 FUSE_DEV_NAME=/dev/fuse、HOME 和固定 PATH。
远端还依赖 /opt/google/drive/roots.pem、Linux FUSE、mount 工具及进程权限。
这些组件由 VM 镜像提供，Rust 客户端无法单独保证它们存在。
作者声明 flags 从特定 google.colab.drive._mount 版本复制，不等于永久接口。
**待验证**：每种 VM 镜像、DriveFS 版本和 readonly 模式的实际行为。

## 8. 远端文件与进程生命周期

| 对象 | 路径 / 身份 | 原实现生命周期 |
|---|---|---|
| shim 配置 | `/tmp/dfs_creds.json` | 包含长期凭据，0600；未见卸载时删除 |
| shim 脚本 | `/tmp/dfs_creds_shim.py` | 写入 VM，detached Python 子进程运行 |
| shim 日志 | `/tmp/dfs_creds_shim.log` | 请求路径和异常；启动时重开 |
| Drive stdout/stderr | `/tmp/dfs_drive.log` | Drive 子进程重定向输出 |
| DriveFS 状态 | `/root/.config/Google/DriveFS` | 可选 purge_state 删除 |
| DriveFS 日志 | `/root/.config/Google/DriveFS/Logs/drive_fs.txt` | 错误诊断 |
| 挂载点 | 默认 `/content/drive` | My Drive 子目录存在被当作成功 |

shim 和 drive 都以 start_new_session=True 脱离当前 kernel 执行调用。[D12][D14]
因此本地 CLI 退出后，VM 内两类进程继续存在，以便文件系统持续工作。
这不需要本地后台 daemon，但确实需要远端常驻进程。
当前挂载流程在已经看到 My Drive 时立即返回成功，除非 --force。[D02]
未证明它核对挂载身份、实际账号或刷新能力。
**设计**：以 mount table + PID/启动标识 + 账号摘要共同判定现有挂载可复用。

## 9. 挂载、卸载与清理缺口

mount teardown 先杀 drive/shim 进程、尝试 umount，再检查 /proc/mounts。[D15]
仍然 mounted 时退出，避免 rm -rf 进入真实 Drive 文件系统。
但进程匹配是全 VM 范围；并非按挂载点或操作 ID 精确清理。
固定 shim 配置文件也使多挂载实例存在互相覆盖的风险。

**高风险源码缺口**：UMOUNT_REMOTE 依次 fusermount/fusermount3/umount 后，[D16]
无论是否成功，都会杀全局 drive/shim 并执行 `rm -rf MP`。
这里没有 mount teardown 同样的“仍挂载则禁止删除”保护。
MP 在这些 shell command 中未经 shell quote；带 shell 元字符的路径可改变命令含义。
CLI mount 只拒绝空格，并不构成完整 shell 安全边界。[D17]
status 的 mounted 使用 os.path.ismount，mount 成功却使用 My Drive 是否为目录。
两个定义不一致，可能产生难以判断的部分成功状态。[D02][D16]

**Rust 必须修正的设计**：

1. 路径作为 subprocess argv，避免经 shell 字符串解释。
2. 每次卸载后读 mount table，失败时绝不递归删除挂载点。
3. 使用 per-operation 私有目录和记录 PID/启动时间，精确终止所属进程。
4. 卸载完成后移除 shim 长期凭据文件，并明确日志保留策略。
5. DriveFS flush/unmount 的成功条件必须线上验证，不能用 kill 替代安全卸载。
6. 输出清理失败及残留位置，让用户可重试，不伪报成功。

## 10. 重试与结果恢复

remote.exec_code 每次新建 ColabRuntime，最多重试三次，退避2、4秒。[D18]
先额外等 WS ready 最长45秒，补偿依赖默认10秒等待不足。
部分 RuntimeError、较早 TimeoutError 和 requests 异常被认为可重试。
401/403/404 不重试；finally 关闭客户端，避免非 daemon 线程挂住退出。

**缺口**：连接断开并不能证明远端代码未开始，直接重试可能重复杀进程/启动挂载。
payload 的 base64 是编码不是加密；整个配置包含 refresh token。[D19]
如果执行历史记录原始代码，必须专项检查凭据是否被持久化到历史。
fallback CLI 不检查 subprocess returncode，只合并 stdout/stderr 文本。[D18]
parse_result 取首个 DFS_RESULT 行；没有 operation ID 校验或签名。[D19]

**设计**：引入 operation_id、phase 和幂等状态文件。
发送前失败可自动重试；发送后断开先只读查询挂载/进程/phase。
只有确认未开始或已回滚，才重新执行 mutation。
结果协议包含 operation_id、ok、stage、mounted、pid、可恢复错误码。
重连不能依赖“再跑一次看看”；长期任务应能重新获取最终结果。

## 11. Rust 化交付选择

| 方案 | 本地 Python | 远端 Python | 代价 |
|---|---|---|---|
| Rust CLI + 现有 payload | 不需要 | 需要 | 最接近现有行为，快速建立兼容 |
| Rust CLI + Rust metadata helper | 不需要 | 可减少 | 需交付 Linux binary、架构和 TLS 依赖 |
| Rust 重写全部 DriveFS | 不需要 | 不需要 | 超出现有源码覆盖；无法据本项目保证等价 |

建议先实现 Rust CLI 与可审阅、脱敏的远端 payload。
如用户明确要求远端也零 Python，再交付独立 helper，而不是把本地 Python 隐藏打包。
两种方案都保留 Google DriveFS、FUSE 及服务端 OAuth 策略的依赖。

## 12. 验收与未知

- 单测：凭据配置优先级、Gaia ID、scopes 精确匹配、账号分离。
- OAuth mock：sdk/custom/dfs、pending 恢复、code_verifier、撤销和拒绝。
- 本地 shim 契约：四个端点、404/500、Content-Length、并发刷新和缓存。
- 故障注入：端口占用、token refresh 失败、Drive 提前退出、mount timeout。
- 安全回归：卸载失败不 rm、特殊字符路径、per-operation 隔离、日志脱敏。
- 恢复测试：发送后断线不重复 mutation，重连只读核对后恢复。
- 现有测试主要 mock 账号和执行文本，能证明分发，不能证明真实挂载。[D20]
- 真实集成必须验证首次授权、再次挂载、跨 token 到期、文件读写和安全卸载。
- scopes、私有 DriveFS flags、组织策略、token 长期可用性仍待线上验证。

## 13. 固定源码证据索引

[D01]: https://github.com/googlecolab/google-colab-cli/blob/02d2321c38dd215b7804eebf78b61d98b856bcd3/src/colab_cli/commands/automation.py#L69-L148
[D02]: https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/drive_mount.py#L53-L94
[D03]: https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/drive_mount.py#L307-L402
[D04]: https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/dfs_creds.py#L404-L432
[D05]: https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/dfs_creds.py#L109-L135
[D06]: https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/drive_mount.py#L216-L242
[D07]: https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/dfs_creds.py#L229-L264
[D08]: https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/dfs_creds.py#L288-L312
[D09]: https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/dfs_creds.py#L320-L401
[D10]: https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/dfs_creds.py#L441-L474
[D11]: https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/dfs_creds.py#L543-L579
[D12]: https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/drive_mount.py#L152-L215
[D13]: https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/drive_mount.py#L127-L150
[D14]: https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/drive_mount.py#L243-L285
[D15]: https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/drive_mount.py#L96-L124
[D16]: https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/remote.py#L148-L193
[D17]: https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/drive_mount.py#L405-L441
[D18]: https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/remote.py#L52-L144
[D19]: https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/drive_mount.py#L514-L527
[D20]: https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/tests/test_colab_ext.py#L538-L647
[D21]: https://github.com/googlecolab/google-colab-cli/blob/02d2321c38dd215b7804eebf78b61d98b856bcd3/src/colab_cli/commands/automation.py#L219-L237
