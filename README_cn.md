# sift

> **A股 / 港股 数据采集 CLI — 为 AI Agent 而生**

[English README](./README.md)

`sift` 是一个用 Rust 写的单二进制命令行工具，从公开端点（cninfo / 东方财富 / 新浪 / 腾讯 / 巨潮）拉取 A 股 + 港股 + 美股的上市列表、财务报表、公告、行情快照、K 线、以及公告 PDF 文本/OCR 提取，输出 Unix 友好的 TSV / NDJSON / 对齐表格。

## 这是什么 / 为什么写它

写 `sift` 的核心动机是：**让大模型 Agent（Claude、Kimi、Hermes、OpenClaw 等）也能像分析师一样查看 A 股财报与 K 线。**

主流投资数据工具几乎都是面向人类用户的 GUI 或半结构化 SDK：要么需要登录窗口，要么返回嵌套 JSON 让 LLM 难以稳定解析。`sift` 反其道而行——做成一个**纯 stdout 的 CLI**，每条命令的输出都是 `#header\tcol\tcol\n` 形式的 TSV（兼容 awk / pandas / Polars），或 NDJSON（每行一个对象），让 Agent 工具调用层（Claude Code / OpenCode / Kimi K2 等）可以零成本地把结果回传给 LLM 进行分析。

一句话：**`sift` 是 LLM 的数据自来水管。**

## 主要特色

| 特色 | 说明 |
|---|---|
| **🤖 Agent-First 输出** | 默认 TSV，首行 `#header` 兼具列名 + 注释符（VCF/MAF 风格）；`--format json` 输出 NDJSON。任何模型 / 工具链都能稳定解析。 |
| **🔗 管道友好** | `sift search 银行 --format json \| jq -r .code \| xargs sift quote` —— 子命令之间天然可拼接。 |
| **📊 三套财报源 + 一次成功即返回** | 东方财富 + 新浪 + （未来）巨潮多源并发竞速，谁先返回用谁；附带 `--source` 显式钉源，便于复现。 |
| **🗂 公告全流程** | `announce list` 浏览 / `show` 详情 / `download` 下载 PDF / `extract` 把 PDF 转 Markdown（含 OCR 升级，扫描件也能读）。 |
| **📈 K 线 + 行情** | `quote` 拿当前价快照，`bars` 拿日 / 周 / 月线，前复权 / 不复权 / 后复权可选。 |
| **🏷 原样保留科目名** | 财报科目名直接沿用上游原始字段，不做易失真的映射。A 股（东方财富）用东财英文列名（`TOTAL_OPERATE_INCOME`），港股 / sina 用各自的中文科目名；`--items` 按这些原始名精确过滤。 |
| **💾 本地缓存** | 上市列表（24h 文件缓存）、财报（DuckDB + 三档 TTL，越新越短）、公告元数据（DuckDB，无 TTL）、PDF（永久文件缓存）。单只股票二次查询毫秒级返回。 |
| **🔒 离线优先 / 失败优雅降级** | `$HOME` 不可达 → 关掉缓存继续跑；某只股票失败 → stderr 打 `[warn]` 但其他股票照常输出；从不整体崩溃。 |
| **🚀 单二进制 / 无运行时** | DuckDB 静态链接，无需 Python / Node / 系统库；一份 `~/.local/bin/sift` 即可。 |

## 典型 Agent 使用场景

把 `sift` 配到 Claude Code / OpenClaw / Kimi 的工具白名单里，Agent 就能这样工作：

```text
用户：帮我看看贵州茅台最近 4 个季度净利润趋势，并对比同行业。

Agent 内部调用：
  1. sift search 茅台 --limit 1 --format json           → 拿到 600519
  2. sift report income 600519 --last 4 --unit yi --format tsv
  3. sift search 白酒 --limit 5 --format json | ...     → 拿到同行业 code 列表
  4. sift report income <codes> --last 4 --unit yi --format tsv
  5. （LLM 综合两份 TSV 给出分析）
```

```text
用户：把茅台 2024 年报第 1-30 页做成结构化摘要

Agent 内部调用：
  1. sift announce list 600519 --type 年报 --start 2024-01-01 --format json
  2. sift announce download <id> -o /tmp
  3. sift extract <id> --mode auto --pages 1-30 > /tmp/report.md   # 扫描页自动 OCR
  4. （LLM 读取 markdown 写摘要）
```

```text
用户：今天哪些 A 股放量上涨？

Agent 内部调用：
  1. sift search 600 --limit 50 --format json | jq -r .code | xargs sift quote --format tsv
  2. （LLM 按 pct_change 排序 + 与 5 日均量比较）
```

## 安装

### 预编译二进制（推荐）

一行命令搞定 —— 自动检测系统 + 架构，从 GitHub Releases 下载，安装到 `PATH` 里第一个可写目录（找不到则退回 `~/.local/bin`）：

```bash
curl -fsSL https://raw.githubusercontent.com/eavae/sift-cli/main/scripts/install.sh | bash
```

可选环境变量：

```bash
SIFT_VERSION=v0.2.0 \
SIFT_INSTALL_DIR=/usr/local/bin \
  curl -fsSL https://raw.githubusercontent.com/eavae/sift-cli/main/scripts/install.sh | bash
```

支持平台：`x86_64-unknown-linux-gnu`、`aarch64-unknown-linux-gnu`、`aarch64-apple-darwin`、`x86_64-pc-windows-msvc`。脚本会自动校验 Release 附带的 SHA-256 校验和。

**github.com 上不去时（如中国大陆）：** 脚本会自动检测 GitHub 不可达并改用公共镜像下载二进制。由于连脚本本身也是从 GitHub 拉的，把这一步也走镜像即可：

```bash
curl -fsSL https://cdn.gh-proxy.org/https://raw.githubusercontent.com/eavae/sift-cli/main/scripts/install.sh | bash
```

也可用 `SIFT_MIRROR` 显式指定：`auto`（默认）、`off`（只走直连）、或某个镜像地址（如 `SIFT_MIRROR=https://cdn.gh-proxy.org`）。

**Windows**（`x86_64-pc-windows-msvc`）以 `.zip` 发布。在 **[Git Bash](https://git-scm.com/downloads) / MSYS2** 里跑同一条命令即可 —— 脚本会识别 Windows、下载 `.zip` 并安装 `sift.exe`。（想手动装也行：到 [Releases](https://github.com/eavae/sift-cli/releases) 页下载压缩包，解出 `sift.exe` 放进 `PATH`。）事实库位于 `%USERPROFILE%\.sift`（如 `C:\Users\you\.sift`）；设 `HOME` 可整体改路径。

### 源码编译（备选）

如果你的平台不在 Release 覆盖范围里，或者想跑 `main` 最新代码：

```bash
git clone https://github.com/eavae/sift-cli.git
cd sift-cli
./install.sh                                  # 编译 release 并复制到 ~/.local/bin
# 或手动：
cargo build --release && cp target/release/sift ~/.local/bin/
```

> 首次编译约 3 分钟（DuckDB 捆绑编译），增量秒级。需要 Rust ≥ 1.95。

### 验证

确认 `~/.local/bin` 在 `PATH` 里，然后：

```bash
sift --help
sift search 茅台
sift report income 600519 --last 4 --unit yi
```

## 命令速查

```bash
sift search <kw> [--limit N] [--no-cache]                  # 模糊搜索（代码 / 名称 / 拼音首字母）
sift report {income|balance|cashflow|indicator|periods} <code...> [--last N | --period 2024Q3 | --start --end]
sift announce {list|show|download|types} ...               # 公告浏览 / 下载
sift extract <id|path> [--pages 1-30] [--mode fast|fine|auto]   # PDF → Markdown
sift quote <code...>                                        # 实时快照
sift bars  <code...> [--period daily|weekly|monthly] [--limit N] [--adjust pre|none|post]
```

每条命令都支持 `--format tsv|json`（默认人类对齐表格）。多 symbol 时单只失败不影响其他。

Symbol 写法：裸 6 位数字 = A 股（`600519`），裸 5 位 = 港股（`00700`），也接受 `600519.SH` / `00700.HK` 后缀和 `sh600519` 前缀。**指数**必须带交易所前缀，且仅 `quote` / `bars` 支持：`sh000001` = 上证指数、`sz399001` = 深证成指（输出保留小写前缀形式；`report` / `announce` 会拒绝指数）。注意裸 `000001` 永远是平安银行，不是上证指数。

## 环境变量

### 1. PaddleOCR（`sift extract --mode fine|auto` 必需）

`extract` 在 `fast` 模式下纯本地、无 API 调用；遇到扫描件需要 OCR 时升级到 `fine` 或 `auto`，此时调用 PaddleOCR 云服务。**支持两种配置方式，按个人 / 企业身份二选一即可。**

#### ① Token 模式（个人 / 自托管）

适用于 **PaddleX 官方托管服务** 或自己部署的 PaddleOCR 服务，一次性 token 鉴权，最简单。

```bash
export PADDLEOCR_API_BASE="https://<your-paddleocr-endpoint>"   # 不带 /layout-parsing 后缀
export PADDLEOCR_API_TOKEN="<your-token>"
```

- 单次 HTTP 同步返回，延迟最低。
- 适合：个人开发者、自部署 PaddleX、试用免费配额。

#### ② OAuth 模式（企业 / 百度智能云）

适用于 **百度智能云 AI 开放平台**，企业账号 + API Key/Secret 鉴权，走异步任务接口。

```bash
export PADDLEOCR_API_KEY="<your-baidu-api-key>"
export PADDLEOCR_SECRET_KEY="<your-baidu-secret-key>"
# 可选：自定义百度主机（一般无需改动）
export SIFT_BAIDU_HOST="https://aip.baidubce.com"
```

- 自动 OAuth 换 access token（30 天有效，进程内缓存）。
- 异步任务：提交 → 轮询 → 拉取结构化结果（含 layout / table / images）。
- 适合：企业用量、需要按账号结算、走百度合规通道。

#### 优先级 / 检测顺序

`sift` 在启动 OCR 时按 **Token → OAuth** 顺序检测：

1. `PADDLEOCR_API_BASE` + `PADDLEOCR_API_TOKEN` 同时非空 → 走 Token 模式
2. 否则 `PADDLEOCR_API_KEY` + `PADDLEOCR_SECRET_KEY` 同时非空 → 走 OAuth 模式
3. 都没配 → `--mode fine|auto` 时报错并提示

只配一种就够了。两种都配的话以 Token 模式优先。

### 2. 其他可选环境变量

| 变量 | 用途 | 默认 |
|---|---|---|
| `SIFT_DOWNLOAD_DELAY_MS` | `announce download` 在多 PDF 之间的休眠（防反爬），毫秒 | `0` |
| `SIFT_CNINFO_BASE` | 巨潮 API 根地址（测试 / 镜像用） | `http://www.cninfo.com.cn` |
| `SIFT_EM_HSF10_BASE` | 东财 F10 数据中心根地址 | 东财默认 |
| `SIFT_EM_DATACENTER_BASE` | 东财 datacenter-web 根地址 | 东财默认 |
| `SIFT_EM_QUOTE_BASE` | 东财行情接口根地址 | 东财默认 |
| `SIFT_EM_BARS_BASE` | 东财 K 线接口根地址 | 东财默认 |
| `SIFT_TENCENT_BARS_BASE` | 腾讯 K 线接口根地址 | 腾讯默认 |
| `SIFT_SINA_BASE` | 新浪财报根地址 | 新浪默认 |

后 7 个主要给 mockito 测试 / 内网镜像用；生产环境一般不用动。

## 缓存目录

```
~/.sift/cache/
├── cninfo/{szse,hke}_stock.json     # F1 上市列表，24h TTL
├── announcements/<id>.pdf           # F3 公告 PDF 二进制，永久
├── announcements/<id>/images/       # F4 提取出的图片
└── records.duckdb                   # F2 财报 + F3 公告元数据
```

强制刷新：删对应文件即可（或加 `--no-cache`）。

## 项目状态

- ✅ **F1 search** — 模糊搜索 4 / 4
- ✅ **F2 report** — 财报 5 / 5（+ 转置布局 / `--source` 钉源 / 原样保留科目名）
- ✅ **F3 announce** — list / show / download / types 全部完成
- ✅ **F4 extract** — fast / fine / auto 三种模式
- ✅ **F5 realtime** — quote / bars 完成
- 🚧 **US market** — 枚举存在但 `Symbol::parse` 拒绝美股；端到端未通

详见 [`docs/`](./docs/) 下各 feature 的 `README.md` + `story-NN-*.md` 卡片。

## 许可证

（待补充 LICENSE 文件 — MIT / Apache-2.0 / 私有 任选其一。）
