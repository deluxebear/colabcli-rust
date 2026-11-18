# 外部身份可执行令牌来源

## 配置、权限与入口

external_account 的 credential_source.executable 现在支持 Unix 非交互来源，复用现有 STS 和可选 IAM 交换。CLI 必须显式设置 GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES=1；Environment 在启动快照该值，AccountContext/provider 逐层传递，不修改父进程环境。未启用时连输出缓存也不读取。AdcProvider::new(path, client) 保持默认禁止执行；库调用者可通过显式 AccountContext 开启。实现见 [executable.rs](../src/auth/adc/executable.rs)。

协议参考 [AIP-4117](https://google.aip.dev/auth/4117) 和 [google-auth 非交互实现](https://github.com/googleapis/google-auth-library-python/blob/main/google/auth/pluggable.py)。command 的程序必须为绝对路径，可包含引号参数；不隐式使用 shell。timeout_millis 默认 30000，范围 5000–120000；output_file 若提供必须为绝对路径。拒绝混用 file/url/format、AWS environment_id 或 certificate。交互超时选项、交互授权和 revoke 尚未支持；Windows 可执行来源在执行前明确拒绝。

命令按 Python shlex.split 的 posix=True、comments=False 规则拆分，保留 #、空参数、Unicode 和转义细节；不展开变量、通配符、重定向或 shell 操作符。若配置明确调用 /bin/sh -c，该脚本行为属于来源程序。无新增本地 Python 依赖。独立 [标准库探针](../tools/probe_executable_command.py) 只拆分字符串、不执行命令；[21 项记录](../research/executable-command-probe.json) 由 Rust 单元测试逐项核对。可用固定 CLI 虚拟环境复跑：

```sh
/path/to/google-colab-cli-venv/bin/python3 -I tools/probe_executable_command.py --output research/executable-command-probe.json
```

## 子进程与响应

子进程继承一般环境，显式设置 AUDIENCE、TOKEN_TYPE、INTERACTIVE=0、REVOKE=0、ALLOW_EXECUTABLES=1，以及 ID（有 IAM 目标时为已校验的目标标识，否则为空）。IMPERSONATED_EMAIL 和 OUTPUT_FILE 只在相应配置存在时注入，先移除父环境中这两个变量的旧值；上述名称均带 GOOGLE_EXTERNAL_ACCOUNT_ 前缀。不会把本程序新取得的 Google bearer/token 写入命令参数或提示变量。

stdin 为 null；stderr 丢弃，不混入 JSON 或回显。stdout 使用非阻塞管道、最多 64 KiB，期限内等待程序退出并验证零退出码。每次创建独立 Unix 进程组。超时、输出过大、取消或其他错误都会终止组并回收直接子进程；主程序成功退出后也清理组内剩余后代再读完管道。使用 waitid WNOWAIT 保留 leader PID，清理后才 wait，避免先回收 leader 再凭旧 PID 杀组。

SIGINT/SIGTERM/SIGHUP 受控取消，作用域结束移除信号注册。启用可执行来源时，账户状态和跨账户汇总将取消标记放入认证上下文：不把取消吞为普通行错误，不在取消后启动排队插件，最终按 128+signal 退出。进行中的其他 HTTP 请求仍遵循各自期限。SIGKILL、进程崩溃以及后代自行脱离进程组不受 Rust 清理保证；这是受信任本地代码执行能力，不是任意程序沙箱。执行期限也不覆盖所有文件 I/O 或 OS 回收耗时。

响应为版本 1 的 JSON，success 必须为 bool。成功输出支持 OIDC 的 id_token 或 SAML 的 saml_response，保持主体字节；token_type 必须与配置属同一 OIDC 类型族或同为 SAML，不本地校验签名/claims。expiration_time 若提供须为有效且未过期的整数 Unix 秒；校验时在调用方给定的开始时刻加上单调执行耗时，避免接受运行期间已经过期的令牌；配置 output_file 时新输出必须提供它。不成功输出要求 code/message，但错误只返回静态原因，绝不回显这些动态内容、命令、stderr、主体或响应原文。

## 缓存、失败与清理责任

输出文件先以非阻塞方式打开、检查为普通文件，最多 64 KiB；允许来源管理的 symlink。有效且未过期的缓存跳过执行；过期或缺 expiration_time 的成功缓存视为需要重新执行。文件不存在也执行程序。其他畸形、错误、超大或不可读缓存直接失败，不以执行覆盖它。与某些 google-auth 版本忽略无法解析 JSON 后执行的行为不同，此处采用 AIP 的无效缓存拒绝规则。

程序 stdout 为空且配置了 output_file 时，读取程序写出的文件；否则解析 stdout。Rust 不创建、覆盖或删除输出缓存，文件权限和更新由来源程序负责。程序失败、取消时已写出的文件不回滚；后续调用可能采用有效缓存，或因残缺缓存拒绝。来源程序或用户负责修复其输出文件；不同来源应使用独立缓存路径。

最终 token 和 STS 缓存仍按 provider 隔离。主体获取失败不会覆盖有效旧 token；显式刷新失败仍报错，后续普通调用可使用仍有效旧缓存。此功能不保存 Google token 到钥匙串，不回写 ADC，不创建云端资源。子进程可能按用户配置产生自己的副作用，超出本程序缓存与云资源管理范围。

doctor 只检查基本配置和 opt-in 快照，报告 external_account_executable_configured、executable_policy=enabled/disabled，online_validity=not_checked；禁用时增加 executable_adc_disabled issue。诊断不执行程序、不读取输出文件、不验证命令实际可运行。

## 验证与缺口

[13 项子进程测试](../tests/adc_executable.rs) 使用本地 /bin/sh、cat、临时文件和 loopback，覆盖 opt-in（含缓存）、引用参数、环境提示/旧值覆盖、stdout/stderr、文件缓存/过期/畸形、程序写文件后空 stdout、错误/过期/超大输出、超时回收、成功退出后组内后代清理、账户隔离、SAML、真实 CLI 的 IAM 交换与取消。取消用例分别运行 login、五账户 account status 和跨账户 sessions，验证并发四个插件退出且第五个不启动。另有上述 21 项 Python 分词参考的单元测试。

CLI 差分增加六项 Rust 扩展：login、sessions、whoami、IAM account status、禁用和 doctor，检查程序 marker、HTTP 顺序和脱敏。旧 352 场景输入与分类须保持；旧 adc-external-unsupported 名称保留，实际覆盖的是不完整可执行配置拒绝。没有运行用户的真实 IdP 程序、读取真实凭据或访问真实 STS/Colab。交互/revoke、AWS/证书来源、跨平台及线上验收仍有缺口，完整设计目标尚未完成。

无默认特性首次全量回归曾出现 loopback 在 5 秒内未收到请求的失败；单独用例和整组重跑均通过（该用例单独约 0.13 秒）。该等待同时覆盖插件启动，现将测试服务等待调为 15 秒；来源的配置执行期限和超时/取消断言继续独立验证。后续完整矩阵以修订后的测试为准，不将首次失败记为通过。

最终验证：默认特性 367 项、无默认特性 365 项 Rust 测试通过；完整 CLI 差分 358 个场景为 92 一致、54 保留差异、212 Rust 扩展，零断言失败，旧 352 场景输入与分类不变；业务场景 294 个。修订后的无默认特性全量复跑通过，不以首次失败计通过。研究工具 55 项中 54 通过、1 项 Linux 专属跳过；另有 21 项 Python 标准库分词观察由 Rust 单元测试逐一核对。

[本机发行校验脚本](../tools/validate_native_release.py) 对打包后的优化二进制另跑 opt-in 拒绝、完整插件→STS 登录与执行途中令牌过期拒绝，使用清空环境和 loopback；结果写入 [发行记录](../research/native-release-validation.json)。候选工作流已配置每个本机 runner 执行并上传验证报告，尚未远程运行。可在 cargo build --release --locked 后执行 python3 tools/validate_native_release.py 复现本机检查。

最终边界检查新增“插件运行期间令牌过期”的真实慢进程用例：修复前成功复现错误，修复后要求在 STS 之前以过期错误拒绝，并检查没有请求到达。该修订后重新执行完整矩阵，不以此前 366 项结果替代最终工作树证据。

过期修复后的最终工作树验证：默认 367 项、无默认特性 365 项测试通过；两种特性 Clippy、fmt、Python 语法和研究工具检查通过。358 项完整差分分类保持不变，旧 352 项输入/分类保持。83 路径合并 help 无缺失；两份报告哈希与当前 debug 一致。本机 release 的 11 项打包/运行检查通过，含过期执行拒绝；[修复前后记录](../research/executable-expiry-validation.json) 包含不同二进制哈希和请求次数。未推送或发布，真实 IdP 和跨平台仍待验收。

后续已接入 AWS aws1 的环境/EC2 与 SigV4 主体，见 [40](40-adc-aws-implementation.md)；并未验证真实云端接受性。
