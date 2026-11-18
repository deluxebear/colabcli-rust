# 终端图片与默认图片产物

## 输入与最终行为

V05.6 的图片文件保存之外，新增文本执行输出的终端 PNG 和缺省图片产物。[图片显示模块](../src/execution/images.rs) 由共用 render_output 调用；exec、交互/管道 REPL、run 与自动化入口共用这一行为。带 --output-image 的 exec/REPL 显式传递目标上下文，避免另生成默认临时图片。

MIME 优先选择非空 image/png，其次 image/jpeg。先移除 ASCII 空白并严格解码 base64；仅 stdout 为终端且 PNG 有正确签名字节时发送 Kitty 数据。每个包的 base64 payload 不超过 4096 字节，第一包指定 a=T、f=100、q=2，m=1 表示续包、m=0 表示结束。q=2 抑制终端响应，避免回应串入 REPL 输入。stdout 是管道时不发送图片控制序列；--json 不调用图片显示，不生成默认图片或路径提示。

未指定 --output-image 时，每次显示图片（包括 display 更新）在系统临时目录新建 colab-image-*.png 或 *.jpeg，权限 0600，落盘后报告 `[Image saved to: PATH]`。这些文件是保留给用户的产物，退出或 clear_output 不自动删除，最终由用户或操作系统清理；持续生成图片会持续产生文件，不能当作无限磁盘保留承诺。测试将 TMPDIR 放在自己的临时目录中，结束时只清理本测试文件。

显式 --output-image 沿用执行结束后保存最后一张图片的路径；终端可以内联 PNG，但不生成额外默认文件。原有 JSON、Notebook、历史中的 MIME 数据、图片导出及显式目标路径保持，显示投影不替换原始字节。JPEG 不冒充 PNG 发送；通过保存文件保留原始数据，没有增加 JPEG→PNG 转码依赖。

## 失败、状态与清理责任

图片 base64 解码失败、默认临时文件创建/写入/保留失败只向 stderr 报告固定错误或本地 I/O 信息，不打印图片正文，不把已经执行的代码改成 Unknown，也不重放代码。未成功保留的 NamedTempFile 由 RAII 删除。成功保留的默认产物由用户负责删除。

显式图片保存的失败仍沿用原有非零错误，发生在远端执行与状态记录之后。stdout 写入或终端传输失败沿用已有输出 I/O 失败路径，不能把它与可忽略的默认保存失败混为一谈。没有新建 VM、挂载、后台进程或账户事务。

## 上游对照与协议差异

固定基线的 [print_kitty/handle_image](https://github.com/googlecolab/google-colab-cli/blob/02d2321c38dd215b7804eebf78b61d98b856bcd3/src/colab_cli/utils.py#L64) 在非 TTY 不发送控制序列，默认保存临时图片，显式目标直接写入文件。新 [纯函数探针](../tools/probe_image_output.py) 核对 source-inventory 中的 SHA-256，只提取这两个函数，用 fake stdout 和自有 TemporaryDirectory 捕获结果；不导入 CLI，不访问凭据，禁止 socket/DNS/连接。

[固定样本](../research/image-output-fixtures.json) 包含 3 组原始 Kitty 传输和 4 组保存行为。对照数据保留官方真实输出、原始字节、后缀和观察权限；显式文件的官方权限受 umask 影响，不能把它误认为 NamedTemporaryFile 的固定 0600。Rust 继续使用既有私有文件写入方式。

根据 [Kitty 官方协议](https://sw.kovidgoyal.net/kitty/graphics-protocol/)，f=100 表示 PNG，base64 数据需按最多 4096 字节分包。固定 CLI 对大图片发送单个大包，并把 JPEG 也传给 f=100。Rust 明确保留分包、q=2、JPEG 仅落盘和非致命默认显示错误这些差异，不将传输头逐字节匹配标成兼容完成。对于合法 PNG，去除分包后传输的图片字节与上游一致。

## 验证与边界

[三项图片测试](../tests/images.rs) 验证分包边界、最后一包标志、响应抑制、原始字节往返、部分写入失败，以及固定上游样本的语义对照。[CLI 集成测试](../tests/jupyter.rs) 通过合成账户、loopback HTTP/WS 和 Unix PTY 覆盖 10 组配置：PNG 管道/终端、显式目标管道/终端、终端 JSON、JPEG 管道/终端、坏 base64、display 更新和默认保存失败。检查默认文件权限及字节、显式目标不产生默认副本、JSON 原始 MIME、stdout 控制序列以及执行结束后没有 running/execution_pending。

```sh
/path/to/official/python tools/probe_image_output.py \
  --cli /path/to/fixed/google-colab-cli \
  --output research/image-output-fixtures.json
cargo test --locked --test images
cargo test --locked --test jupyter cli_images_use_tty_only_packets_and_private_fallback_files_without_polluting_json
```

PTY 证明实际 CLI 的终端判定与协议字节，不证明真实 Kitty/WezTerm 已显示正确像素。大包与 JPEG 的部分样本是合成 wire/opaque byte fixtures，不作为图像解码证据。没有进行真实 Colab 图片、图形终端视觉、Windows 或跨平台完整验收；JPEG 终端转码仍不支持。

本机 aarch64-apple-darwin 的 release 构建及 [16 项打包/运行检查](../research/native-release-validation.json) 通过；优化产物另直接执行 [exec/repl 图片双端差分](../research/release-image-comparison.json)，2 组均 matched，产物哈希与打包校验相同。当前 release 8,770,560 字节，默认特性测试用 debug 41,422,952 字节。两组 release 差分是非终端显式文件保存，不作为 release 图形终端显示的证据。

最终默认特性 411 项、无默认特性 409 项 Rust 全量回归通过，两种特性 Clippy、fmt 通过。既有 CLI 365 场景的名称、命令及分类保持不变（92 一致、54 保留差异、219 Rust 扩展）。新图片样本 7 组独立计数，不与上述场景混计。当前 debug/完整 CLI 对照和 release/发行校验/图片差分的哈希分别核对一致；其余专项报告保留为各自阶段证据。

后续 [51](51-jpeg-terminal-projection-implementation.md) 已补 JPEG→PNG 的终端显示转换；本篇“JPEG 仅落盘/未转码”保留为本阶段历史范围。原始文件与 MIME 继续保留，真实图形终端视觉验收仍待完成。
