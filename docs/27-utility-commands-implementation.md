# 浏览器 URL、身份诊断与内嵌文档

更新日期：2026-11-17。本阶段实现 url、pay、隐藏 whoami、readme/README、skill/SKILL。本阶段结束时 update 与发行渠道仍待实现，后续进展见 [32](32-native-update-and-release-implementation.md)；完整设计与线上兼容验收尚未完成。

## 本地工具命令

```sh
colab url -s work
colab -a compute url -s work --host https://sandbox.example.invalid --open
colab pay --url-only
colab readme
colab SKILL
```

`url -s NAME` 使用已有配置和本地会话，可离线生成链接。显式会话遵守既有账户优先级和唯一归属路由，重名必须明确账户。省略 `-s` 时，如果所选账户有本地会话，CLI 先用现有认证查询控制面，裁剪已消失的会话并选择唯一的有效命名会话。列表查询失败时保留本地记录并返回错误。本地无会话时直接返回，后端孤立会话不参与选择。

连接格式保留官方的 dbu query 与 datalabBackendUrl fragment：query 中是 backend path，fragment 中是同 origin 的完整 backend URL。存在所选计算账户邮箱时按扩展规则加入 authuser。字段缺失或为空时省略 authuser，浏览器使用自身账户选择。成功时 stdout 只有一行 URL。无活动会话、显式会话缺失或存在多个可选会话时，CLI 返回 1 并将提示写入 stderr。

[connection_url](../src/utility.rs) 只接受不带认证、额外路径、query 或 fragment 的 HTTP(S) origin；主机由 URL parser 规范化。endpoint 按一个不透明路径段编码，拒绝空、控制字符和点路径；query 继续编码该路径，fragment 保留正确的完整 URL。特殊字符不能注入 query/hash 或改写后端路径。生成 URL 不证明 VM 仍在线，也不传播 runtime proxy token。

--open 显式调用系统 open/xdg-open/explorer，只传递一个 URL argv，不经 shell。子进程 stdio 不污染 URL；启动或退出失败返回非零，已打印的 URL 仍可复制。pay 默认按官方格式提示并打开固定 Colab signup 页面；Rust 扩展 --url-only 仅打印地址，适合无浏览器环境。不会由这些命令自动购买或订阅。

readme/README 输出编译时内嵌的本项目 README；skill/SKILL 输出 [原生 CLI Agent 指引](../resources/SKILL.md)，含账户选择、会话/文件/执行、独立 Drive、未知执行和清理边界。两类资源不依赖运行时源码目录、本地 Python 或网络；不初始化配置。

## whoami 联网身份诊断

```sh
colab -a compute whoami
colab -a compute whoami --json
```

whoami 只按显式账户、COLAB_ACCOUNT、active 选取身份，不按某个 VM 或 Drive 绑定切换。provider 遵守 --auth 覆盖及账户 provider。它明确刷新 OAuth 或当前支持的 authorized_user ADC，即使已有 access token 尚有效；不是只读本地检查，离线诊断请用 ext doctor。

OAuth 的 [refresh_access_token](../src/auth.rs) 沿用账户生命周期锁、每账户刷新锁、轮换暂存及提交协议。原 access_token 缓存路径行为不变；强制刷新在同账户串行，第二次读取第一次保存的 refresh token，不能并发用旧 token 覆盖轮换。后续 tokeninfo 失败不会回滚已提交凭据；凭据后端写失败保留暂存，原恢复路径仍可采用该结果。

ADC 使用其独立来源刷新，不修改 OAuth 文件或 ADC 源文件。初版只支持 authorized_user，其他类型静态失败，不执行外部来源。本轮没有扩大 ADC 身份支持矩阵。后续 service_account 已接入，见 [30](30-adc-service-account-implementation.md)。

tokeninfo GET 仅携带 access_token query，不附加 Bearer header。使用 10 秒期限、禁用重定向、限制 64 KiB 响应，只接受 email、audience/aud、expires_in 和 scope 等允许字段。错误不打印原始 body、未知字段、reqwest URL 或 traceback；身份字段含控制字符或完整当前 token 时拒绝，scope 限长并按空白拆分排序。--json 输出 account、provider 和 identity；文本额外显示账户来源/存储方式，均不打印 token。

OAuth 身份查询成功后，只在所选账户仍为 OAuth、邮箱为空且当前存储 bearer 与被查询 token 完全一致时补充邮箱。检查遵守 registry -> profile 锁顺序，不在网络查询期间持有全局锁；不会用已替换凭据的旧查询结果回填新身份，不覆盖已有邮箱。元数据保存失败不把已成功的身份查询伪报失败，会在 stderr 给出静态提示。ADC/显式 provider 覆盖不会将其邮箱写入原 OAuth 元数据。

## 差异与验证范围

固定官方 `url` 和 Rust 现在都在省略会话名时查询控制面。官方会将同步及选择提示与 URL 一起输出，Rust 保持 stdout 为单行 URL。异常 host 与不透明 endpoint 的更严格编码也属于 Rust 修正。whoami 保留扩展账户提示，并增加 JSON；tokeninfo 错误脱敏，ADC 不回填 OAuth 元数据。README/SKILL 内容采用原生实现的真实说明，保留资源命令和大小写别名，不复制 Python 环境或注入操作。这些差异保留在报告中，不归一化为假相同。

集成测试覆盖 URL 字符编码、计算/Drive 邮箱分离、无认证会话路由和歧义、信息命令不创建配置、浏览器精确 argv/无 stdout 污染/失败、OAuth 缓存强制刷新、旋转后身份失败保留、跨账户隔离、ADC 文件不变、tokeninfo 重定向/超长/秘密回显拒绝，以及查询期间更换凭据不回填旧邮箱。额外认证测试覆盖并发强制刷新和凭据后端失败恢复。

CLI 对比使用固定官方命令实现；whoami 的凭据对象采用真实 Google Credentials，只把签发及 tokeninfo 地址指向 loopback，浏览器启动写入合成捕获文件。保留禁止真实 keyring、浏览器和非 loopback 网络的隔离，不替换工具命令业务逻辑。不会因为这些模拟服务通过就宣称真实 Google 认证、浏览器登录态或线上 VM 连接已验收。

最终验证：默认特性 273 项、无默认特性 271 项 Rust 测试通过；两种特性 Clippy、fmt、默认构建和研究文档检查通过。完整 CLI 差分 262 个场景为 92 一致、53 保留差异、117 Rust 扩展，无断言失败，原有 240 场景分类不变；root/account/drive/ext 清单一致，报告二进制哈希与当前 debug 文件相符。

## 后续：实际合并版 URL 对照

[URL 对照工具](../tools/compare_merged_urls.py) 实际加载固定三仓库合并 CLI，使用临时 HOME、合成账户/会话、file 后端及无原生凭据库 Rust。原版禁止网络、钥匙串、浏览器和子进程；每组逐文件检查已有 payload 不变，并验证输出没有 runtime token 或计算 OAuth 材料。结果见 [merged-url-comparison.json](../research/merged-url-comparison.json)，同时记录实际 Python/Typer/Click/Rich 版本。

16 组覆盖 active 中重名会话优先、唯一归属、显式账户与环境优先级、邮箱选择、Drive 账户不参与 authuser、sandbox host、origin 规范化、不透明 endpoint，以及会话缺失、归属歧义和账户缺失。十二组 stdout、退出码和诊断一致，包含缺失邮箱、空邮箱和具名会话缺失。其余四组各自验证具体差异。

| 差异 | 组数 | 固定行为 |
| --- | --- | --- |
| 全局选项位置 | 1 | 原版 url 子命令后不接受 -a，退出 2；Rust 支持全局参数在此处出现，生成所选账户 URL |
| origin 规范化 | 1 | Rust 将大写主机转小写并去除默认 443；原版保留输入形式 |
| endpoint 编码 | 1 | Rust 将斜线/Unicode/空格/#?% 编为单个不透明路径段；原版 fragment 保留原始 endpoint |
| 错误诊断 | 1 | 账户缺失时两端均退出 1 且 stdout 为空，文案不同 |

只有临时 HOME 和 ANSI SGR 显示序列可规范化；业务 URL 的 host、query、fragment 不被折叠成“相同”。authuser 明确取会话所属计算账户邮箱，绑定或环境 Drive 账户的邮箱不混入。没有为复刻特殊 endpoint 的原始 fragment 改动已实现的路径隔离。

本次只核对显式 -s 的本地 URL 路径，不执行未指定会话时的原版后端同步、--open 或真实前端连接。本次对照使用加入跨账户路由提示后的构建。原有全量 CLI、Rust、Linux 和发行报告仍对应各自构建。

## 隐式会话同步对照

[专项对照](../research/url-session-sync-comparison.json) 使用固定官方 CLI 和本地服务。原有唯一会话和裁剪后只剩一个会话的场景均验证同一 URL 和一次 assignments 查询。Rust 回归额外覆盖空配置零请求、孤立会话排除，以及 HTTP 失败时本地记录保留。
