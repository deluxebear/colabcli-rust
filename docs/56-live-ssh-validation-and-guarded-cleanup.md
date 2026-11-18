# 真实 SSH 验证入口与条件清理

本章最初交付真实 CPU/SSH 验证脚本及其资源保护；当时文件凭据后端返回“需要登录或重新授权”，没有创建 VM。历史失败预检查见 [live-ssh-preflight.json](../research/live-ssh-preflight.json)。后续用户明确选择现有系统钥匙串，五项真实 SSH 检查已通过并确认资源释放，见本章末节；两种凭据来源的结果分别保存。

## 条件保护

[命令入口](../src/main.rs) 将已有 SessionService::stop_if_endpoint 暴露给内部清理调用：stop -s NAME --colab-expected-endpoint EXPECTED。该参数隐藏、非空且必须同时指定 -s；不改变普通 stop 的 help 或缺失会话兼容输出。guarded stop 在会话缺失时明确失败，在实际账户锁内比较已保存释放事务或本地会话的 endpoint。错配时不发送释放请求，也不改日志；恢复旧 endpoint 时不会删除同名替代 VM 的状态。

SSH 的既有 endpoint 保护也允许用于命令/交互父入口，要求显式 -s；保护在建连和启动 OpenSSH 之前检查。父命令生成的 ProxyCommand 继续携带捕获的 endpoint，再次阻止并发替换后的连接。proxy、command 和显式交互三条路径在名称缺失或 endpoint 被替换时都拒绝，不自动创建替代 VM。公开默认行为及原版兼容命令不变，隐藏参数用于测试和拥有资源的内部调用。

[三项 CLI 回归](../tests/guarded_stop.rs) 覆盖错配后文件/日志不变且无网络、精确释放和账户隔离、缺失/空参数在认证前拒绝、普通缺失 stop 仍保持退出 0，以及已有释放事务优先核对且不释放替代 VM。既有 [SSH 拒绝路径测试](../tests/ssh.rs) 扩展为两种状态 × 三种模式，共六组配置。

## 真实测试的输入和状态

[开发验证脚本](../tools/validate_live_ssh.py) 默认只预检查，只有 --execute 才分配一台 CPU VM。它不是产品运行时依赖，不改变 Rust CLI 无本地 Python 的边界。

```sh
python3 tools/validate_live_ssh.py --state-dir /tmp/new-private-ssh-preflight
python3 tools/validate_live_ssh.py --state-dir /tmp/new-private-ssh-test --execute
python3 tools/validate_live_ssh.py --state-dir /tmp/new-private-ssh-test --cleanup-only
```

若只为本次测试完成计算账户文件登录，可先执行 ./target/debug/colab --token-storage file account login --no-drive，避免启动独立 Drive 授权。

可显式指定 --config-dir、--account、--token-storage。默认 file 不读系统钥匙串；auto/keyring 仅在用户明确选用该认证来源时调用。脚本冻结实际配置根、账户和 provider，后续命令均显式传入，不依赖变化中的 active account 或账户选择环境变量。它不会登录、导出或复制用户凭据。

state-dir 必须位于 CLI 配置目录外，新目录 0700；ledger 与 report 原子保存为 0600。private ledger 包含账户、配置路径、基线 endpoint 集合、本次唯一名称和捕获的 endpoint，不含 token。该文件留在本地供恢复，不能将其原样提交。report 仅保留二进制哈希、测试标签、退出码、计数、错误分类和清理状态。

预检查拒绝已有控制面事务，不处理不属于本次测试的 pending。sessions --json 即使进程退出 0，也必须检查单账户 error、账户一致性和 assignments 类型；不能因合并 JSON 的包装退出码而误以为认证成功。新分配必须名称匹配且 endpoint 不在基线内；确认本次所有权后立即写入私有账本。若服务端意外返回非 CPU 资源，先记录拥有关系用于清理，再拒绝执行，不留下无法清理的意外 VM。

## SSH 检查与本地进程责任

只生成本次临时 ed25519 密钥，不读取用户私钥或 agent。PATH 中的临时 ssh 包装器调用 /usr/bin/ssh -F /dev/null，启用 BatchMode、IdentitiesOnly 和连接期限，隔离用户 SSH 配置及密码输入。所有 SSH 入口都传入捕获的 endpoint，raw proxy 也不能因本地名称消失而重新分配。

计划执行五项真实检查：raw proxy 的 SSH banner/EOF；root 身份与 /content 工作目录；带空格/引号/反斜线/Unicode/类似选项的 argv；1024 字节二进制 stdin/stdout；远端退出码 23 的传播。探针不挂载 Drive，不写现有用户目录，不安装服务端软件，不重放远端命令。

本地调用独占新的 Unix process group；超时先 SIGTERM，再在期限内 SIGKILL/等待，仅处理自身拥有的组。banner 探针保持 stdin 开启直到读取 banner，然后以 EOF 结束连接。任何异常都会进入 VM 清理；临时私钥目录删除，私有 ledger 保留。

## 失败与清理

清理只能使用账本中已证明拥有且不属于基线的 endpoint，始终调用 guarded stop。随后重新获取后端 assignments，以该 endpoint 确认消失为成功依据；本地记录消失或 stop 返回 0 均不足以证明释放。并发产生的其他 VM 只计数并记录在私有账本中，不自动停止。

若 new 返回结果未知且仍有本次名称的 allocate 事务，最多用相同参数重试一次，沿用事务中的 notebook identity 调和，然后只清理找回的资源。没有可核实的 endpoint 时不猜测、不按别的同名状态释放，也不删事务。无法确认释放则退出失败，输出账户、已知 endpoint 和私有 ledger 路径；--cleanup-only 仅调和/清理，不重放 SSH 或用户代码。

[八项脚本安全回归](../tools/tests/test_live_ssh_validation.py) 使用合成结果和本地拥有的进程，验证 JSON 包装错误、账户错配、条件清理与后端确认、基线/未知资源拒绝、分配身份和意外硬件、0600 原子账本，以及超时后父子进程组回收。它们不使用真实凭据或 VM。

当前线上结果仅为预检查失败、created_sessions=0、cleanup=not_allocated、checks=[]；V06 的真实认证与互操作仍未关闭。需要可用文件登录、明确允许的现有系统凭据来源或其他已授权配置目录后，才能运行 --execute。

本机 release 的 [16 项发行检查](../research/native-release-validation.json) 与 [两组非 TTY 图片对照](../research/release-image-comparison.json) 通过，未增加优化版 endpoint 条件保护的专项测试。其他合并报告及 Linux/Secret Service 结果保留对应阶段的历史快照，本轮未重新执行这些专项；没有推送或发布。

最终默认特性全量 461 项、无默认特性全量 459 项 Rust 测试通过；两种特性的 Clippy 与格式检查通过。原有 365 个 CLI 场景名称、命令和分类保持不变（92 一致、54 保留差异、219 Rust 扩展）；Python 工具 67 项中 66 项通过、1 项 Linux 专属在 macOS 跳过。本机 release 为 9,140,576 字节，默认测试用 debug 为 44,658,168 字节。研究文档检查覆盖 61 篇 Markdown、687 个相对链接、127 个固定源码引用和 143 个固定源码哈希，无错误。

## 后续：完整脚本流程回归

补齐六项真实子进程流程测试，使用临时合成 CLI 模型记录调用与分配状态，不导入产品、不打开网络，也不访问用户配置。覆盖成功预检查、退出 0 的账户错误阻止分配、五项 SSH 检查完成后确认释放、握手失败后清理、分配结果未知时以相同参数调和后清理，以及首次 stop 失败后通过 --cleanup-only 恢复。实际执行本机 ssh-keygen 生成临时测试密钥，逐流程检查密钥目录已删除、报告权限为 0600；成功流程的 SSH 服务行为为合成响应，不能替代真实服务器。

流程测试发现预检查成功时报告仍为 passed=false，现已修复：passed=true 与退出 0 一致，preflight_only=true、checks=[] 和 created_sessions=0 明确限定范围。另补一项畸形 assignments 行验证，非对象行按受控错误拒绝，不再产生 AttributeError。认证失败的既有线上预检查报告仍保留为失败，未改写为通过。

目前该脚本共 15 项测试通过，全部 Python 工具测试共 74 项，其中 73 项通过、1 项 Linux 专属跳过。记录见 [脚本流程验证](../research/live-ssh-harness-validation.json)。本次仅修改开发脚本、测试和文档，没有重编译或重跑 Rust 全量、官方差分及发行检查；这些报告继续代表上一阶段的产品构建。真实 SSH 仍等待可用认证，未创建云资源。

## 后续：现有钥匙串的真实 CPU/SSH 验收

2026-11-18（Asia/Shanghai），用户明确选择使用现有系统钥匙串。只读 [钥匙串预检查](../research/live-ssh-keyring-preflight.json) 成功，基线 assignments=0，无待恢复控制面操作。随后使用当前默认 debug（二进制哈希与完整 CLI 报告一致），显式 keyring 后端创建一台 NONE CPU VM；私有账本立即记录拥有关系，所有 SSH/stop 调用绑定捕获的 endpoint。

[真实 SSH 结果](../research/live-ssh-validation.json) 五项全部通过：raw WebSocket SSH banner 与 stdin EOF；原生 OpenSSH 的 root uid=0、cwd=/content 及随机标记；空格/单引号/双引号/反斜线/Unicode/类似选项的 argv；1024 字节二进制 stdin/stdout 原样往返；远端退出码 23 原样返回。使用临时 ed25519 密钥和隔离 SSH 配置，无用户私钥/agent、Drive 或远端软件安装。

finally 按账本 endpoint 执行条件 stop，随后查询后端 assignments，确认本次 endpoint 已消失，cleanup=confirmed_released，其他新增会话为 0。临时私钥目录删除已核查；私有账户/endpoint 账本留在宿主临时目录供恢复追溯，不提交。脱敏报告没有真实账户、endpoint、token 或运行时服务 URL。

本次不是新的 OAuth 登录，也没有测试交互 PTY shell、--rm 自动分配、真实信号故障、Linux/Intel/Windows 到 Colab 的 SSH，不能关闭完整 V06/U10。没有修改产品或重新编译，既有离线/发行报告仍匹配本次使用的构建。文件后端缺登录的历史失败仍成立，不把系统钥匙串成功改写成所有凭据后端成功。

## 后续：交互与信号的扩展验证入口

验证脚本新增显式 `--execute --extended`，在同一台专属 CPU 上先执行原五项检查，再执行真实交互 shell 与命令模式 SIGTERM：

```sh
python3 tools/validate_live_ssh.py --token-storage keyring --state-dir /tmp/new-private-ssh-extended --execute --extended
```

`--extended` 不允许单独预检查或搭配 cleanup-only。交互探针创建本地 PTY，三个流均接入同一个 slave，调用没有 CMD 的 SSH 入口；从登录 shell 执行合成 Python，检查远端 fd 0/1/2 均是 TTY、存在 SSH_TTY 且 cwd=/content，随后 exit。随机标记必须单独出现在远端结果行首；终端回显整条输入命令不能作为执行成功。读取有期限和 1 MiB 上限，兼容 Linux PTY 的 EIO EOF，失败回收所拥有进程并进入 VM finally 清理。

命令信号探针运行打印随机标记并 sleep 的远端脚本；只有收到精确的标记行后，才向 CLI 主进程发送 SIGTERM，要求 15 秒内退出 143。它不向其他进程组发送测试信号，也不声称远端脚本已立即停止；本轮最终由 VM 释放结束剩余工作。超时回收仍仅限本探针的进程组。

已有命令检查失败时，原始 stdout/stderr 字节以 base64 写入 state-dir 下 0600 的 failed-check.json，不进入公开报告，避免服务器诊断带出真实路由信息。公开结果仍保留失败检查名和退出码。新增五项安全回归覆盖 PTY、回显误报、标记/信号时序、缺标记拒绝，以及失败诊断权限和清理；完整工具测试 79 项中 78 项通过、1 项 Linux 专属在 macOS 跳过，见 [扩展脚本回归](../research/live-ssh-extended-harness-validation.json)。这些本地合成结果不替代真实 SSH。

第一次扩展实测在原有 binary_stdio 检查返回 255，交互和信号检查尚未执行；没有保存该检查的 stderr，原因未定位，不能推断为产品或服务端故障。该次 VM 已经独立查询后端确认释放，其他新增会话为 0，见 [首次尝试](../research/live-ssh-extended-first-attempt.json)。历史五项成功记录保留，不将失败覆盖为通过。

第二次独立尝试五项基础检查通过，但交互探针没有识别到预期结果，尚未进入信号检查；原始终端字节当时未保存，不能确认原因，见 [第二次尝试](../research/live-ssh-extended-second-attempt.json)。该次资源也已确认释放。之后增加 0600 的 interactive-check.json，仅私有保存终端原始字节和进程退出码，以便区分回显、控制序列、shell 行为与实际失败；本地回归验证其权限与失败路径。

第三次独立尝试 [七项全部通过](../research/live-ssh-extended-validation.json)：基础五项、无 CMD 的登录 shell（远端 fd 0/1/2 均为 TTY、SSH_TTY 非空、cwd=/content），以及实际收到远端标记后仅向 CLI 发送 SIGTERM，退出 143。后端再次确认本次 endpoint 已消失，其他新增 assignment 为 0，临时密钥目录已删除。当前 Rust 二进制未修改，哈希继续匹配原 CLI 对照；验证脚本哈希单独记录。

三次尝试顺序创建三台专属 CPU，均完成后端确认释放。成功结果不覆盖前两次失败，也不能解释其原因或证明长期连接稳定性；本次只建立单次真实 PTY 与命令 SIGTERM 的正向证据。仍缺交互 Ctrl-C/窗口变化、其他信号、proxy --rm 自动创建/释放、真实故障清理恢复、长连接和其他平台，不关闭完整 V06/U10。
