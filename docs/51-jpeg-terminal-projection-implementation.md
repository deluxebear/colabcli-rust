# JPEG 终端显示转换

## 输入与显示

[V05.6](06-validation-and-open-questions.md) 和 [运行时输出契约](03-runtime-and-wire-protocols.md) 要求显示 PNG/JPEG。此前 [47](47-terminal-image-output-implementation.md) 仅内联 PNG，JPEG 只保留文件。本阶段在 [图片显示模块](../src/execution/images.rs) 增加 JPEG→PNG 投影，仍使用已有 Kitty f=100、q=2 和最多 4096 字节 base64 分包；f=100 按 [Kitty 协议](https://sw.kovidgoyal.net/kitty/graphics-protocol/) 表示 PNG，不能把 JPEG 原始字节直接标成 PNG。

MIME 优先级保持非空 image/png、其次 image/jpeg。只有 stdout 为 TTY 且文本输出钩子显示 JPEG 时才转码；管道不转码，JSON 不调用图片渲染。JPEG 转码前先按原有默认/显式产物约定保留原始数据；不会额外创建转换后的 PNG 文件。图片更新各自产生原始保留文件，并显示转换结果。

转码只影响终端：默认 *.jpeg、显式 --output-image、JSON、Notebook、历史中的 MIME 数据和导出格式保持原始 JPEG。它不自动应用 EXIF 旋转或进行独立 ICC 色彩管理，不承诺所有特殊 JPEG 变体与图形终端的视觉结果；测试比较 RGB/灰度 JPEG 经解码后的像素与转换 PNG 的解码像素。

## 限制、失败与清理

公开 jpeg_to_png 辅助函数指定 JPEG 解码，不按内容猜测其他图像格式；输入最多 8 MiB，每边最多 8192 像素，总像素最多 16,777,216，解码像素缓冲声明大小最多 64 MiB。先读尺寸并检查像素数量，再分配解码像素；输出 PNG 通过有界 writer，最多 64 MiB。解码器也设置尺寸和分配限制，但这些不是整个进程所有内部临时分配之和的硬上限，也没有可取消的本地 CPU 期限。

空、坏、截断、超限 JPEG 的常规解码错误只报告固定显示警告，不包含图片正文，不把已完成执行改为 Unknown，也不重放远端代码。已保留的原始文件和路径提示继续有效。转换临时 Vec 随调用释放；成功保留的原图仍由用户/OS 清理。显式保存失败和 stdout I/O 失败沿用原有错误语义，不能当作可忽略的转换失败。

只启用 image 0.25.9 的 jpeg/png 特性，不启用默认格式集合或 rayon。锁定该版本以保留 Rust 1.85；0.25.10 要求 Rust 1.88。版本和功能来源为下载的 crate Cargo.toml/CHANGES 及 cargo info；该依赖与间接包共新增 16 个锁文件包。生产转换不调用 Python、系统图片程序或外部进程。

## 验证与边界

[图片测试](../tests/images.rs) 从三项扩展为六项，新增 RGB/灰度真实编解码像素对照、坏/截断/超长输入、修改 JPEG SOF 的尺寸和像素数超限检查。已有分包、部分写入失败和固定上游样本测试继续保留；原有 opaque JPEG wire 样本不能当作 JPEG 解码证据。

[实际 CLI/PTY/WS 测试](../tests/jupyter.rs) 扩展至 16 组配置，新增 JPEG 的显式保存、JSON、display 更新、坏数据 TTY/管道，以及 Notebook：

- TTY 收到真实 PNG，PNG 解码像素与 JPEG 解码像素相同；管道/JSON 不发控制序列。
- 默认/显式文件字节保持 JPEG，默认文件为 0600，转换不产生额外 PNG 文件。
- JSON 和 Notebook 保留原始 image/jpeg，没有替换为 image/png。
- 坏 JPEG 在 TTY 下警告但保留原始文件；管道不尝试转换。已有 base64 错误、默认保存失败和更新语义继续覆盖。
- 所有配置执行退出成功，running/execution_pending 已清理，未重放或创建额外 kernel。

```sh
cargo test --locked --test images
cargo test --locked --test jupyter cli_images_use_tty_only_packets_and_private_fallback_files_without_polluting_json
```

没有进行真实 Colab 图片、Kitty/WezTerm 图形显示、特殊 CMYK/渐进 JPEG 全覆盖或跨平台完整验收。PTY 和编解码断言证明协议及像素数据，不代替真实终端视觉验收；没有创建真实云资源。

全量回归期间还修订了既有总期限测试：服务端立即发送进度，将 80ms 调度窗口改为 400ms，并要求明确的“超时”错误，避免把服务端 EOF 的 Unknown 误判为期限生效。此处只调整测试可靠性和断言强度，不改变生产请求或执行期限。

默认特性全量 438 项、无默认特性全量 436 项 Rust 回归通过；既有官方 CLI 365 场景名称、命令和分类保持不变（92 一致、54 保留差异、219 Rust 扩展）。本机 release 的 [16 项发行检查](../research/native-release-validation.json) 与 [两组非 TTY exec/repl 图片对照](../research/release-image-comparison.json) 通过；release 为 9,123,616 字节，默认测试用 debug 为 44,547,224 字节。优化产物的两组图片对照验证原始文件保存路径，不作为 JPEG 转码或图形终端视觉的优化版验收。其他合并版专项报告保留为对应阶段的历史证据，本轮未重新采集。没有推送或发布。

两种特性的 Clippy 和格式检查通过。研究文档检查覆盖 56 篇 Markdown、606 个相对链接、125 个固定源码引用和 143 个源码哈希，无错误。
