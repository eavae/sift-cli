# sift 测试报告与测试用例集

- **测试对象**：`sift` v0.1.0（面向 AI agent 的 A股/港股基本面数据 CLI）
- **测试日期**：2026-07-17
- **测试方式**：**黑盒测试** —— 不阅读实现代码，仅以最终用户 / AI agent 视角构建并驱动二进制
- **测试环境**：Linux x86_64，Rust 1.95.0，外网可达（cninfo / 东方财富 / sina / tencent 均连通）
- **被测二进制**：`cargo build`（debug）+ `cargo build --release`，两者均验证可用
- **缓存隔离**：手工测试统一使用 `HOME=/tmp/sift_test/home`，模拟全新用户环境

---

## 1. 结果汇总

| 类别 | 用例数 | 通过 | 失败/缺陷 |
|---|---|---|---|
| 自动化测试（`cargo test`，10 个测试二进制） | 492 | 492 | 0 |
| `cargo clippy --workspace --all-targets` | — | 0 警告 | — |
| 手工黑盒用例（本文件第 3 节） | 109 | 76 | 25 例失败（去重后 17 项缺陷，见第 4 节）+ 8 例观察项 |

自动化测试构成：单元测试 422 + 集成/E2E 测试 70（search / quote / bars / report 数据源 schema / announce list / announce download / extract fast·fine·auto），全部通过。

**总体结论**：六大命令（search / report / announce / extract / quote / bars）主流程全部可用，TSV/NDJSON 输出契约稳定，缓存与降级机制按设计工作，管道组合顺畅。但 **A 股与港股的数据质量存在系统性差距**：港股 quote 价格放大 10 倍、volume 放大 100 倍（第 4 节 BUG-12 / BUG-8），且 quote 输出无币种列——agent 做 A/H 溢价分析会得出完全错误的结论（如招行 A/H 比被算成 38.02 : 470.80 ≈ 1:12.4，真实约 38 CNY : 47 HKD）。双重上市 H 股的公告列表陈旧、港股公告类型过滤失效（BUG-13 / BUG-14）。共发现 17 项缺陷（高 2 / 中 3 / 低 12），建议高严重度两项修复后再用于港股场景。

### 退出码约定（实测归纳）

| 退出码 | 含义 | 示例 |
|---|---|---|
| 0 | 成功（含部分符号失败的 best-effort 成功） | 正常查询 |
| 1 | 运行时错误（IO、解析、内部错误） | `extract --pages abc`、PDF 未缓存 |
| 2 | CLI 参数错误（clap 层） | 缺必填参数、非法枚举值 |
| 3 | 所有上游源均失败 | `quote abc`、`report income 999999` |
| 4 | 无匹配结果 | `search zzzzz`、`announce show <不存在id>` |

---

## 2. 测试范围

**已覆盖**：六大命令全部子命令与主要选项；三种输出格式；多符号降级；缓存命中/绕过/失效降级；管道组合；异常输入；上游不可用降级。

**未覆盖（建议后续补充）**：
- `extract --mode fine` 真实 OCR 调用（无 PaddleOCR 凭证，仅验证到"未配置时正确报错"）
- 扫描版 PDF 的 auto 模式 OCR 升级路径（手头 PDF 均有文本层，未触发升级）
- US 市场（README 声明未接通，实测确认拒绝）
- 安装脚本 `install.sh` / GitHub Releases 预编译包
- 高并发压测（仅验证了缓存文件锁设计可支撑常见并发调用）
- `--format json` 下 `extract` 的行为（stdout 恒为 Markdown，`--format` 对其无意义但不报错）

---

## 3. 手工测试用例

### 3.1 通用 / CLI 参数层

| 编号 | 用例 | 命令 | 预期 | 实际 | 结果 |
|---|---|---|---|---|---|
| G-01 | 版本号 | `sift --version` | 输出版本，exit 0 | `sift 0.1.0`，exit 0 | ✅ |
| G-02 | 总帮助 | `sift --help` | 列出全部子命令与示例，exit 0 | 符合 | ✅ |
| G-03 | 子命令帮助 | `sift search --help` / `sift report income --help` | exit 0 | exit 0 | ✅ |
| G-04 | `--format table` 应被拒并给出提示 | `sift --format table search 茅台` | exit 2 + 提示"省略 --format 即为表格" | 提示 `table is the default — omit `--format` to get it`，exit 2 | ✅ |
| G-05 | 非法 format 值 | `sift --format xml search 茅台` | exit 2，回显非法值并列出合法值 | 符合 | ✅ |
| G-06 | 未知子命令 | `sift badcommand` | exit 2 | exit 2 | ✅ |
| G-07 | 缺必填参数 | `sift search`（无 QUERY）等 8 个子命令 | 全部 exit 2 | 全部 exit 2 | ✅ |
| G-08 | 非法数值参数 | `sift search 茅台 --limit abc` | exit 2 | exit 2 | ✅ |
| G-09 | 非法枚举值 | `bars --period hourly`、`bars --adjust foo`、`report --source bogus` | exit 2 并列出 possible values | 符合 | ✅ |
| G-10 | US 股票符号 | `sift quote AAPL` / `sift quote US:AAPL` | 拒绝（README 声明 US 未接通） | exit 3，parse error；但错误消息将输入小写化（`got "aapl"`） | ⚠️ OBS-6 |

### 3.2 `sift search`（F1 列表检索）

| 编号 | 用例 | 命令 | 预期 | 实际 | 结果 |
|---|---|---|---|---|---|
| S-01 | 中文名检索 | `sift search 茅台` | 命中 600519 贵州茅台 | 命中，0.5s（含首次列表下载） | ✅ |
| S-02 | 拼音首字母 | `sift search gzmt --limit 3` | 命中贵州茅台 | 命中 | ✅ |
| S-03 | 代码前缀 | `sift search 600 --limit 5` | 返回 5 条代码含 600 的记录 | 符合（000600/002600/300600/301600/600000） | ✅ |
| S-04 | 港股 | `sift search 00700` | 结果含腾讯控股 00700.HK | 符合（同时列出 A 股相似代码） | ✅ |
| S-05 | TSV 格式 | `sift search 茅台 --format tsv` | 首行 `#`-前缀表头 | `#code\tname\tmarket\t...` | ✅ |
| S-06 | NDJSON 格式 | `sift search 茅台 --format json` | 单行一个 JSON 对象 | 符合；但字段名与表格列名不一致（`zwjc` vs `name`） | ⚠️ OBS-1 |
| S-07 | 无匹配 | `sift search zzzzz不存在` | exit 4，stderr 提示 | `sift: no match for query "..."`，exit 4 | ✅ |
| S-08 | 空查询串 | `sift search ""` | 优雅处理 | exit 4，no match | ✅ |
| S-09 | `--no-cache` 绕过缓存 | `sift search 茅台 --no-cache` | 重新拉取列表 | 耗时 0.71s（缓存命中时 0.02s），结果一致 | ✅ |

### 3.3 `sift report`（F2 财报与指标）

| 编号 | 用例 | 命令 | 预期 | 实际 | 结果 |
|---|---|---|---|---|---|
| R-01 | 可用期间枚举 | `sift report periods 600519` | 列出最近报告期，新→旧 | 2026-03-31 … 2022-12-31，含 period_type | ✅ |
| R-02 | 利润表最近 4 期（亿元） | `sift report income 600519 --last 4 --unit yi --format tsv` | 表头+4 行，金额以亿计 | 符合；2025 年报营业总收入 1720.54 亿 | ✅ |
| R-03 | 资产负债表 | `sift report balance 600519 --last 2 --unit yi --format tsv` | 正常输出 | 符合 | ✅ |
| R-04 | 现金流量表 | `sift report cashflow 600519 --last 2 --unit yi --format tsv` | 正常输出 | 符合（该次由 sina 源胜出） | ✅ |
| R-05 | 关键指标年度序列 | `sift report indicator 600519 --start 2023 --end 2025 --annual --format tsv` | 3 个年度行 | 符合（毛利率 91.9% 等） | ✅ |
| R-06 | `--items` 精确名过滤 | `--items ROE加权` / `--items 基本每股收益` | 仅输出指定列 | 符合（ROE加权=36.02） | ✅ |
| R-07 | `--items` 别名过滤（帮助示例） | `--items ROE,EPS,毛利率` | 三列均输出 | 仅`毛利率`命中；`ROE`/`EPS`匹配不到（实际列名为 `ROE加权`/`基本每股收益`） | ❌ BUG-6 |
| R-08 | 母公司口径 | `sift report income 600519 --last 1 --scope parent` | 中文规范列名 | 数值正确，但含 40 个未中文化英文列（`ABLE_OCI_QOQ` 等 _QOQ 环比列） | ❌ BUG-5 |
| R-09 | 指定单季 | `sift report income 600519 --period 2024Q3` | 仅 2024-09-30 | 符合 | ✅ |
| R-10 | 指定多个年度 | `sift report balance 600519 --period 2024A,2023A` | 两个年度行 | 符合 | ✅ |
| R-11 | 固定数据源 | `sift report income 600519 --last 1 --source sina` | source 列为 sina | 符合 | ✅ |
| R-12 | 多源竞速 | 同一命令重复运行 | first-success-wins | 不同次运行 source 在 eastmoney/sina 间变化（设计如此，重现已用 `--source` 固定） | ⚠️ OBS-2 |
| R-13 | 港股财报 | `sift report income 00700 --last 2 --unit yi --format tsv` | 腾讯最近两期 | 符合 | ✅ |
| R-14 | 无效代码 | `sift report income 999999 --last 1` | exit 3 + 可读错误 | `all sources failed: eastmoney: ... symbol may be unsupported`，exit 3 | ✅ |
| R-15 | `--last` 与 `--period` 同给 | `sift report income 600519 --last 2 --period 2024Q3` | 应报参数冲突或明确优先级 | 不报错，`--period` 静默胜出 | ❌ BUG-9 |
| R-16 | `--last 0` | `sift report income 600519 --last 0` | 参数校验拒绝或提示 | 静默返回空，exit 0 | ❌ BUG-10 |
| R-17 | 每股指标单位缩放 | `sift report income 600519 --last 1 --unit yi` | 每股收益保持 21.76 元 | 基本每股收益被按 1e8 缩放 → 显示 0.00（`--unit wan` 同样归零） | ❌ BUG-3 |
| R-18 | 表格标题 | `sift report income/balance/cashflow ... （默认表格）` | 标题与报表类型对应 | 三种报表标题均为"财务指标" | ❌ BUG-7 |

### 3.4 `sift announce`（F3 公告）

| 编号 | 用例 | 命令 | 预期 | 实际 | 结果 |
|---|---|---|---|---|---|
| A-01 | 类型字典 | `sift announce types` | 列出中文类型↔cninfo 类目映射 | 符合（年报/半年报/董事会…） | ✅ |
| A-02 | 列表（无过滤） | `sift announce list 600519 --limit 10` | 最近 10 条公告 | 符合 | ✅ |
| A-03 | 类型过滤 | `sift announce list 600519 --type 年报 --limit 5` | 仅年报 | 符合（含年报/英文版/摘要） | ✅ |
| A-04 | 双边日期范围 | `--start 2024-01-01 --end 2026-12-31` / `--start 2026-01-01 --end 2026-12-31` | 范围内记录 | 符合（9 条 / 3 条） | ✅ |
| A-05 | 仅 `--start`（单边） | `sift announce list 600519 --type 年报 --start 2024-01-01` | 应默认 end=今天并返回 2024 至今 | **0 条**（三种格式均 0 条） | ❌ BUG-2 |
| A-06 | 仅 `--end`（单边） | `sift announce list 600519 --type 年报 --end 2024-12-31` | 应默认 start 并返回历史 | 上游 cninfo 500，exit 3 | ❌ BUG-2 |
| A-07 | 全市场关键词检索 | `sift announce list --start ... --end ... --keyword 减持 --limit 3` | 跨标的命中 | 符合（3 条减持公告） | ✅ |
| A-08 | 元数据详情 | `sift announce show 1225114741` | 11 字段 kv 展示 | 符合（含 cached 路径与状态） | ✅ |
| A-09 | 不存在的 id | `sift announce show 9999999999` | exit 4 | `no match`，exit 4 | ✅ |
| A-10 | 下载 PDF | `sift announce download 1225114741 -o /tmp/pdfs` | 下载到指定目录 | 1057 KB 落盘；二次执行显示 `cached` 命中 | ✅ |

### 3.5 `sift extract`（F4 PDF 提取）

| 编号 | 用例 | 命令 | 预期 | 实际 | 结果 |
|---|---|---|---|---|---|
| E-01 | 按 id 提取（已缓存） | `sift extract 1225114741 --pages 1-3 --mode fast` | stdout 输出 Markdown | 73 行，正文完整；143 页、文本层检测正确 | ✅ |
| E-02 | 按本地路径提取 | `sift extract ./pdfs/1225114741.pdf --pages 2 --mode fast` | 输出第 2 页 Markdown | 符合 | ✅ |
| E-03 | auto 模式（有文本层） | `sift extract 1225114741 --pages 1 --mode auto`（无 OCR 凭证） | 有文本层时不触发 OCR，正常输出 | 正常输出 Markdown，exit 0 | ✅ |
| E-04 | fine 模式无凭证 | `sift extract ... --mode fine`（无 OCR 环境变量） | 报配置错误并提示两类凭证 | `PaddleOCR not configured. Set either (...)` exit 1 | ✅ |
| E-05 | 页码列表语法 | `--pages 3,5` | 提取第 3、5 页 | 符合 | ✅ |
| E-06 | 非法页码 | `--pages abc` | exit 1 解析错误 | `expected positive integer`，exit 1 | ✅ |
| E-07 | 页码越界 | `--pages 999`（共 143 页） | exit 1 并提示总页数 | `page 999 out of range (PDF has 143 pages)` | ✅ |
| E-08 | 文件不存在 | `sift extract /tmp/nonexistent.pdf` | exit 1 | `PDF not found`，exit 1 | ✅ |
| E-09 | id 未下载 | `sift extract 9999999999` | 提示先 download | `PDF not cached ...; run sift announce download ... first`，exit 1 | ✅ |

### 3.6 `sift quote`（F5 实时快照）

| 编号 | 用例 | 命令 | 预期 | 实际 | 结果 |
|---|---|---|---|---|---|
| Q-01 | 单标的快照 | `sift quote 600519` | 13 列行情 | 符合（价/涨跌/量额/时间/源） | ✅ |
| Q-02 | 多标的混合市场 | `sift quote 600519 00700 sh000001 --format tsv` | 三行 | A股/港股正确；`sh000001` 解析为**平安银行 000001.SZ**而非上证指数 | ❌ BUG-4 |
| Q-03 | NDJSON | `sift quote 600519 --format json` | 按 README"每命令支持 tsv|json"应输出 NDJSON | 拒绝：`--format json is not supported by sift quote`，exit 1 | ❌ BUG-1 |
| Q-04 | 好坏符号混合 | `sift quote 600519 abc 600036` | 好的照常输出，坏的 stderr 警告，exit 0 | `[warn] quote abc: parse error...`，两行正常，exit 0 | ✅ |
| Q-05 | 非法符号 | `sift quote abc` / `sift quote 6005199` / `sift quote 99999` | exit 3 + 明确原因 | 符合（非数字/位数错误/上游无数据分别提示） | ✅ |
| Q-06 | 港股成交量自洽性 | `sift quote 00700` 的 volume×price ≈ amount | 量级自洽 | volume=3,623,765,700，是东财原始值 36,237,657 的 **100 倍**（见 BUG-8） | ❌ BUG-8 |
| Q-07 | 市场前缀 | `sift quote sz000001` / `sift quote sh000001` | 前缀用于消歧 | 两者均返回平安银行，前缀未生效 | ❌ BUG-4 |
| Q-08 | hk 前缀 | `sift quote hk00700` | 腾讯控股 | 符合 | ✅ |

### 3.7 `sift bars`（F5 历史 K 线）

| 编号 | 用例 | 命令 | 预期 | 实际 | 结果 |
|---|---|---|---|---|---|
| B-01 | 日线最近 N 根 | `sift bars 600519 --limit 5` | 5 行日线 | 符合 | ✅ |
| B-02 | 周线多标的 | `sift bars 600519 00700 --period weekly --limit 3 --format tsv` | 每标的 3 行 | 结构符合；但港股 volume 同样放大 100 倍 | ❌ BUG-8 |
| B-03 | 月线 | `sift bars 600519 --period monthly --limit 3` | 3 行月线 | 符合 | ✅ |
| B-04 | 显式区间+前复权 | `--start 2024-01-01 --end 2024-03-01 --adjust pre` | 区间内日线 | 符合 | ✅ |
| B-05 | 无连字符日期 | `--start 20240101` | 宽容解析或报错 | 宽容解析为 2024-01-01 | ✅ |
| B-06 | 指定源 | `--source tencent` / `--source eastmoney` | source 列对应 | 符合 | ✅ |
| B-07 | 好坏符号混合 | `sift bars 600519 abc --limit 2` | 警告+正常输出，exit 0 | 符合 | ✅ |
| B-08 | NDJSON | `sift bars 600519 --format json` | 按 README 应支持 | 同 quote 被拒绝，exit 1 | ❌ BUG-1 |
| B-09 | 窗口首行涨跌字段 | `bars --limit/--start` 查询 | pct_change/change/amplitude 有值 | 日线 `--limit` 首行正常；周线/月线及区间查询首行三字段恒为 0.00 | ⚠️ OBS-3 |
| B-10 | 复权类型 | `--adjust none/pre/post` | 输出 adjust 列标识 | 符合（默认 pre） | ✅ |

### 3.8 缓存 / 降级

| 编号 | 用例 | 操作 | 预期 | 实际 | 结果 |
|---|---|---|---|---|---|
| C-01 | 列表缓存命中 | 第二次 `search 茅台` | 毫秒级返回 | 0.02s（首次 0.5s） | ✅ |
| C-02 | 财报缓存命中 | 第二次 `report income` | 明显加速 | 0.17s | ✅ |
| C-03 | 缓存目录结构 | `ls ~/.sift/cache` | 与 README 一致 | `cninfo/*.json`、`announcements/*.pdf`、`records.duckdb` | ✅ |
| C-04 | `$HOME` 不可写 | `HOME=/nonexistent_dir_xyz sift search 茅台` | 缓存禁用但命令可用 | exit 0，两行 `[warn] cache write failed`，结果正确 | ✅ |
| C-05 | 上游不可达+有过期缓存 | `SIFT_CNINFO_BASE=http://127.0.0.1:1 sift search 茅台 [--no-cache]` | 回退过期缓存并警告 | `[warn] upstream failed ... using cache from ...`，结果正确 | ✅ |
| C-06 | 公告下载幂等 | 重复 download 同一 id | 命中缓存不重复下载 | 第二次输出 `cached →` | ✅ |

### 3.9 管道组合（agent 核心场景）

| 编号 | 用例 | 命令 | 预期 | 实际 | 结果 |
|---|---|---|---|---|---|
| P-01 | search→quote（jq） | `sift search 银行 --limit 3 --format json \| jq -r .code \| xargs sift quote --format tsv` | 输出 3 家银行行情 | 符合（平安/兰州/宁波银行） | ✅ |
| P-02 | search→quote（awk，无 jq） | `... --format tsv \| awk -F'\t' '!/^#/{print $1}' \| xargs ...` | `#`表头被 awk 天然跳过 | 符合 | ✅ |
| P-03 | announce list→show | `sift announce list 600519 --type 年报 --format json \| sift announce show 1225114741` | stdin NDJSON 供 show 解析 | 符合 | ✅ |
| P-04 | announce list→download | `... \| jq -r .id \| head -1 \| xargs sift announce download -o ./pdfs` | 下载对应 PDF | 符合 | ✅ |
| P-05 | TSV 被 pandas 消费 | `pd.read_csv(sep='\t')` | 表头/数据完整 | 13 列、2 数据行，列名带 `#` 前缀可 strip | ✅ |
| P-06 | 管道截断 | `sift bars 600519 --limit 200 \| head -2` | Unix 惯例静默退出（SIGPIPE） | stderr 打印 `sift: internal: io: Broken pipe`，exit 1 | ❌ BUG-11 |

### 3.10 A+H 两地上市 / 港股专项（第二轮追加）

背景：用户反馈"A+H 同时上市的股票可能有些问题"。本组针对代码歧义（5 位港股码 vs 6 位 A 股码）、跨市场数据量纲、双重上市公司的公告/财报行为做专项验证。测试标的：招商银行（600036/03968）、中国平安（601318/02318）、比亚迪（002594/01211）、腾讯（00700）、京东（09618）、中国石油（00857）、久立特材（002318，与平安H 数字碰撞对照）、北交所梓橦宫（832566→920566 代码切换）。

| 编号 | 用例 | 命令 | 预期 | 实际 | 结果 |
|---|---|---|---|---|---|
| AH-01 | 同名检索返回 A+H 两行 | `sift search 招商银行 --format tsv` | 同时列出 600036 CN-A 与 03968 HK | 符合（两行同 orgId） | ✅ |
| AH-02 | 同上（平安） | `sift search 中国平安` | 601318 + 02318 | 符合 | ✅ |
| AH-03 | 港股 quote 价格真实性 | `sift quote 03968 02318 00700 01211 09618 00857` | 与真实行情一致 | **全部放大 10 倍**（470.80 vs 真实 47.08 等，6 只全部复现，腾讯接口独立交叉验证确认） | ❌ BUG-12 |
| AH-04 | 数字碰撞消歧 | `sift quote 02318 002318` | 02318→平安H，002318→久立特材，互不干扰 | 符合（5 位=HK，6 位=A 股） | ✅ |
| AH-05 | 5/6 位歧义不错配 | `sift quote 00857 000857` | 00857→中石油H；000857 无此 A 股→清晰报错 | 符合（best-effort 警告+正常输出） | ✅ |
| AH-06 | 6 位不存在代码 | `sift quote 003968` | 报错而非错配到他股 | `missing data object`，exit 3 | ✅ |
| AH-07 | 港股 bars 价格（tencent 源） | `sift bars 03968 --limit 2` | 与腾讯原始 K 线一致 | 46.30 / 47.08 完全一致 | ✅ |
| AH-08 | 港股 bars 指定 eastmoney 源 | `sift bars 03968 --source eastmoney` | 正常输出或明确不支持 | `missing data object`，exit 3（auto 模式走 tencent 正常） | ❌ OBS-12 |
| AH-09 | 港股 bars 量额量纲 | `sift bars 03968 --limit 2` | volume/amount 与原始一致 | volume ×100（11,353,675→1,135,367,500），**amount 同样 ×100**（5.3 亿→534.5 亿） | ❌ BUG-8 |
| AH-10 | 港股财报科目命名 | `sift report income 03968 --last 1` | 数据可用 | 数据正确但科目为港股准则命名（经营溢利/股东应占溢利/营业额），与 A 股科目完全不同名 | ⚠️ OBS-8 |
| AH-11 | 港股财报币种列 | 同上输出 currency 列 | 应标 CNY/HKD | **为空**（A 股正常显示 CNY） | ❌ BUG-16 |
| AH-12 | 港股财报 `--unit yi` | `sift report income 03968 --last 1 --unit yi` | 金额缩放、每股指标不动 | 经营收入总额 862.05 正确；每股基本盈利 0.00（BUG-3 港股同样成立） | ❌ BUG-3 |
| AH-13 | 港股 indicator | `sift report indicator 03968 --last 1` | 指标数据 | **静默返回空表**（仅元数据列，exit 0）；对照 600036 正常 | ❌ BUG-17 |
| AH-14 | A/H 报告期一致性 | `report periods 03968` vs `600036` | 一致 | 完全相同 | ✅ |
| AH-15 | 双重上市 H 股公告时效 | `sift announce list 03968 --start 2010-01-01 --end 2026-12-31` | 含近年公告 | **最新仅 2014-05-29**；02318 最新仅 2013-11-20（普遍现象） | ❌ BUG-13 |
| AH-16 | 纯港股公告时效 | `sift announce list 00700 --limit 5` | 近期公告 | 2026-07 公告正常 | ✅ |
| AH-17 | 港股公告类型过滤 | `sift announce list 00700 --type 年报` | 命中"2025 年报"（存在于 2026-04-09） | **空结果**——近 100 条港股公告 type 全为 250501，类型未区分 | ❌ BUG-14 |
| AH-18 | A+H 公司 H 股公告的实际位置 | `sift announce list 601318` | — | 2026 年的"中国平安H股公告"挂在 **A 股代码**下（解释了 AH-15 的陈旧） | ⚠️ OBS-9 |
| AH-19 | 北交所代码切换后检索 | `sift search 梓橦宫` | 返回一个有效代码 | 返回 832566 与 920566 两行（同 orgId），旧码未标注 | ⚠️ OBS-10 |
| AH-20 | 北交所旧代码 quote | `sift quote 832566` | 报错或重定向新码 | 返回 `梓橦宫(已切换)` **price=0.00** 占位行，无警告 | ❌ BUG-15 |
| AH-21 | 北交所旧代码 bars | `sift bars 832566` vs `920566` | 与 quote 行为一致 | bars 报错 `no match`（exit 3），920566 正常——quote/bars 对旧码处理不一致 | ❌ BUG-15 |
| AH-22 | 4 位港股码 | `sift quote 9618` | 拒绝并提示位数 | `expected 5 or 6 digits, got 4-digit`，exit 3（须补前导零 09618） | ✅ |
| AH-23 | 单标的偶发超时 | 首次 `sift quote 600036`（与 03968 同批） | 稳定成功 | 一次 `network error: timeout: connect`，重试即恢复 | ⚠️ OBS-11 |

**A/H 溢价分析场景（agent 高危场景）**：`sift quote 600036 03968` 一次调用拿回 `38.02`（CNY）与 `470.80`（HKD，且已放大 10 倍），输出**无币种列**。agent 直接相除得到 AH 比 ≈ 12.4，而真实约为 38 CNY : 47 HKD ≈ 1 : 1.08（经汇率换算）。该场景叠加 BUG-12 + 无币种标注，结论完全错误。

---

## 4. 缺陷清单（按严重度排序）

### 修复状态回归（2026-07-18）

本轮回归后 17 项缺陷的处置如下（代码级已全部修复或确认为上游数据限制）：

| 缺陷 | 状态 | 修复点 |
|---|---|---|
| BUG-1 `--format json` 被拒 | ✅ 已修 | `quote`/`bars` 走通用 RenderRow→NDJSON，`run()` 不再拦截 json |
| BUG-2 单边日期范围失效 | ✅ 已修 | `build_form` 补全缺失端点（缺 end→今天，缺 start→1990 历史底） |
| BUG-3 `--unit` 误缩放每股指标 | ✅ 已修 | `apply_unit` 跳过含「每股」的科目（与 Indicator 同样不缩放） |
| BUG-4 `sh000001` 前缀失效 | ✅ 已修 | 新增 `InstrumentKind::Index`，`sh000001`/`sz399001` 解析为指数 |
| BUG-5 `--scope parent` 英文列 | ✅ 已修 | `translate` 增加 `_QOQ` 后缀过滤（同 `_YOY`） |
| BUG-6 `--items ROE,EPS` 示例失配 | ✅ 已修 | 字典补 `EPS`→基本每股收益别名；`--items` 未命中时 `[warn]` 提示；帮助示例改 `ROE加权` |
| BUG-7 报表标题恒为「财务指标」 | ✅ 已修 | `Statement::cn_label()` 按报表类型命名首列 |
| BUG-8 港股量/额放大 100 倍 | ✅ 已修 | `volume_factor(market)` 港股=1.0；bars amount 由 volume 派生随之修正 |
| BUG-9 `--last`/`--period` 无冲突提示 | ✅ 已修 | clap `conflicts_with = "period"` |
| BUG-10 `--last 0` 静默空 | ✅ 已修 | `parse_positive_usize` 拒绝 0（exit 2） |
| BUG-11 管道截断报错 | ✅ 已修 | `io_err` 将 EPIPE 映射为 `SiftError::BrokenPipe`→静默 exit 0 |
| BUG-12 港股 quote 价格放大 10 倍 | ✅ 已修 | `price_factor(market)` 港股/美股 ÷1000 |
| BUG-13 双重上市 H 股公告陈旧 | ✅ 已修 | **实为 sift 路由缺陷**：`column_groups` 按 `org_id` 前缀分列，而 H 股（招行 03968=`gssh0600036`、平安 02318=`9900002221`）复用 A 股/数字 orgId，被误路由到 `column=szse`（仅 3 条 2014 年旧数据）。改按 `market` 分列后走 `column=hke`，返回全量当期公告（03968 达 342 条、最新 2026-07） |
| BUG-14 港股公告 `--type` 失效 | ⚠️ 上游限制（已缓解） | cninfo 未对港股公告分类（`columnId` 恒为 250501、`announcementType` 全同、`category` 过滤返回 0）；无法代码级修复，已在 `--type` + 港股标的时打 `[warn]` 提示改用 `--keyword` |
| BUG-15 北交所旧代码行为不一致 | ✅ 已修 | EM「(已切换)」占位行在 quote 侧改判 `NotFound`（与 bars 一致，不再给 0.00 假价） |
| BUG-16 港股财报 currency 空 | ⚠️ 上游限制 | EM 列报币种接口已下线（`RPT_CUSTOM_HKSK_APPFN_CASHFLOW_SUMMARY` 全 source 变体均返 `code:9501`）；其余在线接口（orgprofile、MAININDICATOR）的 CURRENCY 恒为**交易**币种 HKD（连以 USD 列报的汇丰 00005 也标 HKD），与实际 CNY 列报金额矛盾——填入反而错误，故保持留空。待上游 summary 接口恢复即可零成本接回 |
| BUG-17 港股 indicator 静默空表 | ✅ 已修 | 命令层 `validate_indicator_market` 拒绝非 A 股 indicator，指向 income/balance/cashflow |

> ⚠️ 剩余两项「上游限制」（BUG-14 港股公告不分类、BUG-16 港股列报币种接口下线）为 cninfo / 东方财富 数据结构问题，非 sift 逻辑缺陷。
>
> 📌 **接口调研补充（2026-07-18）**：
> - BUG-13 经真实接口比对确认为 sift 路由 bug 而非上游陈旧，已修复。
> - BUG-17 的港股指标数据其实**可获取** —— `RPT_HKF10_FN_MAININDICATOR`（`source=F10`）返回 76 字段的完整港股指标（ROE_AVG / BASIC_EPS / GROSS_PROFIT_RATIO / DEBT_ASSET_RATIO …）。当前先以「明确报错」兜底；真正接通港股 indicator 需要一张 EM 列名→中文字典，属独立 feature story（非 bug 修复范畴）。

### 第二轮黑盒回归（2026-07-18，字典移除后）

期间的一次产品决策：**移除中文项名字典**（`data/items.txt` + `domain/items_dict.rs`），财报科目名改为**原样输出上游字段**——A 股 EM 为英文列名（`TOTAL_OPERATE_INCOME`），港股 / sina 为各自中文原名；`--items` 按原始名精确过滤。为避免 EM(英文)/sina(中文) 在 first-success-wins 下字段名漂移，sina 退出自动竞速（`auto_dispatch()==false`），仅 `--source sina` 显式可达。这一并消除了原报告的 OBS-2（同一命令多次运行源可能不同）。

黑盒复测（全新 `HOME`、清空 `records.duckdb`，driver=debug 二进制）——27 项断言全部通过：

| 组 | 断言 | 结果 |
|---|---|---|
| 字段 | A 股默认=英文码 / 源恒为 eastmoney / `--source sina`=中文 / 港股=中文原名 / `--items` 精确匹配 / typo 触发 `[warn]` | ✅ 6/6 |
| 指标 | A 股 indicator 正常；港股 indicator 明确报错（非静默空表） | ✅ 2/2 |
| 指数 | `sh000001`=上证指数 / `sz399001`=深证成指 / report 拒绝指数 / `sz600519` 矛盾前缀报错 | ✅ 4/4 |
| 行情 | quote·bars 接受 `--format json`；港股价格量级正确 | ✅ 3/3 |
| 北交所 | 旧码 832566 报错(exit 3)；新码 920566 正常 | ✅ 2/2 |
| 公告 | 双重上市 H 03968 更新至 2025+（原 2014）；港股 `--type` 告警；A 股 `--type` 正常 | ✅ 3/3 |
| 校验 | `--last 0` 拒绝；`--last`+`--period` 冲突；管道截断静默 | ✅ 3/3 |
| 基础 | search / periods / version | ✅ 4/4 |

**头条场景复核**：
- **多源一致性**：连跑 5 次 A 股财报，`_source` 全部 `eastmoney`（OBS-2 漂移已消除）。
- **A/H 溢价场景**：招行 `600036=38.02 CNY` / `03968=47.08 HKD` → 比值 ≈ 1:1.24（真实溢价），不再是旧 BUG-12 的 `38:470 ≈ 1:12`。

自动化：`cargo test` 503 passed / 0 failed，`cargo clippy --workspace --all-targets` 0 警告。

### 高严重度（港股数据失真，agent 结论级错误）

**BUG-12（价格正确性）港股 quote 价格系字段放大 10 倍**
- 现象：`sift quote 00700` price=4616.00、`03968`=470.80、`02318`=546.00、`01211`=887.00、`09618`=1163.00、`00857`=95.90。price/change/open/high/low/prev_close 全部 ×10；pct_change 因比值抵消而正常。
- 独立交叉验证（腾讯 `qt.gtimg.cn` 原始行情）：真实值分别为 461.60 / 47.08 / 54.60 / 88.7 / 116.3 / 9.59 HKD，6 只全部 ×10，属系统性错误而非个例。
- 根因线索：东财原始字段 f43（如 00700=461600）A 股按 ÷100 缩放，港股需 ÷1000，sift 对港股误用 A 股缩放。
- 影响：所有港股快照价格错误；叠加 quote 无币种列，A/H 比价类分析完全失真（见 3.10 末的场景说明）。
- 注意：bars（tencent 源）港股价格**正确**（与原始 K 线逐值一致），仅 quote（eastmoney 源）失真。

**BUG-8（成交量/成交额正确性）港股 volume 放大 100 倍，bars amount 同步放大**
- quote（eastmoney 源）：`00700` volume=3,623,765,700，原始 f47=36,237,657。A 股 f47 单位是"手"（×100=股数，换算正确）；港股 f47 已是股数，sift 仍 ×100。
- bars（tencent 源）：`03968` 2026-07-17 volume=1,135,367,500，原始 K 线=11,353,675（×100）；**amount 同样 ×100**（真实 5.34 亿 HKD → 显示 534.5 亿）。A 股 bars 量额均正确。
- 自洽性验证：正确成交量 ×均价 ≈ amount ✓；放大后与 amount 差两个数量级。
- 影响：港股放量/换手率/量比分析全部错误。

### 中严重度

**BUG-3（数据正确性）`--unit yi/wan` 误缩放每股类指标**
- 现象：`sift report income 600519 --last 1 --unit yi` 中`基本每股收益=0.00`（raw 单位下为 21.76，正确）。
- 分析：每股指标（元/股）被按金额单位 1e8/1e4 缩放后四舍五入归零。`--unit wan` 同样归零；港股财报（`03968` 每股基本盈利）同样复现。
- 影响：agent 按 README 推荐用法 `--unit yi` 取利润表时，EPS 列恒为 0，会误导估值分析。

**BUG-13（覆盖完整性）双重上市 H 股公告列表陈旧**
- 现象：`announce list 03968` 最新公告 2014-05-29；`announce list 02318` 最新 2013-11-20（宽日期范围 + limit 50 下复测依旧）。纯港股（00700）公告则更新正常。
- 根因线索：A+H 公司的 H 股公告在 cninfo 挂在 **A 股代码**下（`announce list 601318` 可见 2026 年"中国平安H股公告"），H 股代码自身的索引常年不更新。
- 影响：agent 按 search 结果直接查 H 股代码公告，拿到十年前的数据且**无任何陈旧提示**。
- 临时规避：A+H 公司查公告用 A 股代码。

**BUG-14（过滤失效）港股公告 `--type` 过滤不可用**
- 现象：`announce list 00700 --type 年报` 返回空；但"2025 年报"公告确实存在（2026-04-09）。
- 分析：近 100 条港股公告 type 字段全为 `250501`，港股公告在 cninfo 未做类型区分，导致任何类型过滤对港股都返回空。

### 低严重度

**BUG-1（文档/契约不一致）`quote`/`bars` 不支持 `--format json`**
- README："Every command accepts `--format tsv|json`"；实际两者拒绝 json（exit 1，且错误前缀为 `internal:`，像是非预期路径）。要么补实现，要么改文档。

**BUG-2（边界行为）`announce list` 单边日期范围失效**
- 仅 `--start` → 恒 0 条；仅 `--end` → cninfo 返回 500（exit 3）。应对缺省一侧做默认补全（end=今天 / start=上市日或 end-1y）。

**BUG-4（解析歧义）`sh000001`/`sz000001` 市场前缀未生效**
- 两者都返回平安银行（000001.SZ）；CLI 帮助示例 `sift quote 600519 00700 sh000001` 暗示可查上证指数，实际得到个股。指数查询要么支持，要么从示例中移除。

**BUG-5（输出规范化缺口）`--scope parent` 含 40 个英文列名**
- 如 `ABLE_OCI_QOQ`、`BASIC_EPS_QOQ`（EM 环比原始列），未走中文字典规范化；合并报表口径（consolidated）则为 0 个英文列。

**BUG-6（文档错误）`--items ROE,EPS,毛利率` 示例仅命中 1 项**
- `--items` 为精确匹配，实际列名是 `ROE加权`/`基本每股收益`。帮助与 README 示例中的 `ROE,EPS` 匹配为空（静默返回空表，无任何提示）。

**BUG-7（展示）三种报表默认表格标题均为"财务指标"**
- income/balance/cashflow 应分别标注 利润表/资产负债表/现金流量表。

**BUG-9（参数校验）`--last` 与 `--period` 同给无冲突提示**，`--period` 静默胜出。
**BUG-10（参数校验）`--last 0` 静默返回空结果**（exit 0）。

**BUG-11（Unix 惯例）管道截断不静默**
- `| head` 截断时 stderr 报 `internal: io: Broken pipe` 且 exit 1；惯例是吞掉 SIGPIPE 静默退出，否则 agent 管道里大量 `head` 截断会产生噪音告警。

**BUG-15（代码切换）北交所 920 切换后旧代码行为不当且不一致**
- `search 梓橦宫` 返回 832566 + 920566 两行（同 orgId，旧码无标注）；`quote 832566` 返回 `梓橦宫(已切换)` 的 **price=0.00** 占位行（无警告）；`bars 832566` 则报 `no match`（exit 3）。quote 应拒绝或重定向到新码，至少不要在静默中给 agent 一个 0.00 价格。

**BUG-16（元数据缺失）港股财报 currency 列为空**
- `report income 03968/00700` 的 currency 字段为空（A 股正常显示 CNY）。腾讯/招行 H 实际以人民币列报，但 agent 无法从输出获知。

**BUG-17（静默空结果）`report indicator` 对港股返回空表**
- `report indicator 03968 --last 1` exit 0 但仅有元数据列、无任何指标（对照 600036 正常）。应像 bars/eastmoney 源那样明确报"港股不支持 indicator"，而非静默空表。

### 观察项（非缺陷，但值得知道）

- **OBS-1**：`search --format json` 字段名与表格列名不一致（`zwjc` vs `name`），agent 跨格式编程时需注意。
- **OBS-2**：report 多源 first-success-wins，同一命令两次运行 source 可能不同（eastmoney/sina）；重现实验务必 `--source` 固定。
- **OBS-3**：bars 周线/月线及区间查询的首行 `pct_change/change/amplitude` 恒为 0.00，日线 `--limit` 首行正常——行为不一致，疑似上游窗口边界特性。
- **OBS-6**：quote 解析错误消息将用户输入小写化（`AAPL` → `got "aapl"`）。
- **OBS-7**：`--start 20240101`（无连字符）被宽容解析为 2024-01-01，文档未说明。
- **OBS-8**：港股财报科目为港股准则命名（经营溢利/股东应占溢利/营业额/除税前溢利），与 A 股科目完全不同名；A/H 财报横比需要先建科目映射。
- **OBS-9**：A+H 公司的 H 股公告实际索引在 A 股代码下（`announce list 601318` 含"H股公告"），与 BUG-13 互为因果；公告查询入口建议统一走 A 股代码。
- **OBS-10**：北交所 920 代码切换后，`search` 会同时返回新旧两个代码（同 orgId），agent 需自行选新码（920xxx）。
- **OBS-11**：单标的 quote 偶发 `timeout: connect`（600036 一次失败、重试即恢复），批量调用建议加一次重试。
- **OBS-12**：eastmoney bars 源不支持港股（`missing data object`，exit 3）；auto 模式由 tencent 兜底正常，但显式 `--source eastmoney` + 港股会整体失败。
- **OBS-13**：`quote` 输出无币种列（price 对 A 股是 CNY、对港股是 HKD），多市场批量快照需 agent 自行按 symbol 后缀区分。

---

## 5. 复现环境说明

```bash
# 构建
cargo build && cargo build --release        # 首次约 6-12 分钟（DuckDB 静态链接）

# 自动化
cargo test                                  # 492 passed / 0 failed
cargo clippy --workspace --all-targets      # 0 warnings

# 手工用例统一缓存隔离
export HOME=/tmp/sift_test/home
SIFT=target/debug/sift
```

> 注意：`quote`/`bars` 为实时数据，本报告中的具体价格/成交量为 2026-07-17 收盘前后快照，复测时数值会变，断言应针对结构与自洽性（如 volume×price≈amount）而非具体数值。
