# DriveFS 独立凭据与授权实现

更新日期：2026-11-17。本章记录扩展 DriveFS 的本地凭据阶段。本阶段当时尚未实现 `drive mount/umount/status`、远端 metadata shim、DriveFS 启动和清理；后续核心编排与租约已接入，见 [24](24-drivefs-lifecycle-implementation.md)；官方 `drivemount` 继续走 [16](16-colab-automation-implementation.md) 的 Colab 凭据传播。完整目标及固定源码证据见 [04](04-drivefs.md)，本章不代表 P5 或整套设计完成。

## 已交付入口

```sh
colab -a storage drive consent --flavor custom --url-only \
  --client-oauth-config /absolute/path/client.json
colab -a storage drive consent --code AUTHORIZATION_CODE
colab -a storage drive show
colab -a compute drive show --drive-account storage
colab -a storage drive token --whoami --json
colab -a storage drive clear
```

`drive` 有 `dfs` 别名。全局账户参数可以放在子命令之前或之后；`--client-oauth-config` 同样是全局选项。不会加载 Python 或修改进程环境来切换身份，也不会查询 Colab 控制面或申请 VM。Drive 凭据使用 `accounts/<profile>/dfs_credentials.json`，与 CLI OAuth token、ADC 和官方 VM 授权相互独立。

show/clear 的合并版输出后续已对齐，见 [57](57-merged-drive-credentials-implementation.md)：有效凭据的脱敏 JSON 位于顶层，移除旧 credentials 包装；空凭据采用原版单行文本。安全字段白名单和本地恢复责任继续保留。

同章后续增加实际 token/--whoami 的七组 loopback 对照，验证请求身份、轮换保存、失败后恢复、UTC/亚洲时区和短有效期。已观察原版的轮换丢失与期限误差，Rust 保留原有正确处理；不是 Google 线上授权证据。

`consent/import/clear` 默认针对所选计算账户本身，保留现有 consent 路由约定；显式环境 Drive 账户仍有优先级。`show/token` 按显式 --drive-account、Drive 环境绑定、逐计算账户绑定、本账户已有 Drive 凭据、全局默认绑定顺序解析，具体优先级由 [context.rs](../src/context.rs) 实现。后续已补齐 consent 开始阶段的自动登记与所选 Drive 账户邮箱提示，见 [58](58-drive-consent-profile-implementation.md)；完成阶段及其他操作仍只解析已有账户。也可先用 account add --configure-only 建立账户，或按 [34](34-account-login-and-drive-consent-implementation.md) 完成联合授权。

## 三种授权来源

| flavor | 输入与 redirect | 当前边界 |
| --- | --- | --- |
| sdk | 提供 SDK OAuth client 配置，默认 SDK remote redirect，附 token_usage=remote | URL 两步流程适合在另一台机器打开浏览器；配置不内嵌于程序 |
| custom | --client-oauth-config，或 --client-id/--client-secret/--token-uri；可显式 redirect | 必须使用该 client 实际允许的 scopes 和 redirect |
| dfs | 外部提供匹配 DriveFS client 的配置，默认 localhost callback | 只保留公开 client ID 作匹配检查；程序不复制上游内嵌 client secret |

未指定 flavor 时，有 --client-id 使用 custom；--url-only 使用 sdk；其他情况使用 dfs，与扩展默认选择方向一致。自建 client 配置配合 --flavor custom；不能因为文件内存在 OAuth client 就把它认作 DriveFS 自带 client。未提供 client-id 时，配置路径优先 --client-oauth-config，其次 ~/.colab-cli-oauth-config.json。没有本地 Python 探测或读取已安装 Python 包的 fallback。

consent 支持 scopes、minimal、port、redirect-uri、login-hint、email、user-id、include-granted-scopes、no-browser 和 consent-timeout。minimal 默认 true，`--minimal false` 选择完整 scope 列表；也支持裸 `--minimal`。显式 --scopes 按空白拆分后优先。client-id/client-secret/token-uri/scopes/user-id/email 可从对应 COLAB_DRIVE_* 环境变量读取。include_granted_scopes 默认关闭，仅显式打开；某些 client 的当前 Google 授权策略仍需真实验证。

--url-only 的 stdout 只有授权 URL，操作提示写 stderr。状态、随机 PKCE verifier、client 配置、redirect、scopes、账户和时间戳写入私有 dfs_pending.json。使用同一账户 --code 或 --callback-url 完成；参数互斥，不能同时提供。callback URL 严格匹配 scheme/host/port/path 和唯一 state/code，拒绝重复参数、错误 state、非匹配路径及授权拒绝。直接 --code 是明确的人工复制流程，没有伪造一个收到并验证过浏览器 state 的回调。

交互 loopback 在打印 URL 前实际绑定本机端口，端口占用时选择空闲端口，并把实际端口写入 redirect/pending。默认仅绑定 IPv4 loopback；显式 ::1 使用 IPv6。callback 请求头限 8 KiB，每连接读取最多约 2 秒，并受整体 consent-timeout 限制，输出响应不含授权码或 token。错误 state、无关路径及畸形请求不消耗真实授权；有效授权拒绝、交换失败立即返回。尚未做原生 Windows 浏览器/IPv6 双栈全链路验证。

非 loopback 交互授权使用终端粘贴代码；复用可轮询终端输入，关闭回显，Ctrl-C/TERM/HUP 或期限结束恢复终端并保留 pending。非 TTY 的这一路径要求先 --url-only，再显式完成。--no-browser 不调用系统浏览器；否则按平台尝试打开 URL，不能据此证明浏览器已完成授权。Windows 原生不回显粘贴尚未实现；可使用 URL 两步授权，整体 Windows 验收仍未完成。

## 导入、刷新和身份

```sh
# 变量由调用者的凭据系统提供；不要把真实值写进版本库。
colab -a storage drive import --client-id REGISTERED_CLIENT_ID \
  --user-id GAIA_ID --scopes 'openid https://www.googleapis.com/auth/drive'
```

import 从 --refresh-token/--client-id/--client-secret/--token-uri/--scopes 等参数或对应 COLAB_DRIVE_* 变量取值。refresh token 必须与 OAuth client 配套；导入总会真实刷新一次，刷新失败不覆盖既有凭据。默认 flavor=imported，也可显式 sdk/dfs/custom；标签本身不构成来源或授权成功证明。

没有显式 user-id 时，用本次 Drive access token 请求 Google userinfo.sub/email；不使用计算账户 token。显式提供 user-id 可在 userinfo 不可用时导入；客户端不把这种输入当作已经向 Google 核验过身份。结果保存在 Drive 凭据文件，不修改注册表的 compute email/user_id/dfs_email 元数据，这是目前与扩展的另一项差异。

`drive token` 每次用存储的 client 和 refresh token 刷新，返回 JSON 的 access_token、expires_in、token_type、scope、user_id/email；`--json` 是兼容参数，输出始终为 JSON。这个命令明确要求打印 access token；其他凭据命令均不打印 token。--whoami 用本次 Drive token 重新查询身份，并持久化结果。响应未提供 expires_in 时按上游风格显示 3599 秒估计值，不能据此宣称获得了服务端有效期证明；不会把实际不足 60 秒的期限人为抬高。

刷新不增加 scope 请求。服务端响应包含 scope 时，以响应为准并记录 scope_source=token_response；明确返回空 scope 时也保留空权限集，不能回退到请求的完整权限。未返回时分别标记 requested_unverified/configured_unverified，保留请求或已有 scope 元数据。旧的已确认 scope 在刷新没有新 scope 时保留。未知 scope 来源不展示原始任意字段。挂载预检只接受完整 `https://www.googleapis.com/auth/drive` 成员，drive.file/drive.readonly 或包含 drive 的任意字符串都不能替代；scope 元数据仍不等于已验证真实挂载权限。rt 模式预检还要求 dfs flavor 与公开 DriveFS client ID 匹配，其他 client 使用 shim；远端模式尚未交付。

## 落盘、恢复与清理责任

所有凭据文件均由 [Storage](../src/storage.rs) 原子写入，目录 0700、文件 0600；遵循扩展的文件后端，不额外写系统 keyring。schema 保留未知凭据字段，但 show 只输出已知元数据与 `<set>` 标记，既不遍历任意扩展字段，也不显示 access/refresh token 或 client secret。token_uri 必须为 HTTPS（loopback 测试可 HTTP），禁止 userinfo/password、fragment 和预设 query，避免通过配置 URI 把敏感参数带到显示输出。HTTP 不跟随重定向；token/userinfo 响应读取上限 64 KiB，错误只含类型和状态码，不回显原始 body。

操作先持有对应账户生命周期锁，再核对注册表和账户事务；账户已删除或迁移未完成时拒绝读写。网络过程保持该账户锁以串行化刷新，同一账户的并发操作可能受既有 5 秒锁等待期限影响；不能声称所有本地操作在网络阻塞时仍立即完成。不同账户互不替换身份。

授权码 POST 前持久化 exchange_started；收到结果后先保存 result，再做 userinfo 和最终保存。POST 失败或崩溃导致交换结果未知时，禁止再次提交同一 code，需开始新的授权。若 result 已保存而 userinfo 失败，后续完成操作复用 result，不重新兑换代码。pending 默认 15 分钟有效，未来时间偏移和未知 schema 拒绝。

刷新可能轮换 refresh token；成功响应先写 dfs_refresh_pending.json，再更新主文件，最后移除暂存。后续 load/token/import 会恢复已暂存的同账户结果；恢复文件账户或 schema 不匹配时拒绝覆盖。网络成功与首次暂存写入之间仍存在进程/磁盘故障窗口，无法保证远端 token 轮换可逆；不把落盘恢复描述为远端事务。

`show` 在发现合法刷新暂存时会完成本地提交，然后显示脱敏结果；因此它不承诺完全零写入。import 的刷新已成功但 userinfo 失败时保留暂存，避免丢失新 refresh token；可补齐身份后重新授权或导入，缺失 Gaia ID 的凭据不能通过挂载预检。重命名账户会移动凭据，改写 dfs_pending.json 和 dfs_refresh_pending.json 的 account，不能把旧账户的恢复文件留在新账户下。

`clear` 显式清除选中账户的授权 pending、刷新暂存和主凭据，顺序确保已删除凭据不会被旧暂存复活；不清计算 OAuth、不停止 VM、不访问或删除 Drive 文件，也不撤销 Google grant。需要撤销时由用户在 Google 账户权限管理中操作。清除中断后可能保留旧凭据，可以再次 clear；不能手删账户事务日志绕过迁移保护。

## 验证与剩余工作

[Drive 凭据测试](../tests/drive_credentials.rs) 新增 16 项：导入与 userinfo、计算/存储账户绑定隔离、精确 scope 与 rt client 检查、未知字段脱敏与保留、PKCE/state/duplicate callback、交换未知禁止重放、userinfo 失败后复用交换结果、刷新轮换及清除恢复、账户重命名和删除、并发刷新、schema/过期/迁移保护、CLI import/show/token/clear/dfs、两步 URL/代码跨重命名恢复、localhost 错误 state 后成功回调、拒绝/超时/信号，以及真实 PTY 的授权码不回显与终端恢复。

[CLI 差分工具](../tools/compare_cli.py) 为新 Drive 入口和五个子命令追加 help 与 Rust 扩展场景，覆盖空/已有凭据显示、别名、清除、显式 token/whoami、拒绝路径和 URL pending。固定官方 CLI 不包含扩展 drive 组，这些场景单列 rust_extension，不能声称已完成 colab-ext 全命令的双端等价测试。原有官方 drivemount 的合成协议对比继续保留。

测试仅用临时账户、合成 OAuth 材料、loopback HTTP 和本机 PTY，没有接触开发者真实 Drive 凭据或 Google 授权。首次真实 consent、组织策略、client/scopes 可用性、长期刷新、真实挂载/读写/安全卸载、远端 shim、私有 DriveFS flags、各平台完整验收仍需后续工作。

本阶段完整默认 225 项、无默认特性 223 项测试通过，两种特性的 Clippy、fmt、默认构建和研究文档检查通过；193 个官方差分与 Rust 扩展场景无断言失败。新增 Drive 场景只作为扩展覆盖，不关闭远端 DriveFS 或真实授权的验收缺口。后续远端 metadata 组件的独立实现见 [21](21-drivefs-metadata-implementation.md)，尚未部署或接入挂载。
