# 43. 根级账户选择与变更前校验

## 输入与作用域

账户清单、current、use/switch、set-drive、rename 和 remove/rm 在执行业务前校验所选全局账户：非空显式参数优先，其次是去除首尾空白的 COLAB_ACCOUNT，最后使用 active。重复的根级 --account/-a 与合并版一样取最后一个值；空根级值回退到环境或 active。显式非空参数不进行资料名空白修剪，邮箱选择仍沿用既有解析规则。

set-drive 有两个独立作用域：根级 `-a` 选当前上下文，子命令后的 `-a` 选待修改的计算账户。根级选择器必须有效，即使局部目标有效或操作使用 --default。根级有效时，局部目标可覆盖它；--default 则忽略局部目标。示例：

```sh
colab -a personal account set-drive storage -a training
```

此命令先验证 personal，再把 training 的 Drive 绑定设为 storage。此前 clap 会把子命令的全局参数向上传播，覆盖根级值，导致无效根级选择器被忽略。现在在解析前只捕获根级选择器，按照 clap 声明跳过参数值，并在首个命令或 -- 处停止。配置路径、OAuth client 参数恰好名为 account 时不会误判边界；短参数附值和长参数等号形式都可识别。此扫描只保留作用域信息，不替代 clap 参数验证，也不读配置或环境。

## 状态与失败路径

无效全局选择器以合并版 `[colab]` 文案和退出码 1 失败，在账户变更或清单输出之前终止；业务命令自己的选择器错误仍为 `[account]`。例如根级资料不存在时，不会继续重命名、删除、切换 active 或修改全局 Drive；即使新名称也无效，先报告根级错误。

正常查询及变更仍采用既有 bootstrap、锁、事务日志和清理规则。前置校验本身不新增凭据读取、浏览器、网络或远端资源，也不更改进程环境；不会承诺尚未初始化的配置目录零写入。账户上下文直接使用启动环境快照；COLAB_ACCOUNT 与 Drive 环境别名现在在启动时修剪首尾空白。

repair 仍先恢复事务，不能因错误选择器阻断恢复。account status 在本阶段尚未校验全局选择；后续 [44](44-merged-inventory-implementation.md) 已补齐校验，但仍查询全部资料；授权 add/login 和其他顶层命令继续由各自编排处理。本阶段不宣称这些入口与原版根回调完全一致，也不覆盖全部缺参数、无效类型等解析优先级。原版依据见固定 [根回调](https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/cli_hooks.py#L540) 与 [账户解析](https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/accounts.py#L790)。

## 验证

[21 组真实合并命令对照](../research/merged-account-selection.json) 包含根级/局部优先级、未知资料、错误先后、重复参数、邮箱、空值、环境空白和分隔符。每步比较输出、退出码、账户状态与文件哈希，失败时校验负载字节不变。采集复用 [变更探针](../tools/compare_merged_account_mutations.py)，仍强制无原生凭据库构建，并禁止 Python 端网络、keyring、浏览器和子进程；没有真实云资源。

```sh
cargo build --locked --no-default-features
mkdir -p target/no-keyring
cp target/debug/colab target/no-keyring/colab
python3 tools/compare_merged_account_selection.py \
  --cli /path/to/google-colab-cli --ext /path/to/colab-ext \
  --kernel /path/to/jupyter-kernel-client --python /path/to/official-python
cargo build --locked
```

[五项 Rust 集成测试](../tests/account_selection.rs) 额外验证各变更命令没有负载变化、附值参数、声明参数值的跳过，以及局部绑定确实写入目标账户。仍需后续覆盖全部合并业务与真实平台；本报告不关闭 U01。

最终验证：默认特性 392 项、无默认特性 390 项 Rust 测试通过；两种特性 Clippy、fmt 通过。工具测试 57 项中 56 通过、1 项 Linux 专属跳过。21 组选择对照、27 组既有变更对照和 16 项清单对照全部一致；原有完整 CLI 365 场景的命令输入及分类不变（92 一致、54 保留差异、219 Rust 扩展），83 个合并帮助路径无缺失。三份默认 debug 报告、两份无钥匙串报告分别匹配对应构建哈希。本机 release 的 15 项检查通过，新增“无效根级＋有效局部目标”被拒绝且注册表字节不变的打包后验证。未推送或发布；授权及其余顶层入口仍需继续对照。
