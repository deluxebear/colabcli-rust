# 执行富文本的文本显示

## 输入与行为

补齐 V05.6 与 exec 契约中的富文本转文本。此前 render_output 只读取 text/plain，HTML/Markdown 独占的结果会没有可见文本；同时存在纯文本 fallback 时，也没有遵守上游优先级。

现在 display_data、execute_result 以及 update_display_data 的文本投影依次选择 text/markdown、text/html、text/plain。Markdown 使用 CommonMark 解析（启用表格和删除线），再与 HTML 共用本地 HTML 文本转换；输出宽度为 80 列，不添加 ANSI 配色。保留段落、代码和列表/表格文字，标题、列表符号、表格边框不要求复刻 Python Rich 的终端排版。原有 stream 的 stdout/stderr 分流和错误 traceback 路径保持。

实现分为 [文本投影](../src/execution/display.rs) 和既有 render_output 写入入口；exec、REPL、run 及自动化的共用输出钩子均采用同一转换。MIME 字符串数组按顺序拼接；坏类型跳过并选择有效的低优先级表示，未知 MIME 无文本输出。

输入是远端输出数据，不是网页地址；不执行脚本、样式或抓取链接/图片。HTML 实体由解析器解码。对超过 1 MiB 的富文本输入或转换失败，优先显示 text/plain，否则保留原始富文本源码，避免显示失败将已经执行的代码变成“结果未知”。这个阈值限制 DOM 转换，不截断保存的原始输出；累计输出仍受既有 32 MiB 限制。富文本转换不新增远端资源、文件或清理责任。

## 原始结果与边界

转换只用于文本 stdout。--json 的输出、Notebook 的 data/metadata、历史输出、display_id 更新后的最终 MIME bundle 和图片保存继续使用原始数据，不把 HTML 或 Markdown 替换成投影文字。Notebook 文本模式继续按既有兼容保存规则将文本字符串拆为行数组，拼接后与原始文本一致；JSON 扩展模式保持原字符串。显示更新可以追加可见文本，但不会重放远端代码。

本阶段没有实现 Kitty 终端图片，后续接入见 [47](47-terminal-image-output-implementation.md)；语法高亮或完整浏览器/CSS 布局仍未实现；已有 --output-image 文件保存仍是独立路径。不能据此认定 V05 所有终端交互或真实 Colab 富文本验收已经完成。

## 固定上游对照

使用固定 CLI 的 [render_display_data](https://github.com/googlecolab/google-colab-cli/blob/02d2321c38dd215b7804eebf78b61d98b856bcd3/src/colab_cli/utils.py#L87) 作为文本 MIME 优先级依据。纯函数探针 [probe_display_text.py](../tools/probe_display_text.py) 在读取后核对 source-inventory 中的文件 SHA-256，只提取该函数，使用实际 Rich Console 与 Python html2text，不导入 CLI 或认证模块，并禁止 socket 创建、DNS 与连接。

[16 组固定样本](../research/display-text-fixtures.json) 包含纯文本、Unicode、Markdown、HTML、MIME 优先级、链接、实体、代码、标题、列表、表格、script/style 不可见内容、换行、纯图片和空 Markdown。13 组在规范化排版空白后文本一致；标题/列表/表格三组明确保留布局差异，分别检查文字内容且禁止遗留 HTML 标签，不改标成严格匹配。样本记录实际依赖版本、固定源码和原始官方显示文本。

```sh
/path/to/official/python tools/probe_display_text.py \
  --cli /path/to/fixed/google-colab-cli \
  --output research/display-text-fixtures.json
cargo test --locked --test display
cargo test --locked --test jupyter cli_rich_mime_streaming_and_updates_render_text_but_json_and_notebooks_keep_original_bundles
```

[三项投影测试](../tests/display.rs) 覆盖上述 16 组实际上游样本、多行/坏类型和超限降级。另在 [Jupyter 集成测试](../tests/jupyter.rs) 使用临时账户、合成 token、真实 loopback HTTP/WS 与 CLI 子进程，分别验证文本和 JSON 模式；断言流式富文本优先级、display 更新、stderr 分流，以及输出 Notebook 保留原始 MIME 和 metadata。没有使用真实账户或新建 Google VM。文本模式的既有 Notebook 进度行仍保留；测试分别严格检查文本模式行数组和 JSON 模式字符串的保存约定。

新增 Rust 依赖为 html2text 0.17.4 和 pulldown-cmark 0.13.6，关闭非必要默认功能；版本与间接依赖已锁定。这是 Rust 库实现，不调用 Python 渲染；Python 仅用于生成上游对照样本。库接口参考 [html2text Config](https://docs.rs/html2text/latest/html2text/config/struct.Config.html) 和 [pulldown-cmark Event](https://docs.rs/pulldown-cmark/latest/pulldown_cmark/enum.Event.html)。

本机 aarch64-apple-darwin 的 release 构建及 [16 项打包/运行检查](../research/native-release-validation.json) 通过；release 8,737,456 字节，debug 41,393,896 字节。相比上一份已记录的 7,568,176 字节 release，增加 1,169,280 字节（约 1.12 MiB）；期间包含每日检查与富文本变更，不能把总增量精确归因于解析库。该测量不是四平台体积承诺。

最终默认特性 407 项、无默认特性 405 项 Rust 回归通过，两种特性 Clippy、格式检查通过。既有 365 组 CLI 差分再次通过，名称、命令和分类均与此前一致（92 一致、54 保留差异、219 Rust 扩展）。新增 16 组纯函数显示样本独立计数；其余合并版专项报告未重新捕获，不宣称当前所有报告二进制哈希相同。

最终默认 debug 与 CLI 对照记录、release 与发行校验记录的 SHA-256 分别核对一致。debug 体积取本轮差分使用的默认特性测试构建产物；普通 cargo build 与 cargo test 的开发依赖特性合并可能产生不同字节，因此不把跨构建命令的 debug 哈希相同当成通用保证。
