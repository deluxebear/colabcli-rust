# 账户添加、登录与 Drive 联合授权

## 输入与流程

本阶段依据[固定扩展的 add/login 顺序](https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/account_commands.py#L300-L474)，将原来的只登记 add 改为默认认证流程。入口实现在 [account_cli.rs](../src/account_cli.rs)，Drive 授权复用 [drive_cli.rs](../src/drive_cli.rs) 的同一个 consent 实现，不通过 Python 子进程，也不修改环境切换账户。

```sh
# 只登记资料，不登录、不切 active；适合初始化与离线操作
colab account add work --configure-only
# 新账户的远程两步授权；保存 pending 后默认切 active
colab -c /absolute/path/client.json account add work2 --url-only --no-use
colab account login work2 --code AUTHORIZATION_CODE
# 手动粘贴计算授权码；成功后不申请 Drive 授权
colab -c /absolute/path/client.json account add compute --no-drive
# 计算授权完成后，显式继续独立 Drive 授权
colab account login work --code AUTHORIZATION_CODE --with-drive --no-browser
```

add 接受 --email、--adc-file、--drive-account、--use/--no-use、--url-only、--with-drive/--no-drive、兼容 --with-dfs/--no-dfs、--flavor、--no-browser。--with-dfs/--no-dfs 优先于主 Drive 开关；同一对正反开关同时给出时参数错误，不采用最后一个覆盖。flavor 限定 dfs/sdk/custom。

默认 add 完成计算 OAuth，未绑定其他 Drive 账户时尝试本账户 Drive 授权，然后切 active；--no-drive 关闭 Drive 阶段，--no-use 保留 active。--drive-account 先解析已存在的账户并绑定，随后跳过本账户 Drive 授权。--url-only 保存计算 PKCE pending 后返回，此时可切 active，但没有 token、没有启动 Drive 阶段。后续 login --code 若需要继续 Drive，必须显式带 --with-drive；不隐式保存或重放一个 Drive 操作意图。

--configure-only 是 Rust 扩展，保留旧的离线登记方式；不认证、不切 active，可登记 ADC 路径和 Drive 绑定。默认流程的迁移需要离线初始化调用显式增加此选项，不能再假设裸 account add 没有认证副作用。

login 支持显式 name、--email、Drive 开关与 flavor/no-browser。开始新授权时可创建缺失的命名账户，完成 --code/--callback-url 时不自动创建；邮箱提示保存在相应账户并传给授权 URL，不作为已验证 Google 身份。login 不改变 active，默认不发起 Drive 授权。已有 --redirect-uri、--callback-url 和 PKCE/state 校验继续有效。

计算授权始终使用手动 code，不自动打开浏览器；--no-browser 也作用于后续 Drive 浏览器启动。无 TTY 时默认要求 --url-only/--code；显式 --no-browser 可从 stdin 读取计算授权码，空输入失败并保留 pending。Drive 的 loopback/终端输入、期限和中断处理沿用原 consent，不因联合入口改变。

## ADC 和身份隔离

--adc-file 必须是存在的普通文件，并隐含 ADC provider，即使同时传 --auth oauth2。默认 add 取得 ADC token 成功后才切 active，不启动 Drive 授权；--configure-only 只记录路径不获取 token。ADC 登录不接受 OAuth code/url-only/callback 参数，不把获得 token 写成已经具备 Colab 权限。login 显式 --with-drive 时，可在 ADC 验证之后独立启动 Drive 授权。

OAuth、ADC 与 Drive 每一步都显式传入目标 profile，登录计算账户时不会按全局或账户的 Drive 绑定改用其他计算身份。联合 Drive 阶段针对同一个 profile；指定 --drive-account 的 add 只绑定既有存储账户并跳过本账户 Drive 阶段。不会复用计算 token 充当 DriveFS 完整授权。

Drive client 仍由外部配置提供，支持既有 COLAB_DRIVE_CLIENT_ID/SECRET/TOKEN_URI/SCOPES/USER_ID/EMAIL 参数来源；未单独提供时沿用 -c 配置。dfs 必须匹配 DriveFS client，不能把任意计算 OAuth client 当作 DFS client。程序不复制上游内嵌 secret。两类凭据和 pending 文件分开保存。

## 状态、失败与清理

先验证名称、ADC 路径与绑定账户，再登记 profile；计算授权失败保留已登记资料和该账户 pending，active 保持原值，不启动 Drive。OAuth 兑换使用已有 exchange_started/result 恢复约定，不能不确定时重放授权码；认证成功后的凭据不会因后续 Drive 失败而删除。

add 的 Drive 阶段失败会提示可重试命令并继续切 active，成功退出；login 显式申请 Drive 时的失败返回非零。用户中断的 CliExit 不转换成普通警告，不继续切 active。URL-only 的 active 切换只表示 profile 被选中，不代表已认证。

不持跨账户注册表锁等待用户输入；各服务在读写时使用现有账户生命周期和 flow 锁，账户重命名/删除或迁移冲突由原保护拒绝。active 更新发生在最后，若账户已被其他进程改变，不声称切换成功。没有创建 VM，无远端运行资源需回收；失败 pending 由相同账户的授权流程恢复，不手删账户事务。

保留的差异：已有同名 profile 的 add 仍明确拒绝，避免静默重配身份/绑定；需用 login、set-drive 等专用入口。Google 身份信息不会自动查询，--email 仍是提示资料，真实身份由 whoami 验证。Shell completion 和 Drive status 覆盖参数的后续实现见 [35](35-completion-and-drive-status-implementation.md)；完整合并版业务差分仍待补齐。

## 验证

新增八项 Rust 集成测试，使用临时 HOME、文件凭据与 loopback：URL-only/active/no-use、离线登记和绑定预检、命名 login 创建与 code 拒绝创建、email hint、计算兑换失败的 pending/active、add 警告与 login 非零的区别、Drive 兼容开关优先级与绑定跳过，以及 ADC provider 优先级/成功失败。

联合成功测试实际执行完整子进程：输入计算 code，接收 Drive 授权 URL，向该进程的本地 callback 监听器提交匹配 state，再进行第二次 token 交换；核对独立计算/Drive 凭据、pending 清理、active 和 default 账户隔离。这是合成 dfs 协议验证，不代表真实 Google 接受。sdk/custom 复用已有 consent 路径，三类线上联合授权仍未验收。

CLI 差分新增十项 Rust 扩展场景，核查 URL 与 pending state 相符、active 切换时点、绑定、只登记、命名登录、错误路径和 Drive 失败保留计算授权。原 account-add 离线场景改为显式 --configure-only，不能把它算作裸 add 行为不变。合并版 help 快照重新采集，实际参数已展示；帮助成功仍不证明合并版业务完全等价。

重新采集的合并帮助仍为 83 路径，长短 help 全部成功，Rust 缺失命令路径为零。账户 add/login 的参数缺口已消除；未展示参数记录从八个路径降为四个（root、drive/dfs status、ssh），其中 root/ssh 部分为已有隐藏参数。release 重建、打包校验和与新账户参数 help 均通过，证据见 [发行验证记录](../research/native-release-validation.json)。

最终复查将账户被并发删除时的邮箱读取改为类型化错误，避免直接索引 panic。两项 SSH 信号测试的就绪等待调整为 10 秒、请求期限 15 秒，先等到有效子进程 PID 再发送信号；修复原先 2 秒请求期限早于 3 秒就绪等待的竞态。中断完成期限仍单独验证。

最终验证：默认特性 318 项、无默认特性 316 项 Rust 测试通过；两种特性 Clippy、fmt、默认构建和研究文档检查通过。完整 CLI 差分 318 个场景为 92 一致、54 保留差异、172 Rust 扩展，无断言失败，原有 308 场景分类不变（account-add 已按新的离线参数迁移）。合并版 83 路径长短 help 均成功，账户参数缺口已消除；两份报告哈希与当前 debug 二进制相符。真实 Google/Drive 授权与平台验收仍未完成。

## 默认计算 OAuth 客户端恢复（2026-11-19）

计算账户 `add/login` 的配置顺序为显式 `-c`、用户默认文件、固定上游公开配置。只有未显式指定且默认文件不存在时才回退；已有文件损坏、权限错误和断开的符号链接不回退，避免意外切换客户端身份。无 HOME 时亦可使用公开默认配置。默认下载固定为研究基线 `02d2321c38dd215b7804eebf78b61d98b856bcd3` 的 `src/colab_cli/oauth_config.json`，校验 SHA-256 `d3127103bea2884c9f8269d17bcfa74b21356837672bb5b46067cecfe219fc5d`。禁止重定向，连接期限 5 秒、总期限 15 秒，响应最多 16 KiB，失败不输出响应正文。

这是原版内嵌配置的联网获取替代：Rust 源码和仓库不保存公开配置中的 client secret；登录启动需要 GitHub 网络可达，不支持无配置的离线 URL 生成。所选客户端仍随账户隔离的 PKCE pending 私有保存；code/callback 完成和令牌刷新不依赖 GitHub。获取失败不会写 pending，原有 pending 仍保留。账户 add 已登记的元数据也保留，可用 login 重试。自定义客户端的 redirect 仍需调用者匹配。

此补充仅覆盖计算 OAuth 默认客户端，不把它当作 DFS 客户端；独立 DriveFS 授权仍遵循前述外部 DriveFS 配置要求。离线测试使用合成客户端和 loopback HTTP，核对选择优先级、无效文件拒绝、状态/大小/哈希/JSON 拒绝与响应脱敏。固定公开配置获取及编译版 URL 生成验证不等于实际 Google 同意、code 兑换或 DriveFS 首次授权已验收。

本轮验证：默认特性完整测试 522 项通过，随后补充的断链回归与另两项默认客户端回归共 3 项通过；无默认特性完整测试 521 项通过。两种特性全目标 Clippy、fmt 与固定源码/文档检查通过。新构建的 release 二进制在临时 HOME/配置目录中实际获取固定公开配置并生成 Google 授权 URL，核对 SDK redirect、remote 标志、S256 和 pending 的账户及 0600 权限；临时数据已清理。没有兑换真实授权码或创建 VM。
