# 合并 CLI 契约采集与别名对齐

## 范围和证据

此前 [CLI 差分](14-cli-comparison.md) 的 Python 端只加载官方 CLI，不包含 colab-ext；不能据此声称已经核对扩展的全部运行时入口。本阶段新增 [合并采集工具](../tools/capture_merged_help.py) 与 [固定快照](../research/merged-help.json)，同时加载固定的 CLI、KERNEL、EXT 源码，先逐文件验证固定清单哈希与 HEAD，再显式 register 扩展，不运行已安装的 .pth。

```sh
cargo build --locked
python3 tools/capture_merged_help.py \
  --cli /absolute/path/google-colab-cli \
  --ext /absolute/path/colab-ext \
  --kernel /absolute/path/jupyter-kernel-client \
  --python /absolute/path/official-venv/bin/python3 \
  --output research/merged-help.json
python3 -B tools/tests/test_merged_help.py
```

解释器必须有官方运行依赖。报告记录其实际版本、依赖版本和 Rust 二进制哈希；不宣称依赖与上游锁文件完全一致。先取得运行时命令树和参数元数据，再实际执行每个路径的 --help/-h，包括隐藏别名。Rust 端对相同命令路径执行 --help 并采集声明参数，自动发现遗漏入口。终端文案与布局允许不同，不能把 help 成功解释为业务完成。

采集覆盖 83 个命令路径（含 root、隐藏命令及别名），合并版 --help/-h 全部成功；Rust 对应 --help 全部成功。首次采集发现六个真实缺失路径；补齐后遗漏为零。没有重新生成或更新上游源码基线。

## 输入、隔离、失败与清理

父进程使用只包含必要路径/终端参数的环境，为子进程建立临时 HOME、配置目录和 cwd，不传递当前账户、ADC 或 Drive 环境。Python 子进程用 -S 启动，禁止 socket 连接/DNS、keyring 读写删除、浏览器与外部程序。后台更新检查单独禁用，其余命令注册与解析不替换；帮助过程中没有业务调用。任何禁止操作的尝试会记录并令采集失败。子进程最长 90 秒，每次 Rust help 最长 10 秒；失败不标为兼容通过。

扩展的全局回调会在临时目录 bootstrap 默认账户；退出后临时 HOME 自动清理。工具不改变真实配置、账户、VM 或仓库状态。输出只是规范化 help 与参数元数据，不采集真实凭据。参数元数据为运行时对象的声明；--help/-h 另外实际调用，避免仅依赖静态 AST 推导解析行为。

## 对齐行为

- account/acct 下新增隐藏别名 ls → list、switch → use、rm → remove，复用原处理函数与恢复保护。
- sessions、usage 新增 -A 和 --all-accounts，等同 --all；与显式 --account 的冲突规则不变。
- 别名不会改变账户添加、认证、资源释放、错误退出码或列表 JSON 格式。

一个 Rust CLI 集成测试核查 account/acct 列表、active 切换、存在会话时拒绝删除、受管计划阻止强制删除以及无事务遗留。成功删除与凭据后端清理由已有内存替身测试覆盖，真实 CLI 测试只进入凭据访问之前的拒绝路径。

CLI 差分新增十个 Rust 扩展场景，覆盖两个账户组下的三个别名以及两种汇总别名。汇总使用两个合成账户，断言结果同时包含两者，且每个账户恰好一个查询；不能把“接受参数但仍只查当前账户”作为通过。官方未加载扩展的旧报告仍单独保留，不将这些场景记为官方双端匹配。

## 仍保留的差异

快照的 options_not_shown_in_rust_help 表示参数未出现在 Rust help 的声明中，并不等于解析器一定不支持。本阶段对齐后曾有八个路径存在此类记录；后续账户参数实现见 [34](34-account-login-and-drive-consent-implementation.md)，当前数量以最新快照为准：

| 范围 | 当前状态 |
| --- | --- |
| root --config / --logtostderr | 已接受，Rust 隐藏兼容参数；不是新增缺失 |
| root --install-completion / --show-completion | 后续原生入口见 [35](35-completion-and-drive-status-implementation.md)，Rust 静态生成与显式安装 |
| ssh --colab-internal-mode | 已接受且隐藏，用于内部子进程 |
| account/acct add/login | 本阶段的缺口；后续已实现默认认证、active 切换、联合授权与这些参数，见 [34](34-account-login-and-drive-consent-implementation.md) |
| drive/dfs status --drive-account | 后续已接入该参数与本地选择展示，见 [35](35-completion-and-drive-status-implementation.md) |

该报告只证明命令树、参数声明和帮助解析；默认值、重复参数、错误解析、JSON 及业务副作用尚需合并版场景覆盖。U01 因此仅取得 help 部分证据，不能整体关闭。真实 Google、DriveFS、原生凭据库及其他平台验收仍未完成。

本机 macOS arm64 release 已重建，打包字节/清单/校验和与 version 再次通过；另外实际执行打包后 account/acct 下六个别名的 --help 均成功，见 [发行验证记录](../research/native-release-validation.json)。其他平台与远程发布仍未验证。

最终验证：默认特性 310 项、无默认特性 308 项 Rust 测试通过；两种特性 Clippy、fmt、默认构建和研究文档检查通过。完整官方 CLI 差分 308 个场景为 92 一致、54 保留差异、162 Rust 扩展，无断言失败，原有 298 场景分类不变。合并版 83 路径长短 help 均成功，Rust 无缺失命令路径；五项研究工具测试通过。两份 CLI 报告的二进制哈希与当前 debug 文件相符。

后续账户 list/current 的 16 项合并版真实离线业务对照见 [41](41-account-inventory-implementation.md)，仅补充该子集，U01 仍未整体关闭。
