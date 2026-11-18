# Shell 补全与 Drive 状态账户选择

## 入口与输入

合并版运行时帮助暴露的 shell completion 和 drive status --drive-account 已接入。补全定义位于 [completion_cli.rs](../src/completion_cli.rs)，运行解析、help 和脚本生成共用同一份 Clap 命令树；根入口允许没有业务子命令的补全操作，进入账户初始化之前完成并退出。

```sh
colab --show-completion bash
colab --show-completion zsh > /tmp/_colab
colab --install-completion bash
colab --install-completion zsh
colab --install-completion fish
colab drive status -s work --drive-account storage --json
colab dfs status -s work --drive-account storage
```

show 支持 bash/zsh/fish/powershell/pwsh，后两个生成相同的 PowerShell 脚本。不指定 shell（或 auto）时读取 SHELL 的文件名，不运行外部程序探测父进程；无法识别则静态错误，需显式指定。show 不需要 HOME，不读账户/凭据/会话，不发网络请求，不创建配置。--show-completion 与 --install-completion 互斥；--help 仍优先显示帮助，不安装。

脚本使用 [clap_complete 的静态生成接口](https://docs.rs/clap_complete/latest/clap_complete/aot/fn.generate.html)，锁定依赖见 [Cargo.lock](../Cargo.lock)。补全来自已安装二进制的命令定义，不执行 colab 查询账户或 VM，也不嵌入环境中的凭据值。升级 CLI 后需要重新安装脚本才能获得新增命令。没有动态查询 Google 资源或 profile 名称。

固定 Typer 环境的入口为自动检测 shell 的 eager bool 开关；Rust 同时允许显式 shell 值。脚本内容使用原生静态生成器，不复制 Python 动态补全协议。安装当前只支持 Unix 的 Bash/Zsh/Fish；PowerShell 可显示脚本后手动配置，Windows 自动安装尚未实现。

## 安装状态与清理责任

只有显式 install 写入当前 HOME，HOME 必须是现有绝对路径且由当前用户拥有。不会 chmod HOME 或已有父目录。新目录为私有目录，新文件默认私有权限，已有文件保留其权限。

| shell | 脚本 | 启动配置 |
| --- | --- | --- |
| Bash | ~/.bash_completions/colab.sh | 向 ~/.bashrc 追加带边界标记的 source 块 |
| Zsh | ~/.zfunc/_colab | 向 ~/.zshrc 追加 fpath/compinit 块，不设置 zstyle |
| Fish | ~/.config/fish/completions/colab.fish | 使用标准自动加载路径，不修改 config.fish |

同一目标持有 .colab-completion.lock，最多等待 5 秒，多个本程序安装器不会重复追加块。路径组件、脚本、rc 和锁拒绝 symlink，文件拒绝硬链接、非当前用户所有或组/其他用户可写；不会顺着 dotfiles 链接覆盖其他位置。可用 show 后手动配置此类环境。

旧文件最多读 1 MiB；rc 必须为 UTF-8，遇到不完整或不同的受管块拒绝覆盖。写入使用同目录临时文件、同步和原子替换，提交前重新检查已观察到的文件内容，发现变化即停止。外部编辑器不参与安装锁，不能将检查写成对任意外部并发修改的原子 CAS 保证。

预检 rc 后先写脚本，再写 rc；两者不是跨文件事务。若中断或第二步失败，可能只更新了脚本，重新运行同一安装操作可继续；强制中断可能遗留受限临时文件、空目录或锁文件，不自动删除这些未知归属文件。配置块和脚本均可供用户审阅，安装器不执行用户 rc。show 不具有这些写入副作用。

## Drive 状态账户选择

Status 接受 --drive-account，dfs 别名相同。选择沿用现有优先级：显式参数、Drive 环境账户、计算账户绑定、本账户已有 Drive 凭据、全局默认绑定、计算账户。无效选择在认证和分配 probe 之前拒绝。

计算身份始终来自所选会话所属账户，Drive 选择只用于本地上下文展示，不获取该存储账户的 OAuth/ADC，不读取 dfs_credentials.json 内容，不触发授权/刷新。即使该文件内容损坏，仍可只读检查已有 VM。原有独立 probe kernel、未知结果保护和清理责任保持不变。

既有 JSON 结果保留远端字段，新增 account、drive_account、local_credentials_present。前两者是本地所选计算/存储 profile，presence 只表示路径存在，不说明凭据有效，也不证明远端挂载实际属于该 profile。远端 identity/mounted/helper 等字段仍由原探针给出。无法检查本地路径时返回文件错误，不将未知状态当作不存在。

## 验证与边界

八项 Rust 测试只使用临时 HOME，覆盖五种显示格式和自动选择、不依赖 HOME/账户、不泄露合成环境凭据、安装幂等/权限、特殊字符路径、并发安装、symlink/hardlink/可写 rc 拒绝、超大/非 UTF-8/残缺块拒绝，以及参数冲突/help 不安装。实际 Bash 候选函数返回 account；本机实际 Zsh 语法检查和自动加载通过。Zsh 不存在时该分支不执行，Fish/PowerShell 未实际运行，不能声称跨 shell 全部验收。

CLI 差分增加九个补全扩展场景和五个 Drive 状态账户场景。后者将存储账户配置为不可认证的 ADC，并放置不能解析的合成 Drive 文件，仍用计算身份成功检查 VM；验证了显式/环境选择、优先级、别名、仅看文件存在及错误选择不发请求。均为 loopback 模拟；真实 VM 的新展示路径尚待验收。

合并帮助快照仍采集 83 个路径；当前公开声明选项均已提供对应入口，剩余未展示记录仅为已有隐藏的 --config/--logtostderr 和 --colab-internal-mode。整体合并版业务、错误/JSON、其他 ADC、真实 Google/Drive 及跨平台验收仍未完成。

最终验证：默认特性 326 项、无默认特性 324 项 Rust 测试通过；两种特性 Clippy、fmt、默认构建和研究文档检查通过。完整 CLI 差分 332 个场景为 92 一致、54 保留差异、186 Rust 扩展，无断言失败，原有 318 场景的输入与分类不变。合并版 83 路径长短 help 均成功，剩余未展示项仅为已有隐藏参数；两份报告哈希与当前 debug 二进制相符。release 重建、打包校验和、version 和五种补全输出通过；真实平台/Google 验收仍未完成。
