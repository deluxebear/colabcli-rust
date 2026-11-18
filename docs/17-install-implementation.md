# 远端软件包安装实现

更新日期：2026-11-16。已接入官方 `install [-r FILE] [PKG...]`，协议与字符串构造目标见 [命令契约](01-features-and-cli.md)。只在用户选择的已有 VM 上运行安装，不调用本机 pip/uv，也不隐式分配 VM。

## 输入与执行

```sh
colab install -s cpu-demo numpy pandas
colab install -s cpu-demo -r requirements.txt extra-package
colab install -s cpu-demo --timeout 120 --json numpy
# pip/uv 的选项通过 -- 之后的独立参数传递：
colab install -s cpu-demo -- --no-deps package-name
```

`-r/--requirement` 指本地普通文件，最多 32 MiB；认证前读取并检查文件、包参数和执行消息大小。空输入、空包参数、NUL 和非有限 timeout 本地拒绝。仅指定包时无需本地 Python。默认不限制等待时间，与原版 install 的 timeout=None 一致；`--timeout N` 扩展为本次相关消息的空闲期限，非正数仍不限时。

requirements 以原始字节上传到 `content/<basename>`，远端参数为 `-r /content/<basename>`，随后追加所有包参数。保留 Unicode、空格及特殊字符，不把参数拼接成 Python 语法或 shell 命令。嵌套 requirements、本地项目目录等不会递归上传；额外输入仍按远端安装器语义解释。

远端模板先调用 `['uv','pip','install','--system',...]`，普通异常时回退到 `[sys.executable,'-m','pip','install',...]`，成功通知与原版一致。捕获 Exception，避免把 KeyboardInterrupt/SystemExit 当作需要回退的安装失败；pip 再次失败继续向执行层报告错误。传参使用两层 JSON 字符串编码和 json.loads，包名中的单引号、双引号、换行或模板标记均为数据。

相较固定原版的字符串拼接，含单引号包参数不再造成 Python 语法错误。差分工具保留该有意差异：用本地 AST 判定合成源代码是否有效，再返回固定协议响应；不是执行原版安装器或真实安装验收。

## 账户、生命周期与文件恢复

安装使用所选算力账户、已有 VM 和独立 automation kernel，复用 [16](16-colab-automation-implementation.md) 的持久阶段与 automation-cleanup。不会覆盖普通 exec 的 kernel/session，也不会自动回复远端 stdin 或传播 Drive 凭据。

文件上传在该 VM 的执行租约内、kernel 创建前完成；上传失败不创建 kernel、不伪装成已执行安装。文件本身可能已经上传成功，但本地历史失败时会明确报告上传已完成。上传成功的 requirements 按原版留在 VM，不自动删除或回滚已安装软件包。

恢复指纹包含实际安装模板、参数，以及本地 requirements 的远端路径和原始字节。只有尚未执行、参数和文件内容完全相同的 ready 状态可恢复连接；改变文件或参数会拒绝推进。executing/unknown/finished 阶段不重放，也不重新上传文件。显式清理仍只删除待恢复的独立 kernel，不删除 requirements、普通 kernel 或 VM。

## 历史与凭据边界

来源为 automation:install，保存上传文件操作、automation、automation_result 与 execution 事件。安装参数可能含私有源凭据，因此本地历史用 `# install: argument values omitted` 代替生成的包装源码，不保存包参数或 requirements 正文；pending 只保存 SHA-256、阶段及资源标识。远端安装器的 stdout/stderr 仍按普通执行输出处理，这不是任意远端输出的通用脱敏机制。

## 验证与缺口

新增五项 Rust 协议/CLI 测试，覆盖参数作为数据传输、私有参数不写包装源码历史、Unicode requirements 字节往返与请求顺序、上传失败不建 kernel、文件变化后拒绝恢复、未知执行不重新上传及输入预检。测试只使用临时账户和 loopback HTTP/WS，不运行安装器。

[模板探针](../tools/probe_install_wrapper.py) 从 Rust 源码提取实际模板，用假的 subprocess 执行七项检查：uv 成功、uv 缺失、uv 普通异常、pip 失败、Ctrl-C 不回退、引号/换行/中文数据和 requirements 加包参数。结果与模板/源文件 SHA-256 见 [记录](../research/install-wrapper-probe.json)。此研究工具依赖本地 Python，Rust CLI 的运行不依赖它。

尚未在真实 Colab 上安装软件包，也未验证实际 uv/pip 版本、网络、索引权限、构建依赖和具体包的兼容性；单次 CPU 文件/执行验收不能替代这些门槛。REPL、SSH、扩展 DriveFS 和其余完整设计功能继续推进。
