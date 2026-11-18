# 合并版文件命令对照与 JSON 下载字节

## 实际对照范围

[对照工具](../tools/compare_merged_files.py) 实际加载固定官方 CLI、colab-ext 和 Kernel Client，通过独立临时 HOME、两个合成账户及单一 loopback origin，执行十二组 ls/upload/download/rm。没有替换文件命令或 ContentsClient 的业务实现；只将控制面实例指向本地服务，并复用已有探针的外部网络、浏览器、子进程和系统凭据库禁用策略。Rust 要求无原生 keyring 特性的构建，显式文件后端，不读取用户凭据。

每次要求仅 work 账户请求一次 assignments，再请求一次 Contents；验证 compute bearer 不进入运行时、proxy token/query、路径一次编码、上传完整 payload/base64 字节、HTTP 方法、退出码、输出和下载文件摘要。注册表、两账户凭据、default 会话以及上传源文件保持不变。失败与无关命令不得改变预先存在的下载目标；二进制、空文件与文本还直接核对期望字节。

十二组包含目录排序、单文件列表、缺失文件列表、二进制/空文件上传、二进制/空文件/中文文本/JSON 下载、目录与缺失文件下载错误、删除。JSON 含中文键、BMP 字符、emoji、DEL、数组与嵌套对象。实际依赖版本、固定源码和所用 Rust 二进制哈希保存在 [结果](../research/merged-files-comparison.json)。这些有限场景不证明所有 JSON 数值、畸形响应或全部文件错误都等价；edit 的编辑器及故障流程仍独立验收。

## 发现与修正

首次十二组中十组一致，一组 JSON 下载字节不同，另一组目录下载错误的 stdout/stderr 和文本不同，保留 [首次结果](../research/merged-files-first-comparison.json)。首次 JSON 样本还未加入 DEL；后续最终样本增加该字符，不能将两份摘要直接比较为同一输入。

原版通过 json.dumps(content, indent=2) 下载 JSON，默认 ensure_ascii=True；Rust 的 serde_json pretty 输出使用实际 UTF-8，语义等价但字节不同。现下载 JSON 保持两空格缩进，将非 ASCII 和 DEL 写为小写 Unicode 转义，非 BMP 采用 UTF-16 surrogate pair，继续施加 32 MiB 输出限额。text/base64 路径保持原有字节语义。

下载目录的文本失败现在与原版一致：stdout 输出 `[colab] Download failed: Cannot download a directory: content`，退出 1；继续在下载与替换前拒绝目录，不覆盖本地目标。JSON 扩展命令的错误机制保持独立。

## 本地验证

最终十二组全部一致，无保留差异。已有 Contents 测试增加含中文、emoji、DEL 的固定期望字节；探针另有三项反向测试，证明它会记录错误账户令牌、compute bearer、重复编码路径和错误上传内容。全量 Rust/工具及 Clippy 结果和输入摘要在 [回归记录](../research/merged-files-regression-validation.json) 中单独记录。

默认／无默认 Rust 497／495 项全部通过，两种全目标 Clippy 与 fmt 通过；Python 工具 185 通过、1 跳过。默认 debug 已重建，依赖许可材料 --check 通过。本轮 release 未重建，上一章的 release 证据仅对应其当时源码。

本阶段没有真实 Google 请求、新建 VM 或云盘对象，也不关闭 U01 全部命令业务、U03 的真实 ADC、其他平台或正式发行验收。

后续 [76](76-json-number-download-compatibility.md) 发现并修正大整数精度与浮点指数差异，增加 1024 个有限浮点样本等范围，最终十八组一致；本章十二组和旧二进制结果保持历史范围。
