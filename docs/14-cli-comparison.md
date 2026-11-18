# 已实现 CLI 的自动差分测试

更新日期：2026-11-17。测试对象为本仓库 Rust CLI 和官方 `googlecolab/google-colab-cli` 的固定源码；不是 colab-ext 合并版，也不是 Google 线上互通验收。

## 基线与隔离

官方 CLI 基线为 `02d2321c38dd215b7804eebf78b61d98b856bcd3`，KERNEL 为 `f18e982c3265df5e923aa9def101ab3fd737e139`。工具在运行前检查 HEAD，并逐文件核对 [源码清单](../research/source-inventory.json) 的 SHA-256。Python 依赖版本及 Rust 二进制哈希记录在报告中；Python 运行依赖没有全部锁为上游 uv.lock，因此不能把结果当作锁定全依赖环境的等价证明。

[compare_cli.py](../tools/compare_cli.py) 为两版分别创建临时 HOME、配置、会话、历史和合成文件凭据，连接同规格 loopback HTTP/WebSocket 服务。分配、释放、上传、删除、编辑、exec 和 run 操作的都是模拟资源。WebSocket 只按测试协议返回固定输出，不执行收到的任意代码。

[official_cli_probe.py](../tools/official_cli_probe.py) 使用 `python -S` 跳过已安装的 `.pth`，避免 colab-ext 注入；显式加载固定官方及 KERNEL 源码。它只替换认证 Session、服务地址、状态目录及自动更新检查，不替换命令、Client、Contents 或 ColabRuntime 业务逻辑。真实 keyring 和浏览器操作禁止；socket 连接及 DNS 仅允许 loopback。Rust 使用合成 file token 和 loopback 地址，不读取开发者真实配置。

## 复现

先检出文档固定的 CLI 与 KERNEL 仓库，准备具有官方 Python 依赖的独立解释器。测试工具本身只使用 Python 标准库；官方子进程需要官方运行依赖。不要直接使用带扩展注入的 `colab` 入口替代隔离启动器。

```sh
cargo build --locked
python3 tools/compare_cli.py \
  --official /absolute/path/google-colab-cli \
  --kernel /absolute/path/jupyter-kernel-client \
  --official-python /absolute/path/official-venv/bin/python3 \
  --run-rust-tests \
  --output research/cli-comparison.json
```

可用 `--only '^(exec-|run-)'` 选择部分场景，报告会只包含这些场景；完整运行检查 root help 与 account/drive/ext 子命令清单，发现未映射命令或清单不符时失败。`--deps` 可显式指定官方依赖的 site-packages，默认通过该解释器的 sysconfig 取得。每个 CLI 子进程有 25 秒外部上限；超时记为未通过，不当作一致。工具遇到模拟契约断言失败返回非零；已记录的兼容差异不使工具自身失败。

## 覆盖内容

覆盖当前 root help 的全部入口（含 `account` 分组及内建 `help`）、10 个 account 子命令、11 个 drive 子命令，以及 `acct`/`dfs` 别名：

| 范围 | 自动检查 |
| --- | --- |
| help / version | 入口发现、各命令 help、`help new`、版本及退出码 |
| new | CPU、GPU、TPU、高内存、单形状硬件、自动命名、412、无效型号、GPU/TPU 冲突、裸 GPU 参数；GET/POST query、凭据分离与长度头、持久化 |
| sessions / status | 空列表、命名与孤立会话、失效裁剪、HTTP 失败、指定名称不存在；JSON 和 all 扩展 |
| stop / usage | 分配释放、无名称与未知名称、余额/消耗；JSON 和 all 扩展 |
| install | 包参数、ASCII/Unicode requirements 字节与上传、空输入、缺失文件、JSON 扩展、单引号语法缺陷；不实际安装软件包 |
| auth / drivemount | drivemount 在 Rust 端显式 --upstream；无/有既存 kernel 的启动、输出与持久化对比；合成已授权路径，不执行 Google 授权或真实挂载；专用 HTTP/WS 流程由 Rust 离线集成测试另行覆盖 |
| ls / upload / download / rm / edit | 目录排序、单文件、二进制、Unicode 路径、404、缺失本地文件、真实编辑器子进程修改及上传 |
| repl | 非 TTY 输入、唯一会话、空白、Python 错误、PNG 保存与不存在会话；交互按键另由 Rust PTY 验证 |
| drive | consent/import/show/token/clear、status/probe-cleanup、mount/umount、leases/lease-cleanup 和 dfs 别名的 Rust 扩展验证；含未知 mutation 旁只读核查、仅清理 probe、pending 展示、挂载预览、环境来源、计算 token scope 拒绝、来源冲突、原生传输与额外参数拒绝；固定官方 CLI 没有 drive 组 |
| url/pay/whoami/readme/skill | 浏览器 URL、只读路由、浏览器捕获、强制刷新与 tokeninfo、内嵌资源及大小写别名；[行为差异](27-utility-commands-implementation.md) 单独记录 |
| 根 drivemount | 默认缺凭据拒绝、no-fallback、预览、官方参数冲突、显式/环境/旧环境官方策略；[策略与锁](28-drivemount-strategy-implementation.md) |
| ext | doctor/status 本地快照、version、无写入 install、隐藏 drive/dfs 别名；固定官方 CLI 没有对应命令，均为 Rust 扩展 |
| ssh | 三种模式、创建/复用矩阵、--rm、二进制 stdout、重复 query/header、精确 argv/PTY、退出码、握手错误与早期清理；另有 endpoint 绑定扩展 |
| exec / restart-kernel | stdin、Python、Notebook、环境变量、空输入、kernel 错误、空闲超时、PNG 保存、结果落盘及 kernel 重启 |
| run / runs / run-cleanup | 成功、脚本错误、自动命名、keep、释放、缺失脚本、待恢复计划列举、所属资源清理、非法清理 ID |
| log | 空列表、命名历史、类型/行数过滤、缺失历史及 JSONL/TXT/Markdown/Notebook 导出 |
| ADC | IAM impersonation login/sessions/whoami/status、IAM 失败与 doctor；service_account 无效密钥拒绝、doctor 基本配置、状态行错误与 external_account 禁止执行；文件/URL external_account 的 login/sessions/whoami/status、IAM 失败和离线 doctor；成功的真实签名/交换由 Rust 集成测试覆盖 |
| account | status 联合查询及部分失败、list/current/add/use/set-drive/repair、URL 授权与 loopback code 兑换、rename/remove 缺失账户拒绝路径；JSON 与别名 |

account rename/remove 的成功、凭据迁移、账户隔离、并发与中断恢复使用已有 Rust 内存凭据测试；CLI 差分只运行不会访问真实钥匙串的拒绝路径。官方没有 account 对应命令，这些是 Rust 扩展验证，不能冒称双端兼容通过。`--run-rust-tests` 会执行完整默认 Rust 测试，并把实际通过数量与测试名加入报告。无默认特性测试仍用 `cargo test --locked --no-default-features` 单独运行。

## 结果解释

对齐后完整运行 365 个场景：92 个观察结果一致、54 个保留差异、219 个为 Rust 扩展，未通过断言为 0。排除 help 场景后的 301 个业务场景分别为 92 个一致、25 个保留差异、184 个扩展。完整默认 Rust 回归 378 项通过，无默认特性 376 项通过。初始报告中的随机名称和会话 ID 差异已拆分为格式断言与随机值归一化，不把不同随机抽样误判为功能错误。

结果见 [可读报告](../research/cli-comparison.md) 与 [完整 JSON](../research/cli-comparison.json)。JSON 保存每个场景的双端退出码、规范化 stdout/stderr、HTTP 请求摘要、提交的测试代码、会话安全字段、导出产物及契约断言。不保存 OAuth/proxy token 响应内容。

终端文本归一化临时目录、loopback 端口和 UUID。自动生成的会话名称先按官方六位十六进制（run 带 run- 前缀）断言格式，再在比较时替换随机值；Notebook cell ID 和 Jupyter session ID 也只在比较时归一化。JSON 保留原始观察值与 generated_name，未隐藏格式错误。语言、进度文字、包版本、错误通道和请求顺序仍直接比较。导出 Notebook 以解析后的 JSON 比较，二进制以长度和 SHA-256 比较；其他文本导出保留格式差异。HTTP 摘要不等于抓取完整 HTTP/WebSocket 字节，WebSocket 提交代码单独记录，不进行 Python 语义等价判断。

- `matched`：本合成场景选定观察值相同，不代表整个命令与线上等价。
- `different`：至少一项观察值不同；应查看具体字段，区分预期扩展、展示差异和行为差异。
- `rust_extension`：官方没有对应命令或参数，只有 Rust 场景断言通过。
- `failed`：退出码预期、协议模拟或产物断言未通过，应先诊断，不能按兼容通过计数。

## 本轮已对齐

1. new 使用六位十六进制自动名称；run 使用 run- 加六位名称。GPU/TPU 同时指定时优先 TPU；裸硬件选项要求型号；参数错误为 2。
2. stop 支持唯一会话自动选择；无会话、多会话返回 1；指定未知名称返回 0。文件和执行命令的自动选择提示与错误通道也已对齐。
3. new/stop/files/usage/log 的正常输出、404 与本地文件缺失提示，高内存忽略提示和过期会话裁剪提示均对齐。run 生命周期消息按实际分配、释放阶段发到 stderr。
4. 文本 exec 的 Python 异常只显示异常并返回 0；JSON 扩展仍返回 1。Notebook 进度、图像保存提示、source/text 多行表示与原版 execution_count 均对齐。
5. 默认历史导出的 JSONL 空白/ASCII 转义、TXT/Markdown、Notebook 标题、kernelspec 和事件映射对齐。内部审计元数据仍保留，log --json 与原增强库导出继续可用。固定官方样本见 [official-cli-fixtures.json](../research/official-cli-fixtures.json)。
6. 正常释放直接 GET/POST unassign；run 不再额外 DELETE kernel。释放前持久化 attempted，重试仍先查后端。restart-kernel 按官方顺序连接 runtime 后重启，也支持新会话初始化 kernel，但创建结果未知时不重复创建。
7. Jupyter session ID 使用官方 8+24 十六进制分组。文本超时清除旧 running 字段，将防重放状态保存在 execution_pending；重启确认成功后清除，两种执行状态仍都受重放保护。

auth/drivemount 已改为每次新操作创建独立 kernel，保留普通 exec 的 kernel/session 字段；无既有 kernel 和已有 kernel 的四个场景均匹配。新增 automation-cleanup、冲突目标拒绝及待恢复状态展示的 Rust 扩展检查，未把扩展计入双端匹配。

repl 的六个非 TTY 行为场景匹配，包括空白输入初始化、Python 错误退出码和图片保存；交互行为由 [PTY 测试](18-repl-and-websocket-implementation.md) 验证，尚未做双端交互差分。

install 的五项正常/拒绝场景匹配，含单引号参数的有意修复保留为差异。模板实际回退语义另由 [七项替身执行探针](../research/install-wrapper-probe.json) 验证。

## 保留的差异与扩展

剩余 54 项都在报告中保留，没有改成 matched：

| 范围 | 数量 | 保留原因 |
| --- | --- | --- |
| root/子命令 help、help new | 29 | Clap 与 Typer/Rich 的文案/排版不同，Rust 另展示账户、JSON 等扩展选项；[命令规格](01-features-and-cli.md) 允许终端布局变化 |
| install 含单引号参数 | 1 | 原版拼接源码产生 SyntaxError；Rust 用 JSON 数据传参，按命令契约修复该缺陷。保留双端观察差异，不复制无效源码 |
| version | 1 | 输出前缀已对齐为 Version，但 Rust 保留自己的包版本，不伪装官方版本 |
| new 412、sessions/status 503、exec 空闲超时 | 4 | 退出状态一致；官方显示 Python/Rich 调用栈，Rust 保留脱敏的类型化错误，不伪造 Python 栈或输出原始网络响应 |
| SSH 400/401/403/404/429/502 | 6 | 错误分类和退出码一致，Rust 用脱敏中文类型化错误，不复制原版完整文案或原始响应片段 |
| SSH proxy 唯一会话/无会话 | 2 | 原版会将选择提示或错误写进 stdout，Rust 保证字节流 stdout 纯净，提示写 stderr |
| SSH 公钥或握手失败的 --rm | 2 | 原版可能跳过清理，Rust 覆盖这些阶段并实际释放模拟资源；不复现 VM 泄漏 |
| url 唯一/缺失/歧义会话 | 3 | Rust 只读本地记录，保持单行 URL；原版可能刷新后端并输出选择提示，错误文案也不同 |
| readme/README、skill/SKILL | 4 | 内嵌原生 Rust 用法与扩展工作流，资源内容按当前实现编写 |
| whoami 正常及 tokeninfo 拒绝 | 2 | Rust 增加扩展账户提示，错误不回显原始响应 |

上述 SSH 修复的实现、原生 OpenSSH 测试和边界见 [19](19-ssh-implementation.md)。SSH proxy 的严格字节断言同时保留原版污染前缀的基线观察；相同的缺陷没有改标为兼容通过。

117 个扩展场景仍单独分类，其中包含 JSON 错误返回、多账户、kernel 查询及运行恢复。完整输出保留在 JSON，既不删掉有差异的样本，也不将扩展计为双端匹配。

此差分工具不请求真实 Google 资源。另行执行的单次 CPU、kernel 执行与图形生成见 [线上记录](15-live-cpu-validation.md)；真实 OAuth、钥匙串 ACL、配额边界、交互输入与长时会话仍待验证。

Drive 本地凭据新增 18 个扩展场景（含 root 与子命令 help），没有将它们算成与官方相同；授权及恢复约定见 [20](20-drive-credentials-implementation.md)。colab-ext 合并入口的双端差分仍需后续扩展工具，不由固定官方 CLI 的结果替代。

新增工具命令后，原有 240 个场景的分类保持不变。whoami 的 Google Credentials 与 tokeninfo 服务地址仅在测试中替换为 loopback，浏览器操作写入捕获文件，不实际打开浏览器或查询 Google。

根 drivemount 默认 DriveFS 策略新增八项扩展验证；此前 262 个场景分类不变。两个官方 drivemount 场景在 Rust 端显式添加 --upstream，报告记录策略，不能据此声称默认行为相同。共享锁与并发晚核查由 Rust 集成测试覆盖，详见 [28](28-drivemount-strategy-implementation.md)。

account status 新增 help 和六项业务场景，均按 Rust 扩展计数；此前 270 场景分类不变。JSON 业务字段、部分失败、Drive 文件存在语义与固定扩展的契约依据见 [29](29-account-status-implementation.md)。

ADC service_account 新增四项拒绝/诊断/行错误场景，均按 Rust 扩展计数，原有 277 个场景分类不变。成功路径采用临时 RSA 密钥及独立验签的 Rust 集成测试，详见 [30](30-adc-service-account-implementation.md)。没有使用真实 Google 密钥或把 loopback token 当作线上接受证据。

IAM impersonation 新增六项业务场景，均按 Rust 扩展计数，原有 281 个场景分类不变。覆盖源/目标交换顺序、目标身份用于 Colab、IAM 失败停止及 doctor 离线检查；来源轮换、RSA 与并发缓存另由 Rust 集成测试核查。详见 [31](31-adc-impersonation-implementation.md)。

原生 update 增加一个 help 差异与十个 Rust 扩展业务场景，覆盖显式/环境源、无源错误、只读状态、已是最新、坏清单、HTTP 失败、参数冲突、非原生目标拒绝与期限校验。官方 Python 更新依赖 pip/uv，不能用禁止网络的启动器替身声称其更新协议等价。真实原生产物安装与中断恢复另由 Rust 测试验证，见 [32](32-native-update-and-release-implementation.md)。

合并版独立帮助证据见 [33](33-merged-help-and-aliases-implementation.md)，不改变本报告官方 Python 端的隔离规则。新增十项隐藏账户/汇总别名的 Rust 扩展场景。

账户联合授权新增十项 Rust 扩展场景，见 [34](34-account-login-and-drive-consent-implementation.md)。原 account-add 场景为保持离线初始化目的，改为显式 --configure-only；旧场景分类不变不代表此处输入未改变。

补全与 Drive status 账户选择新增十四项 Rust 扩展场景，见 [35](35-completion-and-drive-status-implementation.md)。两类操作均不使用 Python 业务替身声称等价，补全实际 shell 加载由 Rust 集成测试另外验证。

外部身份新增六项为 Rust 扩展，原有 332 场景输入与分类不变；主体/STS/IAM 的缓存与失败证据及真实身份缺口见 [36](36-adc-external-account-implementation.md)。

Workforce 用户 ADC 新增六项 Rust 扩展，原有 338 场景输入与分类不变；Basic/轮换和 options 编码的离线证据见 [37](37-adc-workforce-user-implementation.md)。

metadata ADC 新增八项 Rust 扩展，原有 344 场景输入与分类不变；最后发现、显式文件保护、代理绕过和离线诊断见 [38](38-adc-metadata-implementation.md)。

可执行 ADC 来源增加六项 Rust 扩展，原 352 场景输入与分类保持；分词、缓存、受控取消和首次 fixture 等待失败后的重跑记录见 [39](39-adc-executable-implementation.md)。

AWS ADC 新增七项 Rust 扩展，旧 358 场景输入与分类保持；签名独立对照、IMDSv2 与原生运行证据见 [40](40-adc-aws-implementation.md)。

执行富文本接入后，完整 365 场景重新执行并通过；92 一致、54 保留差异、219 Rust 扩展，既有场景名称、命令与分类保持。新增加的 16 组上游文本投影样本单独保存在 [显示样本](../research/display-text-fixtures.json)，不计入该 365 场景，也不把三组布局差异改标为严格一致。实现及双模式 CLI/WS/Notebook 验证见 [46](46-rich-text-output-implementation.md)。

终端 PNG 与默认图片接入后，既有 365 场景再次通过，名称、命令和分类保持（92 一致、54 保留差异、219 Rust 扩展）。[图片纯函数样本](../research/image-output-fixtures.json) 单独记录 3 组传输与 4 组保存行为，不计入上述 365；另有 [release 图片差分](../research/release-image-comparison.json) 两组 matched，只覆盖非终端显式保存，不能作为图形终端视觉验收。范围与保留协议修复见 [47](47-terminal-image-output-implementation.md)。

共享 WebSocket CONNECT 连接层接入后，既有 365 场景再次通过，名称、命令和分类保持不变（92 一致、54 保留差异、219 Rust 扩展）；代理分支由 10 项独立 Rust/CLI 测试覆盖，不加入或混计为原有官方双端场景。优化版两组图片差分同时验证无代理连接没有回归。协议、环境和验收限制见 [48](48-websocket-http-proxy-implementation.md)。

SOCKS5/SOCKS5h 补充验证见 [49](49-websocket-socks-proxy-implementation.md)，协议回归与 HTTP CONNECT 合计 16 项，其中实际 CLI 分支使用同一临时 SOCKS 服务处理 REST 和 WS。此类合成协议测试独立计数，不冒充官方双端线上代理对照。

SOCKS 接入后的全量 365 场景已重新捕获，名称、命令和分类与 48 阶段相同（92 一致、54 保留差异、219 Rust 扩展）；默认 Rust 回归为 427 项。其他合并专项报告保留各阶段证据，本轮没有重新采集。

HTTPS 代理与嵌套 TLS 接入后，再次捕获全量 365 场景，名称、命令和分类保持不变（92 一致、54 保留差异、219 Rust 扩展）；默认 Rust 回归为 435 项。新增八项证书/双层 TLS 测试独立计数，见 [50](50-websocket-https-proxy-implementation.md)，不冒充官方线上代理差分。

JPEG→PNG 终端投影接入后的全量 365 场景再次通过，名称、命令和分类保持不变（92 一致、54 保留差异、219 Rust 扩展），默认 Rust 回归为 438 项。三项新增编解码回归和 16 组实际 CLI/PTY 配置独立计数，见 [51](51-jpeg-terminal-projection-implementation.md)；真实图形终端视觉验收仍未完成。

ADC 配额项目接入后的全量 365 场景再次通过，名称、命令和分类保持不变（92 一致、54 保留差异、219 Rust 扩展），默认 Rust 回归为 447 项。配额项目优先级、IAM 来源和请求隔离由九项新增合成测试及十组依赖观察补充，见 [52](52-adc-quota-project-implementation.md)，不作为真实项目权限或计费验收。

控制面来源保留与有限认证重试接入后，全量 365 场景名称、命令和分类保持不变（92 一致、54 保留差异、219 Rust 扩展），默认 Rust 全量回归为 458 项。新行为由十项续期测试和一项四组合 CLI/WS 回归补充，见 [53](53-control-credential-refresh-implementation.md)，不作为官方线上 401 或 token 失效行为的证据。

条件 stop/SSH 的隐藏 endpoint 保护接入后，全量 365 场景名称、命令和分类保持不变（92 一致、54 保留差异、219 Rust 扩展），默认 Rust 回归为 461 项。三项新 stop CLI 测试与六组 SSH 拒绝配置单独覆盖，见 [56](56-live-ssh-validation-and-guarded-cleanup.md)；没有把新的真实验证入口当成已通过的 Google SSH 证据。

合并版 Drive show/clear 输出对齐后，全量 365 场景再次通过，名称、命令和分类不变；其中原有扩展场景断言更新为顶层脱敏 JSON 和空凭据文本。新增的 12 组合并版 Drive 对照单独计数：11 组完全一致，1 组保留原版 ValueError 无诊断与 Rust 明确缺凭据提示的差异，见 [57](57-merged-drive-credentials-implementation.md)。原有 219 个 Rust 扩展场景不能据此全部改记为合并版已验证。

后续另有七组实际合并版 Drive token/--whoami loopback 契约对照通过，覆盖刷新与失败恢复、实际账户凭据来源、短期限和两个时区。轮换持久化、身份注册表及期限的有意差异分别登记，不归类为完全等价，详情仍见 57。本次没有改产品或重跑完整 CLI，原有 365 场景报告保留前一阶段构建证据。

Drive consent 自动登记与邮箱提示的九组合并版起始路由/URL 契约另见 [58](58-drive-consent-profile-implementation.md)，不与完整 CLI 的 365 场景混计。使用实际两端授权 URL 和 pending 检查 state/S256 关系，不涉及真实授权或 code 交换。

本次实现后完整 CLI 再次执行，365 场景名称/命令/分类与前一阶段一致（92 一致、54 保留差异、219 Rust 扩展），默认 Rust 回归为 466 项；三份 Drive 专项也重跑到当前无钥匙串构建，分别保留自身范围与差异，不合并成全部上游业务等价。

合并版显式会话 URL 的十六组对照中，十二组一致，包含跨账户路由、候选账户列表、计算账户邮箱缺失或为空，以及具名会话缺失。其余差异覆盖参数位置、origin/endpoint 与错误文案。专项对照单独计数。详见 [27](27-utility-commands-implementation.md)。

kernel dead/restarting Unknown 保护修复后，完整 365 场景再次运行无失败，分类统计仍为 92 一致、54 保留差异、219 Rust 扩展。广播的新增持久化与重放保护由独立 Rust 四组合回归验证，不冒充原有 365 场景已包含该分支；默认/无默认特性 467/465 项全量结果见 [广播修复回归](../research/kernel-broadcast-regression-validation.json)。完整 CLI 与 release 图片报告分别核对各自构建哈希；真实断线证据仍保留先前构建身份，范围见 [61](61-live-execution-disconnect-validation.md)。

分配拒绝专项以 `--only '^new-(412|gpu-400|tpu-400)$'` 运行。GPU 和 TPU 的 HTTP 400 用例在退出码、输出、请求摘要和会话状态上均一致。准备 GET 返回 412 时仍有 stderr 差异，Rust 提示可能原因和可选操作。历史 365 场景报告保留原有构建证据。恢复规则见 [控制面实现](10-control-plane-implementation.md)。

释放恢复专项以 `--only '^(stop-|ext-doctor)'` 运行。三个 `stop` 场景一致，另两个只在请求序列上有差异，Rust 在释放前查询目标并尝试关闭已记录的 kernel。四个诊断扩展场景的本地契约断言通过。分配日志与释放日志共存的故障恢复由 [Rust 控制面回归](../tests/control_plane.rs) 验证。

`exec` 空闲期限专项以 `--only '^exec-idle-timeout'` 运行。在静默超时场景中，官方与 Rust 均退出 1，并在 stderr 明确标出配置的秒数和调整选项。Rust 同时保留结果未知及消息 ID，官方继续打印 Python 堆栈，该用例仍分类为 `different`。持续输出与 `--timeout 0` 两项一致。详见 [超时诊断对照](../research/exec-timeout-diagnostic-comparison.json)。

## 2026-11-18 全量复验

使用当前默认特性 debug 二进制和固定原版，在隔离 loopback 上复跑 370 个场景。91 个一致，60 个有差异，219 个为 Rust 扩展，契约断言失败为零。原 365 个场景全部保留。新增五项为过期会话裁剪后的 URL、两种加速器 HTTP 400 拒绝及两种执行空闲期限。根命令与 account、drive、ext 的清单断言通过。详见 [逐项分类](../research/current-full-cli-comparison.json)。

和上一份全量记录相比，`url` 的具名缺失提示已一致，隐式选择与歧义场景的控制面请求已对齐。六个 `stop`、`ssh --rm` 和 `run` 释放场景的新增差异为请求顺序，来自释放前核对目标和关闭已记录 kernel。退出码、stdout、stderr 与释放后的状态在这六个场景中一致。报告记录每条请求及当前构建哈希。详细释放契约见 [控制面实现](10-control-plane-implementation.md)。

随后重跑五个 `stop` 场景。省略 `-s` 时，两版各查询一次 `assignments`，Rust 另发送一次 kernel DELETE，之后两版均执行 unassign GET/POST。该用例的退出码、输出和会话结果一致。另有 [Rust CLI 回归](../tests/guarded_stop.rs) 验证代理凭据、过期记录裁剪和同名 endpoint 替换。

另一份默认特性 debug 构建以 `colab` 文件名复测，370 个场景的分类和差异字段逐项一致。将同一文件改名后，仅 `run` 缺少脚本时的 stderr 出现新增差异。修正后依据 Clap 提供的缺失参数识别该场景，保留原版的 `Missing argument 'script'.` 和退出码 2。回归测试同时覆盖原文件名和改名副本，并核对命令提前退出时没有创建配置目录。[回归测试](../tests/runs.rs)

修正前的全量 Rust 测试在默认和无默认特性下分别通过 519 项和 517 项。修正后的 `runs` 集成测试在两种特性下各通过 7 项，使用版本化文件名的原版专项对照通过。修正后的默认特性 debug 二进制再次通过全部 370 项对照，每个场景的分类、差异字段与退出码保持一致。Python 工具测试通过 201 项，另有 1 项跳过。[验证记录](../research/current-full-cli-comparison.json)
