# 账户、认证与兼容存储规格

研究日期：2026-11-16。本文是 Rust 合并实现的输入规格，不代表已完成实现。
研究方式：固定提交的静态源码、配置和测试审阅；没有调用真实账户、兑换 token、启动 OAuth 浏览器或分配 Colab VM。

实现进度：多账户、OAuth、authorized_user ADC 与存储基础模块已提供 Rust 代码及离线测试，见 [09](09-foundations-implementation.md)。本篇保留完整研究基线，不能推导其他 ADC 类型或线上兼容已经完成。

## 1. 证据范围与标记

- **事实**：直接由下列固定提交代码建立；证据标签 `Axx` 可在文末定位。
- **设计**：建议的 Rust 行为；遇到原实现缺陷时，明确标注兼容差异。
- **待验证**：依赖库默认行为、Google 服务接受政策或真实账户能力，静态研究不能确定。
- 上游 U：`googlecolab/google-colab-cli@02d2321c38dd215b7804eebf78b61d98b856bcd3`。
- 扩展 E：`deluxebear/colab-ext@980a39748badc3168a5af8734b2467351bdafcbc`。
- 本文不复制真实 OAuth client secret、access token、refresh token 或授权码。

## 2. 凭据类型与身份归属

**事实**：CLI 访问 Colab 控制面使用 OAuth2 或 ADC；文件与执行连接使用分配响应中的 runtime proxy 凭据；DriveFS 另有 OAuth client、scope、refresh token 和 Google user id。[A01][A02][A17]
因此 `colab auth` 的 VM 内 GCP 授权，不能替代本地 CLI 登录；Drive 授权成功也不能推导算力账户可分配 VM。

**设计**：使用三个类型，禁止传递裸 token 字符串来隐式选择用途：

| 类型 | 用途 | 所属账户 |
|---|---|---|
| `ControlPlaneCredential` | Colab assignments、CCU、分配与释放 | compute profile |
| `RuntimeProxyCredential` | Jupyter HTTP/WebSocket 与 SSH 隧道 | 已有 assignment |
| `DriveCredential` | DriveFS 或远程挂载 | drive profile |

每个对象记录来源、scope、到期时间和非秘密身份；日志默认只显示 profile/email，不能输出凭据正文。

## 3. 账户解析是纯函数

### 3.1 profile 标识与查找

**事实**：名称必须匹配 `^[A-Za-z0-9][A-Za-z0-9._@-]{0,63}$`，并额外禁止包含 `..`。
最大 64 字符，不允许路径分隔符；目录名等于 profile key。[A03]
查找先按 key 精确匹配，再按去空格、小写后的 email 查找。
重复 email 必须显式提供 profile key；名称本身没有统一转小写。[A04]
`allow_create` 只用于明确允许创建的路径，例如 account add/login、Drive consent；普通命令不应隐式创建 typo profile。

Rust 后续已按 [58](58-drive-consent-profile-implementation.md) 接入 Drive consent 的授权起始自动登记：同时校验计算与 Drive 环境选择，原子提交新资料。带 code/callback 的完成阶段仍要求已有账户，不因错误名称留下空资料；新登记也不等于已授权。

**设计**：解析返回 `ResolvedAccount { id, source }`，source 是结构化枚举，显示时再转为 `flag/env/session/active`。
不得把 email 当稳定主键：授权后 email 会被更新，同一 email 也允许对应多个 profile。

### 3.2 算力账户优先级

**事实**：扩展按下列顺序解析。[A05]

1. 显式 `-a/--account`。
2. 非空 `COLAB_ACCOUNT`。
3. 当命令允许路由且指定了 session 时，查询本地 session 所属账户。
4. 注册表 `active`。

第 3 项有必须复现的细节：

- active 已有该 session 名：直接选择 active，不检查其他 profile 是否重名。
- active 没有，恰好一个其他账户有：路由到该账户，source=`session:<name>`。
- active 没有，多个其他账户有：报歧义错误，不猜测。
- 所有账户都没有：仍返回 active，之后由 session 解析报不存在。
- 显式 flag/env 已选择账户时，不再跨账户搜索 session。

**事实**：允许路由的顶层命令包括 status、exec、repl、ls、rm、upload、download、edit、restart-kernel、stop、url、log、auth、drivemount、install、ssh；Drive 组只允许 mount/umount/status。
new/run 不参与已有 session 路由；解析遇到 `--` 停止。[A06]
路由提示写 stderr；非 TTY 的 ssh 不输出该提示，以免污染 ProxyCommand 字节流。[A07]

**设计**：从已解析的命令 AST 读取 session/account，不复制 Python 对 raw argv 的二次扫描。
保留全局 account 与部分子命令 account 的现有合法位置；对于脚本参数中的 `-a/-s` 必须保持透明。

### 3.3 Drive 账户优先级

**事实**：compute profile 与 Drive profile 可以不同；挂载时依次选择：[A08]

1. 显式 `--drive-account`。
2. `COLAB_DRIVE_ACCOUNT`，或旧别名 `COLAB_DFS_ACCOUNT`。
3. compute profile 的 `drive_account` 绑定（目标必须仍存在）。
4. compute profile 自己存在 `dfs_credentials.json`。
5. 注册表 `default_drive_account`（目标必须仍存在）。
6. compute profile 自己。

这里“存在自身 DFS 文件”先于“全局默认”，判断仅基于路径存在，不验证内容。
新变量由 Python `or` 优先；若新变量只有空格，会阻断旧别名但随后变为空值，这是边缘兼容行为。

**事实**：consent 选择账户时，完成 flag/env 两层后立即返回当前 profile，不采用绑定或全局默认。
这使创建/更新授权材料默认落在用户正在操作的 profile，而不是被挂载策略重定向。[A08]

**设计**：提供 `resolve_drive_for_mount` 和 `resolve_drive_for_consent` 两个入口，禁止共用一个未标明用途的默认参数。
跨账户挂载时同时展示 compute/drive 两个 profile；不能把 Drive email 回写成 compute email。

## 4. 持久化 schema 与默认值

### 4.1 目录布局

**事实**：根目录优先 `COLAB_CLI_CONFIG_DIR`，否则 `~/.config/colab-cli`，然后 expanduser+绝对化。[A03]

```text
<root>/accounts.json
<root>/accounts.json.lock
<root>/accounts/<profile>/sessions.json
<root>/accounts/<profile>/history/*.jsonl
<root>/accounts/<profile>/dfs_credentials.json
<root>/accounts/<profile>/dfs_pending.json
<root>/accounts/<profile>/oauth_token.json
<root>/accounts/<profile>/oauth_pending.json
<root>/accounts/<profile>/oauth_config.json
```

**事实**：扩展没有把上游 settings 全量迁移为每账户设置；上游默认 settings 仍是 `~/.config/colab-cli/settings.json`。[A09]
**设计**：账户凭据/会话按 profile 隔离；更新策略等机器级设置仍放全局。不能声称“所有文件都按账户隔离”。

### 4.2 accounts.json

**事实**：新建注册表的结构如下，`<local-time>` 是本地时间格式 `%Y-%m-%dT%H:%M:%S%z`。[A10]

```json
{
  "version": 1,
  "active": "default",
  "default_drive_account": null,
  "accounts": {
    "default": {
      "email": null,
      "user_id": null,
      "dfs_email": null,
      "auth_provider": "oauth2",
      "adc_credentials_file": null,
      "drive_account": null,
      "migrated_legacy": true,
      "created_at": "<local-time>",
      "updated_at": "<local-time>"
    }
  }
}
```

首次 bootstrap 的 default 为 `migrated_legacy=true`，普通新账户为 false。
`ensure_account` 只应用非 null 参数；`update_account_meta` 允许显式写 null。[A10]
因此用 `account add` 重配已有账户并不总能清空旧的 ADC 路径或 Drive 绑定。
读取时仅验证顶层对象与 accounts 对象；缺失 version 补 1；空 accounts 补 default；无效 active 改为第一个账户；缺失全局 Drive 默认补 null。
现有字段/账户元数据不会全面补默认，也未拒绝未来 version；保存时强写 version=1。[A10]

**设计**：Rust 读模型支持未知字段透传，使用有序 map 保留 fallback 顺序。
拒绝写入未支持的未来 schema version，避免静默降级；损坏注册表应报告并保留原文件。
将“字段未指定”与“显式清空”表示为不同操作，不能用 `Option<T>` 混淆。

### 4.3 sessions.json 与 settings.json

**事实**：sessions 顶层是 `session_name -> SessionState` 映射。[A09]

| SessionState 字段 | 类型/默认 |
|---|---|
| name / token / url / endpoint | 必填字符串 |
| token_expires_at | 可空 datetime，默认 null |
| variant | 字符串，默认 `DEFAULT` |
| accelerator | 字符串，默认 `NONE` |
| machine_shape | 字符串，默认 `STANDARD` |
| kernel_id / session_id | 可空字符串，默认 null |
| last_execution | 可空三元组 `(str, optional str, str)`，默认 null；JSON 数组 |
| running | 可空字符串，默认 null |

Settings 默认：update_url 指向 PyPI 的 google-colab-cli JSON；last_check/latest_version=null；enable_update_check=true。[A09]
上游对整个 sessions 读取出现任一解析错误时返回空映射；这可能隐藏损坏文件，Rust 建议报告明确错误并允许恢复备份。
`token` 是 runtime proxy token，不能当 CLI OAuth token 导入 keyring。

### 4.4 OAuth 与 DFS 材料

**事实**：CLI token 文件/keyring 存储 `google.oauth2.credentials.Credentials.to_json()` 的序列化结果，扩展没有另定 schema。
Rust 导入至少识别 token、refresh_token、token_uri、client_id、client_secret、scopes、expiry；具体可选字段由原序列化结果决定，不应臆造“所有字段必填”。[A11]

**事实**：CLI 两步 pending 存 account、client_config、scopes、redirect_uri、state、code_verifier、email_hint、created_at；code_verifier 可以是 null。[A12]
DFS pending 另存 flavor、port、client_id/client_secret/token_uri；创建时同样记录 account 和时间。[A17]
DFS credentials 写 flavor、client_id/client_secret、refresh_token、scopes、token_uri、created_at；成功读取 userinfo 后补 user_id/email，读取失败可仍保存。[A18]

**设计**：秘密文件至少 0600；新建目录建议 0700，这是高于原实现 `makedirs` 默认权限的改进。
示例与诊断输出只展示字段名；不得原样导出 pending 的 client_config 或 token 正文。
pending 应绑定 profile+用途+client+redirect，完成后删除；Rust 新增有效期与单次消费状态，作为显式兼容增强。

## 5. OAuth 能力与缺口

### 5.1 上游 CLI 远程复制粘贴

**事实**：使用 `InstalledAppFlow`、PUBLIC_SCOPES、SDK HTTPS landing page，authorization URL 带 `prompt=consent` 与 `token_usage=remote`。
打印 URL 后读 stdin 授权码，再兑换 token；不启动 localhost server，不使用 OOB redirect。[A01]
源码说明该 redirect 与内嵌 cloud-SDK client 匹配；自定义 client 不能仅替换 id 后假定仍可用。
上游配置读取顺序为显式/默认配置路径存在则读取，否则读取内嵌 oauth_config。[A11]

**事实**：PUBLIC_SCOPES 精确包含 openid，以及 userinfo.profile、userinfo.email、cloud-platform、colaboratory、drive.file 六项。
上游兑换前清空 oauth2session.scope，兑换后设为响应实际 granted scope，防止部分同意被当作 scope mismatch。[A01]

**设计**：Rust 将 requested/granted scopes 分开保存；刷新只请求实际已授权 scope。
用户拒绝 Drive scope 但允许 Colab 时，允许对应功能工作；调用缺 scope 的功能时说明需要补授权。
禁止将上游“远程复制粘贴”改成已停用的 OOB URI。

### 5.2 扩展 CLI 登录

**事实**：扩展提供交互登录及 `--url-only` → `--code` 两阶段登录；给 OAuth URL 添加 login_hint，保存 pending 后退出。[A12]
`account add` 默认 with-drive=true、use=true；`account login` 默认 with-drive=false。
ADC add 只配置/检查 ADC；url-only add 创建 pending 后可立即切 active，不代表已经登录成功。[A13]

**事实**：三条扩展 CLI OAuth 路径直接 fetch_token，没有上游新版的部分授权 scopes 处理。[A12][A14]
`record_authorized_identity` 调 userinfo 失败时静默返回；email 与预期不同或重复只警告，随后更新真实 email/user_id。[A15]
`--no-browser` 在 account Colab OAuth 阶段不改变复制粘贴流程，主要被传入后续 Drive consent。[A13]

**设计**：整合时采用上游已修复的部分授权行为；login_hint 保留为提示，不将其宣传为身份强制约束。
若产品希望锁定身份，须新增明确的确认/拒绝策略，不能悄悄改变现有警告语义。

### 5.3 Drive consent 的 remote 与 loopback

**事实**：Drive flavor 支持 dfs/sdk/custom；dfs 的默认 redirect 是 `http://localhost:<port>`，sdk 默认使用 SDK HTTPS landing page，custom 默认 localhost 或显式 redirect。[A16]
CLI 中无显式 flavor 时默认 dfs；无 redirect 的 url-only 自动切 sdk；独立脚本默认 sdk。
显式 flavor 不一定被自动改写，因此不能把所有 `--url-only --flavor dfs` 都描述成 remote flow。[A17]
DFS loopback 先选端口，再创建 flow，监听 localhost，最多等待 consent_timeout；收到 code 后兑换。[A19]
custom 分支只要指定 redirect 就可能走手动复制粘贴分支，应按实际条件测试，不按名称猜测。

**事实**：DFS 默认 scope 集较大，包含 email、client_channel、docs.test、drive、drive.activity.readonly、drive.photos.readonly、experimentsandconfigs、peopleapi.readonly；minimal 为 openid、userinfo.email、drive。[A20]
account add 后自动 Drive consent 会传 `--minimal`，并非总使用完整 scope 集。[A13]

### 5.4 PKCE 与 state：不要把字段等同验证

**事实**：README 宣称两步 PKCE 登录；代码保存/恢复 code_verifier，但这些 flow 构造处没有显式开启 verifier 自动生成。[A12][A16][A21]
**待验证**：实际依赖版本是否默认生成 verifier/challenge；静态应用代码不足以证明所有路径使用 PKCE。
验证应检查授权 URL 存在 code_challenge 与 S256，token 请求带原 verifier，且 challenge 与 verifier 匹配；不需要真实 token 即可做本地 mock 测试。

**事实**：DFS loopback handler 只提取 code/error，没有比较回调 state；后续只给 fetch_token 传 code，不传完整 callback URL。[A19]
两步流程保存的 state 也没有在粘贴裸 code 时做响应比对；赋值 `flow.oauth2state` 本身不能证明验证发生。[A12][A17]
**设计**：Rust loopback 必须检查 state、预期路径与 error；先 bind 再打开浏览器；只接受首个有效回调。
手动 code 模式应明确其无法从裸 code 比对 callback state，依靠强制 PKCE、pending 绑定和短有效期降低串流风险。

## 6. ADC：范围声明与真实能力分离

**事实**：上游把发现链交给 google.auth.default(scopes=PUBLIC_SCOPES)，注释列出环境变量、gcloud、GCE/GKE 等；支持 re-scope 的类型会 with_scopes。[A22]
用户 ADC 已签发 scope 无法靠 with_scopes 扩大；用户需重新执行带 openid/cloud-platform/userinfo.email/colaboratory scopes 的 gcloud 登录。
上游对 invalid compute_engine.Credentials 直接置空而非 refresh，因此注释提到 metadata 并不证明该分支实际可用。

| 凭据类型 | 静态代码建立的结论 | Rust 验证要求 |
|---|---|---|
| gcloud authorized_user | 委托 google-auth 加载/刷新 | 临时目录 fixture 覆盖发现与 refresh |
| 显式 ADC JSON | 扩展保存路径并临时设置 env | 按账户直接加载，检查文件类型 |
| service_account | google-auth 可能支持，Colab身份可用性未知 | 签名/refresh mock；服务端授权另验 |
| impersonated / external_account | 由依赖库能力决定，本文未建立全矩阵 | 枚举支持类型，未知类型明确报错 |
| metadata/GCE/GKE | 有发现意图，存在 invalid 分支限制 | mock metadata；真实平台另外验收 |

service_account 的原生 JWT 交换与离线验签已接入，当前矩阵及真实身份验收缺口见 [30](30-adc-service-account-implementation.md)，IAM impersonation 后续接入见 [31](31-adc-impersonation-implementation.md)。

external_account 的 X.509 workload/mTLS 实现与限制见 [71](71-x509-adc-and-mtls.md)。

external_account 的 STS 可选客户端认证与 workforce 项目规则见 [70](70-external-account-client-auth.md)，真实身份权限仍需独立验收。

不得把“Rust 能读取 ADC”写成“任意 Google Cloud 身份都能使用 Colab”。
**设计**：凭据来源是 profile 对象内字段，绝不依靠多线程修改进程环境变量。
启动时只快照外部 env；明确保留现行为“预置 GOOGLE_APPLICATION_CREDENTIALS 优先于 profile adc_file”，或将改变列为迁移差异。

## 7. 并发、锁与迁移事务

**事实**：扩展 registry 锁为独立 `.lock` 文件上的 flock；非 POSIX 没有 fcntl 时不加锁。
原子写使用同目录临时文件、flush/fsync、chmod 0600、os.replace；没有 fsync 父目录，也没有账户生命周期覆盖所有文件的统一事务。[A23]
读注册表经常不加锁，依靠 replace 提供完整快照；keyring/文件 token 读写没有 profile refresh 锁。

### 7.1 ADC 跨账户串用风险

**事实**：`_get_adc_hook` 在锁外检查全局 env，只有 env 不存在时才进入 `_ADC_ENV_LOCK` 注入 profile 路径。[A24]
`probe_all_accounts` 最多同时启动 8 个 worker，每个 worker 新建 State 并设置 ContextVar。[A25]
**推断，具有明确静态触发条件**：A 注入 ADC env 后，B 可能看到 env 已存在，跳过锁，用 A 的凭据执行 B 查询。
这同时影响 B 有独立路径与 B 使用默认 ADC 两种情况；ContextVar 不隔离 os.environ。
**设计**：各账户独立 credential provider；禁止运行中 set/remove GOOGLE_APPLICATION_CREDENTIALS。

### 7.2 全局 patch 与查询副作用

**事实**：扩展修改 auth 函数、State.history property、全局 state 缓存和 auto_update 方法；命令关闭时恢复部分 ContextVar/env。[A14][A26]
`account status`/`sessions --all` 除查询服务端，还会删除本地失效 session 并刷新 proxy info 写回本地文件。[A25]
**设计**：Rust 原生整合不需要 monkey-patch；每命令建立上下文，查询的本地同步副作用要在帮助说明和测试中保留。
状态探测的某账户错误应变成该账户结果，不使其余账户丢失；JSON stdout 不能混入登录提示或更新广告。

### 7.3 bootstrap、rename、remove

**事实**：bootstrap 仅目标缺失时复制旧数据；原文件和旧 keyring 保留。
旧钥匙串首次读取可复制到新槽位；失败可仍返回旧 token；下一次保存清除 migrated 标志。[A27]
**设计**：bootstrap 幂等；先写数据，再提交 registry；失败保留源与可恢复记录。

**事实**：rename 先移动目录，随后尝试迁移 keyring；迁移异常被吞掉，仍清除 migrated 标志并更新 registry。
绑定、全局 Drive 默认、active 通常更新为新名；但重命名账户自身 meta 的自引用 drive_account 没被循环改写。[A28]
这不构成全事务“无损迁移”，钥匙串失败可能使新名读不到旧 token。
**设计**：rename 采用预检→复制/验证凭据→移动目录→提交引用→清理旧槽位；失败可回滚或重试。
同时改写所有 profile 引用，包括自身；pending 内 account 字段也要验证/迁移，不能只挪文件名。

**事实**：remove 只检查本地 sessions；非空需 force。force 不释放后端 VM，也不撤销 Google token。
删除最后一个 profile 后创建空 default；清除其他 profile 的绑定与全局 Drive 默认。[A29]
`COLAB_TOKEN_STORAGE=file` 时不会删 keyring 槽，auto/keyring 删除 keyring 异常也被吞掉。
**设计**：remove 输出区分本地删除、凭据清理失败和后端 VM；不能报告为“已停止所有资源”。
若需完整凭据清理，应提供跨后端清理结果，保留与旧实现的可见差异。

## 8. 验收测试清单

所有 fixture 使用临时 root、内存 keyring、本地 HTTP/mock token 服务，不访问用户 HOME/keyring。
真实账号测试单独执行，记录 compute/drive 身份、授予 scope、创建资源和清理结果；未执行前标记待验证。

| 编号 | 输入/故障 | 必须断言 |
|---|---|---|
| T01 | flag/env/session/active 同时存在 | flag 优先；逐层移除后符合第 3 节 |
| T02 | active 与他账户同名 session | active 优先；active 无且多 owner 报歧义 |
| T03 | run 脚本参数包含 `-- -a x -s y` | 参数不参与账户路由 |
| T04 | email 大小写、重名 email、非法名称 | 精确 key 优先；email 唯一才解析；阻止路径逃逸 |
| T05 | Drive 显式/env/绑定/自身文件/default | 顺序准确；consent 跳过绑定/default |
| T06 | 新/旧 env 别名同时设置 | 新名优先；空白边缘行为明确列为兼容或修正 |
| T07 | bootstrap 中断后重试 | 不覆盖已存在目标、不删源、不重复丢历史 |
| T08 | legacy keyring 迁移写失败 | 不清 migrated 标志、不丢旧 token |
| T09 | auto/keyring/file 各后端读写异常 | fallback 符合策略、0600、stderr 有可用说明 |
| T10 | rename 每阶段故障注入 | profile 与凭据可恢复，引用含自绑定全部更新 |
| T11 | remove 有 session/最后账户/keyring失败 | force 语义正确，不宣称远端已释放 |
| T12 | 两账户 ADC 并发且不同 token | 请求身份严格对应 profile；进程 env 不变 |
| T13 | 一个 ADC 使用默认、另一个显式文件 | 仍无串用，缺失错误只影响自身 |
| T14 | OAuth 部分 scope 同意及刷新 | 保存/刷新 granted scopes，不扩大授权 |
| T15 | url-only 重启后 code 完成 | verifier/redirect/client/profile 保持一致，pending 单次消费 |
| T16 | loopback 错误 state/error/超时/占端口 | 错误回调拒绝，不保存 token，可重试 |
| T17 | PKCE verifier/challenge | S256 匹配、兑换必须带 verifier |
| T18 | invalid token、refresh拒绝、keyring锁定 | 非交互探测不读 stdin，每账户单独错误 |
| T19 | status/sessions 全账户混合成功失败 | 顺序稳定，JSON干净，本地同步副作用隔离 |
| T20 | 未来 schema version/损坏 JSON | 不覆盖原件、返回可操作错误，未知字段不丢失 |

现有测试覆盖迁移、四层路由、session 隔离、file fallback、CRUD、模拟两步授权及并发探测。[A30]
这些测试不建立真实 Google 授权成功、ADC 全身份矩阵或 callback state 校验；新增 T10/T12/T16/T17 不能只模拟最终成功返回值。

## 9. 稳定证据索引

以下均为固定提交 permalink；行号仅证明所述静态事实，不证明线上策略。

- **A01** U：scopes、远程 flow 与部分授权：[src/colab_cli/auth.py:47–54](https://github.com/googlecolab/google-colab-cli/blob/02d2321c38dd215b7804eebf78b61d98b856bcd3/src/colab_cli/auth.py#L47-L54)；[src/colab_cli/auth.py:144–187](https://github.com/googlecolab/google-colab-cli/blob/02d2321c38dd215b7804eebf78b61d98b856bcd3/src/colab_cli/auth.py#L144-L187)。

- **A02** U：控制面与 runtime proxy 文件接口：[src/colab_cli/client.py:124–169](https://github.com/googlecolab/google-colab-cli/blob/02d2321c38dd215b7804eebf78b61d98b856bcd3/src/colab_cli/client.py#L124-L169)；[src/colab_cli/contents.py:25–52](https://github.com/googlecolab/google-colab-cli/blob/02d2321c38dd215b7804eebf78b61d98b856bcd3/src/colab_cli/contents.py#L25-L52)。

- **A03** E：名称规则与隔离路径：[colab_ext/accounts.py:27–106](https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/accounts.py#L27-L106)。

- **A04** E：key/email 查找与歧义：[colab_ext/accounts.py:269–307](https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/accounts.py#L269-L307)。

- **A05** E：账户解析优先级：[colab_ext/accounts.py:790–822](https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/accounts.py#L790-L822)。

- **A06** E：可路由命令与参数边界：[colab_ext/cli_hooks.py:115–142](https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/cli_hooks.py#L115-L142)；[colab_ext/cli_hooks.py:389–437](https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/cli_hooks.py#L389-L437)。

- **A07** E：stderr 路由提示与 ssh：[colab_ext/cli_hooks.py:644–650](https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/cli_hooks.py#L644-L650)。

- **A08** E：Drive 解析及 consent 特例：[colab_ext/accounts.py:825–861](https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/accounts.py#L825-L861)。

- **A09** U：SessionState、Settings、存储：[src/colab_cli/state.py:25–45](https://github.com/googlecolab/google-colab-cli/blob/02d2321c38dd215b7804eebf78b61d98b856bcd3/src/colab_cli/state.py#L25-L45)；[src/colab_cli/state.py:106–151](https://github.com/googlecolab/google-colab-cli/blob/02d2321c38dd215b7804eebf78b61d98b856bcd3/src/colab_cli/state.py#L106-L151)。

- **A10** E：注册表默认、修复与更新：[colab_ext/accounts.py:170–245](https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/accounts.py#L170-L245)；[colab_ext/accounts.py:310–342](https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/accounts.py#L310-L342)。

- **A11** U：OAuth 配置、缓存、刷新和序列化：[src/colab_cli/auth.py:190–276](https://github.com/googlecolab/google-colab-cli/blob/02d2321c38dd215b7804eebf78b61d98b856bcd3/src/colab_cli/auth.py#L190-L276)。

- **A12** E：CLI OAuth pending/交互实现：[colab_ext/account_commands.py:63–165](https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/account_commands.py#L63-L165)。

- **A13** E：add/login 默认与 Drive consent：[colab_ext/account_commands.py:168–192](https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/account_commands.py#L168-L192)；[colab_ext/account_commands.py:299–472](https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/account_commands.py#L299-L472)。

- **A14** E：认证 patch 与远程 OAuth hook：[colab_ext/cli_hooks.py:240–346](https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/cli_hooks.py#L240-L346)。

- **A15** E：身份查询、提示与元数据更新：[colab_ext/cli_hooks.py:180–232](https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/cli_hooks.py#L180-L232)。

- **A16** E：Drive flavor 与 redirect：[colab_ext/dfs_creds.py:229–263](https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/dfs_creds.py#L229-L263)。

- **A17** E：Drive consent 路由、pending 与兑换：[colab_ext/dfs_creds.py:288–402](https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/dfs_creds.py#L288-L402)。

- **A18** E：Drive credentials 输出字段：[colab_ext/dfs_creds.py:404–438](https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/dfs_creds.py#L404-L438)。

- **A19** E：loopback callback 处理：[colab_ext/dfs_creds.py:479–531](https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/dfs_creds.py#L479-L531)。

- **A20** E：Drive 完整/minimal scope：[colab_ext/dfs_creds.py:109–128](https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/dfs_creds.py#L109-L128)。

- **A21** E：README 的 PKCE 与不阻塞声明：[README.zh-CN.md:12–22](https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/README.zh-CN.md#L12-L22)；[README.zh-CN.md:131–160](https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/README.zh-CN.md#L131-L160)。

- **A22** U：ADC 发现与有效性处理：[src/colab_cli/auth.py:279–364](https://github.com/googlecolab/google-colab-cli/blob/02d2321c38dd215b7804eebf78b61d98b856bcd3/src/colab_cli/auth.py#L279-L364)。

- **A23** E：原子写与 registry 锁：[colab_ext/accounts.py:118–167](https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/accounts.py#L118-L167)。

- **A24** E：ADC 环境变量临时覆盖：[colab_ext/cli_hooks.py:312–339](https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/cli_hooks.py#L312-L339)。

- **A25** E：每账户探测、本地同步与线程池：[colab_ext/cli_hooks.py:669–788](https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/cli_hooks.py#L669-L788)。

- **A26** E：history patch 与全局 State 绑定：[colab_ext/cli_hooks.py:349–381](https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/cli_hooks.py#L349-L381)；[colab_ext/cli_hooks.py:499–642](https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/cli_hooks.py#L499-L642)。

- **A27** E：bootstrap 与 token 后端迁移：[colab_ext/accounts.py:209–257](https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/accounts.py#L209-L257)；[colab_ext/accounts.py:487–560](https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/accounts.py#L487-L560)。

- **A28** E：rename 非事务流程：[colab_ext/accounts.py:665–715](https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/accounts.py#L665-L715)。

- **A29** E：remove 的本地语义：[colab_ext/accounts.py:718–750](https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/accounts.py#L718-L750)。

- **A30** E：隔离 fixture 与既有测试：[tests/test_colab_ext.py:139–181](https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/tests/test_colab_ext.py#L139-L181)；[tests/test_colab_ext.py:201–590](https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/tests/test_colab_ext.py#L201-L590)。

当前 external_account 文件/URL 来源、STS、workforce 与可选 IAM 目标的实现及未支持矩阵见 [36](36-adc-external-account-implementation.md)；成功取 token 不代表 Colab 接受该身份。

Workforce 用户 external_account_authorized_user 支持原生 STS Basic 刷新，也可作为服务账号模拟的嵌入来源，见 [37](37-adc-workforce-user-implementation.md)。

metadata ADC 作为默认 gcloud 文件缺失后的最后发现源，显式或坏文件不降级；实现、环境快照和平台限制见 [38](38-adc-metadata-implementation.md)。

Unix external_account 可执行来源现支持显式 opt-in、非交互执行和输出缓存，见 [39](39-adc-executable-implementation.md)；不会在 doctor 中执行。

后续已接入 AWS aws1 的环境/EC2 与 SigV4 主体，见 [40](40-adc-aws-implementation.md)；并未验证真实云端接受性。

ADC 请求的配额项目取自启动时的 GOOGLE_CLOUD_QUOTA_PROJECT 快照或所选凭据文件，与 token 一起绑定到 provider；IAM 来源/目标和 runtime 边界见 [52](52-adc-quota-project-implementation.md)。

Linux Secret Service 的后续原生隔离测试已验证 Python/Rust 双向读写、旧槽迁移、锁定、真实 D-Bus caller timeout、file fallback 与恢复，见 [55](55-native-secret-service-validation.md)。不使用用户钥匙串，不替代完整原版 CLI 或共享配置文件锁互通。
